//! Codegen: walk a [`Function`] AST and emit the per-function asm bytes
//! BCC's `-S` would have produced. The format-emitter (`emit_s.rs`) calls
//! us between the file-level scaffolding (preamble + debug records +
//! segment scaffold) and the tail.
//!
//! Single-pass-ish shape: we don't build any IR; we walk the AST in
//! source order and write asm directly. Source-line comments are
//! emitted just before the asm for each new source line we encounter
//! (mirroring BCC's interleaving — see `specs/bcc/ASM_OUTPUT.md`).
//! There are two preparatory passes per function: a local-layout
//! analyzer (`locals.rs`) and a label planner (`plan.rs`).

use std::collections::HashMap;
use std::io::Write as _;

use crate::ast::{
    BinOp, Expr, ExprKind, Function, LogicalOp, Stmt, StmtKind, SwitchCase, Type, UnaryOp, Unit,
    UpdateOp, UpdatePosition,
};

/// Maps each function's name to the declared types of its parameters,
/// in source order. Built once per translation unit and consulted at
/// call sites so we know whether to push each argument as a byte or
/// a word (fixture 052: `f(1)` where `f` takes `char` becomes
/// `mov al,1 / push ax`, not `mov ax,1 / push ax`).
#[derive(Debug, Default)]
pub struct Signatures {
    map: HashMap<String, FunctionSig>,
}

#[derive(Debug)]
struct FunctionSig {
    params: Vec<Type>,
    ret_ty: Type,
}

impl Signatures {
    #[must_use]
    pub fn from_unit(unit: &Unit) -> Self {
        let map = unit
            .functions
            .iter()
            .map(|f| {
                (
                    f.name.clone(),
                    FunctionSig {
                        params: f.params.iter().map(|p| p.ty.clone()).collect(),
                        ret_ty: f.ret_ty.clone(),
                    },
                )
            })
            .collect();
        Self { map }
    }

    /// Look up the declared parameter types of a function. Returns
    /// `None` if the name isn't defined in this TU (extern function).
    /// Callers should default to `int` widths for missing signatures —
    /// we have no fixture for extern char-arg calls yet.
    #[must_use]
    pub fn params_of(&self, name: &str) -> Option<&[Type]> {
        self.map.get(name).map(|s| s.params.as_slice())
    }

    /// Look up the declared return type of a function. Returns `None`
    /// for unknown (extern) names. Used by call-site codegen to choose
    /// the right ABI shape for the return value (e.g. fixture 214 —
    /// stash DX:AX after a long-returning call).
    #[must_use]
    pub fn ret_ty_of(&self, name: &str) -> Option<&Type> {
        self.map.get(name).map(|s| &s.ret_ty)
    }
}

mod fold;
mod line_map;
mod locals;
mod plan;

use fold::try_const_eval;

/// Public re-export so the file-emitter can fold a global-variable
/// initializer down to its constant byte value.
#[must_use]
pub fn fold_const_global(expr: &crate::ast::Expr) -> Option<u32> {
    try_const_eval(expr)
}
use line_map::LineMap;
use locals::{LocalLocation, Locals, ParamLoad, Reg};

/// File-scope variable lookup. Built once per translation unit from
/// `Unit::globals` and consulted by codegen whenever an `Ident`
/// reference doesn't match a local — at which point the reference
/// lowers to `<width> ptr DGROUP:_<name>` instead of `[bp-N]`.
#[derive(Debug, Default)]
pub struct GlobalTable {
    map: HashMap<String, crate::ast::Type>,
}

impl GlobalTable {
    #[must_use]
    pub fn from_unit(unit: &Unit) -> Self {
        let map = unit
            .globals
            .iter()
            .map(|g| (g.name.clone(), g.ty.clone()))
            .collect();
        Self { map }
    }

    #[must_use]
    pub fn type_of(&self, name: &str) -> Option<&crate::ast::Type> {
        self.map.get(name)
    }

    /// Find a struct definition by its tag name. Some AST nodes
    /// (notably `Type::Pointer(Struct{name, fields:[], …})`) carry a
    /// name-only placeholder where the recursive struct definition
    /// would otherwise create a cycle. To resolve fields off such a
    /// pointer we look up a full instance of the same tag among the
    /// globals' types. Returns the first struct found whose tag
    /// matches; the struct definition is unique by tag at file scope.
    #[must_use]
    pub fn lookup_struct_by_tag(&self, tag: &str) -> Option<&crate::ast::Type> {
        fn find<'a>(ty: &'a crate::ast::Type, tag: &str) -> Option<&'a crate::ast::Type> {
            match ty {
                crate::ast::Type::Struct { name: Some(t), fields, .. }
                    if t == tag && !fields.is_empty() =>
                {
                    Some(ty)
                }
                crate::ast::Type::Struct { fields, .. } => {
                    fields.iter().find_map(|f| find(&f.ty, tag))
                }
                crate::ast::Type::Array { elem, .. } => find(elem, tag),
                crate::ast::Type::Pointer(inner) => find(inner, tag),
                _ => None,
            }
        }
        self.map.values().find_map(|t| find(t, tag))
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }
}

/// Accumulator for constant data encountered during codegen of a
/// translation unit: string literals **and** stack-array initializer
/// blobs. Each unique entry gets a stable byte offset within the
/// `s@` block; identical entries deduplicate. Emission of the actual
/// `db` block happens in the tail of the file (`emit_s.rs::write_tail`).
#[derive(Debug, Default)]
pub struct StringPool {
    /// Each entry: (raw bytes, whether to append a NUL terminator).
    /// Strings always set `nul = true`; array-init blobs set `nul =
    /// false` because the array's declared size already includes any
    /// trailing zeros. The running total of `bytes.len() + nul as
    /// usize` is the next available offset.
    entries: Vec<PoolEntry>,
}

#[derive(Debug, Clone)]
pub struct PoolEntry {
    pub bytes: Vec<u8>,
    pub nul: bool,
}

impl StringPool {
    /// Intern a NUL-terminated string literal. Returns the offset of
    /// the first byte within `s@`. Identical literals dedupe.
    pub fn intern(&mut self, bytes: &[u8]) -> u32 {
        self.intern_inner(bytes, true)
    }

    /// Intern a raw byte blob (e.g. a stack-array initializer image).
    /// No NUL terminator is appended — the blob's declared size is
    /// already baked into the bytes the caller supplies.
    pub fn intern_blob(&mut self, bytes: &[u8]) -> u32 {
        self.intern_inner(bytes, false)
    }

    fn intern_inner(&mut self, bytes: &[u8], nul: bool) -> u32 {
        let mut offset: u32 = 0;
        for existing in &self.entries {
            if existing.bytes.as_slice() == bytes && existing.nul == nul {
                return offset;
            }
            offset += u32::try_from(existing.bytes.len() + usize::from(existing.nul))
                .expect("pool offset fits in u32");
        }
        self.entries.push(PoolEntry { bytes: bytes.to_vec(), nul });
        offset
    }

    /// True when no entries have been interned. Tail emission can
    /// skip the `db` lines entirely in that case (matching the
    /// "empty s@ block" we used to always emit).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The interned entries in insertion order. Tail emission writes
    /// each as `db '<chars>'` (strings, plus an explicit `db 0`) or as
    /// a raw `db <byte>` sequence (blobs).
    #[must_use]
    pub fn entries(&self) -> &[PoolEntry] {
        &self.entries
    }
}

/// Format a bp-relative address: negative offsets are written
/// `[bp-N]`, positives `[bp+N]`. Used by every `word ptr` / `byte ptr`
/// memory operand a local/param produces.
fn bp_addr(off: i16) -> String {
    if off < 0 {
        format!("[bp-{}]", -i32::from(off))
    } else {
        format!("[bp+{off}]")
    }
}

/// Flatten an aggregate initializer to its little-endian byte image,
/// matching what BCC would place in the `s@` block / `_DATA` segment
/// for a stack array init.
///
/// Returns `None` if any element isn't constant-evaluatable (the
/// caller falls back to a more general lowering path or panics).
///
/// Handles:
///   - `InitList { items }` against an array — recurses element-wise
///     and zero-fills any trailing un-specified slots.
///   - `InitList { items }` against a struct — pairs items with fields
///     in declaration order; zero-fills missing fields and any trailing
///     end-padding the struct carries.
///   - `StringLit(bytes)` against a char array — writes bytes plus a
///     NUL, then zero-fills to the declared length (fixture 1476: `char
///     s[6] = "hi"` → `'h' 'i' 0 0 0 0`).
///   - Any scalar that `try_const_eval` accepts — written little-endian
///     as `ty.size_bytes()` bytes.
fn flatten_init_to_bytes(ty: &Type, init: &Expr) -> Option<Vec<u8>> {
    let total = usize::from(ty.size_bytes());
    let mut buf = vec![0u8; total];
    if write_init_bytes(&mut buf[..], ty, init)? {
        Some(buf)
    } else {
        None
    }
}

/// Write a single initializer into `dst` (already pre-zeroed). The
/// returned `bool` is always `true` on success; the `Option` shape
/// is reserved for "this initializer shape isn't constant" failures.
fn write_init_bytes(dst: &mut [u8], ty: &Type, init: &Expr) -> Option<bool> {
    debug_assert_eq!(dst.len(), usize::from(ty.size_bytes()));
    // String literal initializing a char array: copy bytes, append a
    // NUL, leave the rest zero (the pre-zeroed buffer already covers
    // any padding).
    if let (ExprKind::StringLit(bytes), Type::Array { elem, .. }) = (&init.kind, ty)
        && elem.is_char_like()
    {
        let take = bytes.len().min(dst.len());
        dst[..take].copy_from_slice(&bytes[..take]);
        // The NUL terminator slots in at `bytes.len()` if there's room.
        // For exactly-sized arrays where `bytes.len() == dst.len()` the
        // parser would have already widened the array; if we're called
        // with a too-tight array we just truncate the NUL away (mirrors
        // BCC behavior).
        return Some(true);
    }
    // Aggregate initializer list — recurse element-wise (arrays) or
    // field-wise (structs).
    if let ExprKind::InitList { items } = &init.kind {
        match ty {
            Type::Array { elem, len } => {
                let elem_size = usize::from(elem.size_bytes());
                for (i, item) in items.iter().enumerate() {
                    if i >= *len as usize {
                        break;
                    }
                    let start = i * elem_size;
                    let end = start + elem_size;
                    write_init_bytes(&mut dst[start..end], elem, item)?;
                }
                return Some(true);
            }
            Type::Struct { fields, .. } => {
                for (item, field) in items.iter().zip(fields.iter()) {
                    let start = usize::from(field.offset);
                    let end = start + usize::from(field.ty.size_bytes());
                    write_init_bytes(&mut dst[start..end], &field.ty, item)?;
                }
                return Some(true);
            }
            _ => return None,
        }
    }
    // Scalar: must fold to a constant. Long-like types get all 4 bytes;
    // ints/pointers get 2; chars get 1.
    let v = try_const_eval(init)?;
    for (i, slot) in dst.iter_mut().enumerate() {
        *slot = ((v >> (i * 8)) & 0xFF) as u8;
    }
    Some(true)
}

/// `DGROUP:_<sym>` or `DGROUP:_<sym>+<off>` — the asm-text form BCC
/// uses when addressing into a global's body at a known offset (long
/// halves, struct fields, array element bases). `off == 0` collapses
/// to the bare symbol; otherwise `+<off>` is appended.
fn global_offset_addr(sym: &str, off: i32) -> String {
    if off == 0 {
        format!("DGROUP:_{sym}")
    } else {
        format!("DGROUP:_{sym}+{off}")
    }
}

/// Given an asm address operand (one of: `DGROUP:_<sym>`,
/// `DGROUP:_<sym>+N`, `[bp-N]`, `[bp+N]`, `[<reg>]`, `[<reg>+N]`),
/// return the same operand shifted by +2 bytes. Used by the long-
/// field member-assign path to derive the high-half address from
/// the low-half address.
fn shift_dest_by_two(dest: &str) -> String {
    // `DGROUP:_<sym>` → `DGROUP:_<sym>+2`
    // `DGROUP:_<sym>+N` → `DGROUP:_<sym>+(N+2)`
    if let Some(rest) = dest.strip_prefix("DGROUP:_") {
        if let Some((sym, off)) = rest.split_once('+') {
            let n: i32 = off.parse().expect("global offset is integer");
            return format!("DGROUP:_{sym}+{}", n + 2);
        }
        return format!("DGROUP:_{rest}+2");
    }
    // `[bp-N]` → `[bp-(N-2)]` (less negative); `[bp+N]` → `[bp+(N+2)]`.
    if let Some(rest) = dest.strip_prefix("[bp") {
        let body = rest.strip_suffix(']').expect("malformed bp-relative dest");
        let n: i32 = body.parse().expect("bp offset is integer");
        let shifted = n + 2;
        return if shifted < 0 {
            format!("[bp{shifted}]")
        } else if shifted == 0 {
            "[bp]".to_owned()
        } else {
            format!("[bp+{shifted}]")
        };
    }
    // `[<reg>]` → `[<reg>+2]`; `[<reg>+N]` → `[<reg>+(N+2)]`.
    if let Some(inside) = dest.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if let Some((reg, off)) = inside.split_once('+') {
            let n: i32 = off.parse().expect("reg-indirect offset is integer");
            return format!("[{reg}+{}]", n + 2);
        }
        return format!("[{inside}+2]");
    }
    panic!("shift_dest_by_two: unsupported dest form `{dest}`");
}
use plan::{LabelPlan, SwitchStrategy};

/// Emit the per-function chunk of an `-S` file for one function.
pub fn emit_function(
    out: &mut Vec<u8>,
    source: &str,
    function: &Function,
    func_idx: u32,
    signatures: &Signatures,
    globals: &GlobalTable,
    strings: &mut StringPool,
    helpers: &mut std::collections::HashSet<String>,
) {
    let mut emitter = FunctionEmitter::new(
        out, source, function, func_idx, signatures, globals, strings, helpers,
    );
    emitter.run();
}

/// What BCC prepends to a C symbol when emitting it in the small memory
/// model.
pub fn function_symbol(name: &str) -> String {
    format!("_{name}")
}

struct FunctionEmitter<'a> {
    out: &'a mut Vec<u8>,
    source: &'a str,
    function: &'a Function,
    func_idx: u32,
    lines: LineMap,
    /// 1-based source line of the last comment we emitted, or 0 if we
    /// haven't emitted any comment yet for this function.
    current_line: u32,
    locals: Locals,
    label_plan: LabelPlan,
    signatures: &'a Signatures,
    globals: &'a GlobalTable,
    strings: &'a mut StringPool,
    /// Stack of enclosing loop targets so `break;` / `continue;`
    /// statements can look up their jump destination. The innermost
    /// loop sits at the top (index `len()-1`).
    loop_stack: Vec<LoopTargets>,
    /// Data labels emitted between `_main endp` and `?debug C E9`,
    /// staged here while the function body is being emitted. Used by
    /// the jump-table and linear-search switch strategies, both of
    /// which need a `@<func>@C<num> label word / dw / db` block after
    /// the function ends. Empty for most functions.
    post_function_data: Vec<u8>,
    /// Runtime-helper symbols this function references (e.g.
    /// `N_LXLSH@` for long left-shift). Shared across all functions
    /// in the TU so the tail-emitter can declare each one once and
    /// merge them into the publics ordering. Fixture 228.
    helpers: &'a mut std::collections::HashSet<String>,
    /// When true, `emit_widen_al` is a no-op. Set during char-return
    /// value emission since the ABI leaves AH garbage — the caller
    /// widens with `cbw` after the call.
    skip_widen: bool,
    /// When true, the int-Mod emission in `emit_op_with_source` skips
    /// the trailing `mov ax, dx`. The caller then reads the remainder
    /// directly from DX (e.g. to store via `mov [mem], dx`).
    skip_mod_to_ax: bool,
}

/// Innermost enclosing construct that catches `break;` (and maybe
/// `continue;`). Pushed for `while` / `do-while` / `for` / `switch`.
/// For switches, `continue_target_slot` is `None` — a `continue;` in
/// a switch body threads past the switch to the enclosing loop.
#[derive(Clone, Copy)]
struct LoopTargets {
    break_target_slot: u32,
    continue_target_slot: Option<u32>,
}

impl<'a> FunctionEmitter<'a> {
    fn new(
        out: &'a mut Vec<u8>,
        source: &'a str,
        function: &'a Function,
        func_idx: u32,
        signatures: &'a Signatures,
        globals: &'a GlobalTable,
        strings: &'a mut StringPool,
        helpers: &'a mut std::collections::HashSet<String>,
    ) -> Self {
        Self {
            out,
            source,
            function,
            func_idx,
            lines: LineMap::new(source),
            current_line: 0,
            locals: Locals::analyze(function),
            label_plan: LabelPlan::build(function),
            signatures,
            globals,
            strings,
            loop_stack: Vec::new(),
            post_function_data: Vec::new(),
            helpers,
            skip_widen: false,
            skip_mod_to_ax: false,
        }
    }

    fn exit_label_num(&self) -> u32 {
        LabelPlan::label_number(self.label_plan.exit_slot())
    }

    fn emit_label(&mut self, slot: u32) {
        let n = LabelPlan::label_number(slot);
        let _ = write!(self.out, "@{}@{n}:\r\n", self.func_idx);
    }

    fn label_ref(&self, slot: u32) -> String {
        format!("@{}@{}", self.func_idx, LabelPlan::label_number(slot))
    }

    fn run(&mut self) {
        // Header line: emit `;` comment block for the line where the
        // function definition starts, before the prologue.
        let head_line = self.lines.line_of(self.function.span.start);
        self.advance_to_line(head_line);

        self.out.extend_from_slice(b"\tassume\tcs:_TEXT\r\n");
        let sym = function_symbol(&self.function.name);
        let _ = write!(self.out, "{sym}\tproc\tnear\r\n");

        // Prologue. Order: push bp / mov bp,sp / allocate stack /
        // push callee-saved registers (in order). See
        // specs/bcc/ASM_OUTPUT.md "Prologue and epilogue shape".
        self.out.extend_from_slice(b"\tpush\tbp\r\n");
        self.out.extend_from_slice(b"\tmov\tbp,sp\r\n");
        match self.locals.stack_bytes() {
            0 => {}
            n @ 1..=2 => {
                for _ in 0..n {
                    self.out.extend_from_slice(b"\tdec\tsp\r\n");
                }
            }
            n => {
                let _ = write!(self.out, "\tsub\tsp,{n}\r\n");
            }
        }
        for reg in self.locals.saved_regs() {
            let _ = write!(self.out, "\tpush\t{}\r\n", reg.name());
        }
        // Register-promoted incoming parameters: copy each from its
        // caller-built stack slot into its assigned register. Byte
        // registers (char params) load from `byte ptr` — the caller
        // pushes a full word but only the low byte is meaningful for
        // a char arg (fixture 052).
        let param_loads: Vec<ParamLoad> = self.locals.param_loads().to_vec();
        for pl in &param_loads {
            let width = if pl.reg.is_byte() { "byte" } else { "word" };
            let _ = write!(
                self.out,
                "\tmov\t{},{width} ptr [bp+{}]\r\n",
                pl.reg.name(),
                pl.incoming_offset,
            );
        }

        // Body.
        for stmt in self.function.body.as_deref().unwrap_or(&[]) {
            self.emit_stmt(stmt);
        }

        // Single exit label.
        self.emit_label(self.label_plan.exit_slot());

        // Closing-brace line gets its own comment block. Span end is the
        // byte just past `}`, so back up by one to get the brace itself.
        let close_offset = self.function.span.end.saturating_sub(1);
        let close_line = self.lines.line_of(close_offset);
        self.advance_to_line(close_line);

        // Epilogue: reverse of the prologue.
        let saved: Vec<Reg> = self.locals.saved_regs().to_vec();
        for reg in saved.iter().rev() {
            let _ = write!(self.out, "\tpop\t{}\r\n", reg.name());
        }
        if self.locals.stack_bytes() > 0 {
            self.out.extend_from_slice(b"\tmov\tsp,bp\r\n");
        }
        self.out.extend_from_slice(b"\tpop\tbp\r\n");
        self.out.extend_from_slice(b"\tret\t\r\n");

        let _ = write!(self.out, "{sym}\tendp\r\n");
        // Switch jump-tables and linear-search address tables live
        // between `_main endp` and the next `?debug C E9` line. They
        // were staged into `post_function_data` while the body was
        // emitted (see `emit_switch_jump_table` / `_linear_search`).
        self.out.extend_from_slice(&self.post_function_data);
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Empty => {
                // `;` produces no asm. Fixture 522.
            }
            StmtKind::Return(value) => {
                self.advance_to_stmt_line(stmt);
                self.emit_return_value_load(value.as_ref());
                let exit = self.exit_label_num();
                let _ = write!(self.out, "\tjmp\tshort @{}@{exit}\r\n", self.func_idx);
            }
            StmtKind::Declare { name, init, ty, is_static } => {
                // Static locals are hoisted by the parser into the
                // unit's globals list, so the initializer is emitted
                // once at file scope (load-time) rather than on every
                // function entry. No per-call asm to emit here.
                if *is_static {
                    // The Declare stays in the AST so source-line
                    // tracking can fold its line into the next comment
                    // block, matching BCC's behavior for unused locals.
                } else if let Some(init) = init {
                    // Only emit the source-comment block when there's
                    // actually some asm to label. A declaration with no
                    // initializer produces no code, and BCC folds its
                    // source line into the next comment block (fixture
                    // 061: `int i; int sum = 0;` emits both lines in
                    // one block before `xor di,di`).
                    self.advance_to_stmt_line(stmt);
                    let loc = self.locals.location_of(name);
                    self.emit_init_local(loc, ty, init);
                }
            }
            StmtKind::Assign { name, value } => {
                self.advance_to_stmt_line(stmt);
                // A local shadows a global of the same name (fixture
                // 532). Check locals first.
                if self.locals.has(name) {
                    let loc = self.locals.location_of(name);
                    let ty = self.locals.type_of(name).clone();
                    self.emit_assign_local(loc, &ty, value);
                } else if self.globals.contains(name) {
                    self.emit_assign_global(name, value);
                } else {
                    let loc = self.locals.location_of(name);
                    let ty = self.locals.type_of(name).clone();
                    self.emit_assign_local(loc, &ty, value);
                }
            }
            StmtKind::CompoundAssign { name, op, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_compound_assign(name, *op, value);
            }
            StmtKind::ArrayAssign { array, indices, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_array_assign(array, indices, value);
            }
            StmtKind::ArrayCompoundAssign { array, indices, op, value, from_postfix } => {
                self.advance_to_stmt_line(stmt);
                self.emit_array_compound_assign(array, indices, *op, value, *from_postfix);
            }
            StmtKind::MemberArrayAssign { base, field, indices, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_member_array_assign(base, field, indices, value);
            }
            StmtKind::DerefAssign { target, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_deref_assign(target, value);
            }
            StmtKind::DerefCompoundAssign { target, op, value, from_postfix } => {
                self.advance_to_stmt_line(stmt);
                self.emit_deref_compound_assign(target, *op, value, *from_postfix);
            }
            StmtKind::MemberAssign { base, field, kind, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_member_assign(base, field, *kind, value);
            }
            StmtKind::MemberCompoundAssign { base, field, kind, op, value, from_postfix } => {
                self.advance_to_stmt_line(stmt);
                self.emit_member_compound_assign(base, field, *kind, *op, value, *from_postfix);
            }
            StmtKind::If { cond, then_branch, else_branch } => {
                self.advance_to_stmt_line(stmt);
                self.emit_if(stmt.span.start, cond, then_branch, else_branch.as_deref());
            }
            StmtKind::While { cond, body } => {
                // Don't emit a comment block for the `while` header
                // itself — BCC merges it with the body's first source
                // line via the body label.
                self.emit_while(stmt.span.start, cond, body);
            }
            StmtKind::DoWhile { body, cond } => {
                self.emit_do_while(stmt.span.start, body, cond);
            }
            StmtKind::For { init, cond, step, body } => {
                self.emit_for(
                    stmt.span.start,
                    init.as_deref(),
                    cond.as_ref(),
                    step.as_deref(),
                    body,
                );
            }
            StmtKind::Switch { scrutinee, cases } => {
                self.emit_switch(stmt.span.start, scrutinee, cases);
            }
            StmtKind::Break => {
                self.advance_to_stmt_line(stmt);
                let target = self.loop_stack.last().expect(
                    "`break;` outside any enclosing loop — parser should reject this",
                ).break_target_slot;
                let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(target));
            }
            StmtKind::Continue => {
                self.advance_to_stmt_line(stmt);
                // Walk outward looking for the topmost frame whose
                // continue-slot is `Some(...)` — switch frames have
                // `None` and get skipped.
                let target = self
                    .loop_stack
                    .iter()
                    .rev()
                    .find_map(|f| f.continue_target_slot)
                    .expect("`continue;` outside any enclosing loop — parser should reject this");
                let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(target));
            }
            StmtKind::ExprStmt(expr) => {
                self.advance_to_stmt_line(stmt);
                self.emit_expr_discard(expr);
            }
            StmtKind::Goto { label } => {
                self.advance_to_stmt_line(stmt);
                let _ = write!(
                    self.out,
                    "\tjmp\tshort @{}@user_{label}\r\n",
                    self.func_idx,
                );
            }
            StmtKind::Label { name } => {
                self.advance_to_stmt_line(stmt);
                let _ = write!(self.out, "@{}@user_{name}:\r\n", self.func_idx);
            }
        }
    }

    /// Emit `expr` for its side effects, discarding the value. The
    /// special case is `Update` (`++x;` / `x++;`): BCC emits just the
    /// increment, no `mov ax, ...` afterward (fixture 040). Likewise
    /// for an assignment expression in a `for`-clause: emit the
    /// side-effect store, no value-load afterward.
    fn emit_expr_discard(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Update { target, op, position } => {
                self.emit_update_in_place(target, *op, *position);
            }
            ExprKind::AssignExpr { target, value } => {
                let loc = self.locals.location_of(target);
                let ty = self.locals.type_of(target).clone();
                self.emit_assign_local(loc, &ty, value);
            }
            ExprKind::Comma { left, right } => {
                // Both halves of a comma in discard position are
                // themselves discarded — neither contributes a value.
                // Fixture 469's `a = 1, b = 2, ...` chain.
                self.emit_expr_discard(left);
                self.emit_expr_discard(right);
            }
            _ => {
                self.emit_expr_to_ax(expr);
            }
        }
    }

    /// Emit just the increment/decrement on the named local — no
    /// load-to-AX. Used by `ExprStmt` and by the "first half" of
    /// pre-form Update in expression position.
    ///
    /// Int register: direct `inc/dec <reg>` (fixture 040).
    /// Char register: round-trip through AL — `mov al, <reg> /
    /// inc/dec al / mov <reg>, al` (fixture 047). BCC does not use
    /// `inc/dec <byte-reg>` directly.
    fn emit_update_in_place(&mut self, name: &str, op: UpdateOp, position: UpdatePosition) {
        // Long globals (`g++` / `g--`) use a memory-direct
        // add/adc pair (or sub/sbb for `--`). Acts on memory
        // without loading into registers. Fixture 249 (`g++`).
        if let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
        {
            let (lo_op, hi_op) = match op {
                UpdateOp::Inc => ("add", "adc"),
                UpdateOp::Dec => ("sub", "sbb"),
            };
            let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{name},1\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr DGROUP:_{name}+2,0\r\n");
            return;
        }
        let mnemonic = match op {
            UpdateOp::Inc => "inc",
            UpdateOp::Dec => "dec",
        };
        // Int/char globals: memory-direct `inc word ptr DGROUP:_g`
        // (or `dec`). Fixture 512 (`g++; g++; return g;`).
        // Global pointers scale by sizeof(pointee) — `++p` on `int
        // *p` adds 2, lowering to `add word ptr [_p], 2` rather
        // than `inc`. Fixture 561.
        if let Some(gty) = self.globals.type_of(name) {
            if let Some(pointee) = gty.pointee() {
                let stride = u32::from(pointee.size_bytes());
                if stride != 1 {
                    let arith = match op {
                        UpdateOp::Inc => "add",
                        UpdateOp::Dec => "sub",
                    };
                    let _ = write!(
                        self.out,
                        "\t{arith}\tword ptr DGROUP:_{name},{stride}\r\n",
                    );
                    return;
                }
            }
            // Byte globals split on pre vs post:
            //  - Pre (`++g`): AL load-modify-store — `mov al, _g;
            //    inc al; mov _g, al`. BCC keeps the new value in AL
            //    even when the expression is discarded. Fixture
            //    700.
            //  - Post (`g++`) when discarded: memory-direct
            //    `inc byte ptr _g`. BCC notices the old value
            //    isn't materialized. Fixture 702.
            //
            // (The post-not-discarded case lands at the
            // expression-context update path, not here.)
            if gty.is_char_like() {
                if matches!(position, UpdatePosition::Pre) {
                    let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
                    let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                    let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
                } else {
                    let _ = write!(self.out, "\t{mnemonic}\tbyte ptr DGROUP:_{name}\r\n");
                }
                return;
            }
            let _ = write!(self.out, "\t{mnemonic}\tword ptr DGROUP:_{name}\r\n");
            return;
        }
        // Pointer increment / decrement uses the pointee's size as
        // stride. For `int *p`, `p++` becomes `inc reg / inc reg`
        // (the +2 peephole — 2 bytes vs. 3 for `add reg, 2`),
        // matching fixture 090. For `char *s`, `s++` is a single
        // `inc reg` (stride 1), fixture 093.
        let stride = self
            .locals
            .type_of(name)
            .pointee()
            .map_or(1, |p| u32::from(p.size_bytes()));
        match self.locals.location_of(name) {
            LocalLocation::Reg(reg) if reg.is_byte() => {
                // Pre vs post matters for byte-register stmt-position
                // updates even when the value is discarded:
                //  - Pre (`++c;`): BCC stages through AL — `mov al,
                //    <reg>; inc al; mov <reg>, al`. Fixture 047,
                //    050–054, etc.
                //  - Post (`c++;`): direct `inc <reg>` / `dec <reg>`.
                //    The byte-register form is preferred without the
                //    AL detour. Fixture 1056.
                if matches!(position, UpdatePosition::Pre) {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                    let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                    let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
                } else {
                    let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                }
            }
            LocalLocation::Reg(reg) => {
                // Pointer stride peephole: K=1 → `inc <reg>` (1 byte);
                // K=2 → two `inc`s (2 bytes); K≥3 → `add <reg>, K`
                // (3 bytes — same as the int compound ±K peephole).
                // Stride 4 (long pointer) crosses the threshold: 4
                // incs cost 4 bytes vs `add reg, 4` at 3. Fixture 313.
                let add_mnem = match op {
                    UpdateOp::Inc => "add",
                    UpdateOp::Dec => "sub",
                };
                if stride <= 2 {
                    for _ in 0..stride {
                        let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                    }
                } else {
                    let _ = write!(self.out, "\t{add_mnem}\t{},{stride}\r\n", reg.name());
                }
            }
            LocalLocation::Stack(off) => {
                let ty = self.locals.type_of(name).clone();
                // Long stack-local ++/-- — memory-direct add/adc 1 (or
                // sub/sbb 1) on the two halves. Identical to the
                // `x += 1` compound shape (fixtures 290, 291). Pre and
                // post are byte-identical when the value is discarded.
                if ty.is_long_like() {
                    let (lo_op, hi_op) = match op {
                        UpdateOp::Inc => ("add", "adc"),
                        UpdateOp::Dec => ("sub", "sbb"),
                    };
                    let _ = write!(self.out, "\t{lo_op}\tword ptr {},1\r\n", bp_addr(off));
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {},0\r\n", bp_addr(off + 2));
                    return;
                }
                // Stack-resident ++/-- on a char uses the AL round-trip
                // (fixture 055). Stack ints are still unobserved — keep
                // the panic until a fixture forces us there.
                assert!(
                    ty.is_char_like(),
                    "++/-- on a stack-resident int not yet supported (no fixture)"
                );
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
            }
        }
    }

    fn advance_to_stmt_line(&mut self, stmt: &Stmt) {
        let line = self.lines.line_of(stmt.span.start);
        self.advance_to_line(line);
    }

    fn emit_if(
        &mut self,
        if_span_start: u32,
        cond: &Expr,
        then_branch: &[Stmt],
        else_branch: Option<&[Stmt]>,
    ) {
        // `if (K)` with K a constant — BCC elides the compare/branch
        // entirely. Non-zero K: emit the then-branch inline (the else,
        // if any, is unreachable and BCC drops it too). Zero K: emit
        // only the else-branch. No labels are reserved by the if at
        // all; statements after the if continue sequentially.
        // Fixture 931.
        if let Some(v) = try_const_eval(cond) {
            if v != 0 {
                for s in then_branch {
                    self.emit_stmt(s);
                }
            } else if let Some(else_stmts) = else_branch {
                for s in else_stmts {
                    self.emit_stmt(s);
                }
            }
            return;
        }
        let base = self.label_plan.base(if_span_start);
        // When the cond's outermost operator is `||`, the operands may
        // short-circuit-to-true; we need a label at the start of the
        // then-branch for them to land at. The if's base+0 slot —
        // unused for plain conds — serves as that "then-entry".
        //
        // Same need for signed long-vs-long compares (fixture 234):
        // BCC's 3-jump pattern includes a `jl/jg` direct-to-body
        // jump alongside the false-target jumps, so the body needs
        // an explicit label.
        let cond_has_top_or = matches!(
            cond.kind,
            ExprKind::Logical { op: LogicalOp::Or, .. }
        );
        let needs_then_entry = cond_has_top_or
            || self.is_long_signed_globals_cmp(cond)
            || self.is_long_signed_const_cmp(cond)
            || self.is_long_vs_int_cmp(cond)
            || self.is_long_vs_int_ne(cond)
            || self.is_long_ne_const(cond);
        let then_entry_slot = if needs_then_entry { Some(base) } else { None };

        if let Some(else_stmts) = else_branch {
            // if/else reserves 3 slots; the else label lives at +2,
            // the merge label at +1. The then-branch's trailing jump
            // targets the merge so any post-if-else code (e.g. a
            // following `return r;` that loads AX) executes for both
            // branches. Fixtures 2393, 2419, 2434, 2461.
            let else_slot = base + 2;
            let merge_slot = base + 1;
            self.emit_cond_branch(cond, then_entry_slot, Some(else_slot));
            if let Some(slot) = then_entry_slot {
                self.emit_label(slot);
            }
            for s in then_branch {
                self.emit_stmt(s);
            }
            let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(merge_slot));
            self.emit_label(else_slot);
            for s in else_stmts {
                self.emit_stmt(s);
            }
            self.emit_label(merge_slot);
        } else {
            // if (no else) reserves 2 slots; skip label at +1.
            let skip_slot = base + 1;
            self.emit_cond_branch(cond, then_entry_slot, Some(skip_slot));
            if let Some(slot) = then_entry_slot {
                self.emit_label(slot);
            }
            for s in then_branch {
                self.emit_stmt(s);
            }
            self.emit_label(skip_slot);
        }
    }

    fn emit_while(&mut self, while_span_start: u32, cond: &Expr, body: &[Stmt]) {
        let plan = self.label_plan.loop_plan(while_span_start);
        // `while (<a && b>) { ... }` / `while (<a || b>) { ... }` —
        // short-circuit condition. Use the same recursive lowering as
        // `if (a && b) ...`: the body label is the true target, the
        // break-target label is the false target. The break-target
        // label needs to be emitted unconditionally for this shape
        // since the cond reaches it on the false path. Fixtures 1273,
        // 1352, 2203.
        if matches!(cond.kind, ExprKind::Logical { .. }) {
            let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(plan.check_slot));
            self.emit_label(plan.body_slot);
            self.loop_stack.push(LoopTargets {
                break_target_slot: plan.break_target_slot,
                continue_target_slot: Some(plan.continue_target_slot),
            });
            for s in body {
                self.emit_stmt(s);
            }
            self.loop_stack.pop();
            self.emit_label(plan.check_slot);
            self.emit_cond_branch(
                cond,
                Some(plan.body_slot),
                Some(plan.break_target_slot),
            );
            self.emit_label(plan.break_target_slot);
            return;
        }
        // `while (0)` — BCC still emits the trampoline jump and the
        // body bytes, but elides the check label and the back-edge
        // jump (since the cond is always false, nothing would branch
        // there). Net shape: `jmp past-body / body...` and that's it.
        // Fixture 1587.
        if matches!(try_const_eval(cond), Some(0)) {
            let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(plan.check_slot));
            self.emit_label(plan.body_slot);
            self.loop_stack.push(LoopTargets {
                break_target_slot: plan.break_target_slot,
                continue_target_slot: Some(plan.continue_target_slot),
            });
            for s in body {
                self.emit_stmt(s);
            }
            self.loop_stack.pop();
            self.emit_label(plan.check_slot);
            if body_has_break(body) {
                self.emit_label(plan.break_target_slot);
            }
            return;
        }
        // `while (K)` with K constant non-zero — BCC elides both the
        // trampoline jump and the check label, leaving just `body /
        // jmp body`. Continue jumps to body_slot directly. Fixture
        // 599 (`while (1) { ... break; ... }`).
        if matches!(try_const_eval(cond), Some(v) if v != 0) {
            self.emit_label(plan.body_slot);
            self.loop_stack.push(LoopTargets {
                break_target_slot: plan.break_target_slot,
                continue_target_slot: Some(plan.body_slot),
            });
            for s in body {
                self.emit_stmt(s);
            }
            self.loop_stack.pop();
            let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(plan.body_slot));
            if body_has_break(body) {
                self.emit_label(plan.break_target_slot);
            }
            return;
        }
        // Trampoline jump to the check, then body label.
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(plan.check_slot));
        self.emit_label(plan.body_slot);
        self.loop_stack.push(LoopTargets {
            break_target_slot: plan.break_target_slot,
            continue_target_slot: Some(plan.continue_target_slot),
        });
        for s in body {
            self.emit_stmt(s);
        }
        self.loop_stack.pop();
        self.emit_label(plan.check_slot);
        self.emit_cond_branch(cond, Some(plan.body_slot), None);
        // Break-target label: emitted only if the body actually
        // contained `break;` (BCC suppresses the label otherwise —
        // fixtures 027 vs 063).
        if body_has_break(body) {
            self.emit_label(plan.break_target_slot);
        }
    }

    fn emit_do_while(&mut self, do_span_start: u32, body: &[Stmt], cond: &Expr) {
        assert!(
            !matches!(cond.kind, ExprKind::Logical { .. }),
            "logical condition (`&&`/`||`) in a `do-while` not yet supported (no fixture)"
        );
        let plan = self.label_plan.loop_plan(do_span_start);
        self.emit_label(plan.body_slot);
        self.loop_stack.push(LoopTargets {
            break_target_slot: plan.break_target_slot,
            continue_target_slot: Some(plan.continue_target_slot),
        });
        for s in body {
            self.emit_stmt(s);
        }
        self.loop_stack.pop();
        // `continue` inside a do-while jumps to the slot just before
        // the cmp/jump (it doubles as the check label). Only emit the
        // label if the body actually uses it (fixture 186).
        if body_has_continue(body) {
            self.emit_label(plan.continue_target_slot);
        }
        // `do {} while (K)` — constant condition collapses the test:
        // `K != 0` becomes an unconditional `jmp body` (fixture 1589);
        // `K == 0` runs the body exactly once with no test/branch
        // emitted at all (fixture 1588).
        if let Some(v) = try_const_eval(cond) {
            let cond_line = self.lines.line_of(cond.span.start);
            self.advance_to_line(cond_line);
            if v != 0 {
                let _ = write!(
                    self.out,
                    "\tjmp\tshort {}\r\n",
                    self.label_ref(plan.body_slot),
                );
            }
            if body_has_break(body) {
                self.emit_label(plan.break_target_slot);
            }
            return;
        }
        // Advance to the `while (cond);` line — it should appear as a
        // comment block before the cmp/jump (fixture 062).
        let cond_line = self.lines.line_of(cond.span.start);
        self.advance_to_line(cond_line);
        self.emit_cond_branch(cond, Some(plan.body_slot), None);
        if body_has_break(body) {
            self.emit_label(plan.break_target_slot);
        }
    }

    fn emit_for(
        &mut self,
        for_span_start: u32,
        init: Option<&[Expr]>,
        cond: Option<&Expr>,
        step: Option<&[Expr]>,
        body: &[Stmt],
    ) {
        let plan = self.label_plan.loop_plan(for_span_start);
        // Init runs once, before the loop. Comma-separated clauses
        // are emitted in source order; their values are discarded.
        if let Some(exprs) = init {
            self.advance_to_for_header_line(for_span_start);
            for e in exprs {
                self.emit_expr_discard(e);
            }
        }
        // Trampoline jump to the check. Skip when the cond is absent
        // (`for(;;)`) — the body and check coincide so there's no
        // condition to jump to. Fixture 507.
        if cond.is_some() {
            let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(plan.check_slot));
        }
        self.emit_label(plan.body_slot);
        self.loop_stack.push(LoopTargets {
            break_target_slot: plan.break_target_slot,
            continue_target_slot: Some(plan.continue_target_slot),
        });
        for s in body {
            self.emit_stmt(s);
        }
        self.loop_stack.pop();
        // Step runs after each iteration of the body. Inlined here —
        // no separate label (continue uses the continue_target_slot
        // which sits before any step code; only emitted if continue
        // is present).
        if body_has_continue(body) {
            self.emit_label(plan.continue_target_slot);
        }
        if let Some(exprs) = step {
            for e in exprs {
                self.emit_expr_discard(e);
            }
        }
        self.emit_label(plan.check_slot);
        if let Some(c) = cond {
            self.emit_cond_branch(c, Some(plan.body_slot), None);
        } else {
            // Missing cond means infinite loop — unconditional back-jump.
            let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(plan.body_slot));
        }
        if body_has_break(body) {
            self.emit_label(plan.break_target_slot);
        }
    }

    /// `for`'s header source-line is the `for` keyword's line. The
    /// init expression doesn't have its own statement span, so we
    /// advance the comment cursor manually using the for's span.
    fn advance_to_for_header_line(&mut self, for_span_start: u32) {
        let line = self.lines.line_of(for_span_start);
        self.advance_to_line(line);
    }

    /// Emit a `switch`. Three dispatch strategies are observable; we
    /// currently implement only the **chained** one (fixtures 072,
    /// 075). The shape (fixture 072: 3 cases including a `case 0`,
    /// no default):
    ///
    /// ```text
    ///   ; switch (x) {       ← header source-line block
    ///   mov ax, word ptr [bp-2]   ; load scrutinee
    ///   or  ax, ax                ; case 0 uses `or` (peephole, fixture 035)
    ///   je  short <case 0 body>
    ///   cmp ax, 1
    ///   je  short <case 1 body>
    ///   …
    ///   jmp short <end>           ; or <default body> when present
    /// <case 0 body>:
    ///   ;     case 0: ...
    ///   <body>
    ///   jmp short <end>           ; from `break;`
    /// …
    /// <end>:
    /// ```
    ///
    /// Cases are emitted in source order; the default case is placed
    /// inline at its source position (fixture 075 puts it last because
    /// that's where it appears in C). With no `break;` at the end of
    /// a case body, control falls into the next case's label (the
    /// fixture for that combination is 076, which uses the jump-table
    /// strategy — chained-fallthrough is implied but unobserved).
    fn emit_switch(&mut self, switch_span_start: u32, scrutinee: &Expr, cases: &[SwitchCase]) {
        let plan = self.label_plan.switch_plan(switch_span_start).clone();
        self.advance_to_stmt_line_at(switch_span_start);
        match plan.strategy {
            SwitchStrategy::Chained => {
                self.emit_switch_chained(scrutinee, cases, &plan.case_slots, plan.end_slot);
            }
            SwitchStrategy::JumpTable => {
                self.emit_switch_jump_table(scrutinee, cases, &plan.case_slots, plan.end_slot);
            }
            SwitchStrategy::LinearSearch => {
                self.emit_switch_linear_search(
                    switch_span_start,
                    scrutinee,
                    cases,
                    &plan.case_slots,
                    plan.end_slot,
                );
            }
        }
        self.emit_label(plan.end_slot);
    }

    /// Emit the chained-compare dispatch and all case bodies. After
    /// this returns, the caller emits the end-of-switch label.
    fn emit_switch_chained(
        &mut self,
        scrutinee: &Expr,
        cases: &[SwitchCase],
        case_slots: &[u32],
        end_slot: u32,
    ) {
        // If every case is `default:` (no value-bearing cases),
        // there's no dispatch — BCC skips the scrutinee load and
        // emits a trampoline `jmp default` followed by the default
        // body. The jmp is `eb 00` (jump to the next instruction);
        // it's a no-op but BCC keeps it for shape consistency.
        // Fixtures 1608, 2720.
        let has_value_case = cases.iter().any(|c| c.value.is_some());
        if !has_value_case {
            let default_slot = case_slots[0];
            let _ = write!(
                self.out,
                "\tjmp\tshort {}\r\n",
                self.label_ref(default_slot),
            );
            self.loop_stack.push(LoopTargets {
                break_target_slot: end_slot,
                continue_target_slot: None,
            });
            for (case, &slot) in cases.iter().zip(case_slots) {
                self.emit_label(slot);
                for stmt in &case.body {
                    self.emit_stmt(stmt);
                }
            }
            self.loop_stack.pop();
            return;
        }
        // Load scrutinee into AX. Most cases are bare idents (with
        // char-vs-int-vs-global routing), but non-trivial expressions
        // like `switch (x + 1)` fall through to the generic
        // expression evaluator. Fixture 544.
        let scrut_loaded = match &scrutinee.kind {
            ExprKind::Ident(_) => false,
            _ => {
                self.emit_expr_to_ax(scrutinee);
                true
            }
        };
        if !scrut_loaded {
        let ExprKind::Ident(name) = &scrutinee.kind else {
            unreachable!();
        };
        if let Some(gty) = self.globals.type_of(name) {
            assert!(
                matches!(gty, Type::Int | Type::UInt),
                "non-int global switch scrutinee not yet supported (no fixture)"
            );
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
        } else {
            let ty = self.locals.type_of(name).clone();
            // Char local: load AL then widen (cbw for signed, mov
            // ah,0 for unsigned). Fixture 527 (`switch (c) { case
            // 'A': ... }`).
            if ty.is_char_like() {
                match self.locals.location_of(name) {
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                    }
                    LocalLocation::Reg(reg) => {
                        let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                    }
                }
                self.emit_widen_al(&ty);
            } else {
                assert!(
                    matches!(ty, Type::Int),
                    "non-int local switch scrutinee not yet supported (no fixture)"
                );
                match self.locals.location_of(name) {
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                    }
                    LocalLocation::Reg(reg) => {
                        let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    }
                }
            }
        }
        }
        // Compare/branch chain: one cmp+je per non-default case in
        // source order. `case 0` uses `or ax,ax` (cf. fixture 072).
        let default_slot = cases
            .iter()
            .zip(case_slots)
            .find_map(|(c, &s)| c.value.is_none().then_some(s));
        for (case, &slot) in cases.iter().zip(case_slots) {
            let Some(v) = case.value else { continue };
            let v16 = v & 0xFFFF;
            if v16 == 0 {
                self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            } else {
                let _ = write!(self.out, "\tcmp\tax,{v16}\r\n");
            }
            let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(slot));
        }
        // Trailing jmp: to default body if present, else end-of-switch.
        let trailing = default_slot.unwrap_or(end_slot);
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(trailing));
        // Case bodies in source order. `break;` translates to a
        // `jmp short <end>` via the loop_stack frame we push below.
        self.loop_stack.push(LoopTargets {
            break_target_slot: end_slot,
            continue_target_slot: None,
        });
        for (case, &slot) in cases.iter().zip(case_slots) {
            self.emit_label(slot);
            let case_line = self.lines.line_of(case.span.start);
            self.advance_to_line(case_line);
            for s in &case.body {
                self.emit_stmt(s);
            }
        }
        self.loop_stack.pop();
    }

    /// Like `advance_to_stmt_line(stmt)`, but called with just the
    /// span start when the caller doesn't have the full `Stmt`.
    fn advance_to_stmt_line_at(&mut self, span_start: u32) {
        let line = self.lines.line_of(span_start);
        self.advance_to_line(line);
    }

    /// Emit the dense-jump-table dispatch (fixtures 073, 076). All
    /// cases must be values `0..N-1` in source order; the planner
    /// only picks this strategy when that holds.
    ///
    /// ```text
    ///   mov bx, <scrutinee>
    ///   cmp bx, <N-1>
    ///   ja  short <end>
    ///   shl bx, 1
    ///   jmp word ptr cs:@<func>@C<num>[bx]
    /// <case 0>:
    ///   <body>            ; falls through to next label unless body breaks
    /// <case 1>:
    ///   <body>
    /// …
    /// <end>:
    /// ```
    ///
    /// After `_main endp` (staged in `post_function_data`):
    /// ```text
    /// @<func>@C<num>	label	word
    ///   dw @<func>@<case 0 slot>
    ///   …
    /// ```
    ///
    /// The dispatch loads the scrutinee into BX (not AX) because
    /// `jmp word ptr cs:LBL[bx]` is the only encoding that lets us
    /// index a code-segment table with a register. We currently
    /// assume BX is not allocated to a local — when it is, BCC
    /// would presumably save/restore it, but we have no fixture.
    fn emit_switch_jump_table(
        &mut self,
        scrutinee: &Expr,
        cases: &[SwitchCase],
        case_slots: &[u32],
        end_slot: u32,
    ) {
        // Sanity: planner picked this strategy only for dense 0..N-1.
        let n = cases.len();
        for (i, c) in cases.iter().enumerate() {
            let expected = u32::try_from(i).unwrap_or(u32::MAX);
            assert!(
                c.value == Some(expected),
                "jump-table strategy expects dense 0..N-1 cases; got {:?} at index {i}",
                c.value,
            );
        }
        let case_count = u32::try_from(n).unwrap_or(u32::MAX);
        let max_value = case_count - 1;

        // Load scrutinee into BX.
        let ExprKind::Ident(name) = &scrutinee.kind else {
            panic!("non-ident switch scrutinee not yet supported (no fixture)");
        };
        assert!(
            matches!(self.locals.type_of(name), Type::Int),
            "char-typed switch scrutinee not yet supported (no fixture)"
        );
        match self.locals.location_of(name) {
            LocalLocation::Stack(off) => {
                let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
            }
            LocalLocation::Reg(reg) => {
                assert!(
                    reg.name() != "bx",
                    "scrutinee already in BX — no fixture for BX-resident switch scrutinee yet",
                );
                let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
            }
        }

        // Bounds check: anything > max_value (unsigned, since out-of-
        // range negatives also overflow into > max when treated as
        // unsigned) jumps to the end-of-switch.
        let _ = write!(self.out, "\tcmp\tbx,{max_value}\r\n");
        let _ = write!(self.out, "\tja\tshort {}\r\n", self.label_ref(end_slot));
        self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
        let c_num = switch_c_num(SwitchStrategy::JumpTable, case_count);
        let _ = write!(
            self.out,
            "\tjmp\tword ptr cs:@{}@C{c_num}[bx]\r\n",
            self.func_idx,
        );

        // Case bodies in source order; `break;` inside a body emits a
        // `jmp short <end>` via the loop_stack frame. Cases without
        // `break;` fall through to the next case label.
        self.loop_stack.push(LoopTargets {
            break_target_slot: end_slot,
            continue_target_slot: None,
        });
        for (case, &slot) in cases.iter().zip(case_slots) {
            self.emit_label(slot);
            let case_line = self.lines.line_of(case.span.start);
            self.advance_to_line(case_line);
            for s in &case.body {
                self.emit_stmt(s);
            }
        }
        self.loop_stack.pop();

        // Stage the address table for emission after `_main endp`.
        let _ = write!(
            self.post_function_data,
            "@{}@C{c_num}\tlabel\tword\r\n",
            self.func_idx,
        );
        for &slot in case_slots {
            let _ = write!(
                self.post_function_data,
                "\tdw\t{}\r\n",
                self.label_ref(slot),
            );
        }
    }

    /// Emit the linear-value-search dispatch (fixture 074). Used
    /// when cases are sparse (≥ 4 cases that aren't `0..N-1`).
    ///
    /// ```text
    ///   mov ax, <scrutinee>
    ///   mov word ptr [bp-<spill>], ax     ; spill to a stack slot
    ///   mov cx, <case_count>
    ///   mov bx, offset @<func>@C<num>
    /// <loop top>:
    ///   mov ax, word ptr cs:[bx]
    ///   cmp ax, word ptr [bp-<spill>]
    ///   je  short <dispatch>
    ///   inc bx
    ///   inc bx
    ///   loop short <loop top>
    ///   jmp short <end>                   ; not found
    /// <dispatch>:
    ///   jmp word ptr cs:[bx+<addr table offset>]
    /// <case 0>:
    ///   <body>
    /// …
    /// <end>:
    /// ```
    ///
    /// After `_main endp`:
    /// ```text
    /// @<func>@C<num>	label	word
    ///   db <val 0 low> / db <val 0 high>  ; values, little-endian bytes
    ///   …
    ///   dw @<func>@<case 0 slot>          ; parallel address table
    ///   …
    /// ```
    ///
    /// The "values written as `db` byte pairs" instead of `dw` is a
    /// distinctive BCC fingerprint.
    fn emit_switch_linear_search(
        &mut self,
        switch_span_start: u32,
        scrutinee: &Expr,
        cases: &[SwitchCase],
        case_slots: &[u32],
        end_slot: u32,
    ) {
        // Linear search has no default-case support in our fixtures.
        assert!(
            cases.iter().all(|c| c.value.is_some()),
            "default inside a linear-search switch not yet supported (no fixture)"
        );
        let case_count = u32::try_from(cases.len()).unwrap_or(u32::MAX);
        // Locals analyzer reserved a stack slot for the spilled
        // scrutinee; look up its offset by this switch's span_start.
        let spill_off = self.locals.switch_spill_offset(switch_span_start);

        // Load scrutinee into AX (any local kind works).
        let ExprKind::Ident(name) = &scrutinee.kind else {
            panic!("non-ident switch scrutinee not yet supported (no fixture)");
        };
        assert!(
            matches!(self.locals.type_of(name), Type::Int),
            "char-typed switch scrutinee not yet supported (no fixture)"
        );
        match self.locals.location_of(name) {
            LocalLocation::Stack(off) => {
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
            }
            LocalLocation::Reg(reg) => {
                let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
            }
        }
        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(spill_off));

        // Loop setup. CX = case count, BX = pointer to values table.
        let _ = write!(self.out, "\tmov\tcx,{case_count}\r\n");
        let c_num = switch_c_num(SwitchStrategy::LinearSearch, case_count);
        let _ = write!(self.out, "\tmov\tbx,offset @{}@C{c_num}\r\n", self.func_idx);

        // Pre-dispatch slot layout for linear-search (from fixture 074):
        // - pre slots 0..5 unused (#cases + 2 ghost slots)
        // - Wait: 074 reserves 6 pre-slots (#cases=4 + 2), but actually
        //   2 of those slots are USED: @1@98 (loop-top) and @1@170
        //   (dispatch). Let me re-check.
        //
        // 074 labels:
        //   @1@98  = slot 2   (loop top)
        //   @1@170 = slot 5   (dispatch indirect-jmp)
        //   @1@194 = slot 6   (case 0 body)
        //
        // So pre-slots: 0, 1 unused; 2 = loop_top; 3, 4 unused;
        // 5 = dispatch. case bodies start at 6. That matches `#cases + 2 = 6`
        // pre-slots in total. The loop_top sits at slot 2 (= 0+2) and
        // the dispatch at slot 5 (= #cases + 1).
        let loop_top_slot = case_slots[0] - 4;
        let dispatch_slot = case_slots[0] - 1;

        self.emit_label(loop_top_slot);
        self.out.extend_from_slice(b"\tmov\tax,word ptr cs:[bx]\r\n");
        let _ = write!(self.out, "\tcmp\tax,word ptr {}\r\n", bp_addr(spill_off));
        let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(dispatch_slot));
        self.out.extend_from_slice(b"\tinc\tbx\r\n");
        self.out.extend_from_slice(b"\tinc\tbx\r\n");
        let _ = write!(self.out, "\tloop\tshort {}\r\n", self.label_ref(loop_top_slot));
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(end_slot));
        self.emit_label(dispatch_slot);
        // The dispatch indirect-jmp: BX points to the matched value's
        // entry; the parallel address table sits at BX + 2*case_count.
        let addr_table_offset = case_count * 2;
        let _ = write!(
            self.out,
            "\tjmp\tword ptr cs:[bx+{addr_table_offset}]\r\n",
        );

        // Case bodies in source order. Same break-target setup as the
        // other strategies.
        self.loop_stack.push(LoopTargets {
            break_target_slot: end_slot,
            continue_target_slot: None,
        });
        for (case, &slot) in cases.iter().zip(case_slots) {
            self.emit_label(slot);
            let case_line = self.lines.line_of(case.span.start);
            self.advance_to_line(case_line);
            for s in &case.body {
                self.emit_stmt(s);
            }
        }
        self.loop_stack.pop();

        // Stage value table + address table for post-function emission.
        let _ = write!(
            self.post_function_data,
            "@{}@C{c_num}\tlabel\tword\r\n",
            self.func_idx,
        );
        for case in cases {
            let v = case.value.expect("default handled by assert above") & 0xFFFF;
            let lo = v & 0xFF;
            let hi = (v >> 8) & 0xFF;
            let _ = write!(self.post_function_data, "\tdb\t{lo}\r\n");
            let _ = write!(self.post_function_data, "\tdb\t{hi}\r\n");
        }
        for &slot in case_slots {
            let _ = write!(
                self.post_function_data,
                "\tdw\t{}\r\n",
                self.label_ref(slot),
            );
        }
    }

    /// Emit a conditional branch: control flows to `true_slot` when
    /// `cond` is true, to `false_slot` when false. Exactly one of the
    /// two should be `None` — that direction falls through to the
    /// next instruction emitted.
    ///
    /// `Logical` operators (`&&`, `||`) recurse into this function on
    /// both operands, short-circuiting via fall-through:
    /// - `a && b`: a's false → false_slot; a's true → fall through to
    ///   b's test (a's true target becomes `None`). Then b carries
    ///   the original true/false targets.
    /// - `a || b`: a's true → true_slot; a's false → fall through to
    ///   b's test (a's false target becomes `None`). Then b same.
    /// Whether `cond` is a long-vs-long compare (signed or unsigned)
    /// between two long-family idents — either or both may be a long
    /// global or a long stack local. Triggers the 3-jump pattern.
    /// Used by `emit_if` to decide whether to allocate a
    /// `then_entry_slot` for the test's true-target jump. Fixtures
    /// 234–237 (globals signed), 242 (globals unsigned), 297 (stack).
    fn is_long_signed_globals_cmp(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op, left, right } = &cond.kind else { return false };
        if !matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge) {
            return false;
        }
        let (ExprKind::Ident(a), ExprKind::Ident(b)) = (&left.kind, &right.kind) else {
            return false;
        };
        self.ident_is_long_like(a) && self.ident_is_long_like(b)
    }

    fn ident_is_long_like(&self, name: &str) -> bool {
        if let Some(gt) = self.globals.type_of(name) {
            return gt.is_long_like();
        }
        self.locals.has(name) && self.locals.type_of(name).is_long_like()
    }

    /// `(high-addr, low-addr)` text for a long-like ident, either as
    /// `DGROUP:_g+2` / `DGROUP:_g` (global) or `[bp+N+2]` / `[bp+N]`
    /// (stack). Panics on a register-resident or non-existent ident
    /// — callers should gate with `ident_is_long_like` first.
    fn long_addr_pair(&self, name: &str) -> (String, String) {
        if self.globals.contains(name) {
            (format!("DGROUP:_{name}+2"), format!("DGROUP:_{name}"))
        } else {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long is never register-resident");
            };
            (bp_addr(off + 2), bp_addr(off))
        }
    }

    /// `(high-addr, low-addr)` text for an arbitrary long-valued
    /// lvalue expression. Covers: bare ident (global or stack),
    /// dot-chain (`s.x`, `a[K].x`, nested), array index with
    /// constant subscript (global or stack), and pointer deref
    /// for a register-resident long pointer (`*p`).
    ///
    /// Returns `None` if the lvalue isn't a shape we know how to
    /// fold into a constant address pair (e.g. variable array index,
    /// stack-resident pointer).
    /// Load an array-index expression into BX, pre-scaled by the
    /// element type's stride. The common shape BCC uses when the
    /// index is a non-constant expression and the result will be
    /// used in a `[bx+<symbol>]` addressing form.
    ///
    /// Lowering rules:
    ///   - int index: `mov bx, <idx>` then shl bx, 1 (× stride/2)
    ///     repeatedly. For int stride=2 → one shl; long stride=4 →
    ///     two shls; char stride=1 → no shifts.
    ///   - char index: `mov al, <idx-byte>`, then `cbw` (or `mov
    ///     ah,0` for unsigned), `shl ax, ...`, `mov bx, ax`.
    fn emit_index_into_bx(&mut self, idx: &Expr, elem_ty: &Type) {
        let stride = elem_ty.size_bytes();
        let shifts = match stride {
            1 => 0,
            2 => 1,
            4 => 2,
            _ => panic!("unsupported element stride {stride} for variable-indexed array"),
        };
        // Char-typed index: widen AL → AX with CBW (signed) or
        // mov ah,0 (unsigned), then scale, then move into BX. Fixture
        // 1493.
        let idx_is_char = matches!(&idx.kind, ExprKind::Ident(n)
            if (self.locals.has(n) && self.locals.type_of(n).is_char_like())
            || self.globals.type_of(n).map_or(false, |t| t.is_char_like()));
        if idx_is_char
            && let ExprKind::Ident(name) = &idx.kind
        {
            let unsigned = if self.locals.has(name) {
                self.locals.type_of(name).is_unsigned()
            } else {
                self.globals.type_of(name).map_or(false, |t| t.is_unsigned())
            };
            let src_addr = if self.locals.has(name) {
                let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                    panic!("char index `{name}` should be stack-resident");
                };
                bp_addr(off)
            } else {
                format!("DGROUP:_{name}")
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {src_addr}\r\n");
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            }
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
            }
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
            return;
        }
        // `<int-lvalue> << K` index: load lvalue into BX, then apply
        // K extra shifts followed by the stride shifts. Avoids the
        // AX route. Fixture 2530 (`a[i << 1]`).
        if let ExprKind::BinOp { op: BinOp::Shl, left, right } = &idx.kind
            && let Some(k) = try_const_eval(right)
            && let Some(addr) = self.int_lvalue_addr(left)
        {
            let _ = write!(self.out, "\tmov\tbx,word ptr {addr}\r\n");
            for _ in 0..(k + shifts as u32) {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            return;
        }
        // Int-typed index path: prefer a direct `mov bx, <addr>` over
        // a roundtrip through AX.
        if let Some(addr) = self.int_lvalue_addr(idx) {
            let _ = write!(self.out, "\tmov\tbx,word ptr {addr}\r\n");
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            return;
        }
        // Register-resident int local: `mov bx, <reg>` directly,
        // skipping the AX round-trip.
        if let ExprKind::Ident(name) = &idx.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && !reg.is_byte()
        {
            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            return;
        }
        // `arr[++i]` / `arr[--i]` where i is a register-resident int
        // local: emit the inc/dec on the register, then `mov bx,
        // <reg>` reading the post-update value. Fixture 2837.
        if let ExprKind::Update {
            target,
            op,
            position: crate::ast::UpdatePosition::Pre,
        } = &idx.kind
            && self.locals.has(target)
            && self.locals.type_of(target).is_int_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(target)
        {
            let mnem = match op {
                crate::ast::UpdateOp::Inc => "inc",
                crate::ast::UpdateOp::Dec => "dec",
            };
            let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            return;
        }
        // Fallback: evaluate into AX, scale, then move to BX.
        self.emit_expr_to_ax(idx);
        for _ in 0..shifts {
            self.out.extend_from_slice(b"\tshl\tax,1\r\n");
        }
        self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
    }

    /// True when the given RHS expression can't be reduced to an
    /// OperandSource by `resolve_operand_source` and instead needs to
    /// be evaluated into AX first. Used by the int-reg compound-
    /// assign fallback to decide between the direct `<op> <reg>,
    /// <src>` shape and the AX-route shape.
    fn value_needs_ax_route(&self, e: &Expr) -> bool {
        match &e.kind {
            // Nested binary expressions don't have a single-operand
            // representation.
            ExprKind::BinOp { .. } => true,
            // Variable-indexed arrays / chained members: resolvable
            // only when the chain folds to a constant offset. Use
            // try_lvalue_chain_addr's success as the predicate.
            ExprKind::ArrayIndex { .. } | ExprKind::Member { .. } => {
                self.try_lvalue_chain_addr(e).is_none()
            }
            // Casts (e.g. `(char)b`), ternaries, and calls all need
            // evaluation that produces a value in AX. They don't have
            // a single memory/register operand representation.
            // Fixture 1288 (`a += (char)b`).
            ExprKind::Cast { .. } | ExprKind::Ternary { .. } | ExprKind::Call { .. } => true,
            _ => false,
        }
    }

    /// Find a struct definition with the given tag by scanning both
    /// globals and locals. Globals' types come from `GlobalTable`;
    /// locals' types live in `Locals`. Returns the first complete
    /// (non-placeholder) struct match. Used to resolve fields off a
    /// `Type::Pointer(Struct{name-only, fields:[]})` placeholder that
    /// the AST stores when a recursive struct type would otherwise
    /// require a cycle.
    fn lookup_struct_by_tag(&self, tag: &str) -> Option<Type> {
        fn find<'a>(ty: &'a Type, tag: &str) -> Option<&'a Type> {
            match ty {
                Type::Struct { name: Some(t), fields, .. }
                    if t == tag && !fields.is_empty() =>
                {
                    Some(ty)
                }
                Type::Struct { fields, .. } => fields.iter().find_map(|f| find(&f.ty, tag)),
                Type::Array { elem, .. } => find(elem, tag),
                Type::Pointer(inner) => find(inner, tag),
                _ => None,
            }
        }
        if let Some(g) = self.globals.lookup_struct_by_tag(tag) {
            return Some(g.clone());
        }
        for (_, ty) in self.locals.iter_types() {
            if let Some(found) = find(ty, tag) {
                return Some(found.clone());
            }
        }
        None
    }

    /// True if `e` is a bare ident referring to an int-typed stack
    /// local or global, returning an OperandSource that names the
    /// memory operand. Used by the rhs-clobbers-AX commutative-op
    /// fallback to skip the push/pop dance: evaluate RHS into AX,
    /// then `<op> ax, <mem>` directly on the LHS memory.
    fn try_memory_source(&self, e: &Expr) -> Option<OperandSource> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        if let Some(gty) = self.globals.type_of(name)
            && gty.is_int_like()
        {
            return Some(OperandSource::Global(name.clone()));
        }
        if self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            return Some(OperandSource::Local(off));
        }
        None
    }

    /// Resolve an int-like lvalue (global or stack-resident local) to
    /// its asm memory operand. Returns `None` for register-resident
    /// locals (caller can fall back to a register-source path) and
    /// for non-lvalue expressions.
    fn int_lvalue_addr(&self, e: &Expr) -> Option<String> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        if let Some(gty) = self.globals.type_of(name)
            && gty.is_int_like()
        {
            return Some(format!("DGROUP:_{name}"));
        }
        if self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            return Some(bp_addr(off));
        }
        None
    }

    fn long_lvalue_addr_pair(&self, e: &Expr) -> Option<(String, String)> {
        // Bare ident.
        if let ExprKind::Ident(name) = &e.kind
            && self.ident_is_long_like(name)
        {
            return Some(self.long_addr_pair(name));
        }
        // Dot/arrow member chain folding to a constant address.
        if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } = &e.kind
            && let Some((src, total_off, leaf_ty)) = self.try_member_dot_chain(base, field)
            && leaf_ty.is_long_like()
        {
            if self.globals.contains(&src) {
                return Some((
                    global_offset_addr(&src, total_off + 2),
                    global_offset_addr(&src, total_off),
                ));
            }
            if let LocalLocation::Stack(base_bp) = self.locals.location_of(&src) {
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                return Some((bp_addr(off + 2), bp_addr(off)));
            }
        }
        // Array index with constant subscript (global or stack array).
        if let ExprKind::ArrayIndex { array: arr_expr, index } = &e.kind
            && let ExprKind::Ident(arr_name) = &arr_expr.kind
            && let Some(k) = try_const_eval(index)
        {
            let byte_off = (k as i32) * 4;
            if let Some(arr_ty) = self.globals.type_of(arr_name)
                && let Some(elem) = arr_ty.array_elem()
                && elem.is_long_like()
            {
                return Some((
                    global_offset_addr(arr_name, byte_off + 2),
                    global_offset_addr(arr_name, byte_off),
                ));
            }
            if self.locals.has(arr_name)
                && let Some(elem) = self.locals.type_of(arr_name).array_elem()
                && elem.is_long_like()
            {
                let LocalLocation::Stack(base_off) =
                    self.locals.location_of(arr_name)
                else {
                    unreachable!("array is stack-resident");
                };
                let off = base_off + i16::try_from(byte_off).unwrap_or(i16::MAX);
                return Some((bp_addr(off + 2), bp_addr(off)));
            }
        }
        // `*p` for a register-resident long pointer.
        if let ExprKind::Deref(operand) = &e.kind
            && let ExprKind::Ident(ptr_name) = &operand.kind
            && self.locals.has(ptr_name)
            && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
            && pointee.is_long_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(ptr_name)
        {
            let r = reg.name();
            return Some((format!("[{r}+2]"), format!("[{r}]")));
        }
        None
    }

    /// Try to lower a non-constant long expression into a load/arith/
    /// store skeleton landing at `dest_hi`/`dest_lo`. Returns true
    /// when the value's shape was recognized and emitted; false if
    /// the caller should fall through to its own panic/path.
    ///
    /// Handles:
    /// - `<long-lvalue>` (plain copy): two loads + two stores.
    /// - `<long-lvalue> <op> <const>` for `+`/`-`: load lvalue,
    ///   add/sub imm to DX, adc/sbb 0/-1 to AX, store.
    /// - `<long-lvalue> <op> <long-lvalue>` for `+`/`-`/`&`/`|`/`^`:
    ///   load operand a, op against operand b's halves, store.
    fn try_emit_long_value_to_dest(
        &mut self,
        value: &Expr,
        dest_hi: &str,
        dest_lo: &str,
    ) -> bool {
        // Plain copy: `<dest> = <long-lvalue>`.
        if let Some((src_hi, src_lo)) = self.long_lvalue_addr_pair(value) {
            // Only treat as a copy when value itself is the lvalue
            // (not a sub-expression of a binop). We detect that by
            // re-checking — long_lvalue_addr_pair returns Some only
            // for lvalue-shaped exprs, so a top-level match here is
            // the lvalue itself.
            let _ = write!(self.out, "\tmov\tax,word ptr {src_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {src_lo}\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            return true;
        }
        // `<lvalue> <op> <const>` for arith ops.
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && (matches!(op, BinOp::Add) || matches!(op, BinOp::Sub))
            && let Some((src_hi, src_lo)) = self.long_lvalue_addr_pair(left)
            && let Some(k) = try_const_eval(right)
        {
            let signed = k as i32;
            let (delta, carry) = if matches!(op, BinOp::Add) {
                (signed, 0i16)
            } else {
                (-signed, -1i16)
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {src_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {src_lo}\r\n");
            if let Ok(delta_i8) = i8::try_from(delta) {
                let _ = write!(self.out, "\tadd\tdx,{delta_i8}\r\n");
            } else {
                let delta_u16 = (delta as i32) as u16;
                let _ = write!(self.out, "\tadd\tdx,{delta_u16}\r\n");
            }
            let _ = write!(self.out, "\tadc\tax,{carry}\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            return true;
        }
        // `<lvalue_a> <op> <lvalue_b>` for `+`/`-`/`&`/`|`/`^`.
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && let Some((lo_op, hi_op)) = long_pair_op(*op)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\t{lo_op}\tdx,word ptr {b_lo}\r\n");
            let _ = write!(self.out, "\t{hi_op}\tax,word ptr {b_hi}\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            return true;
        }
        // `<dest> = <a> * <b>` for two long lvalues. Helper convention
        // is CX:BX (LHS) and DX:AX (RHS) → DX:AX. After the call, store
        // DX→dest_hi and AX→dest_lo. Mirrors the return-path shape (line
        // 3365) with the result captured into memory rather than left
        // as a return value. Fixture 1628.
        if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let _ = write!(self.out, "\tmov\tcx,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tbx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {b_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {b_lo}\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            return true;
        }
        // `<dest> = <a> / <b>` / `<a> % <b>` for two long lvalues. Helpers
        // (N_LDIV@/N_LMOD@/N_LUDIV@/N_LUMOD@) take 4 stack words: divisor
        // pushed first, then dividend — each high-first (= push lo, hi
        // in source order, since stack grows down). Result in DX:AX,
        // store to dest. Fixtures 1629 (signed div), 1633 (unsigned div).
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let unsigned = self.expr_is_unsigned(left) || self.expr_is_unsigned(right);
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tpush\tword ptr {b_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {b_lo}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            return true;
        }
        // `<dest> = (long)<int-lvalue>` — widen an int/uint source
        // into DX:AX (cwd for signed, xor dx,dx for unsigned), then
        // store. Same shape as the return-path widen (line 3551).
        // Fixtures 1638 (signed int cast), 1639 (unsigned int cast).
        let widening_src = match &value.kind {
            ExprKind::Cast { ty, operand } if ty.is_long_like() => {
                if let ExprKind::Ident(name) = &operand.kind { Some(name.as_str()) } else { None }
            }
            _ => None,
        };
        if let Some(src_name) = widening_src
            && let Some(addr) = self.int_lvalue_addr(&Expr {
                kind: ExprKind::Ident(src_name.to_owned()),
                span: value.span,
            })
        {
            let src_ty = if let Some(gty) = self.globals.type_of(src_name) {
                gty.clone()
            } else {
                self.locals.type_of(src_name).clone()
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            if src_ty.is_unsigned() {
                // Destination-driven: write 0 directly to the high-
                // half memory slot instead of going through `xor dx,
                // dx`. Saves the DX clobber and matches BCC's actual
                // shape for fixture 1639.
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},0\r\n");
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
            }
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            return true;
        }
        // `<dest> = <a> << K` / `<a> >> K` for long lvalue and a
        // constant K in [1,255]. K=1 inlines `shl ax,1; rcl dx,1` or
        // the rcr shape for shr; K>1 routes through the N_LX*SH@
        // helpers. Mirrors the return-path shape (line 3599) with
        // DX:AX stored into the dest pair. Fixture 1640.
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some(k) = try_const_eval(right)
            && k >= 1
            && k <= 255
        {
            let unsigned = self.expr_is_unsigned(left);
            // K=1 single-step inline: BCC's shift convention here puts
            // AX=high, DX=low (opposite of the helper-call path) so
            // the carry chain matches: `shl dx, 1; rcl ax, 1` (low
            // first into high). Fixtures 1735, 1736, 1782, 1783.
            // The >1 helper path keeps DX=high, AX=low (helper's
            // calling convention).
            if k == 1 {
                let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
                if matches!(op, BinOp::Shl) {
                    self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                    self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                } else {
                    let hi_op = if unsigned { "shr" } else { "sar" };
                    let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                    self.out.extend_from_slice(b"\trcr\tdx,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {a_lo}\r\n");
                let k_u8 = (k & 0xFF) as u8;
                let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                let helper = match (op, unsigned) {
                    (BinOp::Shl, _)     => "N_LXLSH@",
                    (BinOp::Shr, false) => "N_LXRSH@",
                    (BinOp::Shr, true)  => "N_LXURSH@",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            }
            return true;
        }
        // `<dest> = <a> * K_pow2` — strength-reduce to shl. BCC uses
        // inline `shl dx,1; rcl ax,1` (AX=high, DX=low) only for K=2
        // (shift by 1) and for K=1 (just a copy with the same load
        // shape); any larger pow2 routes through N_LXLSH@ with
        // `mov cl, k` and the helper's DX=high, AX=low ABI.
        // Fixtures 1641 (`a * 4L` → helper), 1783 (`a * 1L` → copy).
        if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some(k) = try_const_eval(right)
            && k > 0
            && k.is_power_of_two()
            && k.trailing_zeros() <= 31
        {
            let shifts = k.trailing_zeros();
            if shifts <= 1 {
                let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
                if shifts == 1 {
                    self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                    self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {a_lo}\r\n");
                let k_u8 = shifts as u8;
                let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_LXLSH@\r\n");
                self.helpers.insert("N_LXLSH@".to_string());
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            }
            return true;
        }
        // `<dest> = <a> <</>> <n>` for a long lvalue shifted by an
        // int-typed lvalue. BCC loads the operand into DX:AX and the
        // shift count's *low byte* into CL (the value is assumed to
        // fit in a byte, valid for any C shift count ≤ 31), then
        // calls N_LXLSH@ (left) / N_LXRSH@ (signed right) / N_LXURSH@
        // (unsigned right). Result in DX:AX. Fixtures 1630 (signed
        // shr), 1631 (shl), 1634 (unsigned shr).
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some(n_addr) = self.int_lvalue_addr(right)
        {
            let unsigned = self.expr_is_unsigned(left);
            let helper = match (op, unsigned) {
                (BinOp::Shl, _)     => "N_LXLSH@",
                (BinOp::Shr, false) => "N_LXRSH@",
                (BinOp::Shr, true)  => "N_LXURSH@",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tmov\tcl,byte ptr {n_addr}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            return true;
        }
        false
    }

    /// Whether `cond` is `<long_var> <op> K` for a relational
    /// comparison op (`<,>,<=,>=`) on a long global or stack local.
    /// BCC inlines K into the `cmp <mem>, imm` instruction (per
    /// half), choosing the shorter imm8sx form when each half fits
    /// and the wider imm16 otherwise. Fixtures 240 (i8sx global),
    /// 282 (imm16 global), 293 (i8sx stack local).
    fn is_long_signed_const_cmp(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op, left, right } = &cond.kind else { return false };
        if !matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge) {
            return false;
        }
        let ExprKind::Ident(name) = &left.kind else { return false };
        let is_long_global = self.globals.type_of(name).map_or(false, |t| t.is_long_like());
        let is_long_local = self.locals.has(name) && self.locals.type_of(name).is_long_like();
        if !is_long_global && !is_long_local {
            return false;
        }
        try_const_eval(right).is_some()
    }

    /// Whether `cond` is a long-vs-int relational compare between
    /// a long global and an int global. BCC widens the int with
    /// `cwd` (DX:AX = widened i), then compares against g. The
    /// 3-jump pattern uses operand-swapped mnemonics (since the
    /// operand order is widened-int-LHS / long-RHS, but the
    /// source semantics is long-LHS / int-RHS). Fixtures 273
    /// (`<`), and 280 (`!=`) which uses a different shape.
    fn is_long_vs_int_cmp(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op, left, right } = &cond.kind else { return false };
        if !matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge) {
            return false;
        }
        let (ExprKind::Ident(a), ExprKind::Ident(b)) = (&left.kind, &right.kind) else {
            return false;
        };
        let a_ty = self.globals.type_of(a);
        let b_ty = self.globals.type_of(b);
        a_ty.map_or(false, |t| t.is_long_like())
            && b_ty.map_or(false, |t| matches!(t, Type::Int))
    }

    /// Whether `cond` is `<long_global> != <int_global>`. Same
    /// widen-via-cwd shape as `is_long_vs_int_cmp` but uses the
    /// chained-cmp pattern with both slots (jne→true, je→false).
    /// Fixture 280.
    fn is_long_vs_int_ne(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op: BinOp::Ne, left, right } = &cond.kind else {
            return false;
        };
        let (ExprKind::Ident(a), ExprKind::Ident(b)) = (&left.kind, &right.kind) else {
            return false;
        };
        let a_ty = self.globals.type_of(a);
        let b_ty = self.globals.type_of(b);
        a_ty.map_or(false, |t| t.is_long_like())
            && b_ty.map_or(false, |t| matches!(t, Type::Int))
    }

    /// Whether `cond` is `<long_global> != K` for a small const K —
    /// uses the chained-cmp pattern with both slots (jne→true,
    /// je→false). Fixture 239.
    fn is_long_ne_const(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op: BinOp::Ne, left, right } = &cond.kind else {
            return false;
        };
        let ExprKind::Ident(name) = &left.kind else { return false };
        if !self.globals.type_of(name).map_or(false, |t| t.is_long_like()) {
            return false;
        }
        let Some(k) = try_const_eval(right) else { return false };
        if k == 0 {
            return false; // long != 0 uses the OR-then-test idiom (fixture 238)
        }
        let hi = (k >> 16) as i32;
        let lo = (k & 0xFFFF) as i32;
        (-128..=127).contains(&hi) && (-128..=127).contains(&lo)
    }

    fn emit_cond_branch(
        &mut self,
        cond: &Expr,
        true_slot: Option<u32>,
        false_slot: Option<u32>,
    ) {
        // `<long_global> <relop> <int_global>` mixed compare. BCC
        // widens the int (mov ax, _i / cwd to DX:AX), then compares
        // against g. The operand-order in the cmp is widened-int-LHS
        // / long-RHS, but the source semantics is long-LHS /
        // int-RHS — so the mnemonic flips (e.g. `g < i` lowers to
        // `i > g`). Fixture 273.
        if self.is_long_vs_int_cmp(cond)
            && let ExprKind::BinOp { op, left, right } = &cond.kind
            && let ExprKind::Ident(g) = &left.kind
            && let ExprKind::Ident(i) = &right.kind
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            // Flip the op: g <op> i ⇔ i <flipped> g (operands swapped).
            // Then look up mnemonics for the flipped op.
            let flipped = match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Gt => BinOp::Lt,
                BinOp::Le => BinOp::Ge,
                BinOp::Ge => BinOp::Le,
                _ => unreachable!(),
            };
            // Reuse the same mnemonic table as the globals-vs-globals
            // path. Signedness here is "either operand unsigned" →
            // unsigned. Both long_like for unsigned check covers
            // signed long + signed int = signed, etc.
            let unsigned = self.globals.type_of(g).map_or(false, |t| t.is_unsigned())
                || self.globals.type_of(i).map_or(false, |t| t.is_unsigned());
            let (hi_to_false, hi_to_true, lo_to_false) = match (flipped, unsigned) {
                (BinOp::Lt, false) => ("jg", "jl",  "jae"),
                (BinOp::Gt, false) => ("jl", "jg",  "jbe"),
                (BinOp::Le, false) => ("jg", "jne", "ja"),
                (BinOp::Ge, false) => ("jl", "jne", "jb"),
                (BinOp::Lt, true)  => ("ja", "jb",  "jae"),
                (BinOp::Gt, true)  => ("jb", "ja",  "jbe"),
                (BinOp::Le, true)  => ("ja", "jne", "ja"),
                (BinOp::Ge, true)  => ("jb", "jne", "jb"),
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{i}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            let _ = write!(self.out, "\tcmp\tdx,word ptr DGROUP:_{g}+2\r\n");
            let _ = write!(self.out, "\t{hi_to_false}\tshort {}\r\n", self.label_ref(fslot));
            let _ = write!(self.out, "\t{hi_to_true}\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tax,word ptr DGROUP:_{g}\r\n");
            let _ = write!(self.out, "\t{lo_to_false}\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // `<long_global> != <int_global>` mixed inequality. Same
        // widen-via-cwd as `<` but with the chained-cmp shape:
        // jne→true on the high half (definitive), je→false on the
        // low half (both equal → ==). Fixture 280.
        if self.is_long_vs_int_ne(cond)
            && let ExprKind::BinOp { left, right, .. } = &cond.kind
            && let ExprKind::Ident(g) = &left.kind
            && let ExprKind::Ident(i) = &right.kind
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{i}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            let _ = write!(self.out, "\tcmp\tdx,word ptr DGROUP:_{g}+2\r\n");
            let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tax,word ptr DGROUP:_{g}\r\n");
            let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // Signed long-vs-long compare between two long globals. BCC
        // emits a 3-jump pattern: high-half signed cmp with `jg/jl`
        // for definitive answers, low-half unsigned cmp for the
        // tie-breaker. Caller must supply BOTH slots so the
        // intermediate signed-direction jump can land at the body
        // (true target). Fixture 234.
        if self.is_long_signed_globals_cmp(cond)
            && let ExprKind::BinOp { op, left, right } = &cond.kind
            && let ExprKind::Ident(a) = &left.kind
            && let ExprKind::Ident(b) = &right.kind
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            // Mnemonic table. Signed (fixtures 234–237) vs unsigned
            // (fixture 242) differs only in the high-half jumps:
            // signed uses jl/jg, unsigned uses jb/ja. The non-strict
            // high-half true jump is `jne` in both cases. Low-half
            // is always unsigned (jae/jbe strict; ja/jb non-strict).
            let unsigned = self.cmp_is_unsigned(left, right);
            let (hi_to_false, hi_to_true, lo_to_false) = match (op, unsigned) {
                (BinOp::Lt, false) => ("jg", "jl",  "jae"),
                (BinOp::Gt, false) => ("jl", "jg",  "jbe"),
                (BinOp::Le, false) => ("jg", "jne", "ja"),
                (BinOp::Ge, false) => ("jl", "jne", "jb"),
                (BinOp::Lt, true)  => ("ja", "jb",  "jae"),
                (BinOp::Gt, true)  => ("jb", "ja",  "jbe"),
                (BinOp::Le, true)  => ("ja", "jne", "ja"),
                (BinOp::Ge, true)  => ("jb", "jne", "jb"),
                _ => unreachable!(),
            };
            let (a_hi, a_lo) = self.long_addr_pair(a);
            let (b_hi, b_lo) = self.long_addr_pair(b);
            let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tcmp\tax,word ptr {b_hi}\r\n");
            let _ = write!(self.out, "\t{hi_to_false}\tshort {}\r\n", self.label_ref(fslot));
            let _ = write!(self.out, "\t{hi_to_true}\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tdx,word ptr {b_lo}\r\n");
            let _ = write!(self.out, "\t{lo_to_false}\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // `<long_global> <op> K` for K with both halves fitting
        // i8sx — same 3-jump shape as fixture 234 but using
        // `cmp <mem>, imm` directly (no AX/DX load). Fixture 240.
        if self.is_long_signed_const_cmp(cond)
            && let ExprKind::BinOp { op, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(k) = try_const_eval(right)
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            // Each half is formatted as i8sx-decimal when it fits,
            // u16-decimal otherwise — letting the assembler pick
            // the `83 3E` (5 bytes) vs `81 3E` (6 bytes) opcode
            // automatically. Fixtures 240 (i8sx), 282 (imm16).
            let hi = (k >> 16) as i32;
            let lo = (k & 0xFFFF) as i32;
            let fmt = |v: i32| -> String {
                if (-128..=127).contains(&v) {
                    format!("{v}")
                } else {
                    format!("{}", v as u16)
                }
            };
            let unsigned = if let Some(gt) = self.globals.type_of(name) {
                gt.is_unsigned()
            } else {
                self.locals.type_of(name).is_unsigned()
            };
            let (hi_to_false, hi_to_true, lo_to_false) = match (op, unsigned) {
                (BinOp::Lt, false) => ("jg", "jl",  "jae"),
                (BinOp::Gt, false) => ("jl", "jg",  "jbe"),
                (BinOp::Le, false) => ("jg", "jne", "ja"),
                (BinOp::Ge, false) => ("jl", "jne", "jb"),
                (BinOp::Lt, true)  => ("ja", "jb",  "jae"),
                (BinOp::Gt, true)  => ("jb", "ja",  "jbe"),
                (BinOp::Le, true)  => ("ja", "jne", "ja"),
                (BinOp::Ge, true)  => ("jb", "jne", "jb"),
                _ => unreachable!(),
            };
            // Choose between DGROUP-relative (global) and bp-relative
            // (stack-local) operand text. Fixtures 240 (global), 293
            // (stack local).
            let (hi_addr, lo_addr) = if self.globals.contains(name) {
                (format!("DGROUP:_{name}+2"), format!("DGROUP:_{name}"))
            } else {
                let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                    unreachable!("long is never register-resident");
                };
                (bp_addr(off + 2), bp_addr(off))
            };
            let _ = write!(self.out, "\tcmp\tword ptr {},{}\r\n", hi_addr, fmt(hi));
            let _ = write!(self.out, "\t{hi_to_false}\tshort {}\r\n", self.label_ref(fslot));
            let _ = write!(self.out, "\t{hi_to_true}\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tword ptr {},{}\r\n", lo_addr, fmt(lo));
            let _ = write!(self.out, "\t{lo_to_false}\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // `<long_global> != K` for non-zero K — chained cmp with
        // both slots: jne→true (high differs is definitive), then
        // je→false (high equal AND low equal). Fall-through (low
        // differs, high equal) lands at true. Fixture 239.
        if self.is_long_ne_const(cond)
            && let ExprKind::BinOp { op: BinOp::Ne, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(k) = try_const_eval(right)
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            let hi = (k >> 16) as i32;
            let lo = (k & 0xFFFF) as i32;
            let _ = write!(self.out, "\tcmp\tword ptr DGROUP:_{name}+2,{hi}\r\n");
            let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tword ptr DGROUP:_{name},{lo}\r\n");
            let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // `<long_global> == K` for non-zero K — BCC emits a chained
        // cmp+jne pair: high half against (K>>16), low half against
        // (K&0xFFFF). Both halves use Grp1 imm8sx form, so each half
        // must fit a sign-extended i8. Only the false-slot-only shape
        // shows up in fixture 223 (`if (g == K) ...`); a true-slot
        // form would invert to `je` and pick up later.
        if let ExprKind::BinOp { op: BinOp::Eq, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_long_like()
            && let Some(k) = try_const_eval(right)
            && k != 0
            && true_slot.is_none()
            && let Some(fslot) = false_slot
        {
            let hi = (k >> 16) as i32;
            let lo = (k & 0xFFFF) as i32;
            // Each half must sign-extend cleanly from imm8. BCC has
            // wider forms for out-of-range K (not yet observed); fall
            // through to the generic path when this guard fails.
            if (-128..=127).contains(&hi) && (-128..=127).contains(&lo) {
                let _ = write!(
                    self.out,
                    "\tcmp\tword ptr DGROUP:_{name}+2,{hi}\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tjne\tshort {}\r\n",
                    self.label_ref(fslot),
                );
                let _ = write!(
                    self.out,
                    "\tcmp\tword ptr DGROUP:_{name},{lo}\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tjne\tshort {}\r\n",
                    self.label_ref(fslot),
                );
                return;
            }
        }
        if let ExprKind::Logical { op, left, right } = &cond.kind {
            // The recursive structure handles chained `&&`/`||`
            // (fixtures 620/621): each non-final operand short-
            // circuits to the outer false_slot (for `&&`) or
            // true_slot (for `||`), and the last operand inherits
            // the outer (true, false) target pair. The recursion is
            // safe as long as the AST nests left-associatively
            // (parser default for `&&` and `||` in C).
            match op {
                LogicalOp::And => {
                    // a false → false_slot; a true → fall through to b.
                    // b carries the outer true/false targets.
                    self.emit_cond_branch(left, None, false_slot);
                    self.emit_cond_branch(right, true_slot, false_slot);
                }
                LogicalOp::Or => {
                    // a true → true_slot (jump); a false → fall through to b.
                    // For the rightmost (final) operand of an Or chain
                    // the caller will emit `true_slot`'s label right
                    // after, so b can fall through on true; that's the
                    // case when `false_slot.is_some()` (we're at the
                    // top of an if-cond Or chain). For non-final Ors
                    // (this Or is itself the LHS of an outer Or — the
                    // chained case from fixture 621) b's true must
                    // jump explicitly, since the caller emits more
                    // code (the outer Or's right operand) before the
                    // true label.
                    self.emit_cond_branch(left, true_slot, None);
                    let (right_true, right_false) = if false_slot.is_some() {
                        (None, false_slot)
                    } else {
                        (true_slot, None)
                    };
                    self.emit_cond_branch(right, right_true, right_false);
                }
            }
            return;
        }
        // Base case: single test (comparison or treat-as-bool).
        let (true_mnem, false_mnem) = self.emit_cond_test(cond);
        match (true_slot, false_slot) {
            (Some(slot), None) => {
                let _ = write!(
                    self.out,
                    "\t{true_mnem}\tshort {}\r\n",
                    self.label_ref(slot),
                );
            }
            (None, Some(slot)) => {
                let _ = write!(
                    self.out,
                    "\t{false_mnem}\tshort {}\r\n",
                    self.label_ref(slot),
                );
            }
            (Some(true_slot), Some(_)) => {
                // Both targets specified. Today this only fires from
                // `while (<a && b>)`'s rightmost-operand recursion,
                // where the false target is the immediately-following
                // break-target label (a natural fall-through). Emit
                // just the conditional jump to `true_slot`; the caller
                // is responsible for laying out false_slot as the
                // next emitted label. Fixtures 1273, 1352, 2203.
                let _ = write!(
                    self.out,
                    "\t{true_mnem}\tshort {}\r\n",
                    self.label_ref(true_slot),
                );
            }
            (None, None) => panic!(
                "emit_cond_branch with both targets fall-through: no jump would be emitted"
            ),
        }
    }

    /// Emit the actual test instruction for a simple (non-Logical)
    /// condition and return the (jump-if-true, jump-if-false)
    /// mnemonic pair the caller should use.
    ///
    /// - Comparison `a <op> b`: emit `emit_compare`, return the op's
    ///   `(jump_if_true, jump_if_false)` mnemonics.
    /// - Anything else: treat as boolean. Emit `cmp <expr>, 0` (or
    ///   `or <reg>, <reg>` peephole for register locals); the cond is
    ///   non-zero ⇔ true, so the mnemonic pair is `("jne", "je")`.
    fn emit_cond_test(&mut self, cond: &Expr) -> (&'static str, &'static str) {
        // `if (!<expr>)` — generate the same flag-setting test as
        // `<expr>` but swap the true/false jump mnemonics so the
        // conditional jump takes the inverted path. Fixture 536
        // (`if (!g)` on an int global lowers to `cmp [g], 0 / jne
        // <skip-then>`). Nested `!!x` falls back into this case so
        // the swap composes correctly.
        if let ExprKind::Unary { op: crate::ast::UnaryOp::Not, operand } = &cond.kind {
            let (t, f) = self.emit_cond_test(operand);
            return (f, t);
        }
        // `if (<int-global> & K)` — bit-test against a constant
        // mask. BCC emits `test word ptr [_g], K` (F7 06 lo hi
        // imm16, 6 bytes) which sets ZF based on the AND result
        // without storing it. Fixture 569.
        if let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && let Some(k) = try_const_eval(right)
        {
            let k16 = k & 0xFFFF;
            let _ = write!(
                self.out,
                "\ttest\tword ptr DGROUP:_{name},{k16}\r\n",
            );
            return ("jne", "je");
        }
        // `if (<int-local> & K)` — stack-local sibling. `test word
        // ptr [bp+N], K` (5 bytes) vs the load + and + or sequence.
        // Fixture 1853.
        if let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
            && let Some(k) = try_const_eval(right)
        {
            let k16 = k & 0xFFFF;
            let _ = write!(
                self.out,
                "\ttest\tword ptr {},{k16}\r\n",
                bp_addr(off),
            );
            return ("jne", "je");
        }
        // `(<int-mem> & <int-mem>) == 0` / `!= 0` — both operands
        // are int lvalues. BCC loads one into AX, then `test [other],
        // ax` (sets ZF without storing). Fixture 3539.
        if let ExprKind::BinOp { op: outer_op, left: outer_l, right: outer_r } = &cond.kind
            && matches!(outer_op, BinOp::Eq | BinOp::Ne)
            && try_const_eval(outer_r) == Some(0)
            && let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &outer_l.kind
            && let ExprKind::Ident(lname) = &left.kind
            && let ExprKind::Ident(rname) = &right.kind
            && self.locals.has(lname)
            && self.locals.has(rname)
            && self.locals.type_of(lname).is_int_like()
            && self.locals.type_of(rname).is_int_like()
            && let LocalLocation::Stack(l_off) = self.locals.location_of(lname)
            && let LocalLocation::Stack(r_off) = self.locals.location_of(rname)
        {
            // BCC loads the RHS-ident into AX, then `test [l], ax`.
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(r_off));
            let _ = write!(self.out, "\ttest\tword ptr {},ax\r\n", bp_addr(l_off));
            let mnem_pair = match outer_op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
            return mnem_pair;
        }
        // `(<int-mem> & K) == 0` or `!= 0` — the `& K` already sets
        // ZF via TEST, so the outer compare against 0 is implicit.
        // Routes through the same TestBpRelImm16 / TestGroupSymImm16
        // shape but inverts the true/false mnemonic based on Eq vs
        // Ne. Fixtures 3540 (`(x & 0x10) == 0`), 3264 (`(x & 0xff)
        // != 0`).
        if let ExprKind::BinOp { op: outer_op, left: outer_l, right: outer_r } = &cond.kind
            && matches!(outer_op, BinOp::Eq | BinOp::Ne)
            && try_const_eval(outer_r) == Some(0)
            && let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &outer_l.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(k) = try_const_eval(right)
        {
            let k16 = k & 0xFFFF;
            let mnem_pair = match outer_op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
            if let Some(gty) = self.globals.type_of(name)
                && gty.is_int_like()
            {
                let _ = write!(
                    self.out,
                    "\ttest\tword ptr DGROUP:_{name},{k16}\r\n",
                );
                return mnem_pair;
            }
            if self.locals.has(name)
                && self.locals.type_of(name).is_int_like()
                && let LocalLocation::Stack(off) = self.locals.location_of(name)
            {
                let _ = write!(
                    self.out,
                    "\ttest\tword ptr {},{k16}\r\n",
                    bp_addr(off),
                );
                return mnem_pair;
            }
        }
        // `<long_global> == 0` / `<long_global> != 0` — BCC folds the
        // 32-bit comparison into `mov ax,low / or ax,high`, which
        // sets ZF iff both halves are zero. Fixture 215.
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && matches!(op, BinOp::Eq | BinOp::Ne)
            && let ExprKind::Ident(name) = &left.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_long_like()
            && try_const_eval(right) == Some(0)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\tor\tax,word ptr DGROUP:_{name}+2\r\n");
            return match op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
        }
        // Same shape for a stack-resident long local vs 0 (fixture 292).
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && matches!(op, BinOp::Eq | BinOp::Ne)
            && let ExprKind::Ident(name) = &left.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && try_const_eval(right) == Some(0)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long is never register-resident");
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
            let _ = write!(self.out, "\tor\tax,word ptr {}\r\n", bp_addr(off + 2));
            return match op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
        }
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && op.is_comparison()
        {
            // `<arith>(X, Y) <relop> 0` — the arith op already set
            // the flags we want. Just evaluate the arith expression
            // into AX and use the relop's mnemonic directly. Saves
            // the `or ax,ax` (or `cmp ax,0`) instruction. Fixtures
            // 3254 (`a + b > 0`), 3257 (`a - b == 0`).
            let unsigned = self.cmp_is_unsigned(left, right);
            if try_const_eval(right) == Some(0)
                && let ExprKind::BinOp { op: arith, .. } = &left.kind
                && matches!(
                    arith,
                    BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                )
            {
                self.emit_expr_to_ax(left);
                return (
                    op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                    op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
                );
            }
            // `++<reg>` / `--<reg>` <relop> 0 — the inc/dec
            // instruction sets ZF/SF, which `<relop>` can read
            // directly. Emit just the inc/dec on the register.
            // Fixture 3644 (`while (--n > 0)`).
            if try_const_eval(right) == Some(0)
                && let ExprKind::Update {
                    target,
                    op: upd_op,
                    position: crate::ast::UpdatePosition::Pre,
                } = &left.kind
                && self.locals.has(target)
                && let LocalLocation::Reg(reg) = self.locals.location_of(target)
                && self.locals.type_of(target).is_int_like()
            {
                let mnem = match upd_op {
                    crate::ast::UpdateOp::Inc => "inc",
                    crate::ast::UpdateOp::Dec => "dec",
                };
                let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
                return (
                    op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                    op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
                );
            }
            if try_const_eval(left).is_some() && try_const_eval(right).is_none() {
                let flipped_op = match op {
                    BinOp::Eq | BinOp::Ne => *op,
                    BinOp::Lt => BinOp::Gt,
                    BinOp::Gt => BinOp::Lt,
                    BinOp::Le => BinOp::Ge,
                    BinOp::Ge => BinOp::Le,
                    _ => unreachable!(),
                };
                self.emit_compare(right, left);
                return (
                    flipped_op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                    flipped_op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
                );
            }
            self.emit_compare(left, right);
            return (
                op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
            );
        }
        // Bare long-global ident in condition position — equivalent
        // to `<long> != 0`. Use the OR-then-test idiom (fixture 284:
        // `if (a || b)` for two longs lowers to two of these tests
        // chained by short-circuit).
        if let ExprKind::Ident(name) = &cond.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_long_like()
        {
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\tor\tax,word ptr DGROUP:_{name}+2\r\n");
            return ("jne", "je");
        }
        self.emit_zero_test(cond);
        ("jne", "je")
    }

    /// Whether a comparison between `left` and `right` should use the
    /// unsigned jump mnemonics. Conservative: only inspects bare
    /// `Ident` operands (the common case in our fixtures). An untyped
    /// expression on either side defaults to signed, matching BCC's
    /// "promote literals to int" behavior.
    fn cmp_is_unsigned(&self, left: &Expr, right: &Expr) -> bool {
        self.expr_is_unsigned(left) || self.expr_is_unsigned(right)
    }

    fn expr_is_unsigned(&self, e: &Expr) -> bool {
        let ExprKind::Ident(name) = &e.kind else { return false };
        let ty = if let Some(gt) = self.globals.type_of(name) {
            gt
        } else {
            self.locals.type_of(name)
        };
        // Pointer compares are unsigned (addresses can exceed 0x7FFF
        // in real mode). Fixtures 2929, 2934 (`a < b` / `a >= b` for
        // pointer args).
        ty.is_unsigned() || ty.pointee().is_some()
    }

    /// Like `expr_is_unsigned`, but applies C's integer-promotion
    /// rule: a `char` / `unsigned char` operand is promoted to
    /// `int` (signed) before a shift, because `int` can hold all
    /// char values. The shift mnemonic (`sar` vs `shr`) follows the
    /// *promoted* type's signedness, not the operand's declared
    /// signedness. Used only by the shift dispatch path; comparison
    /// retains the operand's original signedness because BCC emits
    /// unsigned jumps (`jbe`/`jae`) for uchar compares.
    /// Fixture 1015 (`uchar c >> 2` → `sar` after promotion).
    fn expr_shift_is_unsigned(&self, e: &Expr) -> bool {
        let ExprKind::Ident(name) = &e.kind else { return false };
        let ty = if let Some(gt) = self.globals.type_of(name) {
            gt
        } else {
            self.locals.type_of(name)
        };
        if ty.is_char_like() {
            return false;
        }
        ty.is_unsigned()
    }

    /// Resolve a stack-resident lvalue chain (`Ident`, `ArrayIndex`
    /// with constant subscripts, `Member` via `Dot`, or any
    /// composition of those) to `(base_name, total_byte_offset,
    /// leaf_type)`. Returns `None` if the chain includes a
    /// non-constant subscript, a pointer dereference, or anything
    /// outside this lvalue shape. Used by the member/array codegen
    /// to fold `pts[1].x` and friends into a single `[bp-N]` operand
    /// (fixture 185).
    /// Build the textual ModR/M address for a name + byte offset
    /// returned by [`Self::try_lvalue_chain_addr`]. Returns `None`
    /// when the name resolves to a non-stack local (register-resident
    /// or non-existent), since those can't be addressed by memory
    /// operand directly.
    fn resolve_chain_addr(&self, name: &str, off: i32) -> Option<String> {
        if self.globals.contains(name) {
            return Some(if off == 0 {
                format!("DGROUP:_{name}")
            } else {
                format!("DGROUP:_{name}+{off}")
            });
        }
        if let LocalLocation::Stack(base) = self.locals.location_of(name) {
            let final_off = base + i16::try_from(off).unwrap_or(i16::MAX);
            return Some(bp_addr(final_off));
        }
        None
    }

    fn try_lvalue_chain_addr(&self, e: &Expr) -> Option<(String, i32, Type)> {
        match &e.kind {
            ExprKind::Ident(name) => {
                // Look up in globals first, then locals. Caller decides
                // whether to address via DGROUP-relative or bp-relative.
                let ty = if let Some(gt) = self.globals.type_of(name) {
                    gt.clone()
                } else {
                    self.locals.type_of(name).clone()
                };
                Some((name.clone(), 0, ty))
            }
            ExprKind::ArrayIndex { array, index } => {
                let (n, off, ty) = self.try_lvalue_chain_addr(array)?;
                let k = i32::try_from(try_const_eval(index)?).ok()?;
                let Type::Array { elem, .. } = &ty else { return None };
                let stride = i32::from(elem.size_bytes());
                let new_off = off.checked_add(k.checked_mul(stride)?)?;
                Some((n, new_off, (**elem).clone()))
            }
            ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } => {
                self.try_member_dot_chain(base, field)
            }
            _ => None,
        }
    }

    /// Wrapper for `try_lvalue_chain_addr` that takes the base and
    /// field separately, matching what the member-codegen sites
    /// already have on hand (they receive `base, field, kind` rather
    /// than a synthesized `Member` expr).
    fn try_member_dot_chain(
        &self,
        base: &Expr,
        field: &str,
    ) -> Option<(String, i32, Type)> {
        let (n, off, ty) = self.try_lvalue_chain_addr(base)?;
        let (field_off, field_ty) = ty.field(field)?;
        let new_off = off.checked_add(i32::from(field_off))?;
        Some((n, new_off, field_ty))
    }

    /// Emit the post-byte-load widening step needed to promote
    /// AL → AX. Signed char promotes via `cbw` (1 byte, `98`).
    /// Unsigned char promotes via `mov ah, 0` (2 bytes, `B4 00`)
    /// to preserve zero in the upper bits.
    fn emit_widen_al(&mut self, ty: &Type) {
        // Char return ABI: callees only need to populate AL; AH is
        // the caller's job to widen. Skip the widen step entirely
        // when emitting the return-value loader. Fixtures 3019,
        // 3325, 3227, 2881 (all char-return functions).
        if self.skip_widen {
            return;
        }
        if ty.is_unsigned() {
            self.out.extend_from_slice(b"\tmov\tah,0\r\n");
        } else {
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        }
    }

    /// Emit a "test against zero" instruction for a non-comparison
    /// expression — used in boolean contexts (`if (x)`, `x && y`).
    /// Today only `Ident`s are supported; other expressions panic.
    fn emit_zero_test(&mut self, cond: &Expr) {
        // `if ((x = expr))` — evaluate the assignment expression
        // into AX (leaving the value behind), then `or ax, ax` to
        // set the flags. Fixture 513.
        if let ExprKind::AssignExpr { .. } = &cond.kind {
            self.emit_expr_to_ax(cond);
            self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            return;
        }
        // `if (f())` — call yields its result in AX, then `or ax, ax`
        // sets ZF for the conditional branch. Fixture 591.
        if let ExprKind::Call { .. } = &cond.kind {
            self.emit_expr_to_ax(cond);
            self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            return;
        }
        // `while (--n)` / `while (++n)` — pre-inc/dec on a register-
        // resident int local. The inc/dec instruction itself sets ZF
        // based on the result, so we don't need a subsequent `or` or
        // load to AX. Just emit `inc/dec <reg>` and let the caller's
        // conditional jump read the flags. Fixtures 1844, 2361, 2749.
        if let ExprKind::Update {
            target,
            op,
            position: crate::ast::UpdatePosition::Pre,
        } = &cond.kind
            && self.locals.has(target)
            && let LocalLocation::Reg(reg) = self.locals.location_of(target)
            && self.locals.type_of(target).is_int_like()
        {
            let mnem = match op {
                crate::ast::UpdateOp::Inc => "inc",
                crate::ast::UpdateOp::Dec => "dec",
            };
            let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
            return;
        }
        // `while (x--)` — postinc/postdec as a boolean: the
        // current value of `x` is the test, then the side
        // effect happens. BCC's shape: `mov ax, <x>; dec <x>;
        // or ax, ax`. The Update lowering already produces the
        // value-in-AX-and-side-effect sequence; follow with
        // `or ax, ax` to set ZF. Fixture 619.
        if let ExprKind::Update { position: crate::ast::UpdatePosition::Post, .. } = &cond.kind {
            self.emit_expr_to_ax(cond);
            self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            return;
        }
        // `while (*p)` / `if (*p)` — deref of a register-resident
        // pointer local: `cmp <width> ptr [<reg>], 0` directly,
        // avoiding the round-trip through AX. Width follows the
        // pointee type. Fixture 636.
        if let ExprKind::Deref(operand) = &cond.kind
            && let ExprKind::Ident(name) = &operand.kind
            && self.locals.has(name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && let Some(pointee) = self.locals.type_of(name).pointee()
        {
            let width = if pointee.is_char_like() { "byte" } else { "word" };
            let _ = write!(self.out, "\tcmp\t{width} ptr [{}],0\r\n", reg.name());
            return;
        }
        // `if (p[K])` — global-pointer subscript in boolean context.
        // BCC loads the pointer into BX and emits `cmp word ptr
        // [bx+K*stride], 0` directly. Fixture 889.
        if let ExprKind::ArrayIndex { array, index } = &cond.kind
            && let ExprKind::Ident(name) = &array.kind
            && let Some(gty) = self.globals.type_of(name)
            && let Some(pointee) = gty.pointee()
            && let Some(k) = try_const_eval(index)
            && matches!(pointee, Type::Int | Type::UInt)
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let bx_disp = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\tcmp\tword ptr {bx_disp},0\r\n");
            return;
        }
        // `if (a[K])` — stack-resident array element as a zero
        // test. Same memory-direct shape as the int-local arm
        // below, just with a bp-relative element offset. Width
        // follows the leaf type (byte for char arrays, word for
        // int). Fixture 980.
        if let ExprKind::ArrayIndex { .. } = &cond.kind
            && let Some((name, total_off, leaf_ty)) =
                self.try_lvalue_chain_addr(cond)
            && self.locals.has(&name)
            && let LocalLocation::Stack(base_off) = self.locals.location_of(&name)
        {
            let elem_off = base_off + i16::try_from(total_off).unwrap_or(i16::MAX);
            let width = if leaf_ty.is_char_like() { "byte" } else { "word" };
            let _ = write!(self.out, "\tcmp\t{width} ptr {},0\r\n", bp_addr(elem_off));
            return;
        }
        // `if (<reg-local> & K)` — bit test against a constant mask
        // when the LHS is a register-resident int local. BCC emits
        // `test <reg>, K` (4 bytes, F7 C6 imm16 for SI; the `&` result
        // is discarded but flags are set). Fixture 1415 (popcount's
        // inner `if (x & 1)` with x in SI).
        if let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &cond.kind
            && let Some(reg) = self.ident_in_register(left)
            && let Some(k) = try_const_eval(right)
            && !reg.is_byte()
        {
            let k16 = k & 0xFFFF;
            let _ = write!(self.out, "\ttest\t{},{k16}\r\n", reg.name());
            return;
        }
        if let ExprKind::Ident(name) = &cond.kind {
            if let Some(gty) = self.globals.type_of(name) {
                // Global array name decays to its address, which is
                // always non-zero — test the address, not the first
                // element. BCC: `mov ax, offset DGROUP:_arr; or ax,
                // ax`. Fixture 2800.
                if matches!(gty, Type::Array { .. }) {
                    let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{name}\r\n");
                    self.out.extend_from_slice(b"\tor\tax,ax\r\n");
                    return;
                }
                let width = if gty.is_char_like() { "byte" } else { "word" };
                let _ = write!(self.out, "\tcmp\t{width} ptr DGROUP:_{name},0\r\n");
                return;
            }
            match self.locals.location_of(name) {
                LocalLocation::Stack(off) => {
                    let ty = self.locals.type_of(name);
                    let width = if ty.is_char_like() { "byte" } else { "word" };
                    let _ = write!(self.out, "\tcmp\t{width} ptr {},0\r\n", bp_addr(off));
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tor\t{0},{0}\r\n", reg.name());
                }
            }
            return;
        }
        // `if (<global-chain>)` — any lvalue chain rooted at a global
        // (e.g. `s.x`, `a[2]`, `s.a[1]`). BCC emits memory-direct
        // `cmp <width> ptr DGROUP:_<sym>[+K], 0`, avoiding the AX
        // round-trip. Fixture 3586 (`if (s.x && s.y)`).
        if let Some((name, off, ty)) = self.try_lvalue_chain_addr(cond)
            && self.globals.contains(&name)
            && !matches!(ty, Type::Array { .. } | Type::Struct { .. })
        {
            let width = if ty.is_char_like() { "byte" } else { "word" };
            let addr = if off == 0 {
                format!("DGROUP:_{name}")
            } else {
                format!("DGROUP:_{name}+{off}")
            };
            let _ = write!(self.out, "\tcmp\t{width} ptr {addr},0\r\n");
            return;
        }
        // Catch-all: evaluate the condition expression into AX and
        // test with `or ax, ax`. Covers any shape we don't have a
        // dedicated peephole for — `if ("X")` (StringLit address,
        // fixture 1582), `while (*++p)`, `if ((a = f()))`, etc. Not
        // always the tightest byte sequence BCC would pick, but it
        // sets ZF correctly and avoids a crash.
        self.emit_expr_to_ax(cond);
        self.out.extend_from_slice(b"\tor\tax,ax\r\n");
    }

    /// Emit just the `cmp` instruction (no jump). Four shapes,
    /// matching what BCC produces:
    ///
    /// 1. LHS in a register AND RHS is constant 0: `or <reg>, <reg>` —
    ///    a one-byte-shorter alias for `cmp <reg>, 0` (fixture 035).
    ///    Sets ZF/SF/PF the same way and clears OF/CF, which matches
    ///    what a `cmp` against zero produces, so the same signed
    ///    conditional-jump mnemonics work.
    /// 2. LHS in a register: `cmp <reg>, <rhs>`
    /// 3. LHS is a stack local and RHS is a constant: `cmp word ptr [bp-N], K`
    /// 4. Otherwise: `mov ax, <lhs>` then `cmp ax, <rhs>`
    fn emit_compare(&mut self, left: &Expr, right: &Expr) {
        // `<char_lvalue> <relop> <char_lvalue>` — both sides are
        // char-typed memory operands. BCC emits a byte compare:
        // `mov al, byte ptr <left>; cmp al, byte ptr <right>`. We
        // were widening left to AX first, which is unnecessary at
        // byte width. Fixture 1457 (`a[0] == a[2]` for char arr).
        if let Some((l_name, l_off, l_ty)) = self.try_lvalue_chain_addr(left)
            && let Some((r_name, r_off, r_ty)) = self.try_lvalue_chain_addr(right)
            && l_ty.is_char_like()
            && r_ty.is_char_like()
        {
            let l_addr = if self.globals.contains(&l_name) {
                if l_off == 0 {
                    format!("DGROUP:_{l_name}")
                } else {
                    format!("DGROUP:_{l_name}+{l_off}")
                }
            } else if let LocalLocation::Stack(base) = self.locals.location_of(&l_name) {
                let off = base + i16::try_from(l_off).unwrap_or(i16::MAX);
                bp_addr(off)
            } else {
                return;
            };
            let r_addr = if self.globals.contains(&r_name) {
                if r_off == 0 {
                    format!("DGROUP:_{r_name}")
                } else {
                    format!("DGROUP:_{r_name}+{r_off}")
                }
            } else if let LocalLocation::Stack(base) = self.locals.location_of(&r_name) {
                let off = base + i16::try_from(r_off).unwrap_or(i16::MAX);
                bp_addr(off)
            } else {
                return;
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {l_addr}\r\n");
            let _ = write!(self.out, "\tcmp\tal,byte ptr {r_addr}\r\n");
            return;
        }
        // `*p <relop> K` for register-resident pointer p: emit
        // memory-direct `cmp <width> ptr [<reg>], K` instead of
        // loading to AX first. Matches BCC's actual shape for
        // `while (*s != 0)` (fixture 1408) and `if (*r == 0)`
        // (fixture 1566).
        if let ExprKind::Deref(operand) = &left.kind
            && let ExprKind::Ident(name) = &operand.kind
            && self.locals.has(name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && let Some(pointee) = self.locals.type_of(name).pointee()
            && let Some(rhs) = try_const_eval(right)
        {
            let width = if pointee.is_char_like() { "byte" } else { "word" };
            let rhs_masked = if pointee.is_char_like() { rhs & 0xFF } else { rhs & 0xFFFF };
            let _ = write!(
                self.out,
                "\tcmp\t{width} ptr [{}],{rhs_masked}\r\n",
                reg.name(),
            );
            return;
        }
        if let Some(reg) = self.ident_in_register(left) {
            // Char in a byte register: 8-bit cmp with byte-truncated
            // immediate (fixture 054). Non-constant RHS is unobserved.
            if reg.is_byte() {
                if let Some(v) = try_const_eval(right) {
                    let v8 = v & 0xFF;
                    let _ = write!(self.out, "\tcmp\t{},{v8}\r\n", reg.name());
                    return;
                }
                panic!("char-register comparison with non-constant rhs not yet supported");
            }
            if let Some(0) = try_const_eval(right) {
                let _ = write!(self.out, "\tor\t{0},{0}\r\n", reg.name());
                return;
            }
            let src = self.resolve_operand_source(right);
            let _ = write!(self.out, "\tcmp\t{},{}\r\n", reg.name(), src.word());
            return;
        }
        if let (ExprKind::Ident(name), Some(rhs)) = (&left.kind, try_const_eval(right))
            && self.locals.has(name)
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            // Char-typed stack locals use the byte-form compare
            // (`80 7E disp8 imm8` — fixture 524).
            let ty = self.locals.type_of(name);
            if ty.is_char_like() {
                let rhs8 = rhs & 0xFF;
                let _ = write!(self.out, "\tcmp\tbyte ptr {},{rhs8}\r\n", bp_addr(off));
                return;
            }
            let rhs16 = rhs & 0xFFFF;
            let _ = write!(self.out, "\tcmp\tword ptr {},{rhs16}\r\n", bp_addr(off));
            return;
        }
        // `<int-global> <relop> <const>` — emit a memory-direct
        // compare `cmp word ptr DGROUP:_g, K`. BCC prefers the
        // imm8sx form (`83 3E disp16 ii`) when K fits a signed
        // byte; otherwise the imm16 form. Fixture 429.
        // Pointer globals share the same word-sized cmp path
        // (fixture 504: `if (g == 0)` with `int *g`).
        if let (ExprKind::Ident(name), Some(rhs)) = (&left.kind, try_const_eval(right))
            && let Some(gty) = self.globals.type_of(name)
            && (matches!(gty, Type::Int | Type::UInt) || gty.pointee().is_some())
        {
            let rhs16 = rhs & 0xFFFF;
            let _ = write!(self.out, "\tcmp\tword ptr DGROUP:_{name},{rhs16}\r\n");
            return;
        }
        // `<char-global> <relop> <const>` — byte-form memory
        // compare `cmp byte ptr DGROUP:_c, K` (encoded `80 3E ...`).
        // The char's int value is truncated to 8 bits. Fixture 452.
        if let (ExprKind::Ident(name), Some(rhs)) = (&left.kind, try_const_eval(right))
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_char_like()
        {
            let rhs8 = rhs & 0xFF;
            let _ = write!(self.out, "\tcmp\tbyte ptr DGROUP:_{name},{rhs8}\r\n");
            return;
        }
        // `<reg-ptr>-><field> <relop> <const>` — memory-direct compare
        // through a register-resident struct pointer. BCC emits `cmp
        // word ptr [<reg>+off], K` directly (4 bytes for disp!=0, 3
        // for disp=0). Restricted to SI (tasm only has the SI form
        // today) and word fields with imm8sx constants. Fixture 1007.
        if let (
            ExprKind::Member {
                base,
                field,
                kind: crate::ast::MemberKind::Arrow,
            },
            Some(rhs),
        ) = (&left.kind, try_const_eval(right))
            && let ExprKind::Ident(p_name) = &base.kind
            && self.locals.has(p_name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
            && reg.name() == "si"
            && let Some(pty) = self.locals.type_of(p_name).pointee()
            && let Some((field_off, field_ty)) = pty.field(field)
            && !field_ty.is_char_like()
            && i8::try_from(rhs).is_ok()
        {
            let reg_name = reg.name();
            let disp = if field_off == 0 {
                format!("[{reg_name}]")
            } else {
                format!("[{reg_name}+{field_off}]")
            };
            let _ = write!(self.out, "\tcmp\tword ptr {disp},{rhs}\r\n");
            return;
        }
        // `<char-stack> <relop> <char-stack>` — byte-byte compare
        // directly: `mov al, byte ptr <lhs>; cmp al, byte ptr <rhs>`.
        // No `cbw` widening since both sides are already byte values
        // and the compare's signedness is encoded in the *jump*
        // selection (jl/jb), not the operand width. Fixtures 951, 952.
        if let (ExprKind::Ident(ln), ExprKind::Ident(rn)) = (&left.kind, &right.kind)
            && self.locals.has(ln)
            && self.locals.has(rn)
            && self.locals.type_of(ln).is_char_like()
            && self.locals.type_of(rn).is_char_like()
            && let LocalLocation::Stack(loff) = self.locals.location_of(ln)
            && let LocalLocation::Stack(roff) = self.locals.location_of(rn)
        {
            let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(loff));
            let _ = write!(self.out, "\tcmp\tal,byte ptr {}\r\n", bp_addr(roff));
            return;
        }
        // `<stack-array-elem> <relop> <const>` — memory-direct
        // compare on a bp-relative array element. `cmp word ptr
        // [bp+(base+K*stride)], imm`. Same shape as the int-global
        // arm above, just with a bp-relative LHS. Sibling for char
        // arrays uses the byte form. Fixtures 978, 979.
        //
        // Also handles global-struct-field and global-array-member
        // chains: `s.x` resolves to `(name="s", total_off=0)` and
        // `s.a[K]` to `(name="s", total_off=field_off + K*stride)`,
        // both routing through the same memory-direct cmp shape but
        // against `DGROUP:_<name>+off`. Fixture 991 (`s.x == 5`).
        if let (ExprKind::ArrayIndex { .. } | ExprKind::Member { kind: crate::ast::MemberKind::Dot, .. }, Some(rhs)) =
            (&left.kind, try_const_eval(right))
            && let Some((name, total_off, leaf_ty)) =
                self.try_lvalue_chain_addr(left)
        {
            if self.locals.has(&name)
                && let LocalLocation::Stack(base_off) = self.locals.location_of(&name)
            {
                let elem_off = base_off + i16::try_from(total_off).unwrap_or(i16::MAX);
                if leaf_ty.is_char_like() {
                    let rhs8 = rhs & 0xFF;
                    let _ = write!(
                        self.out,
                        "\tcmp\tbyte ptr {},{rhs8}\r\n",
                        bp_addr(elem_off),
                    );
                } else {
                    let rhs16 = rhs & 0xFFFF;
                    let _ = write!(
                        self.out,
                        "\tcmp\tword ptr {},{rhs16}\r\n",
                        bp_addr(elem_off),
                    );
                }
                return;
            }
            if self.globals.contains(&name) {
                let addr = if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                };
                if leaf_ty.is_char_like() {
                    let rhs8 = rhs & 0xFF;
                    let _ = write!(self.out, "\tcmp\tbyte ptr {addr},{rhs8}\r\n");
                } else {
                    let rhs16 = rhs & 0xFFFF;
                    let _ = write!(self.out, "\tcmp\tword ptr {addr},{rhs16}\r\n");
                }
                return;
            }
            // Fall through to generic AX-based compare for non-local,
            // non-global roots (shouldn't normally happen).
        }
        self.emit_expr_to_ax(left);
        // `<expr-in-ax> <relop> 0` — use `or ax, ax` (2 bytes) instead
        // of `cmp ax, 0` (3 bytes) since both set ZF/SF the same way.
        // Fixture 555 (`while ((c = g) > 0)` lowers the post-load
        // zero test through this peephole).
        if let Some(0) = try_const_eval(right) {
            self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            return;
        }
        let src = self.resolve_operand_source(right);
        let _ = write!(self.out, "\tcmp\tax,{}\r\n", src.word());
    }

    /// Emit `a && b` / `a || b` in expression position — the value
    /// (0 or 1) must land in AX. Layout (fixtures 059, 060):
    ///
    /// && (slots: +0 unused, +1 unused, +2 false-mat, +3 end):
    /// ```text
    ///   <cond-branch(a, true=None, false=false-mat)>
    ///   <cond-branch(b, true=None, false=false-mat)>
    ///   mov ax, 1
    ///   jmp short end
    /// false-mat:
    ///   xor ax, ax
    /// end:
    /// ```
    ///
    /// || (slots: +0 unused, +1 true-mat, +2 false-mat, +3 end):
    /// ```text
    ///   <cond-branch(a, true=true-mat, false=None)>
    ///   <cond-branch(b, true=None,     false=false-mat)>
    /// true-mat:
    ///   mov ax, 1
    ///   jmp short end
    /// false-mat:
    ///   xor ax, ax
    /// end:
    /// ```
    fn emit_logical_to_ax(
        &mut self,
        logical_span_start: u32,
        op: LogicalOp,
        left: &Expr,
        right: &Expr,
    ) {
        let base = self.label_plan.base(logical_span_start);
        let true_mat_slot = base + 1;
        let false_mat_slot = base + 2;
        let end_slot = base + 3;
        match op {
            LogicalOp::And => {
                self.emit_cond_branch(left, None, Some(false_mat_slot));
                self.emit_cond_branch(right, None, Some(false_mat_slot));
            }
            LogicalOp::Or => {
                self.emit_cond_branch(left, Some(true_mat_slot), None);
                self.emit_cond_branch(right, None, Some(false_mat_slot));
                self.emit_label(true_mat_slot);
            }
        }
        self.out.extend_from_slice(b"\tmov\tax,1\r\n");
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(end_slot));
        self.emit_label(false_mat_slot);
        self.out.extend_from_slice(b"\txor\tax,ax\r\n");
        self.emit_label(end_slot);
    }

    /// Emit a prefix unary operator. The operand always lands in AX
    /// first, then the per-op tail runs:
    ///
    /// - `-e` → `neg ax`.
    /// - `~e` → `not ax`.
    /// - `!e` → `neg ax / sbb ax,ax / inc ax`. Classic zero-test:
    ///   after `neg`, CF == (operand != 0); `sbb ax,ax` materializes
    ///   `-CF` (0 or 0xFFFF); `inc ax` shifts to 1 or 0. Fixture 038.
    fn emit_unary(&mut self, op: UnaryOp, operand: &Expr) {
        self.emit_expr_to_ax(operand);
        match op {
            UnaryOp::Neg => self.out.extend_from_slice(b"\tneg\tax\r\n"),
            UnaryOp::BitNot => self.out.extend_from_slice(b"\tnot\tax\r\n"),
            UnaryOp::Not => {
                self.out.extend_from_slice(b"\tneg\tax\r\n");
                self.out.extend_from_slice(b"\tsbb\tax,ax\r\n");
                self.out.extend_from_slice(b"\tinc\tax\r\n");
            }
        }
    }

    /// Emit `++x` / `--x` / `x++` / `x--` *as an expression* — the
    /// result must land in AX. Shapes (target in a register, fixtures
    /// 043 and 044):
    ///
    /// - Pre  (`++x`): `inc <reg>` / `mov ax, <reg>`
    /// - Post (`x++`): `mov ax, <reg>` / `inc <reg>`
    ///
    /// Equivalents with `dec` for `--`. Stack-resident targets panic
    /// (no fixture yet).
    fn emit_update_to_ax(&mut self, target: &str, op: UpdateOp, position: UpdatePosition) {
        let mnemonic = match op {
            UpdateOp::Inc => "inc",
            UpdateOp::Dec => "dec",
        };
        // Global ++/-- in expression context. Int/uint uses
        // memory-direct `inc word ptr DGROUP:_g` for the side effect
        // plus a separate AX load for the captured value. Pre-update
        // emits the side effect *before* the load; post-update loads
        // first, then mutates. Char/uchar uses the AL detour
        // (`mov al, mem; inc al; mov mem, al; cbw`) for Pre, and
        // load-then-mutate for Post (the captured value is the
        // pre-update one). Fixtures 962/963 (int) and 964 (char).
        if let Some(gty) = self.globals.type_of(target) {
            let gty = gty.clone();
            if gty.is_char_like() {
                let unsigned = gty.is_unsigned();
                match position {
                    UpdatePosition::Pre => {
                        let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{target},al\r\n");
                    }
                    UpdatePosition::Post => {
                        let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\t{mnemonic}\tbyte ptr DGROUP:_{target}\r\n");
                    }
                }
                if unsigned {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
                return;
            }
            if matches!(gty, Type::Int | Type::UInt) || gty.pointee().is_some() {
                match position {
                    UpdatePosition::Pre => {
                        let _ = write!(self.out, "\t{mnemonic}\tword ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{target}\r\n");
                    }
                    UpdatePosition::Post => {
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\t{mnemonic}\tword ptr DGROUP:_{target}\r\n");
                    }
                }
                return;
            }
            panic!("++/-- in expression on non-int/non-char global `{target}` not yet supported (no fixture)");
        }
        // Stack-resident char ++/-- in expression context: BCC uses
        // memory-direct `inc|dec byte ptr [bp-N]` for the side
        // effect, with the captured value loaded via `mov al,
        // byte ptr [bp-N]` before (post) or after (pre) the
        // memory inc/dec, then `cbw` to widen for the caller.
        // Fixture 731 (`f(c++)` for stack-resident char c).
        let reg = match self.locals.location_of(target) {
            LocalLocation::Reg(r) => r,
            LocalLocation::Stack(off) => {
                let ty = self.locals.type_of(target).clone();
                if ty.is_char_like() {
                    let unsigned = ty.is_unsigned();
                    match position {
                        // Pre: AL detour. BCC threads the new value
                        // through AL even for stack-resident char,
                        // mirroring the way `++g` is lowered for
                        // char globals (batch 128). `mov al, mem;
                        // inc al; mov mem, al; cbw`. Fixture 732.
                        UpdatePosition::Pre => {
                            let _ = write!(
                                self.out,
                                "\tmov\tal,byte ptr {}\r\n",
                                bp_addr(off),
                            );
                            let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                            let _ = write!(
                                self.out,
                                "\tmov\tbyte ptr {},al\r\n",
                                bp_addr(off),
                            );
                        }
                        // Post: load value, memory-direct side
                        // effect, widen. The captured value is the
                        // pre-update one. Fixture 731.
                        UpdatePosition::Post => {
                            let _ = write!(
                                self.out,
                                "\tmov\tal,byte ptr {}\r\n",
                                bp_addr(off),
                            );
                            let _ = write!(
                                self.out,
                                "\t{mnemonic}\tbyte ptr {}\r\n",
                                bp_addr(off),
                            );
                        }
                    }
                    if unsigned {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                    return;
                }
                panic!("++/-- in expression on a stack-resident non-char local not yet supported (no fixture)");
            }
        };
        if reg.is_byte() {
            // Char ++/-- in expression context: load the byte into
            // AL, sign-extend to AX, and apply the side effect to
            // the byte register. For postinc/dec the load goes
            // before the inc/dec so the captured value is the pre-
            // update one. Fixture 649 (`r = c++` with c in DL).
            match position {
                UpdatePosition::Pre => {
                    let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
                UpdatePosition::Post => {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                    let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                }
            }
            return;
        }
        match position {
            UpdatePosition::Pre => {
                let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
            }
            UpdatePosition::Post => {
                let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
            }
        }
    }

    /// Emit a function call: push args right-to-left, `call near ptr
    /// _name`, then clean up the pushed args. Each arg is pushed as a
    /// 16-bit word, but **char** parameters use the byte form for the
    /// value-loading instruction (`mov al, K` or `mov al, <src>`)
    /// before the `push ax` — the high byte of the pushed word is
    /// undefined since the callee only reads the low byte (fixture
    /// 052 and 055).
    ///
    /// Cleanup: `pop cx` per arg when there are ≤ 2 args; for ≥ 3
    /// args BCC switches to `add sp, N*2` (one 3-byte instruction
    /// beats three or more `pop cx`s). Fixtures 010 (0), 033 (1),
    /// 034 (2), 049 (3), 046/048 (4).
    fn emit_call(&mut self, name: &str, args: &[Expr]) {
        let param_tys = self.signatures.params_of(name);
        // Pre-intern string literal args in SOURCE (left-to-right)
        // order before the right-to-left push loop. BCC pools
        // strings in source order regardless of push order.
        // Fixture 2196 (`printf("%s\n", "Hello")` — pool layout
        // `"%s\n\0Hello\0"`, not `"Hello\0%s\n\0"`).
        fn intern_strings_in_order(emitter: &mut FunctionEmitter<'_>, e: &Expr) {
            match &e.kind {
                ExprKind::StringLit(bytes) => {
                    emitter.strings.intern(bytes);
                }
                ExprKind::BinOp { left, right, .. }
                | ExprKind::Logical { left, right, .. }
                | ExprKind::Comma { left, right } => {
                    intern_strings_in_order(emitter, left);
                    intern_strings_in_order(emitter, right);
                }
                ExprKind::Unary { operand, .. }
                | ExprKind::Cast { operand, .. }
                | ExprKind::Deref(operand) => {
                    intern_strings_in_order(emitter, operand);
                }
                ExprKind::Ternary { cond, then_value, else_value } => {
                    intern_strings_in_order(emitter, cond);
                    intern_strings_in_order(emitter, then_value);
                    intern_strings_in_order(emitter, else_value);
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        intern_strings_in_order(emitter, a);
                    }
                }
                ExprKind::ArrayIndex { array, index } => {
                    intern_strings_in_order(emitter, array);
                    intern_strings_in_order(emitter, index);
                }
                ExprKind::Member { base, .. } => {
                    intern_strings_in_order(emitter, base);
                }
                _ => {}
            }
        }
        for arg in args {
            intern_strings_in_order(self, arg);
        }
        let mut total_bytes: u32 = 0;
        for (i, arg) in args.iter().enumerate().rev() {
            // Param type for the i-th arg, defaulting to int when the
            // signature isn't known (extern function — no fixture yet).
            let arg_ty = param_tys
                .and_then(|tys| tys.get(i))
                .cloned()
                .unwrap_or(Type::Int);
            if arg_ty.is_long_like() {
                // Long arg: materialize (AX=high, DX=low), push
                // high then low. 4 bytes per arg. Fixture 216.
                self.emit_long_arg_push(arg);
                total_bytes += 4;
            } else if let Type::Struct { .. } = &arg_ty {
                // Struct-by-value arg. Two shapes by size:
                //   - 4 bytes: push two words high-first, identical
                //     to a long-arg push (fixture 419 byte-matches
                //     fixture 322's long shape).
                //   - > 4 bytes: route through `N_SPUSH@`. Helper
                //     takes the source far pointer in DX:AX and the
                //     byte count in CX; it pushes the bytes onto the
                //     caller's stack in place. Fixture 420.
                let size = arg_ty.size_bytes() as u32;
                let ExprKind::Ident(src_name) = &arg.kind else {
                    panic!("non-ident struct-by-value arg not yet supported (no fixture)");
                };
                let src_is_global = self.globals.type_of(src_name).is_some();
                if size == 4 {
                    if src_is_global {
                        let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{src_name}+2\r\n");
                        let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{src_name}\r\n");
                    } else {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                        else {
                            panic!("struct local `{src_name}` not stack-resident");
                        };
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(src_off + 2));
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(src_off));
                    }
                } else {
                    if src_is_global {
                        let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{src_name}\r\n");
                        self.out.extend_from_slice(b"\tmov\tdx,ds\r\n");
                    } else {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                        else {
                            panic!("struct local `{src_name}` not stack-resident");
                        };
                        let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(src_off));
                        self.out.extend_from_slice(b"\tmov\tdx,ss\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_SPUSH@\r\n");
                    self.helpers.insert("N_SPUSH@".to_string());
                }
                total_bytes += size;
            } else if let Some(push_form) = self.try_direct_arg_push(arg, &arg_ty) {
                // Memory-operand peephole: when the arg is a simple
                // load (stack-local int/ptr, global int/ptr, or a
                // const-index array element resolving to one of those),
                // skip the `mov ax, m / push ax` pair and emit `push
                // word ptr <m>` directly. Fixture 589 (`f(a[1])`).
                let _ = write!(self.out, "\t{push_form}\r\n");
                total_bytes += 2;
            } else if !arg_ty.is_char_like()
                && let ExprKind::BinOp { op: BinOp::Mod, .. } = &arg.kind
            {
                // Mod-result arg: the idiv already leaves the
                // remainder in DX. Skip the `mov ax, dx` and push
                // DX directly. Saves 2 bytes per call. Fixture 1391
                // (`gcd(b, a % b)`).
                self.skip_mod_to_ax = true;
                self.emit_arg_into_ax(arg, arg_ty);
                self.skip_mod_to_ax = false;
                self.out.extend_from_slice(b"\tpush\tdx\r\n");
                total_bytes += 2;
            } else {
                self.emit_arg_into_ax(arg, arg_ty);
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                total_bytes += 2;
            }
        }
        // Direct call to a function symbol vs. indirect call through
        // a function-pointer local. The disambiguator is whether
        // `name` names a local in this frame (fixture 110): if so,
        // emit `call word ptr [bp-N]`; otherwise `call near ptr _N`.
        if self.locals.has(name) {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                panic!(
                    "indirect call through register-resident fn-ptr `{name}` not yet supported"
                );
            };
            let _ = write!(self.out, "\tcall\tword ptr {}\r\n", bp_addr(off));
        } else {
            let _ = write!(self.out, "\tcall\tnear ptr _{name}\r\n");
        }
        // Cleanup: BCC uses `pop cx` per word when total ≤ 4 bytes,
        // `add sp, N` for 6 bytes or more. The threshold is shared
        // across int and long args — fixture 216's single long arg
        // pushes 4 bytes and gets 2 pops, mirroring the 2-int-args
        // shape.
        if total_bytes == 0 {
            // nothing
        } else if total_bytes <= 4 {
            for _ in 0..(total_bytes / 2) {
                self.out.extend_from_slice(b"\tpop\tcx\r\n");
            }
        } else {
            let _ = write!(self.out, "\tadd\tsp,{total_bytes}\r\n");
        }
    }

    /// Push a long argument onto the call stack as two words, **high
    /// half first, then low half** — so the low half ends up at the
    /// lower bp-offset in the callee. Per BCC's calling convention.
    /// Const args materialize into AX/DX first (fixture 216);
    /// lvalues with known addresses push memory-direct (fixtures
    /// 322–325).
    fn emit_long_arg_push(&mut self, arg: &Expr) {
        if let Some(v) = try_const_eval(arg) {
            let lo = v & 0xFFFF;
            let hi = (v >> 16) & 0xFFFF;
            if hi == 0 {
                self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{hi}\r\n");
            }
            if lo == 0 {
                self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tdx,{lo}\r\n");
            }
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            return;
        }
        // Long global ident — push both halves memory-direct via
        // `push word ptr DGROUP:_g+2 / push word ptr DGROUP:_g`.
        // Fixture 322.
        if let ExprKind::Ident(name) = &arg.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_long_like()
        {
            let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}+2\r\n");
            let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}\r\n");
            return;
        }
        // Long stack local — push both halves via `push word ptr
        // [bp+off+2] / push word ptr [bp+off]`. Fixture 323.
        if let ExprKind::Ident(name) = &arg.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
        {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long is never register-resident");
            };
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(off + 2));
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(off));
            return;
        }
        // `*p` for `p: long *` register-resident — push both halves
        // through the pointer register. Fixture 325.
        if let ExprKind::Deref(operand) = &arg.kind
            && let ExprKind::Ident(ptr_name) = &operand.kind
            && self.locals.has(ptr_name)
            && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
            && pointee.is_long_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(ptr_name)
        {
            let r = reg.name();
            let _ = write!(self.out, "\tpush\tword ptr [{r}+2]\r\n");
            let _ = write!(self.out, "\tpush\tword ptr [{r}]\r\n");
            return;
        }
        // Long dot-chain lvalue (`s.x`, `a[K].x`, …) — push both
        // halves memory-direct at the resolved address. Fixture 326.
        if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } = &arg.kind
            && let Some((src, total_off, leaf_ty)) = self.try_member_dot_chain(base, field)
            && leaf_ty.is_long_like()
        {
            let (lo_addr, hi_addr) = if self.globals.contains(&src) {
                (
                    global_offset_addr(&src, total_off),
                    global_offset_addr(&src, total_off + 2),
                )
            } else {
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&src) else {
                    panic!("struct local `{src}` not stack-resident");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                (bp_addr(off), bp_addr(off + 2))
            };
            let _ = write!(self.out, "\tpush\tword ptr {hi_addr}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lo_addr}\r\n");
            return;
        }
        // Long array element (const index) on a global — push both
        // halves at `_a + K*4`. Fixture 328.
        if let ExprKind::ArrayIndex { array: arr_expr, index } = &arg.kind
            && let ExprKind::Ident(arr_name) = &arr_expr.kind
            && let Some(arr_ty) = self.globals.type_of(arr_name)
            && let Some(elem) = arr_ty.array_elem()
            && elem.is_long_like()
            && let Some(k) = try_const_eval(index)
        {
            let byte_off = (k as i32) * 4;
            let lo_addr = global_offset_addr(arr_name, byte_off);
            let hi_addr = global_offset_addr(arr_name, byte_off + 2);
            let _ = write!(self.out, "\tpush\tword ptr {hi_addr}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lo_addr}\r\n");
            return;
        }
        // Long arg from a two-lvalue arith/bitwise expression
        // (`f(a + b)`, `f(a & b)`, …). Compute into AX:DX using the
        // memory-dest register convention (AX=high, DX=low), then
        // push high (AX) first / low (DX) second so the long lands
        // on the stack with low at the lower address. Fixture 386.
        if let ExprKind::BinOp { op, left, right } = &arg.kind
            && let Some((lo_op, hi_op)) = long_pair_op(*op)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\t{lo_op}\tdx,word ptr {b_lo}\r\n");
            let _ = write!(self.out, "\t{hi_op}\tax,word ptr {b_hi}\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            return;
        }
        // Long arg from a long-returning function call (`f(g())`
        // where `long g();`). The call leaves the result in DX:AX
        // (cdecl long-return ABI: DX=high, AX=low) — so to push
        // high first BCC emits `push dx; push ax`. Note the
        // *order* of pushes is flipped relative to the memory-
        // dest path (fixture 386: `push ax; push dx`), because
        // the producer step left the registers in the opposite
        // convention. The push pair adapts to whatever the producer
        // left in DX:AX. Fixture 387.
        if let ExprKind::Call { name: fname, args } = &arg.kind
            && args.is_empty()
        {
            let _ = write!(self.out, "\tcall\tnear ptr _{fname}\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            return;
        }
        // Long arg from a long mul (`f(g * h)`). Same passthrough
        // pattern as the call case: helper returns DX:AX = high:
        // low, so `push dx; push ax`. First operand → CX:BX,
        // second → DX:AX (helper convention). Fixture 388.
        if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &arg.kind
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let _ = write!(self.out, "\tmov\tcx,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tbx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {b_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {b_lo}\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            return;
        }
        panic!("non-constant long argument not yet supported (no fixture)");
    }

    /// Memory-operand `push` peephole: if `arg` is a simple load of a
    /// 2-byte value that resolves to a single addressing mode (stack
    /// local, global, or const-index array element on either), return
    /// the `push word ptr <m>` mnemonic string. The caller substitutes
    /// the `mov ax, <m>; push ax` pair with `push word ptr <m>`,
    /// saving one byte. Fixture 589 (`f(a[1])` over a local int array).
    fn try_direct_arg_push(&self, arg: &Expr, param_ty: &Type) -> Option<String> {
        if param_ty.is_char_like() || param_ty.is_long_like() {
            return None;
        }
        // Bare stack-local int/ptr: `push word ptr [bp+N]` directly.
        // Fixture 3116 (`printf(x)` for x at [bp+4]), 2688 (3-arg int
        // call), 1656.
        if let ExprKind::Ident(name) = &arg.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            return Some(format!("push\tword ptr {}", bp_addr(off)));
        }
        // Bare register-resident int local: `push <reg>` directly
        // (1 byte) instead of `mov ax,<reg>; push ax` (3 bytes).
        // Fixtures 2753, 1506, 1580.
        if let ExprKind::Ident(name) = &arg.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && !reg.is_byte()
        {
            return Some(format!("push\t{}", reg.name()));
        }
        // Bare global int/ptr: `push word ptr DGROUP:_<name>` directly.
        if let ExprKind::Ident(name) = &arg.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_int_like()
        {
            return Some(format!("push\tword ptr DGROUP:_{name}"));
        }
        // `*p` for a register-resident int pointer: `push word ptr
        // [<reg>]` directly. Fixture 1292 (`f(*p)` with p in SI).
        if let ExprKind::Deref(operand) = &arg.kind
            && let ExprKind::Ident(name) = &operand.kind
            && self.locals.has(name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && let Some(pointee) = self.locals.type_of(name).pointee()
            && pointee.is_int_like()
        {
            return Some(format!("push\tword ptr [{}]", reg.name()));
        }
        if let ExprKind::ArrayIndex { array, index } = &arg.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let arr_ty = self.locals.type_of(arr_name).clone()
            && arr_ty.array_elem().is_some_and(|e| e.size_bytes() == 2)
            && let Some((const_off, _leaf)) =
                try_const_array_offset(&arr_ty, std::iter::once(&**index))
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
        {
            let src_off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
            return Some(format!("push\tword ptr {}", bp_addr(src_off)));
        }
        None
    }

    /// Place an argument into AX (the low byte of which is `al`) for
    /// the subsequent `push ax`. For a `char` param the load uses the
    /// 8-bit form so only AL is touched; AH is whatever happened to
    /// be there. For `int`, the standard 16-bit load.
    fn emit_arg_into_ax(&mut self, arg: &Expr, param_ty: Type) {
        if !param_ty.is_char_like() {
            // Array-decay-to-pointer at call sites: passing the bare
            // name of an array global (or array stack local) where a
            // pointer parameter is expected means the array's address,
            // not its value. BCC emits `mov ax, offset DGROUP:_<a>`
            // (or `lea ax, word ptr [bp-N]` for stack arrays) rather
            // than loading. Fixture 923.
            if let ExprKind::Ident(name) = &arg.kind {
                if let Some(gty) = self.globals.type_of(name)
                    && gty.array_elem().is_some()
                {
                    let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{name}\r\n");
                    return;
                }
                if self.locals.has(name)
                    && self.locals.type_of(name).array_elem().is_some()
                    && let LocalLocation::Stack(off) = self.locals.location_of(name)
                {
                    let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
                    return;
                }
            }
            self.emit_expr_to_ax(arg);
            return;
        }
        // Char arg path.
        if let Some(v) = try_const_eval(arg) {
            // 8-bit immediate.
            let v8 = v & 0xFF;
            let _ = write!(self.out, "\tmov\tal,{v8}\r\n");
            return;
        }
        if let ExprKind::Ident(name) = &arg.kind {
            let ty = self.locals.type_of(name);
            assert!(
                ty.is_char_like(),
                "passing non-char `{name}` to a char parameter not yet supported (no fixture)"
            );
            match self.locals.location_of(name) {
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                }
            }
            return;
        }
        panic!("complex char-typed arg expression not yet supported (no fixture)");
    }

    /// If `e` is an identifier that refers to a register-resident
    /// local, return that register. Otherwise `None`.
    fn ident_in_register(&self, e: &Expr) -> Option<Reg> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        if !self.locals.has(name) {
            return None;
        }
        match self.locals.location_of(name) {
            LocalLocation::Reg(r) => Some(r),
            LocalLocation::Stack(_) => None,
        }
    }

    fn emit_return_value_load(&mut self, value: Option<&Expr>) {
        let Some(e) = value else { return };
        // Set `skip_widen` while emitting the char-return value so
        // any byte-load deep in the expression doesn't tack on a
        // useless `cbw` / `mov ah, 0`. Restored after emission.
        // Covers the universal char-return ABI: callee leaves AL,
        // caller widens via `cbw` after the call. Fixtures 3019,
        // 3325, 3227, 2881.
        let skip_widen_prev = self.skip_widen;
        if self.function.ret_ty.is_char_like() {
            self.skip_widen = true;
        }
        let result = self.emit_return_value_load_inner(e);
        self.skip_widen = skip_widen_prev;
        result
    }

    fn emit_return_value_load_inner(&mut self, e: &Expr) {
        // Char-returning function with a constant `return K;` —
        // `mov al, K` (2 bytes) leaves AH undefined per the ABI for
        // char return values, which is exactly what BCC emits for
        // `char f() { return 'Z'; }`. Fixture 562.
        if self.function.ret_ty.is_char_like()
            && let Some(v) = try_const_eval(e)
        {
            let v8 = v & 0xFF;
            let _ = write!(self.out, "\tmov\tal,{v8}\r\n");
            return;
        }
        // `return (char)(<int_lvalue> <op> <int_lvalue>);` — BCC
        // operates at byte width: `mov al, [a]; <op> al, [b]; cbw`.
        // Saves the word load + word op vs narrowing later. Only
        // for additive/bitwise ops where the low byte is independent
        // of the high half. Fixtures 1535, 1538, 1539, 1541, 1542.
        if !self.function.ret_ty.is_long_like()
            && let ExprKind::Cast { ty: cast_ty, operand } = &e.kind
            && cast_ty.is_char_like()
            && let ExprKind::BinOp { op: binop, left, right } = &operand.kind
            && matches!(binop,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some((l_name, l_off, _)) = self.try_lvalue_chain_addr(left)
            && let Some((r_name, r_off, _)) = self.try_lvalue_chain_addr(right)
            && let Some(l_addr) = self.resolve_chain_addr(&l_name, l_off)
            && let Some(r_addr) = self.resolve_chain_addr(&r_name, r_off)
        {
            let mnem = match binop {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {l_addr}\r\n");
            let _ = write!(self.out, "\t{mnem}\tal,byte ptr {r_addr}\r\n");
            if !self.skip_widen {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            }
            return;
        }
        // `return (char)(<int_lvalue> << K);` — byte load + byte
        // shifts + cbw. K in 1..=3 unrolls; K >= 4 uses CL form.
        // Byte form is correct because Shl pushes bits OUT of the
        // low byte (upper bits don't affect the surviving low byte).
        // For Shr, the upper bits shift INTO the low byte — we
        // can't use byte form, so leave Shr to the general word
        // path below. Fixtures 1543 (shl 2), 1546 (shl by 8).
        if !self.function.ret_ty.is_long_like()
            && let ExprKind::Cast { ty: cast_ty, operand } = &e.kind
            && cast_ty.is_char_like()
            && let ExprKind::BinOp { op: BinOp::Shl, left, right } = &operand.kind
            && let Some((src_name, src_off, _)) = self.try_lvalue_chain_addr(left)
            && let Some(src_addr) = self.resolve_chain_addr(&src_name, src_off)
            && let Some(k) = try_const_eval(right)
            && k >= 1
            && k <= 255
        {
            let _ = write!(self.out, "\tmov\tal,byte ptr {src_addr}\r\n");
            if k <= 3 {
                for _ in 0..k {
                    self.out.extend_from_slice(b"\tshl\tal,1\r\n");
                }
            } else {
                let _ = write!(self.out, "\tmov\tcl,{k}\r\n");
                self.out.extend_from_slice(b"\tshl\tal,cl\r\n");
            }
            if !self.skip_widen && !self.function.ret_ty.is_char_like() {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            }
            return;
        }
        // `return (uchar)<int_lvalue>;` / `return (char)<int_lvalue>;`
        // — narrow then widen back. BCC byte-loads from the source
        // (`mov al, byte ptr <src>`) and widens. Signed cast: cbw;
        // unsigned cast: `mov ah, 0`. Fixtures 1524, 1533, 3236.
        if !self.function.ret_ty.is_long_like()
            && !self.function.ret_ty.is_char_like()
            && let ExprKind::Cast { ty: cast_ty, operand } = &e.kind
            && cast_ty.is_char_like()
            && let Some((src_name, src_off, _)) = self.try_lvalue_chain_addr(operand)
            && let Some(src_addr) = self.resolve_chain_addr(&src_name, src_off)
        {
            let _ = write!(self.out, "\tmov\tal,byte ptr {src_addr}\r\n");
            if !self.skip_widen {
                if cast_ty.is_unsigned() {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
            }
            return;
        }
        // `return (char)(<expr>);` for the general case: emit the
        // operand at word width (it would have been the same byte
        // sequence either way for mul/div — the AL truncation
        // happens via cbw on the low byte). Save the word load +
        // word op + cbw vs narrowing via a separate store. Fixtures
        // 1540 (mul), 1545 (div).
        if !self.function.ret_ty.is_long_like()
            && !self.function.ret_ty.is_char_like()
            && let ExprKind::Cast { ty: cast_ty, operand } = &e.kind
            && cast_ty.is_char_like()
            && !matches!(&operand.kind, ExprKind::IntLit(_))
        {
            self.emit_expr_to_ax(operand);
            if !self.skip_widen {
                if cast_ty.is_unsigned() {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
            }
            return;
        }
        // Struct return. Two shapes by size, paralleling the
        // struct-copy and struct-by-value-arg cases:
        //   - 4 bytes: load high to DX, low to AX — *byte-identical*
        //     to a long return (DX:AX = high:low). Fixture 422.
        //   - > 4 bytes: BCC has injected a hidden first param at
        //     [bp+4..7] holding a far pointer to the caller's
        //     return buffer. The callee pushes that buffer's far ptr
        //     and the source's far ptr, calls `N_SCOPY@`, then
        //     returns the buffer's offset in AX. Fixture 423.
        if let Type::Struct { .. } = &self.function.ret_ty {
            let size = self.function.ret_ty.size_bytes() as u32;
            // 1-byte struct (just `char c;`) — byte-load from the
            // struct's first (only) field into AL. Caller picks up
            // the result in AL. Fixture 2537 (`struct Tiny { char c;
            // } make(void) { ... return t; }`).
            if size == 1
                && let ExprKind::Ident(src_name) = &e.kind
                && self.locals.has(src_name)
                && let LocalLocation::Stack(off) = self.locals.location_of(src_name)
            {
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                return;
            }
            if size == 4
                && let ExprKind::Ident(src_name) = &e.kind
                && self.globals.type_of(src_name).map_or(false, |t| t == &self.function.ret_ty)
            {
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}+2\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}\r\n");
                return;
            }
            if size > 4
                && let ExprKind::Ident(src_name) = &e.kind
                && self.globals.type_of(src_name).map_or(false, |t| t == &self.function.ret_ty)
            {
                self.out.extend_from_slice(b"\tpush\tword ptr [bp+6]\r\n");
                self.out.extend_from_slice(b"\tpush\tword ptr [bp+4]\r\n");
                let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{src_name}\r\n");
                self.out.extend_from_slice(b"\tpush\tds\r\n");
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_SCOPY@\r\n");
                self.helpers.insert("N_SCOPY@".to_string());
                self.out.extend_from_slice(b"\tmov\tax,word ptr [bp+4]\r\n");
                return;
            }
            // 2-byte struct (e.g. `struct { char a, b; }`): just
            // load the word. Caller picks it up in AX. Fixture 2531.
            if size == 2
                && let ExprKind::Ident(src_name) = &e.kind
                && self.locals.has(src_name)
                && let LocalLocation::Stack(off) = self.locals.location_of(src_name)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                return;
            }
            // Stack-local struct of size 3, 5, 6, 7, 8+ (not 1, 2,
            // or 4): hidden buffer copy via N_SCOPY@. Far-ptr to
            // caller's buffer is at [bp+4..7]; far-ptr to our local
            // is SS:[bp-N]. Caller cleans up. Fixture 2526 (3-byte),
            // 2671 (5-byte), 2755 (8-byte), 1877/2352 (large).
            if size != 1
                && size != 2
                && size != 4
                && let ExprKind::Ident(src_name) = &e.kind
                && self.locals.has(src_name)
                && let LocalLocation::Stack(off) = self.locals.location_of(src_name)
            {
                self.out.extend_from_slice(b"\tpush\tword ptr [bp+6]\r\n");
                self.out.extend_from_slice(b"\tpush\tword ptr [bp+4]\r\n");
                let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
                self.out.extend_from_slice(b"\tpush\tss\r\n");
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_SCOPY@\r\n");
                self.helpers.insert("N_SCOPY@".to_string());
                self.out.extend_from_slice(b"\tmov\tax,word ptr [bp+4]\r\n");
                return;
            }
        }
        // Long return: standard 8086 32-bit return-value convention
        // puts the high word in DX and the low word in AX. (Note
        // BCC swaps the AX/DX roles when doing in-memory long
        // arithmetic — see fixture 207 — but the boundary at
        // `return` uses the ABI-standard layout.) Fixture 212.
        if self.function.ret_ty.is_long_like() {
            if let Some(v) = try_const_eval(e) {
                let lo = v & 0xFFFF;
                let hi = (v >> 16) & 0xFFFF;
                if hi == 0 {
                    self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tdx,{hi}\r\n");
                }
                if lo == 0 {
                    self.out.extend_from_slice(b"\txor\tax,ax\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tax,{lo}\r\n");
                }
                return;
            }
            // `return <long-lvalue>;` — load high to DX, low to AX
            // per the ABI return convention. Covers bare-ident long
            // global (fixture 213), bare-ident long param/stack
            // local (fixture 217), struct/union dot-chain field
            // (fixture 363), and constant-indexed array element
            // (fixture 364). `long_lvalue_addr_pair` returns the
            // (high, low) address strings for any supported lvalue
            // form, including DGROUP:_g+2/+0, DGROUP:_a+offN/N, and
            // [bp+M+2]/[bp+M].
            if let Some((hi_addr, lo_addr)) = self.long_lvalue_addr_pair(e) {
                let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                return;
            }
            // `return <long-lvalue> + K;` / `... - K;` — load lvalue
            // into DX(high)/AX(low), then add/sub the constant to
            // AX (low) and propagate carry/borrow into DX. ABI
            // return convention (DX=high, AX=low) — note this is
            // the OPPOSITE register assignment from the memory-
            // destination arithmetic shape (see fixture 207, which
            // uses AX=high/DX=low when result is stored back to
            // memory). The compound is also against AX directly,
            // unlike the memory-dest path which adds to DX first.
            // Fixture 362.
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && (matches!(op, BinOp::Add) || matches!(op, BinOp::Sub))
                && let Some((src_hi, src_lo)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
            {
                let signed = k as i32;
                let (delta, carry) = if matches!(op, BinOp::Add) {
                    (signed, 0i16)
                } else {
                    (-signed, -1i16)
                };
                let _ = write!(self.out, "\tmov\tdx,word ptr {src_hi}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {src_lo}\r\n");
                let delta_u16 = (delta as i32) as u16;
                let _ = write!(self.out, "\tadd\tax,{delta_u16}\r\n");
                let _ = write!(self.out, "\tadc\tdx,{carry}\r\n");
                return;
            }
            // `return a <op> b;` for two long lvalues (params, stack
            // locals, globals, struct fields, array elems, *p — any
            // shape `long_lvalue_addr_pair` resolves) and any op in
            // `long_pair_op` (`+`/`-`/`&`/`|`/`^`). Source-storage-
            // agnostic: load a (high→DX, low→AX) per the ABI return
            // convention, then op b's halves against the same
            // registers. The lo op targets AX and the hi op targets
            // DX — flipped from the memory-dest shape (fixture 207),
            // per the destination-driven register-pair rule. For
            // arith ops the hi_op carries (`adc`/`sbb`); for bitwise
            // it's the same op on each half. Fixtures 285 (locals
            // add), 348 (globals add), 365 (struct fields add), 366
            // (array elems add), 367 (mixed global+struct add), 368
            // (`&`), 369 (`|`), 370 (`^`).
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && let Some((lo_op, hi_op)) = long_pair_op(*op)
                && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
                && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
            {
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {a_lo}\r\n");
                let _ = write!(self.out, "\t{lo_op}\tax,word ptr {b_lo}\r\n");
                let _ = write!(self.out, "\t{hi_op}\tdx,word ptr {b_hi}\r\n");
                return;
            }
            // `return -<long-lvalue>;` — long unary negation at return.
            // Load operand into DX:AX (ABI return convention), then
            // run the canonical 32-bit two's complement neg idiom
            // with DX=high: negate high first (no flag dep), negate
            // low (which sets CF iff low was nonzero), then sbb the
            // borrow back into high. Mirror of the memory-dest neg
            // idiom (fixture 226) with the register roles swapped
            // per the destination-driven rule. Fixtures 371
            // (param), 373 (global).
            if let ExprKind::Unary { op: crate::ast::UnaryOp::Neg, operand } = &e.kind
                && let Some((hi_addr, lo_addr)) = self.long_lvalue_addr_pair(operand)
            {
                let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                self.out.extend_from_slice(b"\tneg\tdx\r\n");
                self.out.extend_from_slice(b"\tneg\tax\r\n");
                self.out.extend_from_slice(b"\tsbb\tdx,0\r\n");
                return;
            }
            // `return ~<long-lvalue>;` — long bitwise complement at
            // return. Load operand into DX:AX, then flip each half
            // independently. BCC emits low-first (`not ax / not dx`)
            // — opposite of the neg case where the order is forced
            // by the flag dependency. Fixture 372.
            if let ExprKind::Unary { op: crate::ast::UnaryOp::BitNot, operand } = &e.kind
                && let Some((hi_addr, lo_addr)) = self.long_lvalue_addr_pair(operand)
            {
                let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                self.out.extend_from_slice(b"\tnot\tax\r\n");
                self.out.extend_from_slice(b"\tnot\tdx\r\n");
                return;
            }
            // `return a * b;` for two long lvalues. The mul helper
            // `N_LXMUL@` takes operands in CX:BX and DX:AX (each
            // high:low) and returns the product in DX:AX — which
            // happens to be the return register pair, so no store
            // or move is needed at the boundary. Load order is first
            // operand → CX:BX, second → DX:AX (same as memory-dest
            // `z = x * y` shape). Fixture 374.
            if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &e.kind
                && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
                && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
            {
                let _ = write!(self.out, "\tmov\tcx,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tbx,word ptr {a_lo}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {b_hi}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {b_lo}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                self.helpers.insert("N_LXMUL@".to_string());
                return;
            }
            // `return <int_global>;` (or `return (long)i;`) when the
            // function returns a long-family type — widen the int
            // into DX:AX. Signed sign-extends via `cwd` (fixture
            // 380); unsigned zero-extends via `xor dx, dx` (fixture
            // 381). Distinct from the memory-dest widening shape
            // (fixture 255: `mov [_g+2], 0`) — at return the high
            // half is a register, so BCC writes zero with the
            // shorter `xor dx, dx` (2 bytes) instead of an immediate
            // store. Destination-driven, same logical operation.
            let widening_src = match &e.kind {
                ExprKind::Ident(name) => Some(name.as_str()),
                ExprKind::Cast { ty, operand } if ty.is_long_like() => {
                    if let ExprKind::Ident(name) = &operand.kind { Some(name.as_str()) } else { None }
                }
                _ => None,
            };
            if let Some(src_name) = widening_src
                && let Some(src_ty) = self.globals.type_of(src_name)
                && matches!(src_ty, Type::Int | Type::UInt)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}\r\n");
                match src_ty {
                    Type::Int  => self.out.extend_from_slice(b"\tcwd\t\r\n"),
                    Type::UInt => self.out.extend_from_slice(b"\txor\tdx,dx\r\n"),
                    _ => unreachable!(),
                }
                return;
            }
            // Same shape, but the int source is a stack-resident local
            // or function param. `mov ax, word ptr [bp+N]` then cwd
            // (signed) / xor dx,dx (unsigned). Fixtures 2548 (signed
            // int → long), 2549 (unsigned int → long).
            if let Some(src_name) = widening_src
                && self.locals.has(src_name)
                && matches!(self.locals.type_of(src_name), Type::Int | Type::UInt)
                && let LocalLocation::Stack(off) = self.locals.location_of(src_name)
            {
                let src_ty = self.locals.type_of(src_name).clone();
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                match src_ty {
                    Type::Int  => self.out.extend_from_slice(b"\tcwd\t\r\n"),
                    Type::UInt => self.out.extend_from_slice(b"\txor\tdx,dx\r\n"),
                    _ => unreachable!(),
                }
                return;
            }
            // `(long)<char_local>` — widen byte → int → long: load AL,
            // cbw / mov ah,0, then cwd / xor dx,dx. Fixture 3183.
            if let Some(src_name) = widening_src
                && self.locals.has(src_name)
                && self.locals.type_of(src_name).is_char_like()
                && let LocalLocation::Stack(off) = self.locals.location_of(src_name)
            {
                let unsigned = self.locals.type_of(src_name).is_unsigned();
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                if unsigned {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                }
                return;
            }
            // `return *p;` where p is a global long pointer. Load p
            // into BX, then load DX:AX = *p via [bx+2]/[bx]. Fixture
            // 3286.
            if let ExprKind::Deref(operand) = &e.kind
                && let ExprKind::Ident(ptr_name) = &operand.kind
                && let Some(ptr_ty) = self.globals.type_of(ptr_name)
                && let Some(pointee) = ptr_ty.pointee()
                && pointee.is_long_like()
            {
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{ptr_name}\r\n");
                self.out.extend_from_slice(b"\tmov\tdx,word ptr [bx+2]\r\n");
                self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
                return;
            }
            // `return g();` for a long-returning callee — direct
            // passthrough: the callee's DX:AX result IS the return
            // register pair, so the function emits `call near ptr
            // _g` and goes straight to its epilogue. No moves, no
            // stores. Same passthrough shape as the helper-call
            // return (mul/div/shift); the only difference is the
            // call target. Fixture 382.
            if let ExprKind::Call { name: fname, args } = &e.kind
                && args.is_empty()
            {
                let _ = write!(self.out, "\tcall\tnear ptr _{fname}\r\n");
                return;
            }
            // `return <a> << K;` / `return <a> >> K;` for a long lvalue
            // and constant K in [1,255]. Two shapes:
            //   K=1: inline shift+rotate across DX:AX. The carry
            //        propagates between halves, so the order is forced
            //        by direction — left shifts low first (`shl ax, 1;
            //        rcl dx, 1`), right shifts high first (`sar dx, 1;
            //        rcr ax, 1`). Mirror of the memory-dest K=1 shape
            //        (fixture 227) with the AX/DX roles swapped per
            //        the destination-driven rule. Fixtures 377 (`<<1`),
            //        378 (`>>1` signed).
            //   K>1: load operand → DX:AX, `mov cl, K`, then call
            //        `N_LXLSH@` / `N_LXRSH@` / `N_LXURSH@`. The helper
            //        returns DX:AX = result, which is the return
            //        register pair — no boundary move. `mov cl, K`
            //        lands AFTER the operand load, matching the
            //        non-compound (`=`-form) shape. Fixture 379.
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && matches!(op, BinOp::Shl | BinOp::Shr)
                && let Some((hi_addr, lo_addr)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
                && k >= 1
                && k <= 255
            {
                let unsigned = self.function.ret_ty.is_unsigned();
                let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                if k == 1 {
                    if matches!(op, BinOp::Shl) {
                        self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                        self.out.extend_from_slice(b"\trcl\tdx,1\r\n");
                    } else {
                        let hi_op = if unsigned { "shr" } else { "sar" };
                        let _ = write!(self.out, "\t{hi_op}\tdx,1\r\n");
                        self.out.extend_from_slice(b"\trcr\tax,1\r\n");
                    }
                } else {
                    let k_u8 = (k & 0xFF) as u8;
                    let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                    let helper = match (op, unsigned) {
                        (BinOp::Shl, _)     => "N_LXLSH@",
                        (BinOp::Shr, false) => "N_LXRSH@",
                        (BinOp::Shr, true)  => "N_LXURSH@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                }
                return;
            }
            // `return a | K;` / `& K;` / `^ K;` for a long lvalue and a
            // constant. Load DX:AX = a, then op each half with the
            // matching K-half (high half folds to 0 when K fits in
            // 16 bits but BCC still emits `<op> dx, 0`). Add/sub
            // already have a dedicated carry-propagation path above;
            // bitwise has no carry so each half is independent.
            // Fixture 2876 (`a | 0x100L`).
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
                && let Some((hi_addr, lo_addr)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
            {
                let mnem = match op {
                    BinOp::BitAnd => "and",
                    BinOp::BitOr  => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let lo_k = (k & 0xFFFF) as u16;
                let hi_k = ((k >> 16) & 0xFFFF) as u16;
                let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                let _ = write!(self.out, "\t{mnem}\tax,{lo_k}\r\n");
                let _ = write!(self.out, "\t{mnem}\tdx,{hi_k}\r\n");
                return;
            }
            // `return a * K;` for a long lvalue × power-of-two
            // constant. K=2 → inline `shl ax,1; rcl dx,1` (fixture
            // 3170). K=2^n with n>1 → N_LXLSH@ helper (matches the
            // long-init / `<dest> = a * K_pow2` shape). Non-power-of-
            // 2 const would still need N_LXMUL@.
            if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &e.kind
                && let Some((hi_addr, lo_addr)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
                && k > 0
                && k.is_power_of_two()
                && k.trailing_zeros() <= 31
            {
                let shifts = k.trailing_zeros();
                let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                if shifts == 1 {
                    self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                    self.out.extend_from_slice(b"\trcl\tdx,1\r\n");
                } else if shifts > 0 {
                    let k_u8 = shifts as u8;
                    let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_LXLSH@\r\n");
                    self.helpers.insert("N_LXLSH@".to_string());
                }
                return;
            }
            // `return v / K;` / `% K;` where K is a constant. Same
            // helper-call shape as the lvalue-vs-lvalue path but the
            // divisor is composed in registers from K's halves and
            // pushed. `xor ax,ax` writes the high half when K fits in
            // 16 bits (BCC's preferred encoding); `mov dx, lo_k` then
            // `push ax / push dx`. Fixtures 2829 (unsigned div by 10).
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && matches!(op, BinOp::Div | BinOp::Mod)
                && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
            {
                let unsigned = self.function.ret_ty.is_unsigned();
                let helper = match (op, unsigned) {
                    (BinOp::Div, false) => "N_LDIV@",
                    (BinOp::Mod, false) => "N_LMOD@",
                    (BinOp::Div, true)  => "N_LUDIV@",
                    (BinOp::Mod, true)  => "N_LUMOD@",
                    _ => unreachable!(),
                };
                let lo_k = (k & 0xFFFF) as u16;
                let hi_k = ((k >> 16) & 0xFFFF) as u16;
                if hi_k == 0 {
                    self.out.extend_from_slice(b"\txor\tax,ax\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tax,{hi_k}\r\n");
                }
                let _ = write!(self.out, "\tmov\tdx,{lo_k}\r\n");
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                self.out.extend_from_slice(b"\tpush\tdx\r\n");
                let _ = write!(self.out, "\tpush\tword ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tpush\tword ptr {a_lo}\r\n");
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                return;
            }
            // `return a / b;` / `return a % b;` for two long lvalues.
            // The `N_LDIV@` / `N_LMOD@` helpers take 4 words on the
            // stack — dividend first (lower addresses), divisor
            // second — pushed right-to-left so the push order is:
            // divisor.high, divisor.low, dividend.high, dividend.low.
            // Result lands in DX:AX, which is the return register
            // pair. Fixtures 375 (div), 376 (mod).
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && matches!(op, BinOp::Div | BinOp::Mod)
                && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
                && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
            {
                let unsigned = self.function.ret_ty.is_unsigned();
                let helper = match (op, unsigned) {
                    (BinOp::Div, false) => "N_LDIV@",
                    (BinOp::Mod, false) => "N_LMOD@",
                    (BinOp::Div, true)  => "N_LUDIV@",
                    (BinOp::Mod, true)  => "N_LUMOD@",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\tpush\tword ptr {b_hi}\r\n");
                let _ = write!(self.out, "\tpush\tword ptr {b_lo}\r\n");
                let _ = write!(self.out, "\tpush\tword ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tpush\tword ptr {a_lo}\r\n");
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                return;
            }
            panic!("non-constant long return value not yet supported (no fixture)");
        }
        // Unsigned-char return: BCC doesn't bother widening — the
        // value lives in AL alone, the upper byte is left whatever.
        // Compare with signed-char return (fixture 156): BCC emits
        // `cbw` after the AL load to sign-extend. The two return
        // shapes differ by exactly the widening step. Fixture 466.
        if matches!(self.function.ret_ty, Type::UChar)
            && let ExprKind::Ident(name) = &e.kind
            && self.globals.type_of(name).map_or(false, |t| matches!(t, Type::UChar))
        {
            let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
            return;
        }
        // Char-returning function with a bare-ident char-typed
        // return: load the char into AL without widening. The return
        // ABI for char-returning fns is "AL holds the value, AH is
        // garbage". `cbw` is the caller's job after the call (since
        // the caller may want the widened int). Fixture 643
        // (`char f(char c) { return c; }`).
        if self.function.ret_ty.is_char_like()
            && let ExprKind::Ident(name) = &e.kind
            && self.ident_is_char(name)
        {
            match self.locals.location_of(name) {
                LocalLocation::Stack(off) => {
                    let _ = write!(
                        self.out,
                        "\tmov\tal,byte ptr {}\r\n",
                        bp_addr(off),
                    );
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                }
            }
            return;
        }
        // Char return of `<char_local> <op> <const>` for arithmetic
        // ops: stay at byte width throughout — `mov al, <a>; <op>
        // al, K`. No widening needed since the caller widens after
        // the call. Fixture 3589 (`char inc5(char a) { return a +
        // 5; }`).
        if self.function.ret_ty.is_char_like()
            && let ExprKind::BinOp { op, left, right } = &e.kind
            && matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && let ExprKind::Ident(name) = &left.kind
            && self.ident_is_char(name)
            && let Some(k) = try_const_eval(right)
        {
            let src_addr = if let Some(_gty) = self.globals.type_of(name) {
                format!("DGROUP:_{name}")
            } else if self.locals.has(name)
                && let LocalLocation::Stack(off) = self.locals.location_of(name)
            {
                bp_addr(off)
            } else {
                // Char in register (DL/etc.): mov al, <reg>
                let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
                    unreachable!()
                };
                let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                let v8 = k & 0xFF;
                let mnem = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\t{mnem}\tal,{v8}\r\n");
                return;
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {src_addr}\r\n");
            let v8 = k & 0xFF;
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tal,{v8}\r\n");
            return;
        }
        // Char return with `(<char-like>)<int-local>` cast: just
        // load the low byte of the int. The cast narrows; for a
        // char-return ABI we only need AL, no widening. Fixture 3019
        // (`(unsigned char)x` from int param).
        if self.function.ret_ty.is_char_like()
            && let ExprKind::Cast { ty: cast_ty, operand } = &e.kind
            && cast_ty.is_char_like()
            && let ExprKind::Ident(name) = &operand.kind
        {
            if self.locals.has(name)
                && let LocalLocation::Stack(off) = self.locals.location_of(name)
            {
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                return;
            }
            if self.globals.type_of(name).is_some() {
                let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
                return;
            }
        }
        // Char return with `(<char-like>)(<int-local> <op> K)`: load
        // the low byte of the int, then apply byte arith with K's low
        // byte. Mirrors the bare-cast case but covers expressions
        // like `(unsigned char)(x & 0xFF)` (fixture 2539).
        if self.function.ret_ty.is_char_like()
            && let ExprKind::Cast { ty: cast_ty, operand } = &e.kind
            && cast_ty.is_char_like()
            && let ExprKind::BinOp { op: arith_op, left, right } = &operand.kind
            && matches!(
                arith_op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let ExprKind::Ident(name) = &left.kind
            && let Some(k) = try_const_eval(right)
        {
            let src_addr = if self.locals.has(name)
                && let LocalLocation::Stack(off) = self.locals.location_of(name)
            {
                Some(bp_addr(off))
            } else if self.globals.type_of(name).is_some() {
                Some(format!("DGROUP:_{name}"))
            } else {
                None
            };
            if let Some(addr) = src_addr {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                let v8 = k & 0xFF;
                let mnem = match arith_op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\t{mnem}\tal,{v8}\r\n");
                return;
            }
        }
        // Char return with a const-indexed char array element (global
        // or stack): `mov al, byte ptr <addr>` and no widening. Same
        // ABI as the bare-ident case. Fixture 3337 (`return s[0]`
        // for global `char s[6]`).
        if self.function.ret_ty.is_char_like()
            && let Some((name, total_off, leaf_ty)) = self.try_lvalue_chain_addr(e)
            && leaf_ty.is_char_like()
        {
            if self.globals.contains(&name) {
                let addr = if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                };
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                return;
            }
            if let LocalLocation::Stack(base_off) = self.locals.location_of(&name) {
                let off = base_off + i16::try_from(total_off).unwrap_or(i16::MAX);
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                return;
            }
        }
        self.emit_expr_to_ax(e);
    }

    /// Initialize a freshly-declared local with `init`.
    fn emit_init_local(&mut self, loc: LocalLocation, ty: &Type, init: &Expr) {
        match loc {
            LocalLocation::Stack(off) => {
                // Stack array (and struct) initializer with a constant
                // image: BCC interns the flattened byte image into
                // `_DATA:s@` and emits the copy. Two shape thresholds
                // mirror the struct-return / struct-copy split:
                //   - size ≤ 4: inline AX/DX moves from `s@[+off]`
                //     into the local — the same path long-init takes
                //     when the source is memory. Fixtures 1612 (2B),
                //     1613 (4B).
                //   - size > 4: `N_SCOPY@` with far-far ptrs. Same
                //     helper used by struct returns and >4B struct
                //     copies. Fixtures 1465, 1475-1476, 1481, 1516,
                //     1616 (3-field struct, 6B), and many more.
                if matches!(ty, Type::Array { .. } | Type::Struct { .. })
                    && let Some(bytes) = flatten_init_to_bytes(ty, init)
                {
                    let size = bytes.len() as u32;
                    let pool_off = self.strings.intern_blob(&bytes);
                    if size == 2 {
                        let src = if pool_off == 0 {
                            "DGROUP:s@".to_owned()
                        } else {
                            format!("DGROUP:s@+{pool_off}")
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {src}\r\n");
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},ax\r\n",
                            bp_addr(off),
                        );
                        return;
                    }
                    if size == 4 {
                        let src_hi = if pool_off + 2 == 0 {
                            "DGROUP:s@".to_owned()
                        } else {
                            format!("DGROUP:s@+{}", pool_off + 2)
                        };
                        let src_lo = if pool_off == 0 {
                            "DGROUP:s@".to_owned()
                        } else {
                            format!("DGROUP:s@+{pool_off}")
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {src_hi}\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr {src_lo}\r\n");
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},ax\r\n",
                            bp_addr(off + 2),
                        );
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},dx\r\n",
                            bp_addr(off),
                        );
                        return;
                    }
                    let src_addr = if pool_off == 0 {
                        "offset DGROUP:s@".to_owned()
                    } else {
                        format!("offset DGROUP:s@+{pool_off}")
                    };
                    // For structs (fixture 1616) BCC computes the
                    // dst address first, then pushes ss before the
                    // addr; same for src (mov ax, src; push ds; push
                    // ax). For arrays (fixture 1475) BCC keeps the
                    // pushes outside: push ss, lea ax, push ax,
                    // push ds, mov ax, push ax.
                    let is_struct = matches!(ty, Type::Struct { .. });
                    if is_struct {
                        let _ = write!(self.out, "\tlea\tax,{}\r\n", bp_addr(off));
                        self.out.extend_from_slice(b"\tpush\tss\r\n");
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        let _ = write!(self.out, "\tmov\tax,{src_addr}\r\n");
                        self.out.extend_from_slice(b"\tpush\tds\r\n");
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tpush\tss\r\n");
                        let _ = write!(self.out, "\tlea\tax,{}\r\n", bp_addr(off));
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        self.out.extend_from_slice(b"\tpush\tds\r\n");
                        let _ = write!(self.out, "\tmov\tax,{src_addr}\r\n");
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_SCOPY@\r\n");
                    self.helpers.insert("N_SCOPY@".to_string());
                    return;
                }
                // `long x = K;` stack local — two word stores, high
                // word at the upper slot offset then low word at the
                // lower slot. Mirrors fixture 205's global-long shape.
                // Fixture 210.
                if ty.is_long_like() {
                    if let Some(v) = try_const_eval(init) {
                        let lo = v & 0xFFFF;
                        let hi = (v >> 16) & 0xFFFF;
                        // `off` points to the LOW word (lower address);
                        // the high word lives at `off + 2`.
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},{hi}\r\n",
                            bp_addr(off + 2),
                        );
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},{lo}\r\n",
                            bp_addr(off),
                        );
                        return;
                    }
                    // `long x = g;` long local from long-like global —
                    // load (AX=high, DX=low) then store high (AX → off+2)
                    // and low (DX → off). Fixture 286.
                    if let ExprKind::Ident(src_name) = &init.kind
                        && let Some(src_ty) = self.globals.type_of(src_name)
                        && src_ty.is_long_like()
                    {
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `long x = f();` long local from a function-call
                    // RHS. The call returns DX:AX (ABI: DX=high, AX=
                    // low); store DX → high (off+2), AX → low (off).
                    // Same pattern as `long g = f();` at global level
                    // (fixture 314). Fixture 315.
                    if let ExprKind::Call { .. } = &init.kind {
                        self.emit_expr_to_ax(init);
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `long x = g + K;` / `long x = g - K;` long local
                    // init from a long-global + constant. Same shape
                    // as the global-global path (slice 207) but
                    // storing into the stack local instead. Load g
                    // into AX:DX (globals convention since dest is
                    // memory), `add/sub dx, K_lo`, `adc/sbb ax,
                    // K_carry`, store. Fixture 350.
                    if let ExprKind::BinOp { op, left, right } = &init.kind
                        && matches!(op, BinOp::Add | BinOp::Sub)
                        && let ExprKind::Ident(src_name) = &left.kind
                        && let Some(src_ty) = self.globals.type_of(src_name)
                        && src_ty.is_long_like()
                        && let Some(k) = try_const_eval(right)
                    {
                        let signed = k as i32;
                        let (delta, carry) = if matches!(op, BinOp::Add) {
                            (signed, 0i16)
                        } else {
                            (-signed, -1i16)
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                        if let Ok(delta_i8) = i8::try_from(delta) {
                            let _ = write!(self.out, "\tadd\tdx,{delta_i8}\r\n");
                        } else {
                            let delta_u16 = (delta as i32) as u16;
                            let _ = write!(self.out, "\tadd\tdx,{delta_u16}\r\n");
                        }
                        let _ = write!(self.out, "\tadc\tax,{carry}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // General long arith / lvalue-copy → stack local.
                    // Handles `long x = g + h;`, `long x = s.x + 5;`,
                    // `long x = a[1] + b[2];` etc. Fixture 357.
                    let dest_hi = bp_addr(off + 2);
                    let dest_lo = bp_addr(off);
                    if self.try_emit_long_value_to_dest(init, &dest_hi, &dest_lo) {
                        return;
                    }
                    panic!("non-constant long local init not yet supported (no fixture)");
                }
                // Stack init: prefer the immediate-store form when the
                // initializer folds to a constant. For `char` we emit
                // `byte ptr` (fixture 011); for `int`, `word ptr`.
                // Negative constants like `int x = -5;` come through
                // `try_const_eval` as a wide u32; mask to the width
                // tasm expects (fixture 632).
                if let Some(v) = try_const_eval(init) {
                    let width = ptr_width(ty);
                    let v_masked = if ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                    let _ = write!(
                        self.out,
                        "\tmov\t{width} ptr {},{v_masked}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // Array-to-pointer decay init: `T *p = arr;` for
                // `arr` a global array. Store the symbol's offset
                // directly. Fixture 2541 (`p = arr` for global arr).
                if ty.pointee().is_some()
                    && let ExprKind::Ident(sym) = &init.kind
                    && let Some(gty) = self.globals.type_of(sym)
                    && gty.array_elem().is_some()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset DGROUP:_{sym}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // Address-of-global init: `int *p = &x;` for `x` a
                // non-array global. Store the symbol's offset
                // directly. Fixture 1964 (`int *p = &x`).
                if ty.pointee().is_some()
                    && let ExprKind::AddressOf(sym) = &init.kind
                    && self.globals.type_of(sym).is_some()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset DGROUP:_{sym}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // String-literal init for a pointer local: `char *s =
                // "lit";` lowers to `mov word ptr [bp-N], offset
                // DGROUP:s@+K` directly, no AX roundtrip. Fixture
                // 1931 (`char *s = "ABCD"`).
                if let ExprKind::StringLit(bytes) = &init.kind
                    && let Some(pointee) = ty.pointee()
                    && pointee.is_char_like()
                {
                    let pool_off = self.strings.intern(bytes);
                    let src = if pool_off == 0 {
                        "offset DGROUP:s@".to_owned()
                    } else {
                        format!("offset DGROUP:s@+{pool_off}")
                    };
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},{src}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // `char c = f();` where f returns char — call returns
                // the value in AL; we only need to store the low byte.
                // Skip the cbw widen the call site normally appends.
                // Fixture 2451.
                if ty.is_char_like()
                    && let ExprKind::Call { name, args } = &init.kind
                    && self.signatures.ret_ty_of(name).map_or(false, |t| t.is_char_like())
                {
                    self.emit_call(name, args);
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    return;
                }
                // Function-pointer init: `int (*p)(void) = f;` →
                // `mov word ptr [bp-N],offset _f`. We detect this by
                // the init being a bare ident that names a function
                // defined in this TU (fixture 110).
                if let ExprKind::Ident(name) = &init.kind
                    && self.signatures.params_of(name).is_some()
                {
                    let sym = function_symbol(name);
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset {sym}\r\n",
                        bp_addr(off)
                    );
                    return;
                }
                // Int local init from a non-AX register-resident
                // source: `mov word ptr [bp-N], <reg>` directly.
                // Saves the round-trip through AX (`mov ax, <reg>;
                // mov [bp-N], ax`). Fixture 1711 (`int b = x >> 0`
                // folds to `int b = x` with x in SI).
                if ty.is_int_like()
                    && let ExprKind::Ident(name) = &init.kind
                    && self.locals.has(name)
                    && self.locals.type_of(name).is_int_like()
                    && let LocalLocation::Reg(reg) = self.locals.location_of(name)
                    && !reg.is_byte()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},{}\r\n",
                        bp_addr(off),
                        reg.name(),
                    );
                    return;
                }
                // Int local init from a Mod expression: the idiv
                // leaves the remainder in DX. emit_expr_to_ax would
                // normally `mov ax, dx` to materialize the result in
                // AX before our `mov [dest], ax`. Skip the move and
                // store DX directly. Fixture 2089 (`int r = x % 7`),
                // 2088, 1723.
                //
                // Skip this peephole when the RHS is an
                // unsigned-by-pow2 strength reduction (`x % K` with
                // K=pow2 for unsigned x): that path emits `and ax,
                // K-1` and leaves the result in AX, not DX (fixtures
                // 1935, 2087).
                let mod_strength_reduced = matches!(&init.kind,
                    ExprKind::BinOp { op: BinOp::Mod, left, right }
                    if self.expr_is_unsigned(left)
                        && matches!(try_const_eval(right),
                            Some(v) if v > 0 && v.is_power_of_two())
                );
                if ty.is_int_like()
                    && let ExprKind::BinOp { op: BinOp::Mod, .. } = &init.kind
                    && !mod_strength_reduced
                {
                    // Evaluate up to the idiv/div but inhibit the
                    // final mov ax,dx by setting a one-shot flag.
                    self.skip_mod_to_ax = true;
                    self.emit_expr_to_ax(init);
                    self.skip_mod_to_ax = false;
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},dx\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // Non-constant char init. Peephole for `(char)<int-
                // local>` and the bare-ident `char b = a;` (a is
                // either char or int local): load the low byte of
                // the source slot directly with `mov al, byte ptr
                // [bp-Nsrc]`, then store with `mov byte ptr [bp-Nc],
                // al`. Fixture 1039 (`char c = (char)n;`), fixture
                // 1040 (`char b = a;`).
                if ty.is_char_like() {
                    // Unwrap an outer `(char)` cast — the byte-load
                    // sequence is the same whether the source was
                    // already char or was cast from int.
                    let src_expr = if let ExprKind::Cast { ty: cast_ty, operand } = &init.kind
                        && cast_ty.is_char_like()
                    {
                        operand.as_ref()
                    } else {
                        init
                    };
                    if let ExprKind::Ident(src_name) = &src_expr.kind
                        && self.locals.has(src_name)
                        && (self.locals.type_of(src_name).is_char_like()
                            || self.locals.type_of(src_name).is_int_like())
                        && let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                    {
                        let _ = write!(
                            self.out,
                            "\tmov\tal,byte ptr {}\r\n",
                            bp_addr(src_off)
                        );
                        let _ = write!(
                            self.out,
                            "\tmov\tbyte ptr {},al\r\n",
                            bp_addr(off)
                        );
                        return;
                    }
                    // Char binop on two char locals stored back to a
                    // char destination: BCC stays at byte width (no
                    // int promotion) because the result is truncated
                    // anyway. Pattern: `mov al, byte ptr <a>; <mnem>
                    // al, byte ptr <b>; mov byte ptr <c>, al`.
                    // Fixture 1046 (`char c = a + b;`).
                    if let ExprKind::BinOp { op, left, right } = &src_expr.kind
                        && let ExprKind::Ident(lname) = &left.kind
                        && let ExprKind::Ident(rname) = &right.kind
                        && self.locals.has(lname)
                        && self.locals.has(rname)
                        && self.locals.type_of(lname).is_char_like()
                        && self.locals.type_of(rname).is_char_like()
                        && let LocalLocation::Stack(loff) = self.locals.location_of(lname)
                        && let LocalLocation::Stack(roff) = self.locals.location_of(rname)
                    {
                        let mnem = match op {
                            BinOp::Add => Some("add"),
                            BinOp::Sub => Some("sub"),
                            BinOp::BitAnd => Some("and"),
                            BinOp::BitOr => Some("or"),
                            BinOp::BitXor => Some("xor"),
                            _ => None,
                        };
                        if let Some(mnem) = mnem {
                            let _ = write!(
                                self.out,
                                "\tmov\tal,byte ptr {}\r\n",
                                bp_addr(loff)
                            );
                            let _ = write!(
                                self.out,
                                "\t{mnem}\tal,byte ptr {}\r\n",
                                bp_addr(roff)
                            );
                            let _ = write!(
                                self.out,
                                "\tmov\tbyte ptr {},al\r\n",
                                bp_addr(off)
                            );
                            return;
                        }
                    }
                    // Char-shift-by-const init. Two distinct shapes:
                    //  - `<<` (left shift): byte arithmetic on AL
                    //    directly — `shl al, 1` repeated K times.
                    //    No widen needed because the high bits fall
                    //    off either way. Fixture 1085.
                    //  - `>>` (right shift): C promotes char → signed
                    //    int before the shift, so BCC widens with
                    //    `cbw` (signed char) or `mov ah, 0` (unsigned
                    //    char), then always `sar` regardless of the
                    //    operand's signedness because the promoted
                    //    type is signed int. Fixtures 1082, 1086,
                    //    1087.
                    if let ExprKind::BinOp { op, left, right } = &src_expr.kind
                        && matches!(op, BinOp::Shr | BinOp::Shl)
                        && let ExprKind::Ident(src_name) = &left.kind
                        && self.locals.has(src_name)
                        && self.locals.type_of(src_name).is_char_like()
                        && let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                        && let Some(k) = try_const_eval(right)
                    {
                        let unsigned = self.locals.type_of(src_name).is_unsigned();
                        let k_u = (k as u32) & 0x1F;
                        let _ = write!(
                            self.out,
                            "\tmov\tal,byte ptr {}\r\n",
                            bp_addr(src_off)
                        );
                        if matches!(op, BinOp::Shl) {
                            // Byte-level left shift on AL only.
                            if k_u <= 3 {
                                for _ in 0..k_u {
                                    self.out.extend_from_slice(b"\tshl\tal,1\r\n");
                                }
                            } else {
                                let _ = write!(self.out, "\tmov\tcl,{k_u}\r\n");
                                self.out.extend_from_slice(b"\tshl\tal,cl\r\n");
                            }
                        } else {
                            // Right shift: widen, then signed `sar`
                            // (promoted-int signedness — always
                            // signed because both `char` and `uchar`
                            // promote to `int`).
                            if !unsigned {
                                self.out.extend_from_slice(b"\tcbw\r\n");
                            } else {
                                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                            }
                            if k_u <= 3 {
                                for _ in 0..k_u {
                                    self.out.extend_from_slice(b"\tsar\tax,1\r\n");
                                }
                            } else {
                                let _ = write!(self.out, "\tmov\tcl,{k_u}\r\n");
                                self.out.extend_from_slice(b"\tsar\tax,cl\r\n");
                            }
                        }
                        let _ = write!(
                            self.out,
                            "\tmov\tbyte ptr {},al\r\n",
                            bp_addr(off)
                        );
                        return;
                    }
                    // `char b = s.c;` — char init from a `Dot`-kind
                    // Member whose leaf is char-like. Same byte-load
                    // shape as the assign-from-Member peephole
                    // (batch 266): `mov al, byte ptr <field-addr>;
                    // mov byte ptr <dest>, al`. Fixture 1124.
                    if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } =
                        &src_expr.kind
                        && let Some((src_name, total_off, leaf_ty)) =
                            self.try_member_dot_chain(base, field)
                        && leaf_ty.is_char_like()
                    {
                        if self.globals.contains(&src_name) {
                            let addr = if total_off == 0 {
                                format!("DGROUP:_{src_name}")
                            } else {
                                format!("DGROUP:_{src_name}+{total_off}")
                            };
                            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                            let _ = write!(
                                self.out,
                                "\tmov\tbyte ptr {},al\r\n",
                                bp_addr(off)
                            );
                            return;
                        }
                        if let LocalLocation::Stack(base_bp) =
                            self.locals.location_of(&src_name)
                        {
                            let src_off =
                                base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                            let _ = write!(
                                self.out,
                                "\tmov\tal,byte ptr {}\r\n",
                                bp_addr(src_off)
                            );
                            let _ = write!(
                                self.out,
                                "\tmov\tbyte ptr {},al\r\n",
                                bp_addr(off)
                            );
                            return;
                        }
                    }
                    panic!("non-constant char local init shape not yet supported");
                }
                // Pointers and ints share the int-like word-sized
                // path: compute into AX, then store as `word ptr`.
                assert!(
                    ty.is_int_like(),
                    "non-constant init for non-int-like type {:?} not yet supported",
                    ty
                );
                // Pointer init from `<stack-array> + K_const`: fold
                // the element offset into the LEA's displacement so
                // we emit `lea ax, [bp+(base+K*stride)]` directly
                // instead of `lea ax, [bp+base]; add/inc ax, K`.
                // Same shape as the register-init peephole in
                // `emit_store_reg`. Fixture 1066 (`int *p = a + 1;`
                // with p stack-resident).
                if let ExprKind::BinOp { op: BinOp::Add, left, right } = &init.kind
                    && let ExprKind::Ident(arr_name) = &left.kind
                    && self.locals.has(arr_name)
                    && let Some(elem_ty) = self.locals.type_of(arr_name).array_elem()
                    && let Some(k) = try_const_eval(right)
                    && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
                {
                    let stride = i32::from(elem_ty.size_bytes());
                    let adj_off = i32::from(base_off) + (k as i32) * stride;
                    let adj_off_i16 = i16::try_from(adj_off).expect("array+const offset fits in i16");
                    let _ = write!(
                        self.out,
                        "\tlea\tax,word ptr {}\r\n",
                        bp_addr(adj_off_i16)
                    );
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    return;
                }
                self.emit_expr_to_ax(init);
                let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
            }
            LocalLocation::Reg(reg) => self.emit_store_reg(reg, init),
        }
    }

    /// Emit `name <op>= value;`. Fixtures 067–071 show BCC routes this
    /// through a distinct codegen path that's *tighter* than the
    /// expanded `name = name <op> value` form: when the target sits
    /// in a register, the operation is performed directly on the
    /// register with `<mnemonic> <reg>, <src>` instead of going
    /// through AX. Peepholes:
    ///
    /// - `<reg> += 1` / `<reg> -= 1` → `inc <reg>` / `dec <reg>`
    /// - `<reg> += K` / `<reg> -= K` (K != 1) → `add <reg>, K` / `sub <reg>, K`
    /// - `<reg> += <src>` (src = mem or reg) → `add <reg>, <src>`
    /// - Same shapes for `&=` / `|=` / `^=` with `and` / `or` / `xor`.
    /// - `*=` doesn't have a `reg, imm` form on 8086, so it routes
    ///   through AX via DX: `mov dx, <rhs> / mov ax, <reg> / imul dx
    ///   / mov <reg>, ax`.
    ///
    /// Stack-resident targets are unobserved — every fixture so far
    /// puts the target in a register. Panic until pinned.
    fn emit_compound_assign(&mut self, name: &str, op: BinOp, value: &Expr) {
        // Long-like global `g <op>= K` with K fitting i8sx (per
        // half): memory-direct read-modify-write on each half. The
        // high-half partner depends on the op family — add/sub need
        // carry/borrow propagation (`adc/sbb high,0`), bitwise ops
        // act independently (the same mnemonic against the high
        // word of K). Distinct from `g = g <op> K` (slice 207) which
        // uses the register-load pattern. Fixtures 251 (`+=`), 252
        // (`-=`), 253 (`&=`).
        // Long-like global `g <op>= rhs` where rhs is another long
        // global (mul/div/mod) — emit the same helper-call shapes
        // as the `g = g <op> rhs` form (slices 231–233). The byte
        // output is identical between `g = g op b` and `g op= b`
        // for these ops. Fixtures 260 (`*=`), 261 (`/=`), 262 (`%=`).
        // `long g += K` / `-= K` / bitwise with constant RHS — use
        // memory-direct two-half form. Saves the AX/DX load + cwd
        // (5-7 bytes) for a 10-byte mem-direct shape. Fixture 251
        // (`long g += 5`). Add/Sub use sign-extended low + adc/sbb
        // with zero for positive K (or -1 for negative). Bitwise
        // uses each half independently.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some(k) = try_const_eval(value)
        {
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            let k_signed = k as i32;
            let lo = (k & 0xFFFF) as u16;
            let hi = ((k >> 16) & 0xFFFF) as u16;
            match op {
                BinOp::Add | BinOp::Sub => {
                    let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                        ("add", "adc")
                    } else {
                        ("sub", "sbb")
                    };
                    let _ = write!(self.out, "\t{lo_op}\tword ptr {lhs_lo},{lo}\r\n");
                    // High-half carry/borrow: 0 for non-negative K
                    // (no carry bits), -1 (0xFFFF) for negative K
                    // sign-extension. Since K is typically small,
                    // hi_k is usually 0 — the adc/sbb still has to
                    // ride the carry/borrow from the low half.
                    let hi_imm = if k_signed < 0 && hi == 0 { 0 } else { hi };
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {lhs_hi},{hi_imm}\r\n");
                }
                BinOp::BitAnd => {
                    let _ = write!(self.out, "\tand\tword ptr {lhs_lo},{lo}\r\n");
                    let _ = write!(self.out, "\tand\tword ptr {lhs_hi},{hi}\r\n");
                }
                BinOp::BitOr => {
                    let _ = write!(self.out, "\tor\tword ptr {lhs_lo},{lo}\r\n");
                    let _ = write!(self.out, "\tor\tword ptr {lhs_hi},{hi}\r\n");
                }
                BinOp::BitXor => {
                    let _ = write!(self.out, "\txor\tword ptr {lhs_lo},{lo}\r\n");
                    let _ = write!(self.out, "\txor\tword ptr {lhs_hi},{hi}\r\n");
                }
                _ => unreachable!(),
            }
            return;
        }
        // Long LHS with int RHS (widening): `long g += int x`. BCC
        // widens the int via `cwd` (signed) into DX:AX, then
        // applies memory-direct add/adc (or sub/sbb, or
        // bitwise-pair) to the LHS. Fixture 755. Also accepts
        // `Type::Char` RHS — `emit_expr_to_ax` emits the `cbw`
        // for the byte-to-int widening, and the same `cwd` then
        // extends to long. Fixture 783. RHS can be Ident,
        // ArrayIndex (fixture 827), or Member (fixture 828).
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::Char)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            let (lo_op, hi_op) = match op {
                BinOp::Add => ("add", "adc"),
                BinOp::Sub => ("sub", "sbb"),
                BinOp::BitAnd => ("and", "and"),
                BinOp::BitOr => ("or", "or"),
                BinOp::BitXor => ("xor", "xor"),
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lhs_lo},ax\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {lhs_hi},dx\r\n");
            return;
        }
        // Long LHS + unsigned-int RHS: zero-extends rather than
        // sign-extends, so BCC skips the cwd and instead uses an
        // immediate-0 operand for the high-half op. `mov ax, <x>;
        // <lo_op> word ptr <lhs_lo>, ax; <hi_op> word ptr
        // <lhs_hi>, 0`. For arith the `0` rides on the carry/
        // borrow from the low half (adc/sbb 0); for bitwise it
        // acts directly (and 0 zeros high, or/xor 0 is a no-op
        // on high). Fixture 767. Also accepts `Type::UChar` RHS
        // — `emit_expr_to_ax` emits `mov ah, 0` for the byte-to-
        // int zero-extension, and the same `<hi_op> 0` finishes
        // the long widening with no further widening register.
        // Fixture 784.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::UInt | Type::UChar)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            let (lo_op, hi_op) = match op {
                BinOp::Add => ("add", "adc"),
                BinOp::Sub => ("sub", "sbb"),
                BinOp::BitAnd => ("and", "and"),
                BinOp::BitOr => ("or", "or"),
                BinOp::BitXor => ("xor", "xor"),
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lhs_lo},ax\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {lhs_hi},0\r\n");
            return;
        }
        // Long LHS `*= int x` (signed widening). BCC can't load
        // both AX:DX (LHS) and BX:CX (widened RHS) simultaneously
        // since `cwd` clobbers DX, so it routes the widened RHS
        // through the stack: `mov ax, <x>; cwd; push ax; push dx;
        // mov dx, <lhs_hi>; mov ax, <lhs_lo>; pop cx; pop bx; call
        // N_LXMUL@; store`. The push/pop dance places RHS-high in
        // CX and RHS-low in BX — matching the helper's
        // convention. Fixture 762. Also accepts `Type::Char` —
        // `emit_expr_to_ax` emits the `cbw` byte-to-int step,
        // and the same `cwd` finishes the long-widening. Fixture
        // 785.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::Char)
            && matches!(op, BinOp::Mul)
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
            self.out.extend_from_slice(b"\tpop\tcx\r\n");
            self.out.extend_from_slice(b"\tpop\tbx\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `*= uchar c` (unsigned-byte widening). Same
        // `xor cx, cx` zero-extension as `*= uint`, but the uchar
        // is materialized in AX via `mov al; mov ah, 0` — so AX
        // is occupied. BCC inserts a `push ax; ...; pop bx`
        // shuffle to free AX for the LHS-low load while
        // preserving the widened RHS for BX:
        // `mov al, <c>; mov ah, 0; xor cx, cx; mov dx, <lhs_hi>;
        // push ax; mov ax, <lhs_lo>; pop bx; call N_LXMUL@;
        // store`. Different from the `*= uint` arm (fixture 772)
        // which loads BX directly from a 16-bit memory operand.
        // Fixture 786.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::UChar)
            && matches!(op, BinOp::Mul)
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\txor\tcx,cx\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
            self.out.extend_from_slice(b"\tpop\tbx\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `*= uint x` (unsigned widening). Zero-extension
        // means CX can be cleared with `xor cx, cx` without
        // disturbing DX, so BCC loads BX directly from the uint
        // and skips the push/pop dance the signed path needs:
        // `mov bx, <x>; xor cx, cx; mov dx, <lhs_hi>; mov ax,
        // <lhs_lo>; call N_LXMUL@; store`. Fixture 772.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::UInt)
            && matches!(op, BinOp::Mul)
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            let rhs_addr = if self.globals.contains(b) {
                format!("DGROUP:_{b}")
            } else {
                let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                    unreachable!();
                };
                bp_addr(off)
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr {rhs_addr}\r\n");
            self.out.extend_from_slice(b"\txor\tcx,cx\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `/= int x` / `%= int x` (signed widening). BCC
        // widens via `cwd`, pushes both halves of the widened RHS
        // (DX then AX, high then low), then pushes the two halves
        // of the LHS, calls the helper. Same push convention as
        // the both-globals path. Fixture 763. Also accepts
        // `Type::Char` — `emit_expr_to_ax` emits the `cbw` byte-
        // to-int step, and the same `cwd` finishes the long-
        // widening. Fixture 787.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::Char)
            && matches!(op, BinOp::Div | BinOp::Mod)
        {
            let unsigned = ty_lhs.is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `/= uchar c` / `%= uchar c` (unsigned-byte
        // widening). uchar materializes in AX via `mov ah, 0`, so
        // unlike the `/= uint` arm BCC can't use AX to source the
        // pushed `0` for the widened RHS high half — it zeroes DX
        // instead: `mov al, <c>; mov ah, 0; xor dx, dx; push dx;
        // push ax; push <lhs_hi>; push <lhs_lo>; call <helper>`.
        // Helper still picked from LHS signedness. Fixture 788.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::UChar)
            && matches!(op, BinOp::Div | BinOp::Mod)
        {
            let unsigned = ty_lhs.is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `/= uint x` / `%= uint x` (unsigned widening).
        // Zero-extension lets BCC push a literal 0 (`xor ax, ax;
        // push ax`) for the widened RHS high half, then push the
        // uint directly via `push word ptr <rhs>` without going
        // through AX. The helper consumes the same four words off
        // the stack as the signed path. Fixture 773.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::UInt)
            && matches!(op, BinOp::Div | BinOp::Mod)
        {
            let unsigned = ty_lhs.is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            let rhs_addr = if self.globals.contains(b) {
                format!("DGROUP:_{b}")
            } else {
                let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                    unreachable!();
                };
                bp_addr(off)
            };
            self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {rhs_addr}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS shift by an int/char RHS — same helper-call
        // shape as `long <<= long h`, with the shift count read as
        // `byte ptr` out of the RHS's storage. `mov cl, byte ptr
        // <addr>` works regardless of RHS width (CL only needs the
        // low byte) and regardless of RHS signedness (the shift
        // count is bounded by long width anyway). Fixture 760
        // (int/uint), fixture 789 (char/uchar).
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt | Type::Char | Type::UChar)
            && matches!(op, BinOp::Shl | BinOp::Shr)
        {
            let unsigned = ty_lhs.is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Shl, _)     => "N_LXLSH@",
                (BinOp::Shr, false) => "N_LXRSH@",
                (BinOp::Shr, true)  => "N_LXURSH@",
                _ => unreachable!(),
            };
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            let rhs_lo_byte = if self.globals.contains(b) {
                format!("byte ptr DGROUP:_{b}")
            } else {
                let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                    unreachable!();
                };
                format!("byte ptr {}", bp_addr(off))
            };
            let _ = write!(self.out, "\tmov\tcl,{rhs_lo_byte}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long compound on a long LHS (global or stack local) with
        // a long RHS (global or stack local), but not both globals
        // (which keeps the existing branch). Mul/Div/Mod use the
        // long helper; Add/Sub/Bit* use the inline memory-direct
        // shape. Fixtures 744-746 (Add/And), 747 (Mul).
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && let Some(ty_rhs) = self.rhs_long_type_of_ident(b)
            && ty_rhs.is_long_like()
            && !(self.globals.contains(name) && self.globals.contains(b))
        {
            let unsigned = ty_lhs.is_unsigned() || ty_rhs.is_unsigned();
            let (rhs_lo, rhs_hi) = self.long_halves_of(b);
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            match op {
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let _ = write!(self.out, "\tmov\tax,word ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {rhs_lo}\r\n");
                    let (lo_op, hi_op) = match op {
                        BinOp::Add => ("add", "adc"),
                        BinOp::Sub => ("sub", "sbb"),
                        BinOp::BitAnd => ("and", "and"),
                        BinOp::BitOr => ("or", "or"),
                        BinOp::BitXor => ("xor", "xor"),
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\t{lo_op}\tword ptr {lhs_lo},dx\r\n");
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {lhs_hi},ax\r\n");
                    return;
                }
                // Long `g *= h` — RHS into CX:BX, LHS into DX:AX,
                // helper call, write back. Same shape as the both-
                // globals path. Fixture 747.
                BinOp::Mul => {
                    let _ = write!(self.out, "\tmov\tcx,word ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tbx,word ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                    self.helpers.insert("N_LXMUL@".to_string());
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
                    return;
                }
                // Long `g <<= h` / `g >>= h` (mixed location): same
                // helper-call shape, CL loaded from RHS low byte
                // (which lives at `byte ptr [bp+off]` for a stack
                // RHS or `byte ptr DGROUP:_<sym>` for a global).
                // Fixture 749 (global LHS + stack RHS).
                BinOp::Shl | BinOp::Shr => {
                    let helper = match (op, unsigned) {
                        (BinOp::Shl, _)     => "N_LXLSH@",
                        (BinOp::Shr, false) => "N_LXRSH@",
                        (BinOp::Shr, true)  => "N_LXURSH@",
                        _ => unreachable!(),
                    };
                    // `rhs_lo` is already an address (sans `word
                    // ptr`); reuse for the byte form.
                    let _ = write!(self.out, "\tmov\tcl,byte ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
                    return;
                }
                // Long `g /= h` / `g %= h` — push both halves of
                // RHS then LHS, helper call, write back. Helper
                // selection matches the both-globals path.
                BinOp::Div | BinOp::Mod => {
                    let helper = match (op, unsigned) {
                        (BinOp::Div, false) => "N_LDIV@",
                        (BinOp::Mod, false) => "N_LMOD@",
                        (BinOp::Div, true)  => "N_LUDIV@",
                        (BinOp::Mod, true)  => "N_LUMOD@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tpush\tword ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {lhs_hi}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {lhs_lo}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
                    return;
                }
                _ => {}
            }
        }
        // Long-RHS variant accepting ArrayIndex (const index) and
        // Member in addition to plain Ident global. Same Mul/Div/
        // Mod/Add/Sub/Bit* shapes; only the RHS address strings
        // differ. Fixture 829 (`long_array[0]`).
        if let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
            && let Some((rhs_lo, rhs_hi, rhs_ty)) = self.long_rhs_halves(&value.kind)
            && rhs_ty.is_long_like()
            && !matches!(&value.kind, ExprKind::Ident(b) if self.globals.contains(b))
        {
            let unsigned = ty.is_unsigned() || rhs_ty.is_unsigned();
            match op {
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let _ = write!(self.out, "\tmov\tax,word ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {rhs_lo}\r\n");
                    let (lo_op, hi_op) = match op {
                        BinOp::Add => ("add", "adc"),
                        BinOp::Sub => ("sub", "sbb"),
                        BinOp::BitAnd => ("and", "and"),
                        BinOp::BitOr => ("or", "or"),
                        BinOp::BitXor => ("xor", "xor"),
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{name},dx\r\n");
                    let _ = write!(self.out, "\t{hi_op}\tword ptr DGROUP:_{name}+2,ax\r\n");
                    let _ = unsigned;
                    return;
                }
                BinOp::Mul => {
                    let _ = write!(self.out, "\tmov\tcx,word ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tbx,word ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                    self.helpers.insert("N_LXMUL@".to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                BinOp::Div | BinOp::Mod => {
                    let helper = match (op, unsigned) {
                        (BinOp::Div, false) => "N_LDIV@",
                        (BinOp::Mod, false) => "N_LMOD@",
                        (BinOp::Div, true)  => "N_LUDIV@",
                        (BinOp::Mod, true)  => "N_LUMOD@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tpush\tword ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                _ => {}
            }
        }
        if let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && self.globals.type_of(b).map_or(false, |t| t.is_long_like())
        {
            let unsigned = ty.is_unsigned()
                || self.globals.type_of(b).map_or(false, |t| t.is_unsigned());
            match op {
                BinOp::Mul => {
                    let _ = write!(self.out, "\tmov\tcx,word ptr DGROUP:_{b}+2\r\n");
                    let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{b}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                    self.helpers.insert("N_LXMUL@".to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                BinOp::Div | BinOp::Mod => {
                    let helper = match (op, unsigned) {
                        (BinOp::Div, false) => "N_LDIV@",
                        (BinOp::Mod, false) => "N_LMOD@",
                        (BinOp::Div, true)  => "N_LUDIV@",
                        (BinOp::Mod, true)  => "N_LUMOD@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{b}+2\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{b}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                // Long `g <<= h` / `g >>= h` (both globals). Same
                // helper-call shape as the K-constant K>1 path
                // (slices 263/264), but the shift count comes from
                // h's low byte: `mov cl, byte ptr DGROUP:_h`.
                // Fixture 739 (`g <<= h`).
                BinOp::Shl | BinOp::Shr => {
                    let helper = match (op, unsigned) {
                        (BinOp::Shl, _)     => "N_LXLSH@",
                        (BinOp::Shr, false) => "N_LXRSH@",
                        (BinOp::Shr, true)  => "N_LXURSH@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tmov\tcl,byte ptr DGROUP:_{b}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                // Long `g += h` / `g -= h` / `g &= h` / `g |= h` /
                // `g ^= h` (both globals). BCC loads h's two halves
                // into AX:DX (AX=high, DX=low) — the same convention
                // used for long-to-int truncation reads — then
                // applies the op memory-direct to g, with carry/borrow
                // propagation via `adc/sbb` for arith. Fixture 734
                // (`+=`), 735 (`-=`), 736 (`&=`).
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{b}+2\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{b}\r\n");
                    let (lo_op, hi_op) = match op {
                        BinOp::Add => ("add", "adc"),
                        BinOp::Sub => ("sub", "sbb"),
                        BinOp::BitAnd => ("and", "and"),
                        BinOp::BitOr => ("or", "or"),
                        BinOp::BitXor => ("xor", "xor"),
                        _ => unreachable!(),
                    };
                    let _ = write!(
                        self.out,
                        "\t{lo_op}\tword ptr DGROUP:_{name},dx\r\n",
                    );
                    let _ = write!(
                        self.out,
                        "\t{hi_op}\tword ptr DGROUP:_{name}+2,ax\r\n",
                    );
                    return;
                }
                _ => {}
            }
        }
        // Long-like global compound shifts. Two shapes:
        //   K=1: inlined as `shl/sar/shr` + `rcl/rcr` (same as the
        //        `=` form, slices 227/229/243). Fixtures 265, 266.
        //   K>1: helper call, but with `mov cl, K` emitted BEFORE
        //        the operand loads — distinct from the `=` form
        //        (slices 228/230) where mov cl lands after the
        //        operands. Fixtures 263, 264.
        if let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && k >= 1
            && k <= 255
        {
            let unsigned = ty.is_unsigned();
            if k == 1 {
                let hi_op = match (op, unsigned) {
                    (BinOp::Shl, _)     => "shl",
                    (BinOp::Shr, false) => "sar",
                    (BinOp::Shr, true)  => "shr",
                    _ => unreachable!(),
                };
                let lo_op = if matches!(op, BinOp::Shl) { "rcl" } else { "rcr" };
                // Convention: AX=high, DX=low (the `=` form's
                // pattern). For `<<` the low-half op runs first
                // (shl dx), then rotate carries into high (rcl ax).
                // For `>>` the high runs first (sar ax), then
                // rotate down into low (rcr dx).
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}\r\n");
                if matches!(op, BinOp::Shl) {
                    let _ = write!(self.out, "\tshl\tdx,1\r\n");
                    let _ = write!(self.out, "\trcl\tax,1\r\n");
                } else {
                    let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                    let _ = write!(self.out, "\t{lo_op}\tdx,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // K > 1: helper, with `mov cl, K` FIRST (compound-form
            // reorder).
            let helper = match (op, unsigned) {
                (BinOp::Shl, _)     => "N_LXLSH@",
                (BinOp::Shr, false) => "N_LXRSH@",
                (BinOp::Shr, true)  => "N_LXURSH@",
                _ => unreachable!(),
            };
            let k_u8 = k as u8;
            let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}+2\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            return;
        }
        if let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
            && let Some(k) = try_const_eval(value)
        {
            let k_lo = (k & 0xFFFF) as i32;
            let k_hi = (k >> 16) as i32;
            // Arithmetic uses `83 /n` (imm8sx) so each half must fit
            // i8sx; bitwise uses `81 /n` (imm16) which fits anything
            // in 16 bits — no further restriction. Either way, k_hi
            // for arith is always 0 (the partner is `adc/sbb 0`).
            match op {
                BinOp::Add | BinOp::Sub => {
                    let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                        ("add", "adc")
                    } else {
                        ("sub", "sbb")
                    };
                    // imm8sx-fits: emit compact `83 06 ... ii` (5 bytes)
                    // — slice 251. Otherwise: wider `81 06 ... lo hi`
                    // (6 bytes) — fixture 276. The high partner is
                    // always `adc/sbb 0` (carry comes from low).
                    if let Ok(lo_i8) = i8::try_from(k_lo) {
                        let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{name},{lo_i8}\r\n");
                    } else {
                        let lo_u16 = k_lo as u16;
                        let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{name},{lo_u16}\r\n");
                    }
                    let _ = write!(self.out, "\t{hi_op}\tword ptr DGROUP:_{name}+2,0\r\n");
                    return;
                }
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let mnem = match op {
                        BinOp::BitAnd => "and",
                        BinOp::BitOr => "or",
                        BinOp::BitXor => "xor",
                        _ => unreachable!(),
                    };
                    let lo = (k_lo as i64) & 0xFFFF;
                    let hi = (k_hi as i64) & 0xFFFF;
                    let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{name},{lo}\r\n");
                    let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{name}+2,{hi}\r\n");
                    return;
                }
                _ => {}
            }
        }
        // Long-like stack local compound assigns — memory-direct,
        // same byte-width selection as the global path: arithmetic
        // uses `83` (imm8sx, 4 bytes per half on stack), bitwise uses
        // `81` (imm16, 5 bytes per half). Fixtures 288 (`+=`), 289
        // (`&=`).
        if self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && let Some(k) = try_const_eval(value)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long is never register-resident");
            };
            let k_lo = (k & 0xFFFF) as i32;
            let k_hi = (k >> 16) as i32;
            match op {
                BinOp::Add | BinOp::Sub => {
                    let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                        ("add", "adc")
                    } else {
                        ("sub", "sbb")
                    };
                    if let Ok(lo_i8) = i8::try_from(k_lo) {
                        let _ = write!(self.out, "\t{lo_op}\tword ptr {},{lo_i8}\r\n", bp_addr(off));
                    } else {
                        let lo_u16 = k_lo as u16;
                        let _ = write!(self.out, "\t{lo_op}\tword ptr {},{lo_u16}\r\n", bp_addr(off));
                    }
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {},0\r\n", bp_addr(off + 2));
                    return;
                }
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let mnem = match op {
                        BinOp::BitAnd => "and",
                        BinOp::BitOr => "or",
                        BinOp::BitXor => "xor",
                        _ => unreachable!(),
                    };
                    let lo = (k_lo as i64) & 0xFFFF;
                    let hi = (k_hi as i64) & 0xFFFF;
                    let _ = write!(self.out, "\t{mnem}\tword ptr {},{lo}\r\n", bp_addr(off));
                    let _ = write!(self.out, "\t{mnem}\tword ptr {},{hi}\r\n", bp_addr(off + 2));
                    return;
                }
                BinOp::Shl | BinOp::Shr if k >= 1 && k <= 255 => {
                    // Long stack-local compound shift. Two shapes
                    // by K — mirrors the long-global compound shift
                    // path (fixtures 263–266) but stores back to
                    // `[bp+N]` instead of `DGROUP:_g+N`. K=1 inlines
                    // shift+rotate against AX:DX (memory-dest
                    // convention: AX=high, DX=low). K>1 routes
                    // through the helper, which forces the helper
                    // convention (DX=high, AX=low) for the load —
                    // BCC's register-pair choice tracks the
                    // intermediate operation, not the final memory
                    // store. The `mov cl, K` lands FIRST (compound-
                    // form reorder). Fixtures 383 (K=1 `<<`),
                    // 384 (K=1 `>>` signed), 385 (K>1 `<<`).
                    let unsigned = self.locals.type_of(name).is_unsigned();
                    if k == 1 {
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off));
                        if matches!(op, BinOp::Shl) {
                            self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                            self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                        } else {
                            let hi_op = if unsigned { "shr" } else { "sar" };
                            let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                            self.out.extend_from_slice(b"\trcr\tdx,1\r\n");
                        }
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                    } else {
                        let helper = match (op, unsigned) {
                            (BinOp::Shl, _)     => "N_LXLSH@",
                            (BinOp::Shr, false) => "N_LXRSH@",
                            (BinOp::Shr, true)  => "N_LXURSH@",
                            _ => unreachable!(),
                        };
                        let k_u8 = (k & 0xFF) as u8;
                        let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                        let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                        self.helpers.insert(helper.to_string());
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    }
                    return;
                }
                _ => {}
            }
        }
        // Long stack-local compound `+=` / `-=` / `&=` / `|=` / `^=`
        // with a long stack-local RHS (non-constant). Load y into
        // AX:DX (AX=high, DX=low — globals convention since dest is
        // memory), then memory-direct store with `<op> [mem], reg`.
        // Arith uses carry/borrow propagation, bitwise repeats the
        // same mnemonic. Fixtures 339, 340, 342, 343, 344.
        if self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && let Some((lo_op, hi_op)) = long_pair_op(op)
            && let ExprKind::Ident(rhs_name) = &value.kind
            && self.locals.has(rhs_name)
            && self.locals.type_of(rhs_name).is_long_like()
        {
            let (LocalLocation::Stack(x_off), LocalLocation::Stack(y_off)) =
                (self.locals.location_of(name), self.locals.location_of(rhs_name))
            else {
                unreachable!("long is never register-resident");
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(y_off + 2));
            let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(y_off));
            let _ = write!(self.out, "\t{lo_op}\tword ptr {},dx\r\n", bp_addr(x_off));
            let _ = write!(self.out, "\t{hi_op}\tword ptr {},ax\r\n", bp_addr(x_off + 2));
            return;
        }
        // Long stack-local compound `*=` with a long stack-local RHS.
        // Helper convention swaps from the `z = x * y` shape: here
        // the destination is `x`, so x goes to DX:AX (where the
        // helper deposits the result) and y goes to CX:BX. Fixture
        // 345.
        if self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && matches!(op, BinOp::Mul)
            && let ExprKind::Ident(rhs_name) = &value.kind
            && self.locals.has(rhs_name)
            && self.locals.type_of(rhs_name).is_long_like()
        {
            let (LocalLocation::Stack(x_off), LocalLocation::Stack(y_off)) =
                (self.locals.location_of(name), self.locals.location_of(rhs_name))
            else {
                unreachable!("long is never register-resident");
            };
            let _ = write!(self.out, "\tmov\tcx,word ptr {}\r\n", bp_addr(y_off + 2));
            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(y_off));
            let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(x_off + 2));
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(x_off));
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(x_off + 2));
            let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(x_off));
            return;
        }
        // Long stack-local compound `/=` / `%=` with a long stack-
        // local RHS. Same push convention as the `z = x / y` shape
        // (fixtures 337/338) but result lands back in x. Fixtures
        // 346, 347.
        if self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let ExprKind::Ident(rhs_name) = &value.kind
            && self.locals.has(rhs_name)
            && self.locals.type_of(rhs_name).is_long_like()
        {
            let (LocalLocation::Stack(x_off), LocalLocation::Stack(y_off)) =
                (self.locals.location_of(name), self.locals.location_of(rhs_name))
            else {
                unreachable!("long is never register-resident");
            };
            let unsigned = self.locals.type_of(name).is_unsigned()
                || self.locals.type_of(rhs_name).is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(y_off + 2));
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(y_off));
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(x_off + 2));
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(x_off));
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(x_off + 2));
            let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(x_off));
            return;
        }
        // Int/uint global compound shift with constant RHS — unroll
        // into K `<shl|sar|shr> word ptr DGROUP:_g, 1` instructions
        // directly on memory. Fixture 539 (`g >>= 2` for int global
        // → two `sar word ptr [_g], 1`). Same unrolling principle as
        // the char-register path (fixture 535).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && k >= 1
            && k <= 8
        {
            let signed = !gty.is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            for _ in 0..k {
                let _ = write!(
                    self.out,
                    "\t{mnem}\tword ptr DGROUP:_{name},1\r\n",
                );
            }
            return;
        }
        // Int/uint global compound add/sub with another global as
        // RHS — `mov ax, [_b]; <add|sub> word ptr DGROUP:_a, ax`.
        // Fixture 571 (`a += b;`). The store-back uses Grp1 r/m16,
        // r16 (`01 06` or `29 06`) — no IR change needed, the asm
        // syntax `add word ptr DGROUP:_a, ax` is already routed.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Add | BinOp::Sub)
            && let ExprKind::Ident(rhs_name) = &value.kind
            && let Some(rhs_ty) = self.globals.type_of(rhs_name)
            && matches!(rhs_ty, Type::Int | Type::UInt)
        {
            let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{rhs_name}\r\n");
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr DGROUP:_{name},ax\r\n",
            );
            return;
        }
        // Int/uint global compound add/sub with constant RHS —
        // memory-direct `add|sub word ptr DGROUP:_g, K`. Fixture
        // 519 (`g += 5`). TASM picks the imm8sx form when K fits a
        // signed byte; the asm syntax doesn't differ.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Add | BinOp::Sub)
            && let Some(v) = try_const_eval(value)
        {
            // K=1 peephole: `inc/dec word ptr [_g]` (4 bytes) instead
            // of `add/sub word ptr [_g], 1` (5 bytes). Fixture 3497.
            let v_masked = v & 0xFFFF;
            if v_masked == 1 {
                let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{name}\r\n");
                return;
            }
            let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr DGROUP:_{name},{v_masked}\r\n",
            );
            return;
        }
        // Int/uint global compound bitwise op with constant RHS —
        // memory-direct `<op> word ptr DGROUP:_g, K`. Fixture 517
        // (`g &= 15`). BCC always emits the imm16 form here; the
        // imm8sx peephole is not used for bitwise ops.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && let Some(v) = try_const_eval(value)
        {
            let mnem = match op {
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let v16 = v & 0xFFFF;
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr DGROUP:_{name},{v16}\r\n",
            );
            return;
        }
        // Int/uint global compound add/sub/bit* with a non-const
        // RHS (int/uint, or char/uchar that widens through AX).
        // RHS can be a local, another global, or an array element
        // — `emit_expr_to_ax` handles all of them and emits
        // `cbw` / `mov ah, 0` for the byte-to-int widening. The
        // same memory-direct `<op> word ptr DGROUP:_<name>, ax`
        // finishes the int compound. Fixtures 794 (`g += char c`),
        // 799 (int local RHS), 812 (char global RHS), 821
        // (`g += a[1]` int array element).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt | Type::Char | Type::UChar)
        {
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr DGROUP:_{name},ax\r\n",
            );
            return;
        }
        // Int/uint global compound `*=` with an int/uint local
        // RHS. No widening needed, so BCC uses `imul word ptr
        // [bp+N]` directly (the F7 6E reg=5 form): `mov ax, _g;
        // imul word ptr <rhs>; mov _g, ax`. Fixture 802.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul)
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                unreachable!();
            };
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\timul\tword ptr {}\r\n", bp_addr(off));
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            return;
        }
        // Int/uint global compound `*=` / `/=` / `%=` with a
        // constant RHS. For `*=`, BCC materializes the constant in
        // DX and uses `imul dx`. For `/=` and `%=`, the divisor goes
        // into BX (DX would be clobbered by cwd/xor). Fixtures 3494
        // (`g *= 3`), 3495 (`g /= 4`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let Some(k) = try_const_eval(value)
        {
            let k16 = k & 0xFFFF;
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\tmov\tdx,{k16}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                self.out.extend_from_slice(b"\timul\tdx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tbx,{k16}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                let (widen, mnem) = if gty.is_unsigned() {
                    (&b"\txor\tdx,dx\r\n"[..], "div")
                } else {
                    (&b"\tcwd\t\r\n"[..], "idiv")
                };
                self.out.extend_from_slice(widen);
                let _ = write!(self.out, "\t{mnem}\tbx\r\n");
                let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
                );
            }
            return;
        }
        // Int/uint global compound `*=` / `/=` / `%=` with `*p`
        // where `p` is a register-resident pointer (typically SI
        // for int*). `imul`/`idiv word ptr [si]` uses the deref-
        // through-register addressing form. Fixture 825.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let ExprKind::Deref(inner) = &value.kind
            && let ExprKind::Ident(p_name) = &inner.kind
            && !self.globals.contains(p_name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
            && !reg.is_byte()
            && let Some(pty) = self.locals.type_of(p_name).pointee()
            && matches!(pty, Type::Int | Type::UInt)
        {
            let reg_name = reg.name();
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr [{reg_name}]\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tidiv\tword ptr [{reg_name}]\r\n");
                let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
                );
            }
            return;
        }
        // Int/uint global compound `*=` / `/=` / `%=` with another
        // int/uint value in a DGROUP slot — `imul`/`idiv word ptr
        // <group>:<sym>[+offset]`. Same shape as the local-RHS
        // path, just with a DGROUP operand. Accepts plain
        // identifiers (fixture 809, 810), constant array indices
        // (`a[K]` — fixture 824), and struct members (`s.x` —
        // fixture 826's sibling).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let Some((rhs_addr, rhs_ty)) = self.global_int_rhs_addr(&value.kind)
            && matches!(rhs_ty, Type::Int | Type::UInt)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr {rhs_addr}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            } else {
                // Unsigned LHS: `xor dx, dx; div` instead of `cwd;
                // idiv`. Fixture 949.
                let (widen, mnem) = if gty.is_unsigned() {
                    (&b"\txor\tdx,dx\r\n"[..], "div")
                } else {
                    (&b"\tcwd\t\r\n"[..], "idiv")
                };
                self.out.extend_from_slice(widen);
                let _ = write!(self.out, "\t{mnem}\tword ptr {rhs_addr}\r\n");
                let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
                );
            }
            return;
        }
        // Int/uint global compound `<<=` / `>>=` with int/uint/char/
        // uchar RHS in any memory slot (local, global, array elem,
        // struct member). CL is loaded from the low byte of the
        // shift count; the shift acts memory-direct on the global.
        // Fixture 805 (local), 811 (global), 826 (member).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt | Type::Char | Type::UChar)
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let unsigned = gty.is_unsigned();
            let mnem = match (op, unsigned) {
                (BinOp::Shl, _) => "shl",
                (BinOp::Shr, false) => "sar",
                (BinOp::Shr, true) => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tcl,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{name},cl\r\n");
            return;
        }
        // Int/uint global compound `/=` / `%=` with an int/uint
        // local RHS. Same mem-direct shape as Mul, but with
        // `cwd` for the dividend sign-extension and `idiv word
        // ptr [bp+N]`: `mov ax, _g; cwd; idiv word ptr <rhs>;
        // mov _g, {ax|dx}`. Fixture 803.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                unreachable!();
            };
            // Unsigned LHS: `xor dx, dx; div` instead of `cwd; idiv`.
            // Fixture 949.
            let (widen, mnem) = if gty.is_unsigned() {
                (&b"\txor\tdx,dx\r\n"[..], "div")
            } else {
                (&b"\tcwd\t\r\n"[..], "idiv")
            };
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            self.out.extend_from_slice(widen);
            let _ = write!(self.out, "\t{mnem}\tword ptr {}\r\n", bp_addr(off));
            let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
            );
            return;
        }
        // Int/uint global compound `*=` with a char/uchar RHS
        // (local or global). `emit_expr_to_ax` materializes the
        // widened byte in AX, but AX is needed for the LHS load
        // (which feeds `imul`). BCC inserts a `push ax; ...; pop
        // dx` shuffle to park the widened RHS in DX while AX
        // takes the LHS. `imul dx` then computes DX:AX = AX * DX
        // (signed); the low-16 store back ignores DX. Note BCC
        // uses signed `imul` even for `uchar` — the zero-extended
        // dividend is positive so the low-16 product is
        // identical. Fixture 796 (local), 815 (global).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul)
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Char | Type::UChar)
            && let ExprKind::Ident(_) = &value.kind
        {
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            self.out.extend_from_slice(b"\tpop\tdx\r\n");
            self.out.extend_from_slice(b"\timul\tdx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            return;
        }
        // Int/uint global compound `/=` / `%=` with a char/uchar
        // RHS (local or global). Similar register-pressure dance
        // as the Mul arm, but BCC parks the widened RHS in BX
        // (Div uses BX by convention; Mul used DX). The LHS load
        // now needs both AX (dividend low) and DX (sign-extend
        // via cwd), so the push/pop must stash AX before the cwd:
        // `mov al, <c>; cbw; push ax; mov ax, <lhs>; cwd; pop
        // bx; idiv bx; mov <lhs>, ax` (or `, dx` for `%=`).
        // Signed `idiv` works for `uchar` RHS too — the
        // zero-extended divisor is positive. Fixture 798 (local),
        // 816 (global).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Char | Type::UChar)
            && let ExprKind::Ident(_) = &value.kind
        {
            let _ = ty_rhs;
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tpop\tbx\r\n");
            self.out.extend_from_slice(b"\tidiv\tbx\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
            );
            return;
        }
        // Char/uchar global compound `*= K` — two shapes:
        //  - K is power of two: unroll `shl al, 1` log2(K) times
        //    around an AL load-modify-store. Fixture 690.
        //  - otherwise: widen via `cbw` then 16-bit signed multiply
        //    through DX (`mov dx, K; imul dx`). Note BCC picks DX
        //    as the multiplier register here while `/=` uses BX —
        //    presumably because `imul dx` doesn't touch a register
        //    `div bx` wouldn't already need free. Fixture 693
        //    (`g *= 3` → `mov al, _g; cbw; mov dx, 3; imul dx;
        //    mov _g, al`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Mul)
            && let Some(k) = try_const_eval(value)
        {
            let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
            if k > 0 && (k & (k - 1)) == 0 && k <= 256 {
                let shifts = k.trailing_zeros();
                if shifts <= 3 {
                    for _ in 0..shifts {
                        self.out.extend_from_slice(b"\tshl\tal,1\r\n");
                    }
                } else {
                    let _ = write!(self.out, "\tmov\tcl,{shifts}\r\n");
                    self.out.extend_from_slice(b"\tshl\tal,cl\r\n");
                }
            } else {
                let v16 = k & 0xFFFF;
                self.out.extend_from_slice(b"\tcbw\t\r\n");
                let _ = write!(self.out, "\tmov\tdx,{v16}\r\n");
                self.out.extend_from_slice(b"\timul\tdx\r\n");
            }
            let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
            return;
        }
        // Char/uchar global compound `/=` / `%=` with constant K.
        // BCC widens the global to AX, loads K into BX,
        // sign-extends DX:AX with cwd, then `idiv bx`. For `/=`
        // stores AL (quotient) back; for `%=` stores DL (low byte
        // of remainder). Fixture 691 (signed `g /= 4`) and
        // fixture 694 (unsigned `g /= 4`).
        //
        // Signed widening uses `cbw`; unsigned uses `mov ah, 0`.
        // Interestingly BCC keeps the `cwd; idiv bx` (signed
        // divide) sequence even for `unsigned char` — the
        // zero-extended dividend fits in [0, 255] which is well
        // within the positive `idiv` range, so signed division
        // gives the right answer.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some(v) = try_const_eval(value)
        {
            let v16 = v & 0xFFFF;
            let unsigned = gty.is_unsigned();
            let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            }
            let _ = write!(self.out, "\tmov\tbx,{v16}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tidiv\tbx\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "al" } else { "dl" };
            let _ = write!(
                self.out,
                "\tmov\tbyte ptr DGROUP:_{name},{result_reg}\r\n",
            );
            return;
        }
        // Char/uchar global compound `<<=` / `>>=` with constant K.
        // BCC unrolls into K memory-direct shift-by-1 instructions
        // (one per shift): `shl|sar|shr byte ptr _g, 1`. Signedness
        // picks SAR vs SHR for `>>=` (signed char → SAR). Fixture
        // 688 (`g <<= 2` → two `shl byte ptr _g, 1`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && k >= 1
            && k <= 8
        {
            let unsigned = gty.is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if unsigned => "shr",
                BinOp::Shr => "sar",
                _ => unreachable!(),
            };
            for _ in 0..k {
                let _ = write!(
                    self.out,
                    "\t{mnem}\tbyte ptr DGROUP:_{name},1\r\n",
                );
            }
            return;
        }
        // Char/uchar global compound with constant byte RHS, two
        // shapes:
        //  - Arith (`+=` / `-=`): load-modify-store through AL —
        //    `mov al, _g; <add|sub> al, K; mov _g, al`. BCC
        //    canonicalizes `c -= K` as `add al, (256 - K)` (matches
        //    the broader add-neg-over-sub-const pattern). Fixtures
        //    683 / 684.
        //  - Bitwise (`&=` / `|=` / `^=`): memory-direct
        //    `<op> byte ptr _g, K` (one instruction). Fixture 685.
        //    Asymmetry vs the int-global path (which uses
        //    memory-direct for arith too) is empirical; BCC seems to
        //    pick mem-direct for bitwise but always load-modify-
        //    store for byte arith.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some(v) = try_const_eval(value)
        {
            let v8 = (v & 0xFF) as u8;
            if matches!(op, BinOp::Add | BinOp::Sub) {
                let imm = if matches!(op, BinOp::Add) { v8 } else { v8.wrapping_neg() };
                let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
                // K=±1: BCC uses inc/dec al (2 bytes) instead of
                // add al, K (2 bytes too, but BCC's preference).
                // Fixture 2891 (`char g; g += 1;` → `mov al, [g];
                // inc al; mov [g], al`).
                if imm == 1 {
                    self.out.extend_from_slice(b"\tinc\tal\r\n");
                } else if imm == 0xFF {
                    self.out.extend_from_slice(b"\tdec\tal\r\n");
                } else {
                    let _ = write!(self.out, "\tadd\tal,{imm}\r\n");
                }
                let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
            } else {
                let mnem = match op {
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let _ = write!(
                    self.out,
                    "\t{mnem}\tbyte ptr DGROUP:_{name},{v8}\r\n",
                );
            }
            return;
        }
        // Char/uchar global compound `*=` with variable byte RHS.
        // BCC widens through AL only (no sign-extension needed for
        // 8-bit multiply), then 8-bit `imul byte ptr <src>` and
        // store the low byte AL back. Fixture 695 (`g *= d` →
        // `mov al, _g; imul byte ptr [bp-1]; mov _g, al`). 8-bit
        // multiply doesn't differentiate signed/unsigned at the
        // low-byte level, so BCC picks `imul` for both.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Mul)
            && try_const_eval(value).is_none()
        {
            let src = self.resolve_operand_source(value);
            let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
            self.out.extend_from_slice(b"\timul\t");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
            return;
        }
        // Char/uchar global compound `/=` / `%=` with variable byte
        // RHS. Same 8-bit divide pattern as the local form
        // (fixtures 673 / 677): signed uses `cbw; idiv byte ptr
        // <src>`, unsigned uses `mov ah, 0; div al, byte ptr
        // <src>` with explicit AL accumulator in the TASM listing.
        // Store quotient (AL) for `/=`, remainder (AH) for `%=`.
        // Fixture 696 (signed `g /= d`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Div | BinOp::Mod)
            && try_const_eval(value).is_none()
        {
            let unsigned = gty.is_unsigned();
            let src = self.resolve_operand_source(value);
            let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                self.out.extend_from_slice(b"\tdiv\tal,");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
                self.out.extend_from_slice(b"\tidiv\t");
            }
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "al" } else { "ah" };
            let _ = write!(
                self.out,
                "\tmov\tbyte ptr DGROUP:_{name},{result_reg}\r\n",
            );
            return;
        }
        // Char/uchar global compound `<<=` / `>>=` with variable
        // byte RHS. BCC loads the shift count into CL then issues a
        // memory-direct `<shl|sar|shr> byte ptr _g, cl` — no AL
        // detour (the global stays in memory across the op).
        // Fixture 697 (`g <<= d` → `mov cl, byte ptr [bp-1]; shl
        // byte ptr _g, cl`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && try_const_eval(value).is_none()
        {
            let unsigned = gty.is_unsigned();
            let src = self.resolve_operand_source(value);
            self.out.extend_from_slice(b"\tmov\tcl,");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if unsigned => "shr",
                BinOp::Shr => "sar",
                _ => unreachable!(),
            };
            let _ = write!(
                self.out,
                "\t{mnem}\tbyte ptr DGROUP:_{name},cl\r\n",
            );
            return;
        }
        // Char/uchar global compound `+=` / `-=` / `&=` / `|=` /
        // `^=` with a non-constant byte RHS. BCC loads the RHS into
        // AL and then applies the op memory-direct to the global:
        // `mov al, byte ptr <src>; <op> byte ptr DGROUP:_<g>, al`
        // (fixtures 680/681/682).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && try_const_eval(value).is_none()
        {
            let src = self.resolve_operand_source(value);
            self.out.extend_from_slice(b"\tmov\tal,");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(
                self.out,
                "\t{mnem}\tbyte ptr DGROUP:_{name},al\r\n",
            );
            return;
        }
        let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
            panic!(
                "compound assignment on stack-resident `{name}` not yet supported (no fixture)"
            );
        };
        // Char compound on a byte-register local.
        //
        // BCC splits two ways:
        //  - `+=` / `-=`: round-trip through AL so the 2-byte AL
        //    accumulator forms (`04/2C ii`) can be used. With the
        //    AL ±1 peephole (`fe c0/c8`) the total is still 6 bytes.
        //  - `&=` / `|=` / `^=`: direct `<and|or|xor> <reg>, K`
        //    (`80 /4|/1|/6 reg ii`, 3 bytes). Fixture 556 (`c &= 31`
        //    on DL) shows the direct form is preferred for bitwise.
        if reg.is_byte()
            && matches!(op, BinOp::Add | BinOp::Sub)
            && let Some(v) = try_const_eval(value)
        {
            let v8 = v & 0xFF;
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            if v8 == 1 {
                let inc_mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                let _ = write!(self.out, "\t{inc_mnem}\tal\r\n");
            } else if matches!(op, BinOp::Sub) {
                // BCC canonicalizes `c -= K` (char, K != 1) as `add
                // al, -K` rather than `sub al, K` (same length, same
                // result mod 256). Fixture 623 (`c -= 3` → `04 FD`).
                let neg = (0u32.wrapping_sub(v8 as u32)) & 0xFF;
                let neg_i8 = neg as i8;
                let _ = write!(self.out, "\tadd\tal,{neg_i8}\r\n");
            } else {
                let _ = write!(self.out, "\tadd\tal,{v8}\r\n");
            }
            let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            return;
        }
        if reg.is_byte()
            && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && let Some(v) = try_const_eval(value)
        {
            let v8 = v & 0xFF;
            let mnem = match op {
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\t{},{v8}\r\n", reg.name());
            return;
        }
        // Char compound shift on a byte-register local: unroll into K
        // `<shl|sar|shr> <reg>, 1` instructions directly on the
        // register — no AL round-trip. Fixture 535 (`char c <<= 2`
        // → two `shl dl, 1`). The 8086 has no `r/m8, imm8` shift, so
        // BCC always unrolls for small K and switches to a CL-loop
        // for larger K (threshold not yet pinned).
        if reg.is_byte()
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && k >= 1
            && k <= 8
        {
            let signed = !self.locals.type_of(name).is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            for _ in 0..k {
                let _ = write!(self.out, "\t{mnem}\t{},1\r\n", reg.name());
            }
            return;
        }
        // Char compound `*= K` where K is a small power of two —
        // round-trip through AL and unroll `shl al, 1`. Fixture 633
        // (`c *= 4` → `mov al, dl; shl al, 1; shl al, 1; mov dl, al`).
        if reg.is_byte()
            && matches!(op, BinOp::Mul)
            && let Some(k) = try_const_eval(value)
            && k > 0
            && (k & (k - 1)) == 0
            && k <= 256
        {
            let shifts = k.trailing_zeros();
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            if shifts <= 3 {
                for _ in 0..shifts {
                    self.out.extend_from_slice(b"\tshl\tal,1\r\n");
                }
            } else {
                let _ = write!(self.out, "\tmov\tcl,{shifts}\r\n");
                self.out.extend_from_slice(b"\tshl\tal,cl\r\n");
            }
            let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            return;
        }
        // Char compound `/= K` / `%= K` — widen char to AX (cbw),
        // load divisor into BX, then signed idiv. For `/=` store
        // AL back; for `%=` store DL (the remainder's low byte).
        // Fixture 640 (`c /= 4` → `mov al, cl; cbw; mov bx, 4;
        // cwd; idiv bx; mov cl, al`). Shift unroll wouldn't match
        // signed semantics (rounding differs for negative).
        if reg.is_byte()
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some(v) = try_const_eval(value)
        {
            let v16 = v & 0xFFFF;
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            self.out.extend_from_slice(b"\tcbw\t\r\n");
            let _ = write!(self.out, "\tmov\tbx,{v16}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tidiv\tbx\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "al" } else { "dl" };
            let _ = write!(self.out, "\tmov\t{},{result_reg}\r\n", reg.name());
            return;
        }
        // Char compound `*=` with a non-constant byte RHS: load the
        // dst into AL, then 8-bit `imul byte ptr <src>` (AX = AL *
        // src), then store AL back to the byte register. Fixture
        // 672 (`c *= d` → `mov al, dl; imul byte ptr [bp-1]; mov
        // dl, al`).
        if reg.is_byte() && matches!(op, BinOp::Mul) {
            let src = self.resolve_operand_source(value);
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            self.out.extend_from_slice(b"\timul\t");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            return;
        }
        // Char compound `/=` / `%=` with a non-constant byte RHS.
        // Signed: load dst into AL, `cbw` to sign-extend, 8-bit
        // `idiv byte ptr <src>` (AL=quotient, AH=remainder), then
        // store the quotient (or AH for `%=`) back. Fixture 673
        // (`c /= d` → `mov al, dl; cbw; idiv byte ptr [bp-1]; mov
        // dl, al`).
        //
        // Unsigned: zero-extend via `mov ah, 0`, then 8-bit `div
        // al, byte ptr <src>` — note BCC emits the explicit `al,`
        // operand in the TASM listing. Fixture 677 (`c /= d` with
        // unsigned char → `mov al, bl; mov ah, 0; div al, byte
        // ptr [bp-1]; mov bl, al`).
        if reg.is_byte() && matches!(op, BinOp::Div | BinOp::Mod) {
            let unsigned = self.locals.type_of(name).is_unsigned();
            let src = self.resolve_operand_source(value);
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                self.out.extend_from_slice(b"\tdiv\tal,");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
                self.out.extend_from_slice(b"\tidiv\t");
            }
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "al" } else { "ah" };
            let _ = write!(self.out, "\tmov\t{},{result_reg}\r\n", reg.name());
            return;
        }
        // Char compound `<<=` / `>>=` with a non-constant RHS:
        // load the RHS byte into CL with `mov cl, byte ptr <src>`,
        // then shift the byte register by CL (`sar dl, cl` for
        // signed `>>=`, `shr` for unsigned, `shl` for `<<=`).
        // Fixture 670 (`c >>= d` with c in DL, d at [bp-1]).
        if reg.is_byte() && matches!(op, BinOp::Shl | BinOp::Shr) {
            let signed = !self.locals.type_of(name).is_unsigned();
            let src = self.resolve_operand_source(value);
            self.out.extend_from_slice(b"\tmov\tcl,");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\t{},cl\r\n", reg.name());
            return;
        }
        // Char compound `+=` / `-=` / `&=` / `|=` / `^=` with a
        // non-constant RHS (another char local or global). BCC loads
        // the RHS byte into AL via `mov al, byte ptr <src>` and then
        // applies the op register-to-register (`add dl, al`, etc.).
        // Fixtures 665 (`c += d`), 666 (`c -= d`), 667 (`c &= d`),
        // 668 (`c |= d`), 669 (`c ^= d`).
        if reg.is_byte()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let src = self.resolve_operand_source(value);
            self.out.extend_from_slice(b"\tmov\tal,");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\t{},al\r\n", reg.name());
            return;
        }
        assert!(
            !reg.is_byte(),
            "compound assignment on a char (byte-register) target not yet supported (no fixture)"
        );
        // Complex RHS that resolve_operand_source can't reduce to a
        // single memory/register operand: evaluate it into AX first,
        // then apply the op via `<mnem> <reg>, ax`. Covers:
        //   - `s += a * b` / `a |= (1 << b)` / `a -= b - 1` — RHS is
        //     a BinOp (fixtures 1255, 1258, 1315).
        //   - `s += a[i]` where a is a global array and i is variable
        //     — RHS is variable-indexed ArrayIndex (fixtures 1385,
        //     1462, etc.).
        // Restricted to ops where AX-as-RHS is unambiguous:
        // Add/Sub/BitAnd/BitOr/BitXor. Mul/Shl/Shr/Div/Mod use AX/CL/
        // DX implicitly and route through their own arms below.
        if matches!(
            op,
            BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
        ) && try_const_eval(value).is_none()
            && self.value_needs_ax_route(value)
        {
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\t{},ax\r\n", reg.name());
            return;
        }
        // `<reg> *= <int_lv> <op> <int_lv>` where `<op>` is a
        // non-clobbering binop (Add/Sub/BitAnd/BitOr/BitXor): compute
        // the RHS directly into DX, skipping the `mov dx, ax` shuffle.
        // Both operands must be int-typed memory operands (stack or
        // global). Fixture 1390 (`a *= (b+c)` with a in SI, b/c stack).
        if matches!(op, BinOp::Mul)
            && let ExprKind::BinOp { op: rop, left: rl, right: rr } = &value.kind
            && matches!(rop, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && let Some(l_src) = self.int_lvalue_addr(rl)
            && let Some(r_src) = self.int_lvalue_addr(rr)
        {
            let mnem = match rop {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tdx,word ptr {l_src}\r\n");
            let _ = write!(self.out, "\t{mnem}\tdx,word ptr {r_src}\r\n");
            let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
            self.out.extend_from_slice(b"\timul\tdx\r\n");
            let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
            return;
        }
        // Mul / Div / Mod with nested-binop (or Cast/Ternary/Call)
        // RHS: evaluate RHS to AX (which clobbers AX), then perform
        // the op with the dst register as the source operand
        // (imul/idiv/div take a single r/m argument and use AX as
        // the implicit accumulator). For Mul we want `dst * ax →
        // dst`; BCC's shape is `mov dx, ax; mov ax, dst; imul dx;
        // mov dst, ax`. For Div the accumulator must be the dividend
        // (dst), so we move RHS into BX first, then load dst into
        // AX, cwd, idiv bx. Fixtures 1390 (`a *= (b+c)`), 1393
        // (`a %= b*c`).
        if matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && try_const_eval(value).is_none()
            && self.value_needs_ax_route(value)
        {
            self.emit_expr_to_ax(value);
            match op {
                BinOp::Mul => {
                    self.out.extend_from_slice(b"\tmov\tdx,ax\r\n");
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\timul\tdx\r\n");
                }
                BinOp::Div | BinOp::Mod => {
                    // BCC's shape: `push ax` (save RHS divisor),
                    // `mov ax, dst` (load dividend), `cwd`, `pop
                    // bx` (recover divisor), `idiv bx`. Same
                    // 2-byte total as `mov bx, ax` + `mov ax, dst`
                    // but matches BCC's exact sequence.
                    // Fixture 1393 (`a %= b * c`).
                    let unsigned = self.locals.type_of(name).is_unsigned();
                    self.out.extend_from_slice(b"\tpush\tax\r\n");
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    if unsigned {
                        self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcwd\t\r\n");
                    }
                    self.out.extend_from_slice(b"\tpop\tbx\r\n");
                    let mnem = if unsigned { "div" } else { "idiv" };
                    let _ = write!(self.out, "\t{mnem}\tbx\r\n");
                }
                _ => unreachable!(),
            }
            let result = if matches!(op, BinOp::Mod) { "dx" } else { "ax" };
            let _ = write!(self.out, "\tmov\t{},{result}\r\n", reg.name());
            return;
        }
        match op {
            BinOp::Add | BinOp::Sub => {
                // Pointer compound add/sub: scale the RHS by the
                // pointee's size in bytes (C pointer arithmetic).
                // Fixture 542 (`int *p; p += 2` → `add si, 4` since
                // `sizeof(int)==2`).
                let stride = self
                    .locals
                    .type_of(name)
                    .pointee()
                    .map_or(1u32, |p| u32::from(p.size_bytes()));
                if let Some(v) = try_const_eval(value) {
                    let scaled = (v & 0xFFFF).wrapping_mul(stride) & 0xFFFF;
                    if scaled == 1 {
                        let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                        let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
                        return;
                    }
                    let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
                    let _ = write!(self.out, "\t{mnem}\t{},{scaled}\r\n", reg.name());
                    return;
                }
                let src = self.resolve_operand_source(value);
                let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
                let _ = write!(self.out, "\t{mnem}\t{},{}\r\n", reg.name(), src.word());
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                let mnem = match op {
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let src = self.resolve_operand_source(value);
                let _ = write!(self.out, "\t{mnem}\t{},{}\r\n", reg.name(), src.word());
            }
            BinOp::Mul => {
                // `imul reg, imm` is 80186+; BCC uses single-operand
                // `imul <src>` with AX. For a constant RHS the
                // divisor materializes in DX first (fixture 069).
                // For a memory-resident RHS (stack local or global)
                // BCC uses `imul <mem>` directly — fixture 651 (`x
                // *= y` with y at `[bp-2]` → `mov ax, si; imul word
                // ptr [bp-2]; mov si, ax`).
                if let Some(v) = try_const_eval(value) {
                    let v16 = v & 0xFFFF;
                    let _ = write!(self.out, "\tmov\tdx,{v16}\r\n");
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\timul\tdx\r\n");
                } else {
                    let src = self.resolve_operand_source(value);
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    match &src {
                        OperandSource::Local(_)
                        | OperandSource::Global(_)
                        | OperandSource::GlobalOffset { .. } => {
                            let _ = write!(self.out, "\timul\t{}\r\n", src.word());
                        }
                        OperandSource::Reg(rhs_reg) => {
                            // `imul <reg16>` directly, no DX roundtrip
                            // (matches BCC's shape for `r *= i` with
                            // both r and i in registers — fixture
                            // 1411).
                            let _ = write!(self.out, "\timul\t{}\r\n", rhs_reg.name());
                        }
                        _ => {
                            let _ = write!(self.out, "\tmov\tdx,{}\r\n", src.word());
                            self.out.extend_from_slice(b"\timul\tdx\r\n");
                        }
                    }
                }
                let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
            }
            BinOp::Shl | BinOp::Shr => {
                // `<int-reg> <<= K` / `>>= K` — small K (1, 2, 3)
                // unrolls into repeated single-bit shifts (`<mnem>
                // <reg>, 1`) since each shift is 2 bytes (`D1 /r`)
                // vs 5 bytes for the `mov cl, K; <mnem> <reg>, cl`
                // pair (4 bytes for K, but 5 total). K >= 4 uses
                // the CL load. Same threshold BCC uses in
                // expression context (fixture 626). Fixtures 537
                // (K=4, CL form) and 1022 (K=2, unrolled).
                let signed = !self.locals.type_of(name).is_unsigned();
                let mnem = match op {
                    BinOp::Shl => "shl",
                    BinOp::Shr if signed => "sar",
                    BinOp::Shr => "shr",
                    _ => unreachable!(),
                };
                if let Some(k) = try_const_eval(value) {
                    let k = k as u16;
                    if (1..=3).contains(&k) {
                        for _ in 0..k {
                            let _ = write!(self.out, "\t{mnem}\t{},1\r\n", reg.name());
                        }
                        return;
                    }
                    let k8 = k & 0xFF;
                    let _ = write!(self.out, "\tmov\tcl,{k8}\r\n");
                    let _ = write!(self.out, "\t{mnem}\t{},cl\r\n", reg.name());
                    return;
                }
                // Non-constant shift count — load the low byte of
                // the RHS into CL via the same `mov cl, byte ptr
                // ...` shape we use for constants (but with the
                // operand source instead of an immediate). Fixture
                // 658 (`x <<= y` → `mov cl, byte ptr [bp-2]; shl
                // si, cl`).
                let src = self.resolve_operand_source(value);
                let _ = write!(self.out, "\tmov\tcl,{}\r\n", src.byte());
                let _ = write!(self.out, "\t{mnem}\t{},cl\r\n", reg.name());
            }
            BinOp::Div | BinOp::Mod => {
                // `<int-reg> /= K` (or `%= K`) — load divisor into
                // BX (DX is clobbered by `cwd`), then `mov ax, <reg>;
                // cwd; idiv bx`. `/=` stores AX back, `%=` stores DX
                // (the remainder). Fixtures 584 (`/=`) and 585 (`%=`).
                // For a memory-resident variable RHS BCC uses `idiv
                // <mem>` directly — fixture 653 (`x /= y` → `mov ax,
                // si; cwd; idiv word ptr [bp-2]; mov si, ax`).
                if let Some(v) = try_const_eval(value) {
                    let v16 = v & 0xFFFF;
                    let _ = write!(self.out, "\tmov\tbx,{v16}\r\n");
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                    self.out.extend_from_slice(b"\tidiv\tbx\r\n");
                } else {
                    let src = self.resolve_operand_source(value);
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                    match &src {
                        OperandSource::Local(_)
                        | OperandSource::Global(_)
                        | OperandSource::GlobalOffset { .. } => {
                            let _ = write!(self.out, "\tidiv\t{}\r\n", src.word());
                        }
                        _ => {
                            let _ = write!(self.out, "\tmov\tbx,{}\r\n", src.word());
                            self.out.extend_from_slice(b"\tidiv\tbx\r\n");
                        }
                    }
                }
                let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
                let _ = write!(self.out, "\tmov\t{},{result_reg}\r\n", reg.name());
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                unreachable!("comparison ops are not compound-assignable in C")
            }
        }
    }

    /// `&<name>` — load the effective address of `name`'s stack slot
    /// into AX. Pattern (fixture 080):
    /// ```text
    ///   lea ax, word ptr [bp-N]
    /// ```
    /// `name` must be stack-resident — its address was taken at parse
    /// time, which the locals analyzer uses to force it off the
    /// register pool.
    fn emit_address_of(&mut self, name: &str) {
        // `&<global>` — emit the symbol's offset as an immediate.
        // Pattern from `p = &g;` at runtime, fixture 480 (the
        // file-scope init form is handled separately via the static
        // init path).
        if self.globals.contains(name) {
            let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{name}\r\n");
            return;
        }
        let LocalLocation::Stack(off) = self.locals.location_of(name) else {
            panic!(
                "`&{name}`: register-resident local cannot have its address taken \
                 (locals analyzer should have forced it to the stack)"
            );
        };
        let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
    }

    /// `*<ptr>` in rvalue position. The inner pointer expression can
    /// be a bare `Ident(p)` or — for fixtures 091, 092, 094 — a
    /// `BinOp(Add, Ident(p), <offset>)` (and presumably Sub later).
    /// Both lower to a `<width> ptr [<addressing-mode>]` load:
    ///
    /// - **`*<ident>`** → `[<reg>]` (the pointer must be enregistered;
    ///   stack-resident pointers don't have an addressing form like
    ///   `[[bp-N]]` so we'd need a temp load — no fixture yet).
    /// - **`*(<ident> + K)`** with K constant → `[<reg> + K*stride]`
    ///   (fixture 091: `*(p + 1)` with `p: int *` → `[si+2]`).
    /// - **`*(<ident> + <i>)`** with i variable → the load/shl/add
    ///   sequence with the result in BX (fixture 092). Both pointer
    ///   and index can be either register- or stack-resident; only
    ///   the all-stack form is captured today.
    fn emit_deref_to_ax(&mut self, ptr: &Expr) {
        // `*(a + i)` where `a` is a global array (or char array): the
        // `a + i` is array-decay + variable offset. Same byte shape
        // as the array-index path: scale i into BX, then read
        // through `[bx + _a]`. Fixture 1379.
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &ptr.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(gty) = self.globals.type_of(name)
            && let Some(elem_ty) = gty.array_elem()
        {
            let elem_ty = elem_ty.clone();
            let width = ptr_width(&elem_ty);
            self.emit_index_into_bx(right, &elem_ty);
            if elem_ty.is_char_like() {
                let _ = write!(
                    self.out,
                    "\tmov\tal,byte ptr DGROUP:_{name}[bx]\r\n",
                );
                self.emit_widen_al(&elem_ty);
            } else {
                let _ = write!(
                    self.out,
                    "\tmov\tax,{width} ptr DGROUP:_{name}[bx]\r\n",
                );
            }
            return;
        }
        // `*(p + offset)` shapes go through a shared helper that
        // builds the addressing mode.
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &ptr.kind
            && let ExprKind::Ident(name) = &left.kind
            && self.locals.has(name)
        {
            let ty = self.locals.type_of(name).clone();
            if let Some(pointee) = ty.pointee() {
                return self.emit_deref_pointer_plus_offset(name, pointee.clone(), right);
            }
        }
        // `*p++` / `*p--`: post-update inside a deref (fixture 199).
        // BCC saves the pre-update pointer in BX, advances the
        // register-resident pointer by `stride` 1-byte `inc`/`dec`
        // ops (when stride ≤ 2), then reads through `[bx]`.
        if let ExprKind::Update { target, op, position: UpdatePosition::Post } = &ptr.kind {
            let ty = self.locals.type_of(target).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{target}++`: not a pointer type");
            };
            let LocalLocation::Reg(reg) = self.locals.location_of(target) else {
                panic!("stack-resident pointer in `*p++` not yet supported (no fixture)");
            };
            let reg_name = reg.name();
            let stride = pointee.size_bytes();
            let mnemonic = match op {
                UpdateOp::Inc => "inc",
                UpdateOp::Dec => "dec",
            };
            let _ = write!(self.out, "\tmov\tbx,{reg_name}\r\n");
            if stride == 1 || stride == 2 {
                for _ in 0..stride {
                    let _ = write!(self.out, "\t{mnemonic}\t{reg_name}\r\n");
                }
            } else {
                panic!("`*p++` with pointee stride > 2 not yet supported (no fixture)");
            }
            if pointee.is_char_like() {
                self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            } else {
                let width = ptr_width(pointee);
                let _ = write!(self.out, "\tmov\tax,{width} ptr [bx]\r\n");
            }
            return;
        }
        let (base_name, depth) = deref_chain_root(ptr);
        // Single-deref of a stack/register-resident local stays on
        // the original fast path (`mov al,byte ptr [si]` etc.) so
        // SI/DI-resident pointers don't bounce through BX.
        let is_global = self.globals.type_of(base_name).is_some();
        if depth == 0 && !is_global {
            let ty = self.locals.type_of(base_name).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{base_name}`: not a pointer type");
            };
            let width = ptr_width(pointee);
            let addr_reg = match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) => reg.name().to_owned(),
                LocalLocation::Stack(_) => {
                    panic!("stack-resident bare-`*p` dereference not yet supported (no fixture)");
                }
            };
            if pointee.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr [{addr_reg}]\r\n");
                self.emit_widen_al(pointee);
            } else {
                let _ = write!(self.out, "\tmov\tax,{width} ptr [{addr_reg}]\r\n");
            }
            return;
        }
        // Chain path: land the address-to-be-deref'd-once-more in BX,
        // then do the final load. Fixture 195 (`int **p` → `**p`)
        // hits depth=1; fixture 193 hits depth=0 on a global.
        let final_ty = self.emit_chain_to_bx(base_name, depth);
        if final_ty.is_char_like() {
            self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
            self.emit_widen_al(&final_ty);
        } else {
            let width = ptr_width(&final_ty);
            let _ = write!(self.out, "\tmov\tax,{width} ptr [bx]\r\n");
        }
    }

    /// Walk a deref chain and land the address-to-be-deref'd-once-
    /// more in BX. `depth` is the number of *visible* `*`s above the
    /// base ident (so for `**p` called from the outer `*`, depth=1).
    /// Emits the base load and `depth` intermediate `mov bx,[bx]`
    /// chain steps; the caller emits the final read or write through
    /// `[bx]`. Returns the type of the value at `[bx]` (after
    /// `depth + 1` total pointer peels).
    fn emit_chain_to_bx(&mut self, base_name: &str, depth: u32) -> Type {
        let is_global = self.globals.type_of(base_name).is_some();
        let base_ty = if is_global {
            self.globals.type_of(base_name).expect("checked above").clone()
        } else {
            self.locals.type_of(base_name).clone()
        };
        let mut final_ty = base_ty;
        for _ in 0..=depth {
            let next = final_ty
                .pointee()
                .unwrap_or_else(|| panic!("`*{base_name}`: chain too deep for its type"))
                .clone();
            final_ty = next;
        }
        // When `depth > 0` and the root pointer is in a non-BX
        // register, combine the `mov bx,<reg>` + `mov bx,[bx]` pair
        // into a single `mov bx,[<reg>]` (the first peel). Saves 2
        // bytes per chain. Mirrors BCC's actual shape for fixture
        // 1232 (`**pp` with pp in SI → `mov bx,[si]; mov ax,[bx]`).
        let mut remaining_peels = depth;
        if is_global {
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{base_name}\r\n");
        } else {
            match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) if reg.name() == "bx" => {}
                LocalLocation::Reg(reg) if depth > 0 => {
                    let _ = write!(
                        self.out,
                        "\tmov\tbx,word ptr [{}]\r\n",
                        reg.name(),
                    );
                    remaining_peels -= 1;
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                }
                LocalLocation::Stack(_) => {
                    panic!("stack-resident pointer chain root not yet supported (no fixture)");
                }
            }
        }
        for _ in 0..remaining_peels {
            self.out.extend_from_slice(b"\tmov\tbx,word ptr [bx]\r\n");
        }
        final_ty
    }

    /// `*(<ptr> + <offset>)` for fixtures 091, 092, 094. The pointer
    /// name + pointee type are extracted by the caller; `offset` is
    /// the right side of the `+`.
    fn emit_deref_pointer_plus_offset(
        &mut self,
        ptr_name: &str,
        pointee: Type,
        offset: &Expr,
    ) {
        let stride = u32::from(pointee.size_bytes());
        let load_byte = pointee.is_char_like();
        if let Some(k) = try_const_eval(offset) {
            // Constant offset — fold to indexed addressing on the
            // pointer register. Stack-resident pointers with a
            // constant offset aren't observed yet; assume reg only.
            let LocalLocation::Reg(reg) = self.locals.location_of(ptr_name) else {
                panic!("stack-resident pointer in `*(p+K)` not yet supported (no fixture)");
            };
            let byte_off = k * stride;
            let addr = if byte_off == 0 {
                format!("[{}]", reg.name())
            } else {
                format!("[{}+{byte_off}]", reg.name())
            };
            if load_byte {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
            return;
        }
        // Variable offset. Fixture 092 (both p and i on the stack):
        //   mov ax, word ptr [bp-i]
        //   shl ax, 1               ; * stride (stride=2 for int)
        //   mov bx, word ptr [bp-p]
        //   add bx, ax
        //   mov ax, word ptr [bx]
        // Reg-resident variants are inferred but unobserved.
        self.emit_expr_to_ax(offset);
        if stride == 2 {
            self.out.extend_from_slice(b"\tshl\tax,1\r\n");
        } else if stride != 1 {
            panic!("non-1/2 pointer stride not yet supported (no fixture)");
        }
        match self.locals.location_of(ptr_name) {
            LocalLocation::Stack(off) => {
                let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
            }
            LocalLocation::Reg(reg) => {
                let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
            }
        }
        self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
        if load_byte {
            self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        } else {
            self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
        }
    }

    /// `a[<index>]` in rvalue position. The `array` side can be:
    /// - An ident referencing a local array (077, 078, 082, 079).
    ///   Constant index → direct `[bp-K]` load; variable index → the
    ///   5-instruction effective-address sequence.
    /// - A string literal (089: `"hi"[0]`). The literal is registered
    ///   in the string pool and the access folds to a direct
    ///   `DGROUP:s@<offset>` reference for constant indices. Variable
    ///   indexing of a string literal isn't observed yet.
    fn emit_array_index_to_ax(&mut self, array: &Expr, index: &Expr) {
        if let ExprKind::StringLit(bytes) = &array.kind {
            return self.emit_string_lit_index_to_ax(bytes, index);
        }
        // `b.data[K]` — read an array element inside a struct field.
        // With a constant index we can fold field offset + K*stride
        // into a single byte displacement. Fixture 497.
        if let ExprKind::Member {
            base,
            field,
            kind: crate::ast::MemberKind::Dot,
        } = &array.kind
        {
            if let ExprKind::Ident(base_name) = &base.kind
                && let Some(k) = try_const_eval(index)
            {
                let base_ty = if self.globals.contains(base_name) {
                    self.globals.type_of(base_name).unwrap().clone()
                } else {
                    self.locals.type_of(base_name).clone()
                };
                if let Some((field_off, field_ty)) = base_ty.field(field) {
                    if let Type::Array { elem, .. } = field_ty {
                        let stride = u32::from(elem.size_bytes());
                        let total_off =
                            u32::from(field_off) + (k as u32).wrapping_mul(stride);
                        let elem_ty = *elem;
                        let width = ptr_width(&elem_ty);
                        let addr = if self.globals.contains(base_name) {
                            if total_off == 0 {
                                format!("DGROUP:_{base_name}")
                            } else {
                                format!("DGROUP:_{base_name}+{total_off}")
                            }
                        } else {
                            let LocalLocation::Stack(struct_off) =
                                self.locals.location_of(base_name)
                            else {
                                panic!("struct local `{base_name}` not stack-resident");
                            };
                            let off = struct_off
                                + i16::try_from(total_off).unwrap_or(i16::MAX);
                            bp_addr(off)
                        };
                        if elem_ty.is_char_like() {
                            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                            self.emit_widen_al(&elem_ty);
                        } else {
                            let _ = write!(
                                self.out,
                                "\tmov\tax,{width} ptr {addr}\r\n"
                            );
                        }
                        return;
                    }
                }
            }
        }
        // Walk a nested chain `a[i1][i2]...` down to the base ident,
        // collecting indices from innermost to outermost. A bare
        // `a[i]` lands here with `indices = [i]` after the reversal.
        let mut indices: Vec<&Expr> = vec![index];
        let mut cur = array;
        let array_name = loop {
            match &cur.kind {
                ExprKind::ArrayIndex { array: inner, index: inner_ix } => {
                    indices.push(inner_ix);
                    cur = inner;
                }
                ExprKind::Ident(name) => break name.as_str(),
                _ => panic!(
                    "array base in `a[i]` must be an ident, nested array-index, or string literal (no fixture for {:?})",
                    cur.kind,
                ),
            }
        };
        indices.reverse();
        // Global array? Route to DGROUP-relative addressing.
        // Fixture 189 (`int a[3] = {1, 2, 3}; return a[0] + a[1] + a[2];`).
        if let Some(gty) = self.globals.type_of(array_name) {
            let gty = gty.clone();
            // Global pointer indexed at depth 1: `p[i]` where `p: T*`.
            // Equivalent to `*(p + i)` — load `p` into `bx` from
            // `DGROUP:_p`, then dereference. Fixture 192
            // (`char *p = "hi"; return p[0];`).
            if let Some(pointee) = gty.pointee() {
                if indices.len() == 1 {
                    return self.emit_global_pointer_index_to_ax(
                        array_name,
                        pointee.clone(),
                        indices[0],
                    );
                }
            }
            if let Some((const_off, leaf_ty)) =
                try_const_array_offset(&gty, indices.iter().copied())
            {
                let width = ptr_width(&leaf_ty);
                let addr = if const_off == 0 {
                    format!("DGROUP:_{array_name}")
                } else {
                    format!("DGROUP:_{array_name}+{const_off}")
                };
                if leaf_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                    self.emit_widen_al(&leaf_ty);
                } else {
                    let _ = write!(self.out, "\tmov\tax,{width} ptr {addr}\r\n");
                }
                return;
            }
            // Variable-indexed global array `_a[i]` at depth 1. BCC's
            // shape: load the index into BX (going via AX+cbw if the
            // index is char-typed), scale by the element stride
            // (`shl bx, 1` for int, twice for long, skip for char),
            // then read through `[bx+_a]`. Fixture 1284 (int index),
            // 1493 (char-typed index, requires CBW widening).
            //
            // `_a[i ± K]`: fold the constant offset into the FIXUPP
            // disp (`_a + K*stride[bx]`) so the index becomes just
            // `i`. Fixture 3637 (`arr[i+1]`), 3033 (`arr[i-1]`).
            if indices.len() == 1
                && let Some(elem_ty) = gty.array_elem()
            {
                let elem_ty = elem_ty.clone();
                let stride = i32::from(elem_ty.size_bytes());
                let (idx_expr, const_off) = match &indices[0].kind {
                    ExprKind::BinOp { op: BinOp::Add, left, right }
                        if let Some(k) = try_const_eval(right)
                            && try_const_eval(left).is_none() =>
                    {
                        (left.as_ref(), (k as i32).wrapping_mul(stride))
                    }
                    ExprKind::BinOp { op: BinOp::Sub, left, right }
                        if let Some(k) = try_const_eval(right)
                            && try_const_eval(left).is_none() =>
                    {
                        (left.as_ref(), -(k as i32).wrapping_mul(stride))
                    }
                    _ => (indices[0], 0),
                };
                // Char-array read with index in SI: BCC keeps the
                // index in SI and uses SI-indexed addressing
                // directly (`mov al, byte ptr _a[si]`) with no
                // intermediate `mov bx, si`. Fixture 1426 (`for
                // (i=0..) dst[i] = src[i];`).
                if elem_ty.is_char_like()
                    && let ExprKind::Ident(i_name) = &idx_expr.kind
                    && self.locals.has(i_name)
                    && let LocalLocation::Reg(reg) = self.locals.location_of(i_name)
                    && reg.name() == "si"
                {
                    let addr = if const_off == 0 {
                        format!("DGROUP:_{array_name}[si]")
                    } else if const_off > 0 {
                        format!("DGROUP:_{array_name}+{const_off}[si]")
                    } else {
                        format!("DGROUP:_{array_name}-{}[si]", -const_off)
                    };
                    let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                    self.emit_widen_al(&elem_ty);
                    return;
                }
                self.emit_index_into_bx(idx_expr, &elem_ty);
                let width = ptr_width(&elem_ty);
                let addr = if const_off == 0 {
                    format!("DGROUP:_{array_name}[bx]")
                } else if const_off > 0 {
                    format!("DGROUP:_{array_name}+{const_off}[bx]")
                } else {
                    // Negative const offset: tasm syntax is sym-K.
                    // The disp16 in the FIXUPP will be sign-extended;
                    // the underflow wraps in the OBJ. BCC actually
                    // emits this as `_a-K[bx]` (e.g. `_a-2[bx]` for
                    // `arr[i-1]`).
                    format!("DGROUP:_{array_name}-{}[bx]", -const_off)
                };
                if elem_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                    self.emit_widen_al(&elem_ty);
                } else {
                    let _ = write!(self.out, "\tmov\tax,{width} ptr {addr}\r\n");
                }
                return;
            }
            panic!("variable-indexed global array not yet supported (no fixture)");
        }
        let ty = self.locals.type_of(array_name).clone();
        // `p[i]` where `p` is a pointer (not an array). Equivalent
        // to `*(p + i)`. Fixture 088: `s[0]` with `s: char *` in SI
        // → `mov al, byte ptr [si] / cbw`. Only handled at depth 1.
        if let Some(pointee) = ty.pointee() {
            if indices.len() != 1 {
                panic!("multi-level index through a pointer not yet supported (no fixture)");
            }
            return self.emit_pointer_index_to_ax(array_name, pointee.clone(), indices[0]);
        }
        let LocalLocation::Stack(base_off) = self.locals.location_of(array_name) else {
            panic!("array `{array_name}` should be stack-resident");
        };
        if let Some((const_off, leaf_ty)) =
            try_const_array_offset(&ty, indices.iter().copied())
        {
            let off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
            let width = ptr_width(&leaf_ty);
            if leaf_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                self.emit_widen_al(&leaf_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,{width} ptr {}\r\n", bp_addr(off));
            }
            return;
        }
        // 2D variable-index read: `a[i][j]` for `int a[M][N]`.
        // Fixture 198. Other multi-dim depths aren't fixtured yet.
        if indices.len() == 2 {
            let (outer_stride, inner_stride, leaf_ty) = match &ty {
                Type::Array { elem, .. } => match &**elem {
                    inner_arr @ Type::Array { elem: inner_elem, .. } => (
                        inner_arr.size_bytes(),
                        inner_elem.size_bytes(),
                        (**inner_elem).clone(),
                    ),
                    _ => panic!("`{array_name}[i][j]`: outer element isn't an array"),
                },
                _ => panic!("`{array_name}[i][j]`: not an array type"),
            };
            self.emit_array_addr_2d_to_bx(
                indices[0],
                indices[1],
                outer_stride,
                inner_stride,
                base_off,
            );
            let width = ptr_width(&leaf_ty);
            if leaf_ty.is_char_like() {
                self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{width} ptr [bx]\r\n");
            }
            return;
        }
        if indices.len() != 1 {
            panic!("multi-dim array read with non-constant indices not yet supported (no fixture)");
        }
        let elem = ty
            .array_elem()
            .unwrap_or_else(|| panic!("`{array_name}[i]`: not an array type"));
        let elem_size = elem.size_bytes();
        let width = ptr_width(elem);
        self.emit_array_addr_to_bx(array_name, indices[0], base_off, elem_size);
        if elem.is_char_like() {
            self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        } else {
            let _ = write!(self.out, "\tmov\tax,{width} ptr [bx]\r\n");
        }
    }

    /// `p[<index>]` where `p` is a pointer (not an array). Equivalent
    /// to `*(p + index)`. Fixture 088: `s[0]` with `s: char *` in SI
    /// emits `mov al, byte ptr [si] / cbw`. Variable-indexed pointer
    /// access isn't observed yet — would need an add-into-bx step.
    /// `p[K]` where `p` is a global pointer (not array). Load `p`
    /// into BX from `DGROUP:_p`, then deref. Fixture 192
    /// (`char *p = "hi"; return p[0];`).
    fn emit_global_pointer_index_to_ax(&mut self, ptr_name: &str, pointee: Type, index: &Expr) {
        let Some(k) = try_const_eval(index) else {
            panic!("variable-indexed global pointer access not yet supported (no fixture)");
        };
        let _ = write!(
            self.out,
            "\tmov\tbx,word ptr DGROUP:_{ptr_name}\r\n"
        );
        let stride = u32::from(pointee.size_bytes());
        let byte_off = k * stride;
        let addr = if byte_off == 0 {
            "[bx]".to_owned()
        } else {
            format!("[bx+{byte_off}]")
        };
        if pointee.is_char_like() {
            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        } else {
            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
        }
    }

    fn emit_pointer_index_to_ax(&mut self, ptr_name: &str, pointee: Type, index: &Expr) {
        let Some(k) = try_const_eval(index) else {
            // Variable index. BCC's shape (fixture 1339):
            //   mov ax, <index>
            //   shl ax, 1     ; scale by stride (int = 2)
            //   mov bx, <ptr> ; load pointer
            //   add bx, ax    ; pointer arithmetic
            //   mov ax, [bx]  ; deref
            // Char stride is 1 → no shl; long would need shl ax, 2.
            let stride = u32::from(pointee.size_bytes());
            self.emit_expr_to_ax(index);
            for _ in 0..stride.trailing_zeros() {
                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
            }
            match self.locals.location_of(ptr_name) {
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                }
                LocalLocation::Stack(off) => {
                    let _ = write!(
                        self.out,
                        "\tmov\tbx,word ptr {}\r\n",
                        bp_addr(off),
                    );
                }
            }
            self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
            if pointee.is_char_like() {
                self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                if pointee.is_unsigned() {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
            } else {
                self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
            }
            return;
        };
        let addr_reg = match self.locals.location_of(ptr_name) {
            LocalLocation::Reg(reg) => reg.name(),
            LocalLocation::Stack(_) => {
                panic!("stack-resident pointer in `p[K]` not yet supported (no fixture)");
            }
        };
        // The address operand: `[reg]` for k=0, else `[reg+K*stride]`.
        let stride = u32::from(pointee.size_bytes());
        let byte_off = k * stride;
        let addr = if byte_off == 0 {
            format!("[{addr_reg}]")
        } else {
            format!("[{addr_reg}+{byte_off}]")
        };
        if pointee.is_char_like() {
            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
            self.emit_widen_al(&pointee);
        } else {
            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
        }
    }

    /// `"<string>"[<index>]` — string literal indexed in place. For
    /// a constant index, BCC folds the access to a direct memory
    /// reference (fixture 089: `"hi"[0]` → `mov al, byte ptr DGROUP:s@`).
    /// Variable indexing of a string literal isn't observed yet.
    fn emit_string_lit_index_to_ax(&mut self, bytes: &[u8], index: &Expr) {
        let pool_offset = self.strings.intern(bytes);
        let Some(k) = try_const_eval(index) else {
            panic!("variable-indexed string literal not yet supported (no fixture)");
        };
        let total_offset = pool_offset + k;
        let label = if total_offset == 0 {
            "DGROUP:s@".to_owned()
        } else {
            format!("DGROUP:s@+{total_offset}")
        };
        // Strings are bytes; load AL then sign-extend, matching the
        // char-array constant-index path.
        let _ = write!(self.out, "\tmov\tal,byte ptr {label}\r\n");
        self.out.extend_from_slice(b"\tcbw\t\r\n");
    }

    /// Emit the 4-instruction sequence that lands `&a[index]` in BX
    /// (used as a shared head by `emit_array_index_to_ax` and
    /// Load an integer index into BX and scale by 4 (long stride),
    /// for variable-indexed long-array element access on globals
    /// (the symbol's offset is then folded into the disp16 of the
    /// `[bx+disp]` operand). BCC special-cases the load:
    /// - Int stack local: `mov bx, word ptr [bp-N]` (3 bytes).
    /// - Int register local: `mov bx, <reg>` (2 bytes).
    /// - Anything else: compute into AX, then `mov bx, ax`.
    /// Followed by two `shl bx, 1` (stride 4 = 2^2). Fixtures 303,
    /// 305, 307.
    fn emit_index_into_bx_long_stride(&mut self, index: &Expr) {
        if let ExprKind::Ident(i_name) = &index.kind
            && self.locals.has(i_name)
        {
            match self.locals.location_of(i_name) {
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                }
            }
        } else {
            self.emit_expr_to_ax(index);
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
        }
        self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
        self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
    }

    /// `emit_array_assign` for the variable-index case):
    /// ```text
    ///   mov bx, <index>
    ///   shl bx, 1               ; only when elem stride is 2
    ///   lea ax, word ptr [bp-<base>]
    ///   add bx, ax
    /// ```
    fn emit_array_addr_to_bx(
        &mut self,
        _array: &str,
        index: &Expr,
        base_off: i16,
        elem_size: u16,
    ) {
        // Load index into BX. If it's a register-local, that's a
        // direct `mov bx, <reg>`; otherwise we'd need a stack load —
        // no fixture for that yet.
        let ExprKind::Ident(idx_name) = &index.kind else {
            panic!("non-ident array index not yet supported (no fixture)");
        };
        match self.locals.location_of(idx_name) {
            LocalLocation::Reg(reg) => {
                let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
            }
            LocalLocation::Stack(off) => {
                let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
            }
        }
        if elem_size == 2 {
            self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
        }
        let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(base_off));
        self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
    }

    /// Two-dim variable-index address: lands `&a[i][j]` in BX. BCC's
    /// pattern (fixture 198):
    /// ```text
    ///   mov ax, <outer-reg>       ; outer index into AX
    ///   mov dx, <outer-stride>
    ///   imul dx                   ; AX = outer * outer-stride (signed)
    ///   mov dx, <inner-reg>       ; inner index into DX
    ///   shl dx, 1                 ; only when inner-stride == 2
    ///   add ax, dx                ; AX = outer*os + inner*is
    ///   lea dx, word ptr [bp-base]
    ///   add ax, dx                ; AX = base + total
    ///   mov bx, ax
    /// ```
    /// Currently restricted to stride 2 on the inner axis (the only
    /// fixtured case). Outer stride uses `imul` regardless of whether
    /// it's a power of two — BCC seems to never `shl` the outer
    /// multiplier in observed output, possibly because outer strides
    /// aren't typically powers of two in C2.0-era code.
    fn emit_array_addr_2d_to_bx(
        &mut self,
        outer_idx: &Expr,
        inner_idx: &Expr,
        outer_stride: u16,
        inner_stride: u16,
        base_off: i16,
    ) {
        let outer_reg = self.idx_reg_name(outer_idx);
        let _ = write!(self.out, "\tmov\tax,{outer_reg}\r\n");
        let _ = write!(self.out, "\tmov\tdx,{outer_stride}\r\n");
        self.out.extend_from_slice(b"\timul\tdx\r\n");
        let inner_reg = self.idx_reg_name(inner_idx);
        let _ = write!(self.out, "\tmov\tdx,{inner_reg}\r\n");
        if inner_stride == 2 {
            self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
        } else if inner_stride != 1 {
            panic!("2D inner-stride != {{1,2}} not yet supported (no fixture)");
        }
        self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
        let _ = write!(self.out, "\tlea\tdx,word ptr {}\r\n", bp_addr(base_off));
        self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
        self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
    }

    /// Look up the register name for an index that's an Ident bound
    /// to a register-resident local. Used by the 2D address helper.
    fn idx_reg_name(&self, index: &Expr) -> &'static str {
        let ExprKind::Ident(name) = &index.kind else {
            panic!("non-ident multi-dim index not yet supported (no fixture)");
        };
        match self.locals.location_of(name) {
            LocalLocation::Reg(reg) => reg.name(),
            LocalLocation::Stack(_) => {
                panic!("stack-resident multi-dim index not yet supported (no fixture)");
            }
        }
    }

    /// `a[<i1>][<i2>]... = <value>;` — write into an array slot. With
    /// all-constant indices we fold to a single `mov <width> ptr
    /// [bp-N], K`. Otherwise (single-dim variable index, fixtures
    /// 078/142) we compute `&a[i]` into BX and store through it.
    fn emit_array_assign(&mut self, array: &str, indices: &[Expr], value: &Expr) {
        // Pointer-base: `p[K] = v` is sugar for `*(p + K) = v`. For a
        // long-pointee constant index of 0, this is identical to
        // `*p = v` — same memory-direct pair through `[reg]`/`[reg+2]`.
        // Fixture 312 (`long *p; p[0] = 42;`).
        if self.locals.has(array)
            && let Some(pointee) = self.locals.type_of(array).pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
        {
            let pointee = pointee.clone();
            let LocalLocation::Reg(reg) = self.locals.location_of(array) else {
                panic!("stack-resident pointer indexed write not yet supported (no fixture)");
            };
            let r = reg.name();
            let stride = u32::from(pointee.size_bytes());
            let byte_off = (k * stride) as i32;
            if pointee.is_long_like() {
                let Some(v) = try_const_eval(value) else {
                    panic!("non-constant rhs in `p[K] = v` (long pointee) not yet supported (no fixture)");
                };
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let lo_addr = if byte_off == 0 {
                    format!("[{r}]")
                } else {
                    format!("[{r}+{byte_off}]")
                };
                let hi_addr = format!("[{r}+{}]", byte_off + 2);
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},{hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},{lo}\r\n");
                return;
            }
            // `int *p; p[K] = v` — `mov word ptr [<reg>+byte_off],
            // <value>` (or `[<reg>]` when byte_off==0). Fixture 590.
            let width = ptr_width(&pointee);
            let addr = if byte_off == 0 {
                format!("[{r}]")
            } else {
                format!("[{r}+{byte_off}]")
            };
            if let Some(v) = try_const_eval(value) {
                let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(self.out, "\tmov\t{width} ptr {addr},{v_masked}\r\n");
                return;
            }
            panic!("non-constant rhs in `int *p; p[K] = v` not yet supported (no fixture)");
        }
        // Global array? Route to DGROUP-relative addressing.
        if let Some(gty) = self.globals.type_of(array) {
            let gty = gty.clone();
            if let Some((const_off, leaf_ty)) =
                try_const_array_offset(&gty, indices.iter())
            {
                // Long element: store both halves, high then low.
                // Fixture 302 (`long a[3]; a[1] = 42;`).
                if leaf_ty.is_long_like() {
                    let lo_addr = global_offset_addr(array, const_off);
                    let hi_addr = global_offset_addr(array, const_off + 2);
                    if let Some(v) = try_const_eval(value) {
                        let lo = (v & 0xFFFF) as u16;
                        let hi = ((v >> 16) & 0xFFFF) as u16;
                        let _ = write!(self.out, "\tmov\tword ptr {hi_addr},{hi}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {lo_addr},{lo}\r\n");
                        return;
                    }
                    // Non-constant RHS (e.g. `a[1] = g + h`): route
                    // through the long-value-to-dest helper. Fixture
                    // 359.
                    if self.try_emit_long_value_to_dest(value, &hi_addr, &lo_addr) {
                        return;
                    }
                    panic!("non-constant rhs in long-array element assign not yet supported (no fixture)");
                }
                let width = ptr_width(&leaf_ty);
                let addr = global_offset_addr(array, const_off);
                if let Some(v) = try_const_eval(value) {
                    let v_masked =
                        if leaf_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                    let _ = write!(self.out, "\tmov\t{width} ptr {addr},{v_masked}\r\n");
                    return;
                }
                // Non-constant RHS to a fixed-offset global array
                // element: evaluate to AX, then store. Fixture 1458
                // (`int g[3]; g[1] = v;`).
                self.emit_expr_to_ax(value);
                if leaf_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tbyte ptr {addr},al\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tword ptr {addr},ax\r\n");
                }
                return;
            }
            // Global-pointer subscript assignment: `int *p; p[K] = v`.
            // Load the pointer into BX, then `mov word ptr [bx+off],
            // <ax|imm>`. Mirrors the local-pointer path (fixture 590)
            // but the pointer lives in DGROUP, not a register.
            // Fixture 887 (var RHS).
            if let Some(pointee) = gty.pointee()
                && indices.len() == 1
                && let Some(k) = try_const_eval(&indices[0])
                && matches!(pointee, Type::Int | Type::UInt)
            {
                let stride = i32::from(pointee.size_bytes());
                let off = (k as i32).wrapping_mul(stride);
                let bx_disp = if off == 0 {
                    "[bx]".to_owned()
                } else if off > 0 {
                    format!("[bx+{off}]")
                } else {
                    format!("[bx-{}]", -off)
                };
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
                if let Some(v) = try_const_eval(value) {
                    let v_masked = v & 0xFFFF;
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {bx_disp},{v_masked}\r\n",
                    );
                } else {
                    self.emit_expr_to_ax(value);
                    let _ = write!(self.out, "\tmov\tword ptr {bx_disp},ax\r\n");
                }
                return;
            }
            // Long-pointer subscript assignment: `long *p; p[K] = v`.
            // `mov bx, _p; mov word ptr [bx+off+2], <hi>; mov word
            // ptr [bx+off], <lo>`. High-first store convention same
            // as long-global and long-array paths. Fixture 897.
            if let Some(pointee) = gty.pointee()
                && indices.len() == 1
                && let Some(k) = try_const_eval(&indices[0])
                && pointee.is_long_like()
            {
                let Some(v) = try_const_eval(value) else {
                    panic!("non-constant rhs in `long *p; p[K] = v` not yet supported (no fixture)");
                };
                let stride = i32::from(pointee.size_bytes());
                let off = (k as i32).wrapping_mul(stride);
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let lo_addr = if off == 0 {
                    "[bx]".to_owned()
                } else if off > 0 {
                    format!("[bx+{off}]")
                } else {
                    format!("[bx-{}]", -off)
                };
                let hi_off = off + 2;
                let hi_addr = if hi_off > 0 {
                    format!("[bx+{hi_off}]")
                } else if hi_off < 0 {
                    format!("[bx-{}]", -hi_off)
                } else {
                    "[bx]".to_owned()
                };
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},{hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},{lo}\r\n");
                return;
            }
            // Variable-indexed global char-array write. BCC uses SI
            // (the loop var's register) with no stride scaling
            // (stride=1 for char). For const RHS K: `mov byte ptr
            // _arr[si], K`. Fixture 1366 (`for (i=0..) buf[i] =
            // 'X';` for global char buf[]).
            if indices.len() == 1
                && let Some(elem) = gty.array_elem()
                && elem.is_char_like()
                && let ExprKind::Ident(i_name) = &indices[0].kind
                && self.locals.has(i_name)
                && let LocalLocation::Reg(reg) = self.locals.location_of(i_name)
                && reg.name() == "si"
            {
                if let Some(v) = try_const_eval(value) {
                    let v8 = (v & 0xFF) as u8;
                    let _ = write!(
                        self.out,
                        "\tmov\tbyte ptr DGROUP:_{array}[si],{v8}\r\n",
                    );
                    return;
                }
                // Non-const RHS: load value to AL, then store
                // through SI-indexed addressing.
                self.emit_expr_to_ax(value);
                let _ = write!(
                    self.out,
                    "\tmov\tbyte ptr DGROUP:_{array}[si],al\r\n",
                );
                return;
            }
            // Variable-indexed global int-array write. Load `i` into
            // BX, shl once for stride 2, then `mov word ptr
            // _a[bx], <src>`. Fixture 510 (`a[i] = i`).
            if indices.len() == 1
                && let Some(elem) = gty.array_elem()
                && matches!(elem, Type::Int | Type::UInt)
            {
                let index = &indices[0];
                if let ExprKind::Ident(i_name) = &index.kind
                    && self.locals.has(i_name)
                {
                    match self.locals.location_of(i_name) {
                        LocalLocation::Stack(off) => {
                            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                        }
                        LocalLocation::Reg(reg) => {
                            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                        }
                    }
                } else {
                    self.emit_expr_to_ax(index);
                    self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                }
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
                let src = self.resolve_operand_source(value);
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{array}[bx],{}\r\n",
                    src.word(),
                );
                return;
            }
            // Variable-indexed global long-array write. Load `i` into
            // BX (directly if it's a stack/reg local, otherwise via
            // AX), shl twice for stride 4, then write `mov word ptr
            // _a[bx+0], lo` and `mov word ptr _a[bx+2], hi`. Fixture
            // 305.
            if indices.len() == 1
                && let Some(elem) = gty.array_elem()
                && elem.is_long_like()
            {
                let Some(v) = try_const_eval(value) else {
                    panic!("non-constant rhs in variable-indexed global long-array assign not yet supported (no fixture)");
                };
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let index = &indices[0];
                self.emit_index_into_bx_long_stride(index);
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{array}[bx+2],{hi}\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{array}[bx],{lo}\r\n",
                );
                return;
            }
            panic!("variable-indexed global array assign not yet supported (no fixture)");
        }
        let array_ty = self.locals.type_of(array).clone();
        let LocalLocation::Stack(base_off) = self.locals.location_of(array) else {
            panic!("array `{array}` should be stack-resident");
        };
        if let Some((const_off, leaf_ty)) = try_const_array_offset(&array_ty, indices.iter()) {
            let off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
            // Long element on stack: store both halves, high then low.
            // Fixture 304 (`long a[2]; a[0] = 5;`).
            if leaf_ty.is_long_like() {
                let Some(v) = try_const_eval(value) else {
                    panic!("non-constant rhs in long-stack-array element assign not yet supported (no fixture)");
                };
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let _ = write!(self.out, "\tmov\tword ptr {},{hi}\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tword ptr {},{lo}\r\n", bp_addr(off));
                return;
            }
            let width = ptr_width(&leaf_ty);
            if let Some(v) = try_const_eval(value) {
                let v_masked =
                    if leaf_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(
                    self.out,
                    "\tmov\t{width} ptr {},{v_masked}\r\n",
                    bp_addr(off),
                );
                return;
            }
            // Non-constant RHS for an int/uint/pointer element:
            // materialize RHS in AX, then store AX to the element.
            // Fixture 984 (`a[0] = x` with x a stack local).
            if !leaf_ty.is_char_like() {
                // Reg-resident-ident RHS: store the register directly
                // to the array slot. Fixture 2452 (`a[0] = x` with x
                // in SI → `mov [bp-N], si`).
                if let ExprKind::Ident(src_name) = &value.kind
                    && self.locals.has(src_name)
                    && self.locals.type_of(src_name).is_int_like()
                    && let LocalLocation::Reg(reg) = self.locals.location_of(src_name)
                    && !reg.is_byte()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},{}\r\n",
                        bp_addr(off),
                        reg.name(),
                    );
                    return;
                }
                self.emit_expr_to_ax(value);
                let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                return;
            }
            panic!("non-constant rhs in constant-indexed array assign not yet supported (no fixture)");
        }
        // 2D variable-index write: `a[i][j] = v` for `int a[M][N]`.
        // Same chain as the read path (fixture 198), with a store
        // through `[bx]` instead of a load.
        if indices.len() == 2 {
            let (outer_stride, inner_stride, leaf_ty) = match &array_ty {
                Type::Array { elem, .. } => match &**elem {
                    inner_arr @ Type::Array { elem: inner_elem, .. } => (
                        inner_arr.size_bytes(),
                        inner_elem.size_bytes(),
                        (**inner_elem).clone(),
                    ),
                    _ => panic!("`{array}[i][j] = v`: outer element isn't an array"),
                },
                _ => panic!("`{array}[i][j] = v`: not an array type"),
            };
            self.emit_array_addr_2d_to_bx(
                &indices[0],
                &indices[1],
                outer_stride,
                inner_stride,
                base_off,
            );
            let width = ptr_width(&leaf_ty);
            let Some(v) = try_const_eval(value) else {
                panic!("non-constant rhs in 2D array assign not yet supported (no fixture)");
            };
            let v_masked =
                if leaf_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
            return;
        }
        // Variable-index fallback: only the single-dim path is wired
        // up today (covers fixtures 078, 142). Deeper multi-dim with
        // any non-const subscript isn't fixtured.
        if indices.len() != 1 {
            panic!("multi-dim (>2) array assign with non-constant indices not yet supported (no fixture)");
        }
        let elem = array_ty
            .array_elem()
            .unwrap_or_else(|| panic!("`{array}[i] = v`: not an array type"));
        let elem_size = elem.size_bytes();
        let width = ptr_width(elem);
        self.emit_array_addr_to_bx(array, &indices[0], base_off, elem_size);
        if let Some(v) = try_const_eval(value) {
            let v_masked = if elem.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
            return;
        }
        // Non-constant RHS: evaluate to AX (or AL for byte storage),
        // then store through [bx]. Fixtures 1219 (`a[i] = i` with char
        // array), 1468 (int array), 1276 (`s[i] = 'a' + i`).
        self.emit_expr_to_ax(value);
        if elem.is_char_like() {
            let _ = write!(self.out, "\tmov\tbyte ptr [bx],al\r\n");
        } else {
            let _ = write!(self.out, "\tmov\tword ptr [bx],ax\r\n");
        }
    }

    /// `a[<i1>][<i2>]... <op>= <value>;` — read-modify-write on an
    /// array element. Mirrors `emit_array_assign` for the all-const
    /// index path; emits `<op> <width> ptr [bp-N],<imm>` instead of
    /// `mov` (fixture 184).
    fn emit_array_compound_assign(
        &mut self,
        array: &str,
        indices: &[Expr],
        op: BinOp,
        value: &Expr,
        from_postfix: bool,
    ) {
        // Long-element path. For both global (`long a[];`) and stack
        // (`long a[N];` as a local) array bases with a constant index,
        // a long array element behaves byte-identically to a long
        // struct field at the same effective address — same compound
        // skeletons, just a different disp16. Fixtures 392
        // (`a[1] += K`), 393 (`a[1] &= K`), 394 (`a[1] += y`).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some((const_off, leaf_ty)) =
                try_const_array_offset(g_ty, indices.iter())
            && leaf_ty.is_long_like()
        {
            let lo_addr = global_offset_addr(array, const_off as i32);
            let hi_addr = global_offset_addr(array, const_off as i32 + 2);
            self.emit_long_compound_to_mem(
                &lo_addr,
                &hi_addr,
                op,
                value,
                leaf_ty.is_unsigned(),
            );
            return;
        }
        // Long-pointer subscript compound: `long *p; p[K] += v`.
        // Load the pointer into BX once, then route through the
        // long-compound-to-mem helper with `[bx+off]` / `[bx+off+2]`
        // addresses. Same skeleton as the long-array path, just
        // BX-based instead of DGROUP-direct. Fixture 901.
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && pointee.is_long_like()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let lo_addr = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            let hi_off = off + 2;
            let hi_addr = if hi_off > 0 {
                format!("[bx+{hi_off}]")
            } else if hi_off < 0 {
                format!("[bx-{}]", -hi_off)
            } else {
                "[bx]".to_owned()
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            // Shift K=1 special-case: BCC reloads BX between the
            // register-arith and the store-back (BCC doesn't keep
            // BX alive across `shl/rcl`). The shared helper doesn't
            // know about the BX reload, so we inline this shape
            // here rather than routing through it. Fixture 904
            // (`p[1] <<= 1`).
            if matches!(op, BinOp::Shl | BinOp::Shr)
                && let Some(n) = try_const_eval(value)
                && n == 1
            {
                let unsigned = pointee.is_unsigned();
                let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                if matches!(op, BinOp::Shl) {
                    self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                    self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                } else {
                    let hi_op = if unsigned { "shr" } else { "sar" };
                    let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                    self.out.extend_from_slice(b"\trcr\tdx,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},dx\r\n");
                return;
            }
            self.emit_long_compound_to_mem(&lo_addr, &hi_addr, op, value, pointee.is_unsigned());
            return;
        }
        // Char/int global-array element with a constant index — same
        // shapes as the corresponding char-global / int-global compound
        // patterns, just with a `DGROUP:_<a>+<K>` address. Fixture 706
        // (`a[2] += 5` for `char a[4]` global → `mov al, _a+2; add al,
        // 5; mov _a+2, al`).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some((const_off, leaf_ty)) =
                try_const_array_offset(g_ty, indices.iter())
        {
            let dest = global_offset_addr(array, const_off as i32);
            let store_byte = leaf_ty.is_char_like();
            // Int-element compound with non-constant RHS — mirrors
            // the int-global compound add path (fixture 794): load
            // RHS into AX via emit_expr_to_ax (handles char/uchar
            // widening too), then memory-direct `<op> word ptr
            // <dest>, ax`. Fixture 833 (`a[1] += y`).
            if !store_byte
                && matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                )
                && try_const_eval(value).is_none()
                && matches!(leaf_ty, Type::Int | Type::UInt)
            {
                self.emit_expr_to_ax(value);
                let mnem = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\t{mnem}\tword ptr {dest},ax\r\n");
                return;
            }
            // Int-element compound `*=` / `/=` / `%=` with
            // non-constant local int RHS — `mov ax, <dest>;
            // imul/idiv word ptr [bp+N]; mov <dest>, ax|dx`.
            // Mirrors fixture 802. Fixture 836 (`a[1] *= y`).
            if !store_byte
                && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
                && try_const_eval(value).is_none()
                && matches!(leaf_ty, Type::Int | Type::UInt)
                && let ExprKind::Ident(b) = &value.kind
                && !self.globals.contains(b)
            {
                let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                    panic!("non-stack RHS in array compound Mul/Div not yet supported (no fixture)");
                };
                let _ = write!(self.out, "\tmov\tax,word ptr {dest}\r\n");
                if matches!(op, BinOp::Mul) {
                    let _ = write!(self.out, "\timul\tword ptr {}\r\n", bp_addr(off));
                } else {
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                    let _ = write!(self.out, "\tidiv\tword ptr {}\r\n", bp_addr(off));
                }
                let result_reg = if matches!(op, BinOp::Mul | BinOp::Div) { "ax" } else { "dx" };
                let _ = write!(self.out, "\tmov\tword ptr {dest},{result_reg}\r\n");
                return;
            }
            // Int-element compound `<<=` / `>>=` with non-constant
            // RHS — `mov cl, byte ptr <rhs>; shl/sar/shr word ptr
            // <dest>, cl`. Fixture 837 (`a[1] <<= y`).
            if !store_byte
                && matches!(op, BinOp::Shl | BinOp::Shr)
                && try_const_eval(value).is_none()
                && matches!(leaf_ty, Type::Int | Type::UInt)
                && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
            {
                let unsigned = leaf_ty.is_unsigned();
                let mnem = match (op, unsigned) {
                    (BinOp::Shl, _) => "shl",
                    (BinOp::Shr, false) => "sar",
                    (BinOp::Shr, true) => "shr",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\tmov\tcl,{rhs_byte}\r\n");
                let _ = write!(self.out, "\t{mnem}\tword ptr {dest},cl\r\n");
                return;
            }
            // Char-element compound with non-constant RHS. BCC
            // splits two ways by op family (same asymmetry as
            // char-global compound, batch 121/122):
            //  - `+=`/`-=`: AL-through (`mov al, <dest>; add al,
            //    <rhs>; mov <dest>, al`) — arith canonicalizes
            //    through the accumulator.
            //  - `&=`/`|=`/`^=`: memory-direct (`mov al, <rhs>;
            //    and byte ptr <dest>, al`).
            // Fixture 847 (arith), 850 (bitwise).
            if store_byte
                && matches!(op, BinOp::Add | BinOp::Sub)
                && try_const_eval(value).is_none()
                && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
            {
                let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
                let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
                let _ = write!(self.out, "\t{mnem}\tal,{rhs_byte}\r\n");
                let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
                return;
            }
            if store_byte
                && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
                && try_const_eval(value).is_none()
                && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
            {
                let mnem = match op {
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\tmov\tal,{rhs_byte}\r\n");
                let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest},al\r\n");
                return;
            }
            let Some(v) = try_const_eval(value) else {
                panic!("non-constant rhs in global-array compound assign not yet supported (no fixture)");
            };
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
            // Postfix `a[K]++` / `a[K]--` (discarded): memory-direct
            // `inc|dec byte ptr <dest>` (fixture 717).
            if store_byte
                && from_postfix
                && v_masked == 1
                && matches!(op, BinOp::Add | BinOp::Sub)
            {
                let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest}\r\n");
                return;
            }
            if store_byte && matches!(op, BinOp::Add | BinOp::Sub) {
                let imm8 = if matches!(op, BinOp::Add) {
                    (v_masked & 0xFF) as u8
                } else {
                    ((v_masked & 0xFF) as u8).wrapping_neg()
                };
                let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
                if v_masked == 1 && matches!(op, BinOp::Add) {
                    self.out.extend_from_slice(b"\tinc\tal\r\n");
                } else if v_masked == 1 && matches!(op, BinOp::Sub) {
                    self.out.extend_from_slice(b"\tdec\tal\r\n");
                } else {
                    let _ = write!(self.out, "\tadd\tal,{imm8}\r\n");
                }
                let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
                return;
            }
            let mnemonic = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => panic!("compound op `{op:?}` on global-array element not yet supported (no fixture)"),
            };
            let width = if store_byte { "byte" } else { "word" };
            let _ = write!(self.out, "\t{mnemonic}\t{width} ptr {dest},{v_masked}\r\n");
            return;
        }
        // Global-pointer subscript compound: `p[K] op= …` for `int *p`
        // at file scope. BCC's shape: load the pointer into BX
        // (`mov bx, word ptr DGROUP:_<p>`), then memory-direct
        // `<op> word ptr [bx+offset], <rhs>`. Offset = K *
        // pointee_stride. Fixture 862 (`p[1] += y` — non-const RHS),
        // 864 (`p[1] += K` — const RHS, imm8sx form).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let bx_disp = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            if let Some(v) = try_const_eval(value) {
                let v_masked = v & 0xFFFF;
                // Same `++a[K]` / `--a[K]` memory-direct peephole
                // the array path uses (fixture 547): const RHS of
                // 1 for Add/Sub becomes `inc|dec word ptr [bx+K]`
                // (2-3 bytes vs. 4 bytes for the imm8sx form).
                // Fixture 880 (`p[1]++` discarded).
                if v_masked == 1 && matches!(op, BinOp::Add | BinOp::Sub) {
                    let m = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                    let _ = write!(self.out, "\t{m}\tword ptr {bx_disp}\r\n");
                } else {
                    let _ = write!(
                        self.out,
                        "\t{mnem}\tword ptr {bx_disp},{v_masked}\r\n",
                    );
                }
            } else {
                self.emit_expr_to_ax(value);
                let _ = write!(self.out, "\t{mnem}\tword ptr {bx_disp},ax\r\n");
            }
            return;
        }
        // Int-pointer subscript shift compound with const RHS:
        // `int *p; p[K] <<= N`. BCC unrolls into N repetitions of
        // `<shift> word ptr [bx+K*2], 1` — same shape as the flat
        // int-global shift path (fixture 539), just with BX-based
        // addressing. Fixture 878 (`p[1] <<= 3`).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(n) = try_const_eval(value)
            && n >= 1
            && n <= 8
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let bx_disp = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            let signed = !pointee.is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            for _ in 0..n {
                let _ = write!(self.out, "\t{mnem}\tword ptr {bx_disp},1\r\n");
            }
            return;
        }
        // Int-pointer subscript shift compound with variable RHS:
        // `int *p; p[K] <<= y`. BCC loads the shift count into CL
        // via the byte-RHS path, then `<shift> word ptr [bx+K*2],
        // cl`. Mirrors the int-global variable shift path (batch
        // 175 / fixture 802 family). Fixture 882.
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && try_const_eval(value).is_none()
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let bx_disp = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            let signed = !pointee.is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tcl,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tword ptr {bx_disp},cl\r\n");
            return;
        }
        // Int-pointer subscript Mul/Div/Mod compound:
        // `int *p; p[K] *= y` (or `/=`, `%=`). BCC loads the LHS
        // through BX into AX, then `imul`/`idiv` against the
        // variable RHS, then stores back. Fixture 883
        // (`p[1] *= y`).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let bx_disp = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            let LocalLocation::Stack(boff) = self.locals.location_of(b) else {
                panic!("non-stack RHS in pointer-subscript Mul/Div not yet supported (no fixture)");
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr {}\r\n", bp_addr(boff));
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tidiv\tword ptr {}\r\n", bp_addr(boff));
                // BCC reloads BX after `idiv` (Div and Mod) before
                // the store-back — `idiv` clobbers more state than
                // `imul`, so BCC doesn't keep BX alive across it.
                // Fixture 885 (div), 884 (mod).
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            }
            let result_reg = if matches!(op, BinOp::Mul | BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(self.out, "\tmov\tword ptr {bx_disp},{result_reg}\r\n");
            return;
        }
        // Char-pointee global-pointer subscript compound: `char *p;
        // p[K] += y`. BCC uses the AL-arith-through pattern plus a
        // second `mov bx, _p` reload before the store (BCC doesn't
        // keep BX alive across the byte arith). Fixtures 865, 869
        // (var RHS), 877 (const RHS via imm8), 886 (K=1 peephole).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && pointee.is_char_like()
            && matches!(op, BinOp::Add | BinOp::Sub)
            && let Some(rhs_text) = try_const_eval(value)
                .map(|v| (v & 0xFF).to_string())
                .or_else(|| self.rhs_byte_addr(&value.kind))
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let bx_disp = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            // K=1 memory-direct peephole: `inc|dec byte ptr [bx+
            // K]` (3 bytes vs. 11 for the AL-through pattern).
            // BCC applies the same shape as char-global / char-
            // array postinc (fixtures 717, 721). Fixture 886.
            if let Some(v) = try_const_eval(value)
                && (v & 0xFF) == 1
            {
                let m = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
                let _ = write!(self.out, "\t{m}\tbyte ptr {bx_disp}\r\n");
                return;
            }
            let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
            let _ = write!(self.out, "\t{mnem}\tal,{rhs_text}\r\n");
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tbyte ptr {bx_disp},al\r\n");
            return;
        }
        // Char-pointee global-pointer subscript bitwise compound:
        // `char *p; p[K] &= y` (and `|=`/`^=`). BCC uses the same
        // mem-direct shape as char-global / char-array bitwise:
        // `mov al, <rhs>; <op> byte ptr [bx+K], al` — no BX reload,
        // no AL pre-load. Fixtures 870, 871 (and pending XOR).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && pointee.is_char_like()
            && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && try_const_eval(value).is_none()
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let bx_disp = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            let mnem = match op {
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tal,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {bx_disp},al\r\n");
            return;
        }
        // Register-resident local-pointer subscript compound:
        // `int *p; p[K] op= …` for a stack-local pointer held in a
        // register (BCC's typical SI/DI placement). BCC's shape:
        // `<op> word ptr [<reg>+K*stride], ax` after the RHS lands
        // in AX. Same offset computation as the global-pointer path,
        // just with register addressing. Fixture 863.
        if self.locals.has(array)
            && let Some(pointee) = self.locals.type_of(array).pointee()
            && let LocalLocation::Reg(reg) = self.locals.location_of(array)
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && try_const_eval(value).is_none()
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let reg_name = reg.name();
            let disp = if off == 0 {
                format!("[{reg_name}]")
            } else if off > 0 {
                format!("[{reg_name}+{off}]")
            } else {
                format!("[{reg_name}-{}]", -off)
            };
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tword ptr {disp},ax\r\n");
            return;
        }
        let array_ty = self.locals.type_of(array).clone();
        let LocalLocation::Stack(base_off) = self.locals.location_of(array) else {
            panic!("array `{array}` should be stack-resident");
        };
        let Some((const_off, leaf_ty)) =
            try_const_array_offset(&array_ty, indices.iter())
        else {
            panic!("variable-indexed array compound assign not yet supported (no fixture)");
        };
        let off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
        let store_byte = leaf_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        let Some(v) = try_const_eval(value) else {
            // Non-constant RHS for an int element compound. Load RHS
            // into AX, then `<op> word ptr [bp+elem_off], ax`. Same
            // shape as the global-pointer-subscript compound (sibling
            // path above). Fixture 988 (`a[1] -= x`).
            if !store_byte
                && matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                )
            {
                self.emit_expr_to_ax(value);
                let mnem = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let _ = write!(
                    self.out,
                    "\t{mnem}\tword ptr {},ax\r\n",
                    bp_addr(off),
                );
                return;
            }
            // Char-typed dest + char-typed lvalue RHS: load RHS to
            // AL then `<op> byte ptr [bp+dst], al`. Mirrors the int
            // path above. Fixture 1447 (`a[0] ^= a[1];`).
            if store_byte
                && matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                )
                && let Some((r_name, r_off, r_ty)) = self.try_lvalue_chain_addr(value)
                && r_ty.is_char_like()
                && let Some(r_addr) = self.resolve_chain_addr(&r_name, r_off)
            {
                let _ = write!(self.out, "\tmov\tal,byte ptr {r_addr}\r\n");
                let mnem = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\t{mnem}\tbyte ptr {},al\r\n", bp_addr(off));
                return;
            }
            panic!("non-constant rhs in array compound assign not yet supported (no fixture)");
        };
        let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
        let dest = bp_addr(off);
        // Postfix `a[K]++` / `a[K]--` (discarded) on char-array: BCC
        // uses memory-direct `inc|dec byte ptr [bp-N]`. Sibling of
        // the global-array case (fixture 717). Int arrays use the
        // same shape (fixture 547) since `inc word ptr` already
        // matches BCC's prefix behavior.
        if store_byte
            && from_postfix
            && v_masked == 1
            && matches!(op, BinOp::Add | BinOp::Sub)
        {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest}\r\n");
            return;
        }
        // Int-element `++a[K]` / prefix-K=1 reuse the memory-direct
        // `inc word ptr` shape (fixture 547: `++a[1]` on int array).
        if !store_byte && v_masked == 1 && matches!(op, BinOp::Add | BinOp::Sub) {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\t{width} ptr {dest}\r\n");
            return;
        }
        // Char-element arith (`a[K] += C`) — AL load-modify-store
        // through `bp_addr(off)`, same shape as char-global compound
        // (fixture 719). The K=1 peephole picks `inc al` over
        // `add al, 1`.
        if store_byte && matches!(op, BinOp::Add | BinOp::Sub) {
            let imm8 = if matches!(op, BinOp::Add) {
                (v_masked & 0xFF) as u8
            } else {
                ((v_masked & 0xFF) as u8).wrapping_neg()
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
            if v_masked == 1 && matches!(op, BinOp::Add) {
                self.out.extend_from_slice(b"\tinc\tal\r\n");
            } else if v_masked == 1 && matches!(op, BinOp::Sub) {
                self.out.extend_from_slice(b"\tdec\tal\r\n");
            } else {
                let _ = write!(self.out, "\tadd\tal,{imm8}\r\n");
            }
            let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
            return;
        }
        let mnemonic = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::BitAnd => "and",
            BinOp::BitOr => "or",
            BinOp::BitXor => "xor",
            _ => panic!("compound op `{op:?}` on array element not yet supported (no fixture)"),
        };
        let _ = write!(
            self.out,
            "\t{mnemonic}\t{width} ptr {dest},{v_masked}\r\n",
        );
    }


    /// `<base>.<field>` or `<base>-><field>` in rvalue position.
    /// Computes the field's effective address and loads from there
    /// with the appropriate width.
    ///
    /// - **Dot** (`a.x` — fixture 101 etc.): base must be an `Ident`
    ///   referring to a struct stack local. Field at offset `K` lives
    ///   at `[bp - struct_base + K]` which simplifies to a single
    ///   `[bp-N]` load.
    /// - **Arrow** (`p->x` — fixture 105, 106): base must be an
    ///   `Ident` for a pointer in a register. Field at offset `K`
    ///   lives at `[reg + K]`; `K = 0` collapses to `[reg]`.
    fn emit_member_to_ax(
        &mut self,
        base: &Expr,
        field: &str,
        kind: crate::ast::MemberKind,
    ) {
        // Dot: try the lvalue-chain helper so `a.x`, `pts[1].x`, and
        // nested `a.b.c` all fold to a single load. Works for both
        // stack locals (`[bp-N]`) and file-scope globals
        // (`DGROUP:_<name>+K`, fixture 190).
        if matches!(kind, crate::ast::MemberKind::Dot) {
            if let Some((name, total_off, leaf_ty)) =
                self.try_member_dot_chain(base, field)
            {
                if self.globals.contains(&name) {
                    let load_byte = leaf_ty.is_char_like();
                    let width = if load_byte { "byte" } else { "word" };
                    let addr = if total_off == 0 {
                        format!("DGROUP:_{name}")
                    } else {
                        format!("DGROUP:_{name}+{total_off}")
                    };
                    if load_byte {
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        self.emit_widen_al(&leaf_ty);
                    } else {
                        let _ = write!(self.out, "\tmov\tax,{width} ptr {addr}\r\n");
                    }
                    return;
                }
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) else {
                    panic!("struct local `{name}` not stack-resident (unexpected)");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                if leaf_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                    self.emit_widen_al(&leaf_ty);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                }
                return;
            }
        }
        // `(<dot-chain>)-><field>` — base is a Dot-chain whose leaf
        // is a pointer-to-struct. Load that pointer into BX, then
        // read through `[bx+field_off]`. Fixture 1419 (`a.next->v`
        // with a global struct having a struct-pointer field).
        //
        // The pointed-to struct's fields aren't carried in the AST
        // type (Pointer holds a name-only placeholder), so we look up
        // the full struct definition via `lookup_struct_by_tag`.
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::Member { base: inner_base, field: inner_field, kind: crate::ast::MemberKind::Dot } = &base.kind
            && let Some((root_name, total_off, leaf_ty)) = self.try_member_dot_chain(inner_base, inner_field)
            && let Some(pointee) = leaf_ty.pointee()
            && let Type::Struct { name: Some(tag), .. } = pointee
            && let Some(full_ty) = self.lookup_struct_by_tag(tag)
            && let Some((field_off, field_ty)) = full_ty.field(field)
        {
            let load_addr = if self.globals.contains(&root_name) {
                if total_off == 0 {
                    format!("DGROUP:_{root_name}")
                } else {
                    format!("DGROUP:_{root_name}+{total_off}")
                }
            } else {
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&root_name) else {
                    panic!("struct local `{root_name}` not stack-resident");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                bp_addr(off)
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr {load_addr}\r\n");
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            if field_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // `make().a` — Member access on a function-call result. For a
        // struct that fits in 4 bytes, the callee returns it in DX:AX
        // (AX = low half = first field, DX = high half = second
        // field). After the call, the requested field is already in
        // AX or DX. Fixtures 2629, 2634.
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::Call { name: fname, args } = &base.kind
            && args.is_empty()
            && let Some(ret_ty) = self.signatures.ret_ty_of(fname)
            && let Type::Struct { fields, size, .. } = ret_ty
            && *size <= 4
            && let Some(field_info) = fields.iter().find(|f| f.name == field)
        {
            let _ = write!(self.out, "\tcall\tnear ptr _{fname}\r\n");
            // 2B struct: field at offset 0 already in AX. 4B struct:
            // offset 0 in AX, offset 2 in DX.
            if field_info.offset == 2 {
                self.out.extend_from_slice(b"\tmov\tax,dx\r\n");
            }
            return;
        }
        // `<reg_ptr>-><field>-><inner>` — chained arrow access. Load
        // the base pointer's field (another pointer) into BX, then
        // read the inner field through BX. Two-step indirection;
        // works whether the base is a stack-local pointer or a
        // function parameter. Fixture 2816 (`o->p->v`).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::Member { base: inner_base, field: inner_field, kind: crate::ast::MemberKind::Arrow } = &base.kind
            && let ExprKind::Ident(root_name) = &inner_base.kind
            && (self.locals.has(root_name) || self.globals.contains(root_name))
        {
            let root_ty = if self.locals.has(root_name) {
                self.locals.type_of(root_name).clone()
            } else {
                self.globals.type_of(root_name).unwrap().clone()
            };
            if let Some(pointee_struct) = root_ty.pointee()
                && let Some((mid_off, mid_ty)) = (if let Type::Struct { name: Some(tag), .. } = pointee_struct {
                    self.lookup_struct_by_tag(tag).and_then(|t| t.field(inner_field))
                } else {
                    pointee_struct.field(inner_field)
                })
                && let Some(final_pointee) = mid_ty.pointee()
                && let Some((field_off, field_ty)) = (if let Type::Struct { name: Some(tag), .. } = final_pointee {
                    self.lookup_struct_by_tag(tag).and_then(|t| t.field(field))
                } else {
                    final_pointee.field(field)
                })
            {
                // First indirection: load the intermediate pointer
                // (the field at mid_off through root). For register-
                // resident root pointers BCC uses the root's reg
                // directly as the base (e.g. `mov bx, [si]` when o is
                // in SI), skipping a `mov bx, si` copy.
                if self.locals.has(root_name)
                    && let LocalLocation::Reg(reg) = self.locals.location_of(root_name)
                {
                    let bx_src = if mid_off == 0 {
                        format!("[{}]", reg.name())
                    } else {
                        format!("[{}+{mid_off}]", reg.name())
                    };
                    let _ = write!(self.out, "\tmov\tbx,word ptr {bx_src}\r\n");
                } else {
                    // Stack-resident root, or global pointer. Load
                    // root into BX first, then perform the +mid_off
                    // indirection.
                    if self.locals.has(root_name)
                        && let LocalLocation::Stack(off) = self.locals.location_of(root_name)
                    {
                        let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                    } else {
                        let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{root_name}\r\n");
                    }
                    let bx1 = if mid_off == 0 { "[bx]".to_owned() } else { format!("[bx+{mid_off}]") };
                    let _ = write!(self.out, "\tmov\tbx,word ptr {bx1}\r\n");
                }
                // Final read at [bx+field_off].
                let bx2 = if field_off == 0 { "[bx]".to_owned() } else { format!("[bx+{field_off}]") };
                if field_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {bx2}\r\n");
                    self.emit_widen_al(&field_ty);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {bx2}\r\n");
                }
                return;
            }
        }
        // `<global_struct_array>[<var>].<field>` — Dot access on a
        // variable-indexed global struct array. Compute the scaled
        // element offset into BX, then load through `[bx +
        // <arr_sym> + field_off]`. Fixture 2841.
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::ArrayIndex { array: arr_expr, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &arr_expr.kind
            && let Some(arr_ty) = self.globals.type_of(arr_name)
            && let Some(elem_ty) = arr_ty.array_elem()
            && let Type::Struct { fields, .. } = elem_ty.clone()
            && let Some(field_info) = fields.iter().find(|f| f.name == field)
            && try_const_eval(index).is_none()
        {
            let field_off = field_info.offset;
            let field_ty = field_info.ty.clone();
            self.emit_index_into_bx(index, elem_ty);
            let addr = if field_off == 0 {
                format!("DGROUP:_{arr_name}[bx]")
            } else {
                format!("DGROUP:_{arr_name}+{field_off}[bx]")
            };
            if field_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
            return;
        }
        // `(*<ptr_to_ptr>)-><field>` rvalue: deref the outer ptr
        // through BX, then load the field through that BX. Fixture
        // 2815 (`int extract(struct P **pp) { return (*pp)->x; }`).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::Deref(inner) = &base.kind
            && let ExprKind::Ident(pp_name) = &inner.kind
            && self.locals.has(pp_name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(pp_name)
            && let Some(p_ty) = self.locals.type_of(pp_name).pointee()
            && let Some(struct_ty) = p_ty.pointee()
            && let Some((field_off, field_ty)) = struct_ty.field(field)
        {
            let _ = write!(self.out, "\tmov\tbx,word ptr [{}]\r\n", reg.name());
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            if field_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // `(*<ptr>).<field>` — semantically identical to
        // `<ptr>-><field>`. Rewrite by unwrapping the Deref so the
        // Ident arms below pick it up. Fixture 2960
        // (`int extract(struct P *p) { return (*p).x; }`).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::Deref(inner) = &base.kind
            && matches!(inner.kind, ExprKind::Ident(_))
        {
            return self.emit_member_to_ax(
                inner,
                field,
                crate::ast::MemberKind::Arrow,
            );
        }
        // Arrow path (or Dot whose base isn't a const-chain lvalue):
        // base must be a bare Ident referring to a pointer.
        let ExprKind::Ident(name) = &base.kind else {
            panic!("non-ident base in member access not yet supported (no fixture)");
        };
        // `<global_ptr>-><field>` rvalue: load the pointer into BX,
        // then read through `[bx+field_off]`. Fixture 1429.
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let Some(gty) = self.globals.type_of(name)
            && let Some(pointee) = gty.pointee()
            && let Some((field_off, field_ty)) = pointee.field(field)
        {
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{name}\r\n");
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            if field_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        let base_ty = self.locals.type_of(name).clone();
        let (field_off, field_ty) = match kind {
            crate::ast::MemberKind::Dot => base_ty.field(field).unwrap_or_else(|| {
                panic!("`{name}.{field}`: no such field in {base_ty:?}")
            }),
            crate::ast::MemberKind::Arrow => {
                let pointee = base_ty
                    .pointee()
                    .unwrap_or_else(|| panic!("`{name}->{field}`: not a pointer type"))
                    .clone();
                pointee.field(field).unwrap_or_else(|| {
                    panic!("`{name}->{field}`: no such field in {pointee:?}")
                })
            }
        };
        let load_byte = field_ty.is_char_like();
        if matches!(kind, crate::ast::MemberKind::Arrow) {
            // `p->x`: p holds the address; field at `[reg + K]`.
            let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
                panic!("stack-resident pointer in `p->x` not yet supported (no fixture)");
            };
            let addr = if field_off == 0 {
                format!("[{}]", reg.name())
            } else {
                format!("[{}+{field_off}]", reg.name())
            };
            if load_byte {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
        } else {
            // Dot with an unsupported base shape — the chain helper
            // already failed; surface a clear error.
            panic!("non-ident base in `.x` access not yet supported (no fixture for {:?})", base.kind);
        }
    }

    /// `<base>.<field> = <value>;` or `<base>-><field> = <value>;`.
    /// Mirror of `emit_member_to_ax` for the lvalue path.
    fn emit_member_assign(
        &mut self,
        base: &Expr,
        field: &str,
        kind: crate::ast::MemberKind,
        value: &Expr,
    ) {
        // Dot path: try the lvalue-chain helper. Catches `a.x`,
        // `pts[1].x`, nested `a.b.c`, and global `g.x`.
        let (dest, leaf_ty) = if matches!(kind, crate::ast::MemberKind::Dot)
            && let Some((name, total_off, leaf_ty)) = self.try_member_dot_chain(base, field)
        {
            let dest = if self.globals.contains(&name) {
                if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                }
            } else {
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) else {
                    panic!("struct local `{name}` not stack-resident (unexpected)");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                bp_addr(off)
            };
            (dest, leaf_ty)
        } else {
            // Arrow (or a Dot whose base isn't a const-chain lvalue).
            let ExprKind::Ident(name) = &base.kind else {
                panic!("non-ident base in member assign not yet supported (no fixture)");
            };
            // `<global_ptr>-><field> = …`: load the global pointer
            // into BX, then write through `[bx+field_off]`. Fixture
            // 1429.
            if matches!(kind, crate::ast::MemberKind::Arrow)
                && let Some(gty) = self.globals.type_of(name)
                && let Some(pointee) = gty.pointee()
                && let Some((field_off, field_ty)) = pointee.field(field)
            {
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{name}\r\n");
                let bx_disp = if field_off == 0 {
                    "[bx]".to_owned()
                } else {
                    format!("[bx+{field_off}]")
                };
                if let Some(v) = try_const_eval(value) {
                    if field_ty.is_char_like() {
                        let v8 = v & 0xFF;
                        let _ = write!(self.out, "\tmov\tbyte ptr {bx_disp},{v8}\r\n");
                    } else {
                        let v16 = v & 0xFFFF;
                        let _ = write!(self.out, "\tmov\tword ptr {bx_disp},{v16}\r\n");
                    }
                    return;
                }
                self.emit_expr_to_ax(value);
                if field_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tbyte ptr {bx_disp},al\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tword ptr {bx_disp},ax\r\n");
                }
                return;
            }
            let base_ty = self.locals.type_of(name).clone();
            let (field_off, field_ty) = match kind {
                crate::ast::MemberKind::Dot => base_ty.field(field).unwrap_or_else(|| {
                    panic!("`{name}.{field} = …`: no such field in {base_ty:?}")
                }),
                crate::ast::MemberKind::Arrow => {
                    let pointee = base_ty
                        .pointee()
                        .unwrap_or_else(|| panic!("`{name}->{field} = …`: not a pointer"))
                        .clone();
                    pointee.field(field).unwrap_or_else(|| {
                        panic!("`{name}->{field} = …`: no such field in {pointee:?}")
                    })
                }
            };
            let dest = match kind {
                crate::ast::MemberKind::Dot => {
                    let LocalLocation::Stack(struct_off) = self.locals.location_of(name) else {
                        panic!("struct local `{name}` not stack-resident (unexpected)");
                    };
                    let off = struct_off + i16::try_from(field_off).unwrap_or(i16::MAX);
                    bp_addr(off)
                }
                crate::ast::MemberKind::Arrow => {
                    let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
                        panic!(
                            "stack-resident pointer in `p->x = …` not yet supported (no fixture)"
                        );
                    };
                    if field_off == 0 {
                        format!("[{}]", reg.name())
                    } else {
                        format!("[{}+{field_off}]", reg.name())
                    }
                }
            };
            (dest, field_ty)
        };
        // Long-field store: emit two `mov word ptr <addr>, <half>`
        // instructions (high first, then low). Works for both `s.x`
        // (DGROUP-relative or bp-relative dest) and `p->x` (register-
        // indirect dest). Fixtures 316, 317, 318.
        if leaf_ty.is_long_like() {
            if let Some(v) = try_const_eval(value) {
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let hi_dest = shift_dest_by_two(&dest);
                let _ = write!(self.out, "\tmov\tword ptr {hi_dest},{hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest},{lo}\r\n");
                return;
            }
            // Non-constant RHS (e.g. `s.x = g + h`): route through
            // the long-value-to-dest helper. Fixture 358.
            let hi_dest = shift_dest_by_two(&dest);
            if self.try_emit_long_value_to_dest(value, &hi_dest, &dest) {
                return;
            }
            panic!("non-constant rhs in long struct field assign not yet supported (no fixture)");
        }
        let store_byte = leaf_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        if let Some(v) = try_const_eval(value) {
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr {dest},{v_masked}\r\n");
            return;
        }
        // RHS is `&<global>` — emit the direct immediate-store
        // form `mov word ptr <dest>,offset DGROUP:_<src>` (uses
        // the same two-FIXUPP encoding `MovGroupSymOffsetGroupSym`).
        // Fixture 494 (`head.next = &head`).
        if !store_byte
            && let ExprKind::AddressOf(src) = &value.kind
            && self.globals.contains(src)
        {
            let _ = write!(
                self.out,
                "\tmov\t{width} ptr {dest},offset DGROUP:_{src}\r\n",
            );
            return;
        }
        // Non-constant RHS for an int field: materialize into AX,
        // then store AX to the field. Fixture 990 (`s.x = v;` with
        // v a stack local).
        if !store_byte {
            self.emit_expr_to_ax(value);
            let _ = write!(self.out, "\tmov\tword ptr {dest},ax\r\n");
            return;
        }
        panic!("non-constant rhs in struct field assign not yet supported (no fixture)");
    }

    /// `<base>.<field> <op>= <value>;` — compound assignment through a
    /// struct member. Computes the same `<dest>` operand as
    /// `emit_member_assign`, then emits the matching arithmetic
    /// instruction directly to memory (fixture 182's `p->x += 5`
    /// becomes `add word ptr [si], 5`). Only constant RHS values are
    /// fixture-supported today.
    /// Emit `<dest> op= <value>` where `<dest>` is a long memory
    /// location whose halves' assembly addresses are `lo_addr` and
    /// `hi_addr`. The skeleton matches the long-global compound path
    /// (fixtures 251/253/339) and is destination-storage-agnostic —
    /// works for globals, struct fields, and array elements once the
    /// caller has computed the right disp16 expressions. The
    /// `dest_unsigned` flag only matters for `>>=` (chooses `sar` vs
    /// `shr` for the high half / picks the signed-vs-unsigned shift
    /// helper for K>1).
    fn emit_long_compound_to_mem(
        &mut self,
        lo_addr: &str,
        hi_addr: &str,
        op: BinOp,
        value: &Expr,
        dest_unsigned: bool,
    ) {
        // Shift compound: two shapes by K. K=1 inline uses memory-
        // dest register convention (AX=high, DX=low) — the loaded
        // pair matches the trailing store. K>1 routes through the
        // helper and so loads with the helper ABI (DX=high, AX=low);
        // the trailing store adapts. `mov cl, K` lands FIRST in the
        // compound-form reorder. Mirrors the long-global compound-
        // shift path (fixtures 263–266) and the long-stack-local
        // compound-shift path (fixtures 383–385). Fixtures 395
        // (struct field, K=1 `<<=`), 396 (array elem, K=1), 397
        // (struct field, K=2 helper).
        if matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && (1..=255).contains(&k)
        {
            if k == 1 {
                let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                if matches!(op, BinOp::Shl) {
                    self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                    self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                } else {
                    let hi_op = if dest_unsigned { "shr" } else { "sar" };
                    let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                    self.out.extend_from_slice(b"\trcr\tdx,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},dx\r\n");
            } else {
                let helper = match (op, dest_unsigned) {
                    (BinOp::Shl, _)     => "N_LXLSH@",
                    (BinOp::Shr, false) => "N_LXRSH@",
                    (BinOp::Shr, true)  => "N_LXURSH@",
                    _ => unreachable!(),
                };
                let k_u8 = (k & 0xFF) as u8;
                let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},ax\r\n");
            }
            return;
        }
        // Helper-call compound (`*=`, `/=`, `%=`) with a long-lvalue
        // RHS. Mul loads RHS → CX:BX, LHS → DX:AX (compound-form
        // operand-to-slot swap — see batch 23 fingerprint), calls
        // N_LXMUL@, stores DX:AX back. Div/mod push the four words
        // right-to-left in their standard helper order (divisor
        // first in time, dividend at lower addresses on the helper
        // stack), call the unsigned/signed helper, and store the
        // result. Fixtures 407 (struct mul), 408 (array mul), 409
        // (struct div).
        if matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let Some((y_hi, y_lo)) = self.long_lvalue_addr_pair(value)
        {
            match op {
                BinOp::Mul => {
                    let _ = write!(self.out, "\tmov\tcx,word ptr {y_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tbx,word ptr {y_lo}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                    self.helpers.insert("N_LXMUL@".to_string());
                }
                BinOp::Div | BinOp::Mod => {
                    let helper = match (op, dest_unsigned) {
                        (BinOp::Div, false) => "N_LDIV@",
                        (BinOp::Mod, false) => "N_LMOD@",
                        (BinOp::Div, true)  => "N_LUDIV@",
                        (BinOp::Mod, true)  => "N_LUMOD@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tpush\tword ptr {y_hi}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {y_lo}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {hi_addr}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {lo_addr}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                }
                _ => unreachable!(),
            }
            let _ = write!(self.out, "\tmov\tword ptr {hi_addr},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lo_addr},ax\r\n");
            return;
        }
        // Const RHS: `op [lo], k_lo / op|carry [hi], k_hi_or_0`.
        // Arith uses `83 /n` imm8sx (low half must fit i8sx; high
        // is `adc/sbb 0`). Bitwise uses `81 /n` imm16 (op-family-
        // dependent encoding choice).
        if let Some(k) = try_const_eval(value) {
            let k_lo = (k & 0xFFFF) as u16;
            let k_hi = ((k >> 16) & 0xFFFF) as u16;
            match op {
                BinOp::Add | BinOp::Sub => {
                    let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                        ("add", "adc")
                    } else {
                        ("sub", "sbb")
                    };
                    let lo_signed = k_lo as i16;
                    if let Ok(lo_i8) = i8::try_from(lo_signed) {
                        let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},{lo_i8}\r\n");
                    } else {
                        let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},{k_lo}\r\n");
                    }
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {hi_addr},0\r\n");
                    return;
                }
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let mnem = match op {
                        BinOp::BitAnd => "and",
                        BinOp::BitOr  => "or",
                        BinOp::BitXor => "xor",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\t{mnem}\tword ptr {lo_addr},{k_lo}\r\n");
                    let _ = write!(self.out, "\t{mnem}\tword ptr {hi_addr},{k_hi}\r\n");
                    return;
                }
                _ => {}
            }
        }
        // Variable RHS: load y into AX:DX (memory-dest conv), then
        // memory-direct `<op> [lo], dx / <op|carry> [hi], ax`. Mirror
        // of fixture 339 for any memory destination.
        if let Some((y_hi, y_lo)) = self.long_lvalue_addr_pair(value)
            && let Some((lo_op, hi_op)) = long_pair_op(op)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {y_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {y_lo}\r\n");
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},dx\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {hi_addr},ax\r\n");
            return;
        }
        // Int RHS widening into long memory destination. emit_expr_to_ax
        // loads y into AX (with cbw for char), then cwd extends to
        // DX:AX, then memory-direct `add/adc` (or sub/sbb, or paired
        // bitwise). Mirrors fixture 755 (`long_global += int x`) but
        // for an arbitrary memory destination (struct field, array
        // element). Fixture 845.
        if let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::Char)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            let (lo_op, hi_op) = match op {
                BinOp::Add => ("add", "adc"),
                BinOp::Sub => ("sub", "sbb"),
                BinOp::BitAnd => ("and", "and"),
                BinOp::BitOr => ("or", "or"),
                BinOp::BitXor => ("xor", "xor"),
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},ax\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {hi_addr},dx\r\n");
            return;
        }
        // UInt/UChar RHS widening (zero-extend) into long memory dest.
        // Mirrors fixture 767 (`ulong_global += uint x`).
        if let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::UInt | Type::UChar)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            self.emit_expr_to_ax(value);
            let (lo_op, hi_op) = match op {
                BinOp::Add => ("add", "adc"),
                BinOp::Sub => ("sub", "sbb"),
                BinOp::BitAnd => ("and", "and"),
                BinOp::BitOr => ("or", "or"),
                BinOp::BitXor => ("xor", "xor"),
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},ax\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {hi_addr},0\r\n");
            return;
        }
        panic!("long compound `{op:?}=` to memory not yet supported for this RHS shape (no fixture)");
    }

    /// `<base>.<field>[<i>] = <value>;` — write to an array element inside a
    /// struct field. With all-constant indices we fold the field offset and
    /// each index into a single byte displacement off the struct base, then
    /// emit one `mov <width> ptr <dest>, <imm>`. Fixture 497.
    fn emit_member_array_assign(
        &mut self,
        base: &str,
        field: &str,
        indices: &[Expr],
        value: &Expr,
    ) {
        let base_ty = if self.globals.contains(base) {
            self.globals.type_of(base).unwrap().clone()
        } else {
            self.locals.type_of(base).clone()
        };
        let (field_off, field_ty) = base_ty
            .field(field)
            .unwrap_or_else(|| panic!("`{base}.{field}[…]`: no such field in {base_ty:?}"));
        // Walk through array dimensions matching the index count.
        let mut elem_ty = field_ty;
        let mut total_off = u32::from(field_off);
        for ix in indices {
            let Type::Array { elem, .. } = elem_ty else {
                panic!("`{base}.{field}` indexed but not array");
            };
            let stride = u32::from(elem.size_bytes());
            let k = try_const_eval(ix)
                .unwrap_or_else(|| panic!("variable struct-field array index not supported"));
            total_off = total_off.checked_add((k as u32).wrapping_mul(stride)).unwrap();
            elem_ty = *elem;
        }
        let dest = if self.globals.contains(base) {
            if total_off == 0 {
                format!("DGROUP:_{base}")
            } else {
                format!("DGROUP:_{base}+{total_off}")
            }
        } else {
            let LocalLocation::Stack(struct_off) = self.locals.location_of(base) else {
                panic!("struct local `{base}` not stack-resident");
            };
            let off = struct_off + i16::try_from(total_off).unwrap_or(i16::MAX);
            bp_addr(off)
        };
        let store_byte = elem_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        if let Some(v) = try_const_eval(value) {
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr {dest},{v_masked}\r\n");
            return;
        }
        panic!("non-constant rhs in struct-field array assign not yet supported (no fixture)");
    }

    fn emit_member_compound_assign(
        &mut self,
        base: &Expr,
        field: &str,
        kind: crate::ast::MemberKind,
        op: BinOp,
        value: &Expr,
        from_postfix: bool,
    ) {
        // Long-field path. Resolve the dot/arrow chain to a (lo_addr,
        // hi_addr) pair (struct field at its in-struct offset), then
        // emit the long-compound shape — same skeleton as the long-
        // global compound (fixtures 251/253/339) but with the field's
        // formatted address. Fixtures 389 (`s.x += K`), 390
        // (`s.x &= K` — bitwise uses imm16 even when K fits i8sx),
        // 391 (`s.x += y` — variable RHS).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let Some((src, total_off, leaf_ty)) = self.try_member_dot_chain(base, field)
            && leaf_ty.is_long_like()
        {
            let (lo_addr, hi_addr) = if self.globals.contains(&src) {
                (
                    global_offset_addr(&src, total_off),
                    global_offset_addr(&src, total_off + 2),
                )
            } else {
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&src) else {
                    panic!("struct local `{src}` not stack-resident");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                (bp_addr(off), bp_addr(off + 2))
            };
            self.emit_long_compound_to_mem(
                &lo_addr,
                &hi_addr,
                op,
                value,
                leaf_ty.is_unsigned(),
            );
            return;
        }
        // Arrow access (`p->x op= …`) where `p` is a register-resident
        // pointer to a struct and `x` is a long field. The address
        // pair is `[reg+off]` / `[reg+off+2]` — same skeleton as the
        // other long-compound-to-memory destinations, just with
        // register-base addressing. Fixture 399 (`p->x += K` for the
        // first field, offset 0 → `[si]`/`[si+2]`).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::Ident(p_name) = &base.kind
            && self.locals.has(p_name)
            && let Some(pointee) = self.locals.type_of(p_name).pointee()
            && let Some((field_off, field_ty)) = pointee.clone().field(field)
            && field_ty.is_long_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
        {
            let r = reg.name();
            let (lo_addr, hi_addr) = if field_off == 0 {
                (format!("[{r}]"), format!("[{r}+2]"))
            } else {
                (
                    format!("[{r}+{field_off}]"),
                    format!("[{r}+{}]", field_off + 2),
                )
            };
            self.emit_long_compound_to_mem(
                &lo_addr,
                &hi_addr,
                op,
                value,
                field_ty.is_unsigned(),
            );
            return;
        }
        // Int-width Dot path: try the lvalue-chain helper so we
        // handle globals (`s.x <op>= …` for global struct `s`) the
        // same way `emit_member_assign` does. Fixture 444
        // (`s.x &= 0xFF` for global `s`).
        let (dest, field_ty) = if matches!(kind, crate::ast::MemberKind::Dot)
            && let Some((name, total_off, leaf_ty)) = self.try_member_dot_chain(base, field)
        {
            let dest = if self.globals.contains(&name) {
                if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                }
            } else {
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) else {
                    panic!("struct local `{name}` not stack-resident (unexpected)");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                bp_addr(off)
            };
            (dest, leaf_ty)
        } else {
            let ExprKind::Ident(name) = &base.kind else {
                panic!("non-ident base in member compound assign not yet supported (no fixture)");
            };
            let base_ty = self.locals.type_of(name).clone();
            let (field_off, field_ty) = match kind {
                crate::ast::MemberKind::Dot => base_ty.field(field).unwrap_or_else(|| {
                    panic!("`{name}.{field} <op>= …`: no such field in {base_ty:?}")
                }),
                crate::ast::MemberKind::Arrow => {
                    let pointee = base_ty
                        .pointee()
                        .unwrap_or_else(|| panic!("`{name}->{field} <op>= …`: not a pointer"))
                        .clone();
                    pointee.field(field).unwrap_or_else(|| {
                        panic!("`{name}->{field} <op>= …`: no such field in {pointee:?}")
                    })
                }
            };
            let dest = match kind {
                crate::ast::MemberKind::Dot => {
                    let LocalLocation::Stack(struct_off) = self.locals.location_of(name) else {
                        panic!("struct local `{name}` not stack-resident (unexpected)");
                    };
                    let off = struct_off + i16::try_from(field_off).unwrap_or(i16::MAX);
                    bp_addr(off)
                }
                crate::ast::MemberKind::Arrow => {
                    let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
                        panic!(
                            "stack-resident pointer in `p->x <op>= …` not yet supported (no fixture)"
                        );
                    };
                    if field_off == 0 {
                        format!("[{}]", reg.name())
                    } else {
                        format!("[{}+{field_off}]", reg.name())
                    }
                }
            };
            (dest, field_ty)
        };
        let store_byte = field_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        // Char-field compound with char-typed variable RHS —
        // mirrors the char-global var-RHS pattern (batch 121):
        // load RHS into AL, then memory-direct `<op> byte ptr
        // <dest>, al`. The `dest` already includes any non-zero
        // field offset. Fixture 708 (`g.c += d`).
        if store_byte
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && try_const_eval(value).is_none()
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Char | Type::UChar)
        {
            let src = self.resolve_operand_source(value);
            self.out.extend_from_slice(b"\tmov\tal,");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest},al\r\n");
            return;
        }
        // Char-field compound with int-typed variable RHS (gets
        // truncated to byte). Same op-family asymmetry as char-
        // array (fixtures 847/850): arith goes through AL,
        // bitwise stays memory-direct. Fixture 848.
        if store_byte
            && matches!(op, BinOp::Add | BinOp::Sub)
            && try_const_eval(value).is_none()
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
            let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
            let _ = write!(self.out, "\t{mnem}\tal,{rhs_byte}\r\n");
            let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
            return;
        }
        if store_byte
            && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && try_const_eval(value).is_none()
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let mnem = match op {
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tal,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest},al\r\n");
            return;
        }
        // Int-field compound with non-constant RHS — load RHS
        // into AX, then memory-direct `<op> word ptr <dest>, ax`.
        // emit_expr_to_ax handles int/char/uchar local/global
        // widening (cbw or `mov ah, 0` as appropriate). Fixture
        // 832 (`s.x += y`).
        if !store_byte
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && try_const_eval(value).is_none()
            && matches!(field_ty, Type::Int | Type::UInt)
        {
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tword ptr {dest},ax\r\n");
            return;
        }
        // Int-field compound `<<=` / `>>=` with non-constant RHS
        // — `mov cl, byte ptr <rhs>; shl word ptr <dest>, cl`.
        // `dest` already includes any field offset. Fixture 835
        // (`s.x <<= y`).
        if !store_byte
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && try_const_eval(value).is_none()
            && matches!(field_ty, Type::Int | Type::UInt)
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let unsigned = field_ty.is_unsigned();
            let mnem = match (op, unsigned) {
                (BinOp::Shl, _) => "shl",
                (BinOp::Shr, false) => "sar",
                (BinOp::Shr, true) => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tcl,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tword ptr {dest},cl\r\n");
            return;
        }
        // Int-field compound `*=` / `/=` / `%=` with non-constant
        // local RHS — load LHS into AX, then `imul`/`idiv` against
        // the RHS in `[bp+N]`. Mirrors the int-global path
        // (fixtures 802, 803). Fixture 834 (`s.x *= y`).
        if !store_byte
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && try_const_eval(value).is_none()
            && matches!(field_ty, Type::Int | Type::UInt)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                panic!("non-stack RHS in member compound Mul/Div not yet supported (no fixture)");
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {dest}\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr {}\r\n", bp_addr(off));
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tidiv\tword ptr {}\r\n", bp_addr(off));
            }
            let result_reg = if matches!(op, BinOp::Mul | BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(self.out, "\tmov\tword ptr {dest},{result_reg}\r\n");
            return;
        }
        let Some(v) = try_const_eval(value) else {
            panic!("non-constant rhs in member compound assign not yet supported (no fixture)");
        };
        let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
        // Char-field arith (`+=` / `-=`) follows the BCC byte-arith-
        // through-AL pattern (same as plain char-global, batch 122):
        // `mov al, byte ptr <dest>; add al, K; mov byte ptr <dest>,
        // al`. BCC canonicalizes `-=` as `add al, (256-K)`. Char-
        // field bitwise (`&=` / `|=` / `^=`) keeps memory-direct.
        // Fixture 704 (`g.c += 5`).
        // Postfix `g.c++` / `g.c--` (discarded): memory-direct
        // `inc|dec byte ptr <dest>` — same pre-vs-post asymmetry as
        // `g++` for char globals. Fixture 716 (`g.c++`).
        if store_byte
            && from_postfix
            && v_masked == 1
            && matches!(op, BinOp::Add | BinOp::Sub)
        {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest}\r\n");
            return;
        }
        if store_byte && matches!(op, BinOp::Add | BinOp::Sub) {
            let imm8 = if matches!(op, BinOp::Add) {
                (v_masked & 0xFF) as u8
            } else {
                ((v_masked & 0xFF) as u8).wrapping_neg()
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
            // K=1 peephole: `inc al` / `dec al` instead of
            // `add al, 1` / `add al, 255`. Same byte count but
            // matches BCC's char-field `++`/`--` lowering
            // (fixture 709 `++g.c` → `inc al`).
            if v_masked == 1 && matches!(op, BinOp::Add) {
                self.out.extend_from_slice(b"\tinc\tal\r\n");
            } else if v_masked == 1 && matches!(op, BinOp::Sub) {
                self.out.extend_from_slice(b"\tdec\tal\r\n");
            } else {
                let _ = write!(self.out, "\tadd\tal,{imm8}\r\n");
            }
            let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
            return;
        }
        // Int-field `++` / `--` (discarded postfix or `+= 1` / `-= 1`):
        // memory-direct `inc`/`dec word ptr <dest>` (2-3 bytes via the
        // FF /0 or /1 form) instead of `add word ptr <dest>, 1` (5
        // bytes for sym+disp or 4 for [si]). Mirrors the char-field
        // K=1 peephole above. Fixture 1290 (`p->x++` with int x at
        // offset 0 in struct, p in SI → `inc word ptr [si]`).
        if !store_byte
            && v_masked == 1
            && matches!(op, BinOp::Add | BinOp::Sub)
        {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\tword ptr {dest}\r\n");
            return;
        }
        let mnemonic = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::BitAnd => "and",
            BinOp::BitOr => "or",
            BinOp::BitXor => "xor",
            _ => panic!("compound op `{op:?}` on member not yet supported (no fixture)"),
        };
        let _ = write!(self.out, "\t{mnemonic}\t{width} ptr {dest},{v_masked}\r\n");
    }

    /// `*<target> = <value>;` — indirect store. Pattern (fixture 081):
    /// ```text
    ///   mov word ptr [si], <value>
    /// ```
    /// where SI holds the pointer.
    fn emit_deref_assign(&mut self, target: &Expr, value: &Expr) {
        // `*p++ = v;` — postfix increment of a register-resident pointer
        // in lvalue position. BCC stores first (using the pre-increment
        // address) then advances the pointer by sizeof(*p). Fixture 501.
        if let ExprKind::Update {
            target: name,
            op: crate::ast::UpdateOp::Inc,
            position: crate::ast::UpdatePosition::Post,
        } = &target.kind
        {
            let ty = self.locals.type_of(name).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{name}++ = v`: not a pointer type");
            };
            let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
                panic!("stack-resident pointer in `*p++ = v` not supported");
            };
            let reg = reg.name();
            let width = ptr_width(pointee);
            let Some(v) = try_const_eval(value) else {
                panic!("non-constant rhs in `*p++ = v` not yet supported (no fixture)");
            };
            let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr [{reg}],{v_masked}\r\n");
            let stride = pointee.size_bytes();
            for _ in 0..stride {
                let _ = write!(self.out, "\tinc\t{reg}\r\n");
            }
            return;
        }
        let (base_name, depth) = deref_chain_root(target);
        // Single-deref of a register-resident local pointer keeps the
        // original fast path (`mov word ptr [si], v` etc.). Anything
        // beyond that — globals, deeper chains — bounces through BX
        // via the shared chain helper.
        let is_global = self.globals.type_of(base_name).is_some();
        if depth == 0 && !is_global {
            let ty = self.locals.type_of(base_name).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{base_name} = v`: not a pointer type");
            };
            let addr_reg = match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) => reg.name(),
                LocalLocation::Stack(_) => {
                    panic!("stack-resident pointer in `*p = v` not yet supported (no fixture)");
                }
            };
            // Long pointee: store both halves through `[reg]` /
            // `[reg+2]`. High first, then low (matches all other
            // long memory-direct stores). Fixture 308.
            if pointee.is_long_like() {
                let Some(v) = try_const_eval(value) else {
                    panic!("non-constant rhs in long `*p = v` not yet supported (no fixture)");
                };
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let _ = write!(self.out, "\tmov\tword ptr [{addr_reg}+2],{hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr [{addr_reg}],{lo}\r\n");
                return;
            }
            let width = ptr_width(pointee);
            if let Some(v) = try_const_eval(value) {
                let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(self.out, "\tmov\t{width} ptr [{addr_reg}],{v_masked}\r\n");
                return;
            }
            // Direct-from-register-local peephole: when the RHS is
            // a register-resident int local, skip the AX round-trip
            // and store the register directly through the address
            // register. Fixture 628 (`*p = x` with p in DI, x in SI
            // → `mov [di], si`).
            if !pointee.is_char_like()
                && let ExprKind::Ident(src_name) = &value.kind
                && self.locals.has(src_name)
                && let LocalLocation::Reg(src_reg) = self.locals.location_of(src_name)
                && !src_reg.is_byte()
            {
                let _ = write!(
                    self.out,
                    "\tmov\t{width} ptr [{addr_reg}],{}\r\n",
                    src_reg.name(),
                );
                return;
            }
            // Non-constant RHS: materialize the value in AX/AL,
            // then store through the address register. Fixture 595
            // (`*p = *p + 1` → `mov ax, [si]; inc ax; mov [si], ax`).
            self.emit_expr_to_ax(value);
            let reg_name = if pointee.is_char_like() { "al" } else { "ax" };
            let _ = write!(self.out, "\tmov\t{width} ptr [{addr_reg}],{reg_name}\r\n");
            return;
        }
        // Chain path: same prefix as the read side (fixtures 194 /
        // 196), then a `mov <width> ptr [bx],<imm>` store. Only
        // constant RHS is fixtured today.
        let Some(v) = try_const_eval(value) else {
            panic!("non-constant rhs in chained `*p = v` not yet supported (no fixture)");
        };
        let final_ty = self.emit_chain_to_bx(base_name, depth);
        let width = ptr_width(&final_ty);
        let v_masked = if final_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
        let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
    }

    /// `*<target> <op>= <value>;` — read-modify-write through a
    /// dereferenced pointer. Same shape as `emit_deref_assign` for
    /// address resolution, then emits `<op> <width> ptr [reg],imm`
    /// directly (fixture 183).
    fn emit_deref_compound_assign(
        &mut self,
        target: &Expr,
        op: BinOp,
        value: &Expr,
        from_postfix: bool,
    ) {
        let (base_name, depth) = deref_chain_root(target);
        // Long pointee + register-resident pointer: route through the
        // shared long-compound-to-memory helper. Picks up variable
        // RHS (fixture 398: `*p += y`) for free since the helper
        // already knows the destination addressing. Const RHS still
        // falls through to the existing const-only fast paths
        // immediately below.
        let is_global = self.globals.type_of(base_name).is_some();
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && pointee.is_long_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
        {
            let r = reg.name();
            let lo_addr = format!("[{r}]");
            let hi_addr = format!("[{r}+2]");
            self.emit_long_compound_to_mem(
                &lo_addr,
                &hi_addr,
                op,
                value,
                pointee.is_unsigned(),
            );
            return;
        }
        // Postfix `lv++` / `lv--` (discarded) through a char pointer:
        // BCC emits memory-direct `inc|dec byte ptr [reg]` rather
        // than the AL detour used for prefix `++lv` / explicit
        // `lv += 1`. Same pre-vs-post asymmetry as char-global
        // (fixture 702 `g++`). Fixture 714 (`(*p)++` standalone).
        if depth == 0
            && !is_global
            && from_postfix
            && matches!(op, BinOp::Add | BinOp::Sub)
            && try_const_eval(value) == Some(1)
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && pointee.is_char_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
        {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\tbyte ptr [{}]\r\n", reg.name());
            return;
        }
        // Char-pointee `*p <op>= d` (variable RHS): load RHS into
        // AL, then memory-direct `<op> byte ptr [reg], al`. Mirrors
        // the char-global var-RHS pattern (batch 121). Fixture 713
        // (`*p += d` with p in SI, d at [bp-1]).
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && pointee.is_char_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
            && try_const_eval(value).is_none()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let r = reg.name();
            let src = self.resolve_operand_source(value);
            self.out.extend_from_slice(b"\tmov\tal,");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tbyte ptr [{r}],al\r\n");
            return;
        }
        // Int-pointee `*p <op>= y` (variable RHS): load RHS into
        // AX (with widening if RHS is byte), then memory-direct
        // `<op> word ptr [reg], ax`. Pointer must be register-
        // resident. Fixture 838 (`*p += y` with p in SI, y at
        // [bp-4]).
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && matches!(pointee, Type::Int | Type::UInt)
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
            && try_const_eval(value).is_none()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let r = reg.name();
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tword ptr [{r}],ax\r\n");
            return;
        }
        // Int-pointee `*p *= y` / `/= y` / `%= y` with non-const
        // local RHS: `mov ax, word ptr [r]; imul/idiv word ptr
        // [bp+N]; mov word ptr [r], ax|dx`. Fixture 839.
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && matches!(pointee, Type::Int | Type::UInt)
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
            && try_const_eval(value).is_none()
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                panic!("non-stack RHS in deref compound Mul/Div not yet supported (no fixture)");
            };
            let r = reg.name();
            let _ = write!(self.out, "\tmov\tax,word ptr [{r}]\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr {}\r\n", bp_addr(off));
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tidiv\tword ptr {}\r\n", bp_addr(off));
            }
            let result_reg = if matches!(op, BinOp::Mul | BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(self.out, "\tmov\tword ptr [{r}],{result_reg}\r\n");
            return;
        }
        // Int-pointee `*p <<= y` / `>>= y` with non-const RHS:
        // `mov cl, byte ptr <rhs>; shl|sar|shr word ptr [r], cl`.
        // Needs new IR variants for `<sh> word ptr [si], cl`.
        // Fixture 840.
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && matches!(pointee, Type::Int | Type::UInt)
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
            && try_const_eval(value).is_none()
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let r = reg.name();
            let unsigned = pointee.is_unsigned();
            let mnem = match (op, unsigned) {
                (BinOp::Shl, _) => "shl",
                (BinOp::Shr, false) => "sar",
                (BinOp::Shr, true) => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tcl,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tword ptr [{r}],cl\r\n");
            return;
        }
        let Some(v) = try_const_eval(value) else {
            panic!("non-constant rhs in `*p <op>= v` not yet supported (no fixture)");
        };
        let mnemonic = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::BitAnd => "and",
            BinOp::BitOr => "or",
            BinOp::BitXor => "xor",
            _ => panic!("compound op `{op:?}` on `*p` not yet supported (no fixture)"),
        };
        // Single-deref local stays on the original fast path so a
        // register-resident pointer (SI/DI) can drive the operand
        // directly. Fixture 183 (`*p += K` for local `p` in SI).
        let is_global = self.globals.type_of(base_name).is_some();
        if depth == 0 && !is_global {
            let ty = self.locals.type_of(base_name).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{base_name} <op>= v`: not a pointer type");
            };
            let addr_reg = match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) => reg.name(),
                LocalLocation::Stack(_) => {
                    panic!(
                        "stack-resident pointer in `*p <op>= v` not yet supported (no fixture)"
                    );
                }
            };
            // Long pointee: emit memory-direct read-modify-write pair
            // through `[reg]` / `[reg+2]`. Same byte-width rule as
            // the long-global compound assigns — arith uses imm8sx,
            // bitwise uses imm16. Fixture 311.
            if pointee.is_long_like() {
                let k_lo = (v as i64) & 0xFFFF;
                let k_hi = ((v as i64) >> 16) & 0xFFFF;
                match op {
                    BinOp::Add | BinOp::Sub => {
                        let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                            ("add", "adc")
                        } else {
                            ("sub", "sbb")
                        };
                        if let Ok(lo_i8) = i8::try_from(k_lo as i32) {
                            let _ = write!(self.out, "\t{lo_op}\tword ptr [{addr_reg}],{lo_i8}\r\n");
                        } else {
                            let lo_u16 = k_lo as u16;
                            let _ = write!(self.out, "\t{lo_op}\tword ptr [{addr_reg}],{lo_u16}\r\n");
                        }
                        let _ = write!(self.out, "\t{hi_op}\tword ptr [{addr_reg}+2],0\r\n");
                        return;
                    }
                    BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                        let _ = write!(self.out, "\t{mnemonic}\tword ptr [{addr_reg}],{k_lo}\r\n");
                        let _ = write!(self.out, "\t{mnemonic}\tword ptr [{addr_reg}+2],{k_hi}\r\n");
                        return;
                    }
                    _ => {}
                }
            }
            let store_byte = pointee.is_char_like();
            let width = if store_byte { "byte" } else { "word" };
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
            // Char-pointee arith follows the BCC byte-through-AL
            // pattern: `mov al, byte ptr [reg]; add al, K (or
            // inc/dec for K=1); mov byte ptr [reg], al`. Bitwise
            // ops stay memory-direct. Fixture 711 (`*p += 5` with
            // p in SI → `mov al, [si]; add al, 5; mov [si], al`).
            if store_byte && matches!(op, BinOp::Add | BinOp::Sub) {
                let imm8 = if matches!(op, BinOp::Add) {
                    (v_masked & 0xFF) as u8
                } else {
                    ((v_masked & 0xFF) as u8).wrapping_neg()
                };
                let _ = write!(self.out, "\tmov\tal,byte ptr [{addr_reg}]\r\n");
                if v_masked == 1 && matches!(op, BinOp::Add) {
                    self.out.extend_from_slice(b"\tinc\tal\r\n");
                } else if v_masked == 1 && matches!(op, BinOp::Sub) {
                    self.out.extend_from_slice(b"\tdec\tal\r\n");
                } else {
                    let _ = write!(self.out, "\tadd\tal,{imm8}\r\n");
                }
                let _ = write!(self.out, "\tmov\tbyte ptr [{addr_reg}],al\r\n");
                return;
            }
            // `*pp <add|sub>= K` where *pp itself is a pointer:
            // scale K by sizeof(pointee-of-pointee) for C pointer
            // arithmetic. The inc/dec peephole below assumes
            // stride=1; pointer-of-pointer with non-1 stride must
            // emit `add word ptr [reg], K*stride`. Fixture 3647
            // (`*pp += 1` where pp is `struct Pt**`, stride=4).
            if let Some(inner) = pointee.pointee()
                && matches!(op, BinOp::Add | BinOp::Sub)
                && inner.size_bytes() > 1
            {
                let stride = i32::from(inner.size_bytes());
                let sign = if matches!(op, BinOp::Add) { 1i32 } else { -1 };
                let bytes = sign.wrapping_mul(v_masked as i32).wrapping_mul(stride);
                let imm16 = bytes as i16;
                let _ = write!(self.out, "\tadd\tword ptr [{addr_reg}],{imm16}\r\n");
                return;
            }
            // Int-pointee K=1 peephole: `inc word ptr [reg]` / `dec
            // word ptr [reg]` (2 bytes) instead of `add word ptr
            // [reg], 1` (3 bytes). Fixture 1302 (`++(*p)` with int p
            // in SI).
            if !store_byte && v_masked == 1 && matches!(op, BinOp::Add | BinOp::Sub) {
                let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                let _ = write!(self.out, "\t{mnem}\tword ptr [{addr_reg}]\r\n");
                return;
            }
            let _ = write!(
                self.out,
                "\t{mnemonic}\t{width} ptr [{addr_reg}],{v_masked}\r\n",
            );
            return;
        }
        // Chain path: same prefix as the read/write counterparts
        // (fixtures 194 / 196), then `<op> word ptr [bx],<imm>` in
        // place. Fixture 197 (`*p += 5` for global `p`).
        let final_ty = self.emit_chain_to_bx(base_name, depth);
        let store_byte = final_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
        let _ = write!(
            self.out,
            "\t{mnemonic}\t{width} ptr [bx],{v_masked}\r\n",
        );
    }

    /// Assign to a file-scope variable: `<width> ptr DGROUP:_<name>`
    /// is both the lvalue and the rvalue address. Fixture 085:
    /// `g = 7;` → `mov word ptr DGROUP:_g, 7`.
    fn emit_assign_global(&mut self, name: &str, value: &Expr) {
        let ty = self
            .globals
            .type_of(name)
            .cloned()
            .expect("caller already checked");
        // `long g = K;` — two word stores, **high word first** then
        // low word (fixture 205). Both `long` and `unsigned long`
        // share the same byte-level emission for arithmetic and
        // bitwise ops; only shifts (sar vs shr) and comparisons
        // (signed vs unsigned jumps) need to branch on signedness.
        // Struct-to-struct copy assign at file scope. Two emission
        // shapes by size:
        //   - **4 bytes**: BCC inlines a high-first AX:DX load/store
        //     pair — byte-identical to a long-to-long copy (fixture
        //     211). Source-level type is invisible at the byte level
        //     (a `struct { int x; int y; }` and a `struct { long x; }`
        //     produce the same bytes). Fixtures 410, 412.
        //   - **>4 bytes**: BCC calls the runtime helper `N_SCOPY@`,
        //     passing far pointers to dest and src (DS:offset, dest
        //     pushed first) and the byte count in CX. Fixtures 413
        //     (6-byte), 414 (8-byte).
        // 1-byte and 2-byte struct copies still take the generic
        // single-word path (fixture 411) — same byte output as a
        // plain int copy.
        if let Type::Struct { .. } = &ty
            && let ExprKind::Ident(src_name) = &value.kind
            && self.globals.type_of(src_name).map_or(false, |t| t == &ty)
        {
            let size = ty.size_bytes();
            if size == 4 {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            if size > 4 {
                let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{name}\r\n");
                self.out.extend_from_slice(b"\tpush\tds\r\n");
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{src_name}\r\n");
                self.out.extend_from_slice(b"\tpush\tds\r\n");
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_SCOPY@\r\n");
                self.helpers.insert("N_SCOPY@".to_string());
                return;
            }
        }
        // `a = f();` for `f` returning a 4-byte struct. Same shape
        // as `g = f();` for a long-returning callee — the call
        // leaves DX:AX = high:low and we store back to the struct
        // destination. Byte-identical to the long-return store
        // (fixture 214) for the 4-byte case. Fixture 424.
        if let Type::Struct { .. } = &ty
            && ty.size_bytes() == 4
            && let ExprKind::Call { name: fname, args } = &value.kind
            && self.signatures.ret_ty_of(fname).map_or(false, |t| t == &ty)
        {
            self.emit_call(fname, args);
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            return;
        }
        if ty.is_long_like() {
            if let Some(v) = try_const_eval(value) {
                let lo = v & 0xFFFF;
                let hi = (v >> 16) & 0xFFFF;
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name}+2,{hi}\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},{lo}\r\n",
                );
                return;
            }
            // `g = h;` long-to-long copy between two long globals.
            // Load h into AX:DX (high→AX, low→DX), then store into
            // g. Fixture 211.
            if let ExprKind::Ident(src_name) = &value.kind
                && let Some(src_ty) = self.globals.type_of(src_name)
                && src_ty.is_long_like()
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = x;` long-from-stack-local copy. Same in-memory
            // convention as global-to-global (high→AX, low→DX), with
            // bp-relative loads. Fixture 218 (`g = <long param>`).
            if let ExprKind::Ident(src_name) = &value.kind
                && self.locals.has(src_name)
                && self.locals.type_of(src_name).is_long_like()
            {
                let LocalLocation::Stack(off) = self.locals.location_of(src_name) else {
                    panic!("register-resident long source not yet supported (no fixture)");
                };
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off));
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = f();` where `f` returns long. Call returns DX:AX
            // (high:low) per the standard ABI; store directly back
            // into the long global. Fixture 214.
            if let ExprKind::Call { name: fname, args } = &value.kind
                && self.signatures.ret_ty_of(fname).map_or(false, |t| t.is_long_like())
            {
                self.emit_call(fname, args);
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = ~a;` between two long globals. Independent per
            // half (no carry), so it's just `not` on each register
            // after the load. Fixture 225.
            if let ExprKind::Unary { op: UnaryOp::BitNot, operand } = &value.kind
                && let ExprKind::Ident(a) = &operand.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tnot\tdx\r\n");
                let _ = write!(self.out, "\tnot\tax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a * 2;` long times constant 2 — BCC peepholes
            // this to the same shl/rcl pattern as `g << 1` (slice
            // 227), skipping the N_LXMUL@ helper. Fixture 283. For
            // other small power-of-2 multipliers, BCC's behavior
            // is unprobed (likely helper-call); not yet handled.
            if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
                && let ExprKind::Ident(a) = &left.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
                && try_const_eval(right) == Some(2)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tshl\tdx,1\r\n");
                let _ = write!(self.out, "\trcl\tax,1\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a << 1;` long left-shift-by-one. BCC inlines as
            // shl on the low half (CF gets the high bit) and rcl on
            // the high half (rotates CF into the LSB). Note the
            // AX=high/DX=low convention here matches the rest of the
            // long-arith block; for shift counts >1 BCC switches to
            // the `N_LXLSH@` helper and the standard DX:AX=high:low
            // ABI convention (see the >1 path below). Fixture 227.
            if let ExprKind::BinOp { op: BinOp::Shl, left, right } = &value.kind
                && let ExprKind::Ident(a) = &left.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
                && try_const_eval(right) == Some(1)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tshl\tdx,1\r\n");
                let _ = write!(self.out, "\trcl\tax,1\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a >> 1;` long right-shift-by-one. Mirror of the
            // `<< 1` path: high gets `sar`/`shr` (signed/unsigned),
            // low gets `rcr` (CF threads from high LSB into low MSB).
            // Register convention is AX=high, DX=low. Fixtures 229
            // (signed), 243 (unsigned).
            if let ExprKind::BinOp { op: BinOp::Shr, left, right } = &value.kind
                && let ExprKind::Ident(a) = &left.kind
                && let Some(a_ty) = self.globals.type_of(a)
                && a_ty.is_long_like()
                && try_const_eval(right) == Some(1)
            {
                let hi_op = if a_ty.is_unsigned() { "shr" } else { "sar" };
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                let _ = write!(self.out, "\trcr\tdx,1\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a / b;` / `g = a % b;` long division and modulo.
            // BCC calls helpers:
            //   signed   /  → `N_LDIV@`   (fixture 232)
            //   signed   %  → `N_LMOD@`   (fixture 233)
            //   unsigned /  → `N_LUDIV@`  (fixture 245)
            //   unsigned %  → (likely `N_LUMOD@`; not yet fixtured)
            // Operands passed on the STACK (cdecl order — b pushed
            // first, so a sits at the lowest pushed address). High
            // word pushed before low for each operand: push b+2, b,
            // a+2, a. Result in DX:AX. Helper self-cleans the
            // stack (no `add sp,8` after).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && matches!(op, BinOp::Div | BinOp::Mod)
                && let ExprKind::Ident(a) = &left.kind
                && let ExprKind::Ident(b) = &right.kind
                && let Some(a_ty) = self.globals.type_of(a)
                && a_ty.is_long_like()
                && let Some(b_ty) = self.globals.type_of(b)
                && b_ty.is_long_like()
            {
                let unsigned = a_ty.is_unsigned() || b_ty.is_unsigned();
                let helper = match (op, unsigned) {
                    (BinOp::Div, false) => "N_LDIV@",
                    (BinOp::Mod, false) => "N_LMOD@",
                    (BinOp::Div, true)  => "N_LUDIV@",
                    (BinOp::Mod, true)  => "N_LUMOD@",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{b}+2\r\n");
                let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{b}\r\n");
                let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = a * b;` long multiplication. BCC calls the runtime
            // helper `N_LXMUL@`. Calling convention: operand a in
            // (CX:BX)=(high:low), operand b in (DX:AX)=(high:low),
            // result returned in (DX:AX)=(high:low). Note the order
            // of register loads is high before low for both operands.
            // Fixture 231.
            if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
                && let ExprKind::Ident(a) = &left.kind
                && let ExprKind::Ident(b) = &right.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
                && self.globals.type_of(b).map_or(false, |t| t.is_long_like())
            {
                let _ = write!(self.out, "\tmov\tcx,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{b}+2\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{b}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                self.helpers.insert("N_LXMUL@".to_string());
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = a << K;` / `g = a >> K;` for K > 1 — BCC calls a
            // runtime helper: `N_LXLSH@` for left-shift (fixture
            // 228), `N_LXRSH@` for signed right-shift (fixture 230),
            // `N_LXURSH@` for unsigned right-shift (fixture 244).
            // The register convention SWITCHES to the standard
            // 32-bit ABI: DX=high, AX=low (input *and* output). CL
            // holds the shift count. The helper is declared
            // `extrn <name>:far` in the tail.
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && matches!(op, BinOp::Shl | BinOp::Shr)
                && let ExprKind::Ident(a) = &left.kind
                && let Some(a_ty) = self.globals.type_of(a)
                && a_ty.is_long_like()
                && let Some(k) = try_const_eval(right)
                && k > 1
                && k <= 255
            {
                let helper = match (op, a_ty.is_unsigned()) {
                    (BinOp::Shl, _)        => "N_LXLSH@",
                    (BinOp::Shr, false)    => "N_LXRSH@",
                    (BinOp::Shr, true)     => "N_LXURSH@",
                    _ => unreachable!(),
                };
                let k_u8 = k as u8;
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = -a;` long unary minus. 32-bit two's-complement
            // negate: neg high, neg low (sets CF iff low != 0), sbb
            // high,0 to fold the low-half carry back into the high.
            // The high `neg` comes BEFORE the low `neg` so the carry
            // generated by the low half is the one consumed by sbb.
            // Fixture 226.
            if let ExprKind::Unary { op: UnaryOp::Neg, operand } = &value.kind
                && let ExprKind::Ident(a) = &operand.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tneg\tax\r\n");
                let _ = write!(self.out, "\tneg\tdx\r\n");
                let _ = write!(self.out, "\tsbb\tax,0\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // Long-to-long arithmetic/bitwise between two long globals:
            // `g = <lvalue_a> <op> <lvalue_b>;` for two long lvalues.
            // Same skeleton: load a into (AX=high, DX=low), apply
            // the op's pair to b's halves, store back. Add/Sub need
            // carry/borrow; bitwise ops repeat the same mnemonic.
            // Both lvalues can be any long ident (global/stack),
            // struct field (dot-chain), array element (const index),
            // or `*p` (register pointer). Fixtures 219, 220, 221,
            // 222, 224 (globals-globals); 355 (struct fields).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && let Some((lo_op, hi_op)) = long_pair_op(*op)
                && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
                && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
                let _ = write!(self.out, "\t{lo_op}\tdx,word ptr {b_lo}\r\n");
                let _ = write!(self.out, "\t{hi_op}\tax,word ptr {b_hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = <long-lvalue> + K;` or `g = <long-lvalue> - K;` —
            // load the lvalue's halves into (AX=high, DX=low) globals
            // convention (since dest is the memory global `g`), then
            // add/sub the low half and adc/sbb the high (carry=0 for
            // Add, -1 for Sub). The lvalue can be any long ident
            // (global or stack), struct field, array element (const
            // index), or `*p` for a register-resident long pointer.
            // Fixtures 207 / 208 (self-modify g), 275 (wide K), 352
            // (struct field source), 353 (array element source), 354
            // (deref source).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && (matches!(op, BinOp::Add) || matches!(op, BinOp::Sub))
                && let Some((hi_addr, lo_addr)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
            {
                let signed = k as i32;
                let (delta, carry) = if matches!(op, BinOp::Add) {
                    (signed, 0i16)
                } else {
                    (-signed, -1i16)
                };
                // imm8sx-fits emits `add dx, K_i8` (slice 207);
                // otherwise emits the wider `add dx, K_i16`
                // (fixture 275). Either way the high partner is
                // `adc ax, carry` (carry=0 for Add, -1 for Sub).
                let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                if let Ok(delta_i8) = i8::try_from(delta) {
                    let _ = write!(self.out, "\tadd\tdx,{delta_i8}\r\n");
                } else {
                    let delta_u16 = (delta as i32) as u16;
                    let _ = write!(self.out, "\tadd\tdx,{delta_u16}\r\n");
                }
                let _ = write!(self.out, "\tadc\tax,{carry}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = i + g;` int-LHS plus long-RHS, where the long
            // RHS happens to be the assign target. BCC widens i
            // into DX:AX (mov ax,_i / cwd), then uses MEMORY-direct
            // add/adc on the long — no BX:CX scratch needed. The
            // result lands directly in DX:AX (the widened-int
            // registers) and stores back. Fixture 281.
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && matches!(op, BinOp::Add)
                && let ExprKind::Ident(i_name) = &left.kind
                && let Some(i_ty) = self.globals.type_of(i_name)
                && matches!(i_ty, Type::Int)
                && let ExprKind::Ident(rhs_name) = &right.kind
                && rhs_name == name
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{i_name}\r\n");
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tadd\tax,word ptr DGROUP:_{name}\r\n");
                let _ = write!(self.out, "\tadc\tdx,word ptr DGROUP:_{name}+2\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = g <op> i;` long-self <op> int-global, for
            // add/sub/and/or/xor. BCC widens i first (mov ax,
            // _i / cwd to DX:AX), then loads the long accumulator
            // into BX:CX (high:low — DX:AX is busy with the
            // widened int), does the operation per half, and stores
            // back. Arithmetic uses add/adc or sub/sbb for carry
            // propagation; bitwise repeats the same mnemonic per
            // half since they're independent. Fixtures 257 (`+`),
            // 258 (`-`), 259 (`&`).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && let ExprKind::Ident(lhs_name) = &left.kind
                && lhs_name == name
                && let ExprKind::Ident(i_name) = &right.kind
                && let Some(i_ty) = self.globals.type_of(i_name)
                && matches!(i_ty, Type::Int)
                && let Some((lo_op, hi_op)) = long_pair_op(*op)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{i_name}\r\n");
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{name}+2\r\n");
                let _ = write!(self.out, "\tmov\tcx,word ptr DGROUP:_{name}\r\n");
                let _ = write!(self.out, "\t{lo_op}\tcx,ax\r\n");
                let _ = write!(self.out, "\t{hi_op}\tbx,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,bx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},cx\r\n");
                return;
            }
            // `long g = i;` / `long g = u;` / `long g = (long)i;` —
            // widen an int-family global to long. Signed int
            // sign-extends via `cwd` (fixture 254); `unsigned int`
            // zero-extends by storing 0 directly into the high half
            // (fixture 255). Either way: load into AX first, store
            // high, then low. Peels an explicit `(long)` cast if
            // present (fixture 279); BCC emits identical bytes for
            // implicit and explicit forms.
            let widening_src = match &value.kind {
                ExprKind::Ident(name) => Some(name.as_str()),
                ExprKind::Cast { ty: Type::Long, operand } => {
                    if let ExprKind::Ident(name) = &operand.kind { Some(name.as_str()) } else { None }
                }
                _ => None,
            };
            if let Some(src_name) = widening_src
                && let Some(src_ty) = self.globals.type_of(src_name)
                && matches!(src_ty, Type::Int | Type::UInt | Type::Char)
            {
                match src_ty {
                    Type::Char => {
                        // Signed char widens via cbw (byte→word)
                        // then cwd (word→dword). Fixture 271.
                        let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{src_name}\r\n");
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                        self.out.extend_from_slice(b"\tcwd\t\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    }
                    Type::UInt => {
                        // Zero-extend: store 0 directly into high.
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,0\r\n");
                    }
                    Type::Int => {
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}\r\n");
                        self.out.extend_from_slice(b"\tcwd\t\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    }
                    _ => unreachable!(),
                }
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = a[K];` for a long-element STACK array — load high
            // (`[bp+base+K*4+2]`) then low (`[bp+base+K*4]`) into
            // AX:DX (globals convention), then store. Fixture 306.
            if let ExprKind::ArrayIndex { array: arr_expr, index } = &value.kind
                && let ExprKind::Ident(arr_name) = &arr_expr.kind
                && self.locals.has(arr_name)
                && let Some(elem) = self.locals.type_of(arr_name).array_elem()
                && elem.is_long_like()
                && let Some(k) = try_const_eval(index)
            {
                let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name) else {
                    unreachable!("array is stack-resident");
                };
                let off = base_off + i16::try_from((k as i32) * 4).expect("offset fits");
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off));
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a[K];` / `g = a[i];` for a long-element GLOBAL array RHS.
            // Const index folds to `_a+K*4` / `_a+K*4+2`; var index
            // uses bx-indexed addressing on the global. Fixtures 301
            // (const index), 303 (var index).
            if let ExprKind::ArrayIndex { array: arr_expr, index } = &value.kind
                && let ExprKind::Ident(arr_name) = &arr_expr.kind
                && let Some(arr_ty) = self.globals.type_of(arr_name)
                && let Some(elem) = arr_ty.array_elem()
                && elem.is_long_like()
            {
                let arr_name = arr_name.clone();
                if let Some(k) = try_const_eval(index) {
                    let byte_off = (k as i32) * 4;
                    let lo_addr = global_offset_addr(&arr_name, byte_off);
                    let hi_addr = global_offset_addr(&arr_name, byte_off + 2);
                    let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                    return;
                }
                // Variable index — load `i` into BX, scale by 4 with
                // two `shl bx, 1`s, then read both halves via
                // `<sym>[bx+disp]`. Fixtures 303, 307.
                self.emit_index_into_bx_long_stride(index);
                let _ = write!(
                    self.out,
                    "\tmov\tax,word ptr DGROUP:_{arr_name}[bx+2]\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tmov\tdx,word ptr DGROUP:_{arr_name}[bx]\r\n",
                );
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = *p;` for `p: long *` register-resident — load
            // high through `[reg+2]` and low through `[reg]` into
            // AX:DX (globals convention), then store. Fixture 309.
            if let ExprKind::Deref(operand) = &value.kind
                && let ExprKind::Ident(ptr_name) = &operand.kind
                && self.locals.has(ptr_name)
                && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
                && pointee.is_long_like()
                && let LocalLocation::Reg(reg) = self.locals.location_of(ptr_name)
            {
                let r = reg.name();
                let _ = write!(self.out, "\tmov\tax,word ptr [{r}+2]\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr [{r}]\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = s.x;` / `g = a[K].x;` etc. — long field of a
            // dot-chain lvalue. Resolves to a constant offset within
            // some base storage (global, stack); load both halves
            // memory-direct, then store. Fixture 317.
            if let ExprKind::Member { base: mem_base, field, kind: crate::ast::MemberKind::Dot } = &value.kind
                && let Some((src, total_off, leaf_ty)) = self.try_member_dot_chain(mem_base, field)
                && leaf_ty.is_long_like()
            {
                let (lo_addr, hi_addr) = if self.globals.contains(&src) {
                    (
                        global_offset_addr(&src, total_off),
                        global_offset_addr(&src, total_off + 2),
                    )
                } else {
                    let LocalLocation::Stack(base_bp) = self.locals.location_of(&src) else {
                        panic!("struct local `{src}` not stack-resident");
                    };
                    let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                    (bp_addr(off), bp_addr(off + 2))
                };
                let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            panic!("non-constant long assignment to global not yet supported (no fixture)");
        }
        let width = if ty.is_char_like() { "byte" } else { "word" };
        if let Some(v) = try_const_eval(value) {
            let v_masked = if ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(
                self.out,
                "\tmov\t{width} ptr DGROUP:_{name},{v_masked}\r\n",
            );
            return;
        }
        // Register-resident int local on the RHS: store directly
        // from the register (`mov [_g], si` — 89 36 disp16, 4 bytes)
        // instead of bouncing through AX (`mov ax, si / mov [_g],
        // ax` — 2+3 = 5 bytes). Fixture 477 (`g = x` where x is in
        // SI).
        if !ty.is_char_like()
            && let ExprKind::Ident(src) = &value.kind
            && self.locals.has(src)
            && let LocalLocation::Reg(reg) = self.locals.location_of(src)
            && !reg.is_byte()
        {
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},{}\r\n",
                reg.name(),
            );
            return;
        }
        // `<ptr-global> = &<global>;` — emit the direct immediate-
        // store form `mov word ptr DGROUP:_p, offset DGROUP:_x`
        // (`C7 06 <p-disp> <x-imm>`, 6 bytes with two FIXUPPs)
        // instead of the AX-bounce `mov ax, offset _x / mov [_p],
        // ax` (5 bytes — yes, shorter, but oracle prefers the
        // single immediate-store form). Fixture 480.
        if !ty.is_char_like()
            && let ExprKind::AddressOf(src) = &value.kind
            && self.globals.contains(src)
        {
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},offset DGROUP:_{src}\r\n",
            );
            return;
        }
        // `<ptr-global> = <arr-global>;` — global array decays to
        // its base address. Same `mov word ptr [_p], offset _a`
        // form as `p = &a;`. Fixture 561 (`int a[3]; int *p; p = a;`).
        if !ty.is_char_like()
            && let ExprKind::Ident(src) = &value.kind
            && let Some(src_ty) = self.globals.type_of(src)
            && matches!(src_ty, Type::Array { .. })
        {
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},offset DGROUP:_{src}\r\n",
            );
            return;
        }
        // `<ptr-global> = &<arr>[K];` — same shape as the
        // `&<global>` immediate-store above but with `+offset` on
        // the source symbol. Fixture 483.
        if !ty.is_char_like()
            && let ExprKind::AddressOfArrayElem { array, byte_offset } = &value.kind
            && self.globals.contains(array)
        {
            if *byte_offset == 0 {
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},offset DGROUP:_{array}\r\n",
                );
            } else {
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},offset DGROUP:_{array}+{byte_offset}\r\n",
                );
            }
            return;
        }
        // Non-constant: compute into AX, then store.
        self.emit_expr_to_ax(value);
        if ty.is_char_like() {
            let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
        } else {
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
        }
    }

    fn emit_assign_local(&mut self, loc: LocalLocation, ty: &Type, value: &Expr) {
        match loc {
            LocalLocation::Stack(off) => {
                // Struct-to-stack copy assign. Three shape branches
                // by source storage and byte size:
                //   - 4-byte from global: inline `mov ax / mov dx`
                //     load + `[bp+off]` store pair (fixture 415).
                //   - 4-byte from stack: same inline pair but both
                //     load and store are bp-relative (fixture 417).
                //   - > 4 bytes: route through `N_SCOPY@`. The
                //     destination far pointer uses `PUSH SS` (segment
                //     for stack-resident memory) instead of `PUSH DS`,
                //     and the offset is loaded via LEA `[bp+off]`
                //     instead of `mov OFFSET _sym`. Source picks SS
                //     vs DS based on whether *it* is stack- or globals-
                //     resident. Fixtures 416 (stack ← global), 418
                //     (stack ← stack).
                if let Type::Struct { .. } = ty
                    && let ExprKind::Ident(src_name) = &value.kind
                {
                    let size = ty.size_bytes();
                    let src_is_global = self.globals.type_of(src_name).map_or(false, |t| t == ty);
                    let src_is_stack = self.locals.has(src_name)
                        && self.locals.type_of(src_name) == ty;
                    if (src_is_global || src_is_stack) && size == 4 {
                        if src_is_global {
                            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                            let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                        } else {
                            let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                            else {
                                panic!("struct local `{src_name}` not stack-resident");
                            };
                            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(src_off + 2));
                            let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(src_off));
                        }
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    if (src_is_global || src_is_stack) && size > 4 {
                        let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
                        self.out.extend_from_slice(b"\tpush\tss\r\n");
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        if src_is_global {
                            let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{src_name}\r\n");
                            self.out.extend_from_slice(b"\tpush\tds\r\n");
                        } else {
                            let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                            else {
                                panic!("struct local `{src_name}` not stack-resident");
                            };
                            let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(src_off));
                            self.out.extend_from_slice(b"\tpush\tss\r\n");
                        }
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                        self.out.extend_from_slice(b"\tcall\tnear ptr N_SCOPY@\r\n");
                        self.helpers.insert("N_SCOPY@".to_string());
                        return;
                    }
                }
                // `struct S a; a = f();` for a 4-byte struct return.
                // Same shape as the global-dest variant (fixture 424):
                // the call leaves DX:AX = high:low, store back to the
                // stack-local destination. Fixture 426.
                if let Type::Struct { .. } = ty
                    && ty.size_bytes() == 4
                    && let ExprKind::Call { name: fname, args } = &value.kind
                    && self.signatures
                        .ret_ty_of(fname)
                        .map_or(false, |t| matches!(t, Type::Struct { .. }) && t.size_bytes() == 4)
                {
                    self.emit_call(fname, args);
                    let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    return;
                }
                // `long x; x = K;` — two word stores, high then low.
                // Same shape as the init form (fixture 210/287).
                if ty.is_long_like() {
                    if let Some(v) = try_const_eval(value) {
                        let lo = v & 0xFFFF;
                        let hi = (v >> 16) & 0xFFFF;
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},{hi}\r\n",
                            bp_addr(off + 2),
                        );
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},{lo}\r\n",
                            bp_addr(off),
                        );
                        return;
                    }
                    // `x = g;` from a long-like global — mirror the
                    // init-from-global shape (fixture 286 / 288 family):
                    // load high into AX, low into DX, store back.
                    if let ExprKind::Ident(src_name) = &value.kind
                        && let Some(src_ty) = self.globals.type_of(src_name)
                        && src_ty.is_long_like()
                    {
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `x = f();` — function-call RHS returns DX:AX
                    // (ABI). Store DX → high, AX → low. Same shape as
                    // the init form (fixture 315). Fixture 321.
                    if let ExprKind::Call { .. } = &value.kind {
                        self.emit_expr_to_ax(value);
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x <op> y;` — long stack-local binary
                    // arithmetic (`+`, `-`, `&`, `|`, `^`). Load x
                    // into AX:DX (AX=high, DX=low globals-convention,
                    // since dest is memory). Apply the op pair (with
                    // carry/borrow for `+/-`, same mnemonic per half
                    // for bitwise). Store AX/DX back. Fixtures 329
                    // (add), 330 (sub), 333 (and), 334 (or).
                    if let ExprKind::BinOp { op, left, right } = &value.kind
                        && let Some((lo_op, hi_op)) = long_pair_op(*op)
                        && let ExprKind::Ident(a) = &left.kind
                        && let ExprKind::Ident(b) = &right.kind
                        && self.locals.has(a)
                        && self.locals.has(b)
                        && self.locals.type_of(a).is_long_like()
                        && self.locals.type_of(b).is_long_like()
                    {
                        let (LocalLocation::Stack(a_off), LocalLocation::Stack(b_off)) =
                            (self.locals.location_of(a), self.locals.location_of(b))
                        else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(a_off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(a_off));
                        let _ = write!(self.out, "\t{lo_op}\tdx,word ptr {}\r\n", bp_addr(b_off));
                        let _ = write!(self.out, "\t{hi_op}\tax,word ptr {}\r\n", bp_addr(b_off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x;` — long-from-long-local copy. Load
                    // both halves into AX:DX, store both into z.
                    // Fixture 335.
                    if let ExprKind::Ident(src) = &value.kind
                        && self.locals.has(src)
                        && self.locals.type_of(src).is_long_like()
                    {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src) else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(src_off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(src_off));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x * y;` long stack-local multiply — same
                    // helper convention as the global path: operand
                    // a in CX:BX (high:low), operand b in DX:AX
                    // (high:low). Result returns in DX:AX. Fixture
                    // 336.
                    if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
                        && let ExprKind::Ident(a) = &left.kind
                        && let ExprKind::Ident(b) = &right.kind
                        && self.locals.has(a)
                        && self.locals.has(b)
                        && self.locals.type_of(a).is_long_like()
                        && self.locals.type_of(b).is_long_like()
                    {
                        let (LocalLocation::Stack(a_off), LocalLocation::Stack(b_off)) =
                            (self.locals.location_of(a), self.locals.location_of(b))
                        else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tcx,word ptr {}\r\n", bp_addr(a_off + 2));
                        let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(a_off));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(b_off + 2));
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(b_off));
                        self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                        self.helpers.insert("N_LXMUL@".to_string());
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x / y;` / `z = x % y;` long stack-local
                    // divide/modulo — push operands (rightmost divisor
                    // first, each as high-then-low), call helper.
                    // Fixtures 337, 338.
                    if let ExprKind::BinOp { op, left, right } = &value.kind
                        && matches!(op, BinOp::Div | BinOp::Mod)
                        && let ExprKind::Ident(a) = &left.kind
                        && let ExprKind::Ident(b) = &right.kind
                        && self.locals.has(a)
                        && self.locals.has(b)
                        && self.locals.type_of(a).is_long_like()
                        && self.locals.type_of(b).is_long_like()
                    {
                        let (LocalLocation::Stack(a_off), LocalLocation::Stack(b_off)) =
                            (self.locals.location_of(a), self.locals.location_of(b))
                        else {
                            unreachable!("long is never register-resident");
                        };
                        let unsigned = self.locals.type_of(a).is_unsigned()
                            || self.locals.type_of(b).is_unsigned();
                        let helper = match (op, unsigned) {
                            (BinOp::Div, false) => "N_LDIV@",
                            (BinOp::Mod, false) => "N_LMOD@",
                            (BinOp::Div, true)  => "N_LUDIV@",
                            (BinOp::Mod, true)  => "N_LUMOD@",
                            _ => unreachable!(),
                        };
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(b_off + 2));
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(b_off));
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(a_off + 2));
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(a_off));
                        let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                        self.helpers.insert(helper.to_string());
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x << n;` / `z = x >> n;` long stack-local
                    // shift by a variable count. Load x into DX:AX
                    // (helper-ABI), load shift count into CL as a
                    // byte ptr from n's storage, call helper, store
                    // result. Fixture 341.
                    if let ExprKind::BinOp { op, left, right } = &value.kind
                        && matches!(op, BinOp::Shl | BinOp::Shr)
                        && let ExprKind::Ident(a) = &left.kind
                        && let ExprKind::Ident(n) = &right.kind
                        && self.locals.has(a)
                        && self.locals.has(n)
                        && self.locals.type_of(a).is_long_like()
                    {
                        let LocalLocation::Stack(a_off) = self.locals.location_of(a) else {
                            unreachable!("long is never register-resident");
                        };
                        let unsigned = self.locals.type_of(a).is_unsigned();
                        let helper = match (op, unsigned) {
                            (BinOp::Shl, _)     => "N_LXLSH@",
                            (BinOp::Shr, false) => "N_LXRSH@",
                            (BinOp::Shr, true)  => "N_LXURSH@",
                            _ => unreachable!(),
                        };
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(a_off + 2));
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(a_off));
                        // Load shift count as byte from n's storage
                        // — only the low byte of n is consumed by
                        // the helper.
                        match self.locals.location_of(n) {
                            LocalLocation::Stack(n_off) => {
                                let _ = write!(self.out, "\tmov\tcl,byte ptr {}\r\n", bp_addr(n_off));
                            }
                            LocalLocation::Reg(_reg) => {
                                panic!("register-resident shift count for long shift not yet supported (no fixture)");
                            }
                        }
                        let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                        self.helpers.insert(helper.to_string());
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `z = -x;` long unary negation on a stack local.
                    // BCC's idiom: neg AX / neg DX / sbb AX, 0 — see
                    // "Long unary" in the ASM_OUTPUT spec. Fixture 331.
                    if let ExprKind::Unary { op: UnaryOp::Neg, operand } = &value.kind
                        && let ExprKind::Ident(src) = &operand.kind
                        && self.locals.has(src)
                        && self.locals.type_of(src).is_long_like()
                    {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src) else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(src_off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(src_off));
                        self.out.extend_from_slice(b"\tneg\tax\r\n");
                        self.out.extend_from_slice(b"\tneg\tdx\r\n");
                        self.out.extend_from_slice(b"\tsbb\tax,0\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `z = ~x;` long bitwise complement on a stack
                    // local. Both halves independent: `not dx / not
                    // ax`. Fixture 332.
                    if let ExprKind::Unary { op: UnaryOp::BitNot, operand } = &value.kind
                        && let ExprKind::Ident(src) = &operand.kind
                        && self.locals.has(src)
                        && self.locals.type_of(src).is_long_like()
                    {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src) else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(src_off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(src_off));
                        self.out.extend_from_slice(b"\tnot\tdx\r\n");
                        self.out.extend_from_slice(b"\tnot\tax\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    panic!("non-constant long local assign not yet supported (no fixture)");
                }
                // Char-local store: byte-width immediate. Same byte
                // form as the init path (mov byte ptr [bp-N], K).
                // Fixture 461 (`c = 200;` for a uchar local).
                if ty.is_char_like()
                    && let Some(v) = try_const_eval(value)
                {
                    let v8 = v & 0xFF;
                    let _ = write!(self.out, "\tmov\tbyte ptr {},{v8}\r\n", bp_addr(off));
                    return;
                }
                // Char dest + ternary with both arms constant:
                // `c = cond ? K1 : K2;` — emit `mov al, K1` /
                // `mov al, K2` byte loads instead of `mov ax, K`
                // word loads (saves 1 byte per arm). Fixture 1287
                // (`c = x > 0 ? 'P' : 'N';`).
                if ty.is_char_like()
                    && let ExprKind::Ternary { cond, then_value, else_value } = &value.kind
                    && let Some(t_v) = try_const_eval(then_value)
                    && let Some(e_v) = try_const_eval(else_value)
                {
                    let span_start = value.span.start;
                    let base = self.label_plan.base(span_start);
                    let false_slot = base + 1;
                    let merge_slot = base + 2;
                    let cond_has_top_or = matches!(
                        cond.kind,
                        ExprKind::Logical { op: LogicalOp::Or, .. }
                    );
                    let true_slot = if cond_has_top_or { Some(base) } else { None };
                    self.emit_cond_branch(cond, true_slot, Some(false_slot));
                    if let Some(t) = true_slot {
                        self.emit_label(t);
                    }
                    let t8 = t_v & 0xFF;
                    let _ = write!(self.out, "\tmov\tal,{t8}\r\n");
                    let _ = write!(
                        self.out,
                        "\tjmp\tshort {}\r\n",
                        self.label_ref(merge_slot),
                    );
                    self.emit_label(false_slot);
                    let e8 = e_v & 0xFF;
                    let _ = write!(self.out, "\tmov\tal,{e8}\r\n");
                    self.emit_label(merge_slot);
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    return;
                }
                // Mirror the init form: immediate-store when possible.
                if let Some(v) = try_const_eval(value) {
                    let v16 = v & 0xFFFF;
                    let _ = write!(self.out, "\tmov\tword ptr {},{v16}\r\n", bp_addr(off));
                    return;
                }
                // `y = ++x;` where x is register-resident — update
                // in place, then store the register direct to the
                // stack slot (skip the AX round-trip). Fixture 530.
                if let ExprKind::Update {
                    target,
                    op,
                    position: crate::ast::UpdatePosition::Pre,
                } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(reg) = self.locals.location_of(target)
                    && !reg.is_byte()
                {
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let rname = reg.name();
                    let _ = write!(self.out, "\t{mnem}\t{rname}\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {},{rname}\r\n", bp_addr(off));
                    return;
                }
                // `c = a % b;` on int stack-locals — fold the
                // post-idiv `mov ax, dx` away by storing DX directly
                // into the destination. Fixture 546.
                if let ExprKind::BinOp { op: BinOp::Mod, left, right } = &value.kind
                    && !ty.is_char_like()
                    && !ty.is_long_like()
                {
                    self.emit_arith_setup_for_mod(left, right);
                    let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                    return;
                }
                // `char c = a[K];` — skip the AL→AX widening that
                // `emit_array_index_to_ax` emits for char arrays,
                // since the byte store truncates back anyway. Two
                // shapes:
                //   - global array source: `mov al, byte ptr DGROUP:
                //     _a+K` (fixture 567).
                //   - local array source: `mov al, byte ptr [bp+K]`
                //     (fixture 570).
                if ty.is_char_like()
                    && let ExprKind::ArrayIndex { array, index } = &value.kind
                    && let ExprKind::Ident(arr_name) = &array.kind
                {
                    if let Some(gty) = self.globals.type_of(arr_name)
                        && let Some(const_off) = try_const_array_offset(gty, std::iter::once(&**index))
                            .map(|(o, _leaf)| o)
                        && gty.array_elem().is_some_and(|e| e.is_char_like())
                    {
                        let addr = if const_off == 0 {
                            format!("DGROUP:_{arr_name}")
                        } else {
                            format!("DGROUP:_{arr_name}+{const_off}")
                        };
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                    if self.locals.has(arr_name)
                        && let arr_ty = self.locals.type_of(arr_name).clone()
                        && arr_ty.array_elem().is_some_and(|e| e.is_char_like())
                        && let Some(const_off) =
                            try_const_array_offset(&arr_ty, std::iter::once(&**index))
                                .map(|(o, _leaf)| o)
                        && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
                    {
                        let src_off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
                        let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(src_off));
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                }
                // `char b = s.c;` / `b = s.c;` — char-to-char copy
                // through a struct-member load. Skip the `cbw`-widen
                // that `emit_member_to_ax` would add for the int-
                // promotion path, since the destination is char and
                // the byte store truncates back anyway. Mirrors the
                // char-array-elem peephole just above. Two shapes:
                //   - global struct source: `mov al, byte ptr DGROUP:
                //     _s+K`.
                //   - local struct source: `mov al, byte ptr [bp+K]`.
                // Fixture 1115.
                if ty.is_char_like()
                    && let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } =
                        &value.kind
                    && let Some((src_name, total_off, leaf_ty)) =
                        self.try_member_dot_chain(base, field)
                    && leaf_ty.is_char_like()
                {
                    if self.globals.contains(&src_name) {
                        let addr = if total_off == 0 {
                            format!("DGROUP:_{src_name}")
                        } else {
                            format!("DGROUP:_{src_name}+{total_off}")
                        };
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                    if let LocalLocation::Stack(base_bp) = self.locals.location_of(&src_name) {
                        let src_off =
                            base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                        let _ = write!(
                            self.out,
                            "\tmov\tal,byte ptr {}\r\n",
                            bp_addr(src_off)
                        );
                        let _ = write!(
                            self.out,
                            "\tmov\tbyte ptr {},al\r\n",
                            bp_addr(off)
                        );
                        return;
                    }
                }
                // `c = f();` where c is char, f returns char — call
                // returns value in AL; store the low byte directly,
                // skip the cbw widen. Fixture 2451.
                if ty.is_char_like()
                    && let ExprKind::Call { name, args } = &value.kind
                    && self.signatures.ret_ty_of(name).map_or(false, |t| t.is_char_like())
                {
                    self.emit_call(name, args);
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    return;
                }
                // `c = (char)<int_local>;` — load the low byte of
                // the int directly into AL and store. The cast
                // narrows; for char dest we don't need to widen.
                // Fixture 2455 (`c = (char)i` for int i, char c).
                if ty.is_char_like()
                    && let ExprKind::Cast { ty: cast_ty, operand } = &value.kind
                    && cast_ty.is_char_like()
                    && let ExprKind::Ident(src_name) = &operand.kind
                {
                    let src_addr = if self.locals.has(src_name)
                        && let LocalLocation::Stack(soff) = self.locals.location_of(src_name)
                    {
                        Some(bp_addr(soff))
                    } else if self.globals.type_of(src_name).is_some() {
                        Some(format!("DGROUP:_{src_name}"))
                    } else {
                        None
                    };
                    if let Some(addr) = src_addr {
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                }
                // `c = (char)(<int_local> <op> K);` — byte arithmetic.
                // BCC emits `mov al, [int]; <op> al, K & 0xFF; mov
                // [c], al`. Saves the word load + word op + cbw vs.
                // narrowing at store time. Fixtures 1384, 1535, 1538,
                // 1539, 1540, 1541, 1542, 1543, 1544, 1545, 1546,
                // 1627, 2074.
                if ty.is_char_like()
                    && let ExprKind::Cast { ty: cast_ty, operand } = &value.kind
                    && cast_ty.is_char_like()
                    && let ExprKind::BinOp { op: binop, left, right } = &operand.kind
                    && matches!(binop,
                        BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                    )
                    && let ExprKind::Ident(src_name) = &left.kind
                    && let Some(k) = try_const_eval(right)
                {
                    let src_addr = if self.locals.has(src_name)
                        && let LocalLocation::Stack(soff) = self.locals.location_of(src_name)
                    {
                        Some(bp_addr(soff))
                    } else if self.globals.type_of(src_name).is_some() {
                        Some(format!("DGROUP:_{src_name}"))
                    } else {
                        None
                    };
                    if let Some(addr) = src_addr {
                        let k8 = (k & 0xFF) as u8;
                        let mnem = match binop {
                            BinOp::Add => "add",
                            BinOp::Sub => "sub",
                            BinOp::BitAnd => "and",
                            BinOp::BitOr => "or",
                            BinOp::BitXor => "xor",
                            _ => unreachable!(),
                        };
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        let _ = write!(self.out, "\t{mnem}\tal,{k8}\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                }
                // `c = d;` char-to-char copy through bare ident.
                // Load byte and store byte, no widening. Mirrors the
                // init peephole. Fixture 2685.
                if ty.is_char_like()
                    && let ExprKind::Ident(src_name) = &value.kind
                {
                    let src_is_char = if self.locals.has(src_name) {
                        self.locals.type_of(src_name).is_char_like()
                    } else {
                        self.globals.type_of(src_name).map_or(false, |t| t.is_char_like())
                    };
                    if src_is_char {
                        let src_addr = if self.locals.has(src_name)
                            && let LocalLocation::Stack(soff) = self.locals.location_of(src_name)
                        {
                            Some(bp_addr(soff))
                        } else if self.globals.type_of(src_name).is_some() {
                            Some(format!("DGROUP:_{src_name}"))
                        } else {
                            None
                        };
                        if let Some(addr) = src_addr {
                            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                            let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                            return;
                        }
                    }
                }
                // `<stack-local> = &<global>;` — store the symbol's
                // offset directly into the stack slot. BCC emits this
                // as `C7 46 dd lo hi` with a FIXUPP, saving the
                // intermediate `mov ax, offset ...; mov [bp-N], ax`
                // pair. Fixture 601.
                if !ty.is_char_like()
                    && let ExprKind::AddressOf(sym) = &value.kind
                    && self.globals.contains(sym)
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset DGROUP:_{sym}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // `<pointer-local> = <global-array>;` — array-to-
                // pointer decay. Store the array's symbol offset
                // directly. Same immediate-store shape as `= &g`.
                // Fixtures 2328, 2541.
                if ty.pointee().is_some()
                    && let ExprKind::Ident(sym) = &value.kind
                    && let Some(gty) = self.globals.type_of(sym)
                    && gty.array_elem().is_some()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset DGROUP:_{sym}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // `<stack-int> = <int-global>++` / `--` — BCC loads
                // the pre-update value into AX, stores AX to the
                // stack slot, *then* applies the memory-direct side
                // effect. Order matters: defer the inc/dec until
                // after the use. Fixture 963.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && let Some(gty) = self.globals.type_of(target)
                    && (matches!(gty, Type::Int | Type::UInt) || gty.pointee().is_some())
                {
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{target}\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{target}\r\n");
                    return;
                }
                // `<stack-int> = <char-global>++` / `--` — load AL,
                // widen via cbw (or mov ah, 0 for uchar), store AX to
                // the stack slot, then defer the memory-direct
                // inc/dec on the byte. Fixture 966.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && let Some(gty) = self.globals.type_of(target)
                    && gty.is_char_like()
                {
                    let unsigned = gty.is_unsigned();
                    let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{target}\r\n");
                    if unsigned {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\tbyte ptr DGROUP:_{target}\r\n");
                    return;
                }
                // `<stack-int> = <reg-int>++` / `--` — store the
                // pre-update register value directly to the stack
                // slot, then apply the side effect. Skips the AX
                // round-trip our generic emit_update_to_ax path
                // takes. Fixture 649.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && !src_reg.is_byte()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},{}\r\n",
                        bp_addr(off),
                        src_reg.name(),
                    );
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\t{}\r\n", src_reg.name());
                    return;
                }
                // Char-src + int-dest postinc: `int r = c++` where c
                // is in a byte register. BCC widens to AX (cbw),
                // stores AX to the int slot, then bumps the source.
                // Different from the generic `emit_update_to_ax`
                // shape which inc'd before the store. Fixture 728.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && src_reg.is_byte()
                {
                    let unsigned = self.locals.type_of(target).is_unsigned();
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", src_reg.name());
                    if unsigned {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\t{}\r\n", src_reg.name());
                    return;
                }
                // Char-src + int-dest preinc: `int r = ++c`. BCC
                // threads through AL: load c, bump AL, write back
                // to c, then widen+store to the int slot. Fixture
                // 729.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Pre,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && src_reg.is_byte()
                {
                    let unsigned = self.locals.type_of(target).is_unsigned();
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", src_reg.name());
                    let _ = write!(self.out, "\t{mnem}\tal\r\n");
                    let _ = write!(self.out, "\tmov\t{},al\r\n", src_reg.name());
                    if unsigned {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    return;
                }
                // Char `d = c++` where both d and c are byte. BCC
                // routes the byte through AL without `cbw`-widening,
                // stores to the byte stack slot, then bumps the
                // source register. Pattern: `mov al, <src>; mov
                // byte ptr [bp-N], al; inc <src>`. Fixture 725.
                if ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && src_reg.is_byte()
                {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", src_reg.name());
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\t{}\r\n", src_reg.name());
                    return;
                }
                // Char `d = ++c` where both d and c are byte. BCC
                // works through AL: load c into AL, bump AL, then
                // write back to BOTH c and d. Pattern: `mov al,
                // <src>; inc al; mov <src>, al; mov byte ptr [bp-
                // N], al`. Fixture 727.
                if ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Pre,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && src_reg.is_byte()
                {
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", src_reg.name());
                    let _ = write!(self.out, "\t{mnem}\tal\r\n");
                    let _ = write!(self.out, "\tmov\t{},al\r\n", src_reg.name());
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    return;
                }
                // Reg-to-mem copy: `<stack-local> = <reg-local>` —
                // direct `mov word ptr [bp-N], <reg>` without the AX
                // round-trip. Restricted to plain int on both sides
                // (no pointers/arrays/chars/longs). Mirror of the
                // mem-to-reg peephole in `emit_store_reg`. Fixture
                // 1145 (`t = a;` with a in SI, t on stack).
                if matches!(ty, Type::Int)
                    && let ExprKind::Ident(src_name) = &value.kind
                    && self.locals.has(src_name)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(src_name)
                    && !src_reg.is_byte()
                    && matches!(self.locals.type_of(src_name), Type::Int)
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},{}\r\n",
                        bp_addr(off),
                        src_reg.name()
                    );
                    return;
                }
                self.emit_expr_to_ax(value);
                if ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                } else {
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                }
            }
            LocalLocation::Reg(reg) => self.emit_store_reg(reg, value),
        }
    }

    /// Emit the `mov ax, <a>; cwd; idiv <b>` prefix shared by `%`
    /// fast-paths that want the remainder left in DX (rather than
    /// rounding through AX as the generic `emit_arith_op_to_ax`
    /// does). Used by the int-stack `c = a % b;` peephole in
    /// `emit_assign_local`. Fixture 546.
    fn emit_arith_setup_for_mod(&mut self, left: &Expr, right: &Expr) {
        self.emit_expr_to_ax(left);
        let src = self.resolve_operand_source(right);
        self.out.extend_from_slice(b"\tcwd\t\r\n");
        let _ = write!(self.out, "\tidiv\t{}\r\n", src.word());
    }

    /// Store `expr`'s value into register `reg`. For 16-bit registers
    /// BCC special-cases the zero-init via `xor reg,reg` (one byte
    /// shorter); 8-bit registers use plain `mov reg,0` even for zero
    /// (fixture 050/051).
    fn emit_store_reg(&mut self, reg: Reg, expr: &Expr) {
        if let Some(v) = try_const_eval(expr) {
            if reg.is_byte() {
                let v8 = v & 0xFF;
                let _ = write!(self.out, "\tmov\t{},{v8}\r\n", reg.name());
            } else if v.trailing_zeros() >= 16 {
                let _ = write!(self.out, "\txor\t{0},{0}\r\n", reg.name());
            } else {
                let v16 = v & 0xFFFF;
                let _ = write!(self.out, "\tmov\t{},{v16}\r\n", reg.name());
            }
            return;
        }
        // String-literal init: BCC emits the address as a direct
        // immediate, skipping the AX round-trip used for `&x` (which
        // is a runtime address). Fixture 088: `char *s = "hi";` →
        // `mov si, offset DGROUP:s@`.
        if let ExprKind::StringLit(bytes) = &expr.kind {
            assert!(
                !reg.is_byte(),
                "string-literal address into a byte register is impossible (pointer is 2 bytes)"
            );
            let offset = self.strings.intern(bytes);
            if offset == 0 {
                let _ = write!(self.out, "\tmov\t{},offset DGROUP:s@\r\n", reg.name());
            } else {
                let _ = write!(
                    self.out,
                    "\tmov\t{},offset DGROUP:s@+{offset}\r\n",
                    reg.name(),
                );
            }
            return;
        }
        // `&<global>` direct-to-register: same shape as the string-
        // literal init — a linker-resolved constant, so a direct
        // `mov <reg>, offset DGROUP:_<sym>` works (no AX round-trip).
        // Fixture 308 (`long *p = &g;` with p in SI).
        if let ExprKind::AddressOf(sym) = &expr.kind
            && self.globals.contains(sym)
        {
            assert!(!reg.is_byte(), "global address into a byte register is impossible (pointer is 2 bytes)");
            let _ = write!(self.out, "\tmov\t{},offset DGROUP:_{sym}\r\n", reg.name());
            return;
        }
        // Array decay to a register-resident pointer: `<reg> = <arr>`
        // where `arr` is a global array. Equivalent to `&arr[0]` —
        // and like `&<global>` above, takes the direct `mov <reg>,
        // offset DGROUP:_<sym>` form (no `lea / mov` round-trip).
        // Fixture 313 (`long *p = a;`).
        if let ExprKind::Ident(name) = &expr.kind
            && let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Array { .. })
        {
            assert!(!reg.is_byte(), "array address into a byte register is impossible");
            let _ = write!(self.out, "\tmov\t{},offset DGROUP:_{name}\r\n", reg.name());
            return;
        }
        // Pointer init from `<stack-array> + K_const`: fold the
        // element offset into the LEA's displacement. BCC pattern is
        // `lea ax, [bp+(base + K*stride)]; mov <reg>, ax` — no
        // runtime add of the stride. Fixture 1047 (`int *p = a + 1;`).
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &expr.kind
            && let ExprKind::Ident(arr_name) = &left.kind
            && self.locals.has(arr_name)
            && let Some(elem_ty) = self.locals.type_of(arr_name).array_elem()
            && let Some(k) = try_const_eval(right)
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
        {
            assert!(!reg.is_byte(), "array+const into a byte register is impossible");
            let stride = i32::from(elem_ty.size_bytes());
            let adj_off = i32::from(base_off) + (k as i32) * stride;
            let adj_off_i16 = i16::try_from(adj_off).expect("array+const offset fits in i16");
            let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(adj_off_i16));
            let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
            return;
        }
        // Reg-to-reg copy: `<reg> = <other-reg>` where the RHS is a
        // bare identifier naming another register-resident int
        // local. BCC emits `mov <dest>, <src>` directly, skipping
        // the AX round-trip. Fixture 1143 (`x = y;` with both in
        // SI/DI).
        if let ExprKind::Ident(name) = &expr.kind
            && self.locals.has(name)
            && let LocalLocation::Reg(src_reg) = self.locals.location_of(name)
            && !src_reg.is_byte()
            && !reg.is_byte()
        {
            let _ = write!(self.out, "\tmov\t{},{}\r\n", reg.name(), src_reg.name());
            return;
        }
        // Mem-to-reg copy: `<reg> = <stack-local>` where the RHS is
        // a bare identifier for a stack-resident int/uint/pointer
        // local. BCC emits `mov <reg>, word ptr [bp-N]` directly,
        // skipping the AX round-trip. Fixture 1145 (`b = t;` int),
        // 2852 (`q = p;` int pointer).
        if let ExprKind::Ident(name) = &expr.kind
            && self.locals.has(name)
            && let LocalLocation::Stack(src_off) = self.locals.location_of(name)
            && self.locals.type_of(name).is_int_like()
            && !reg.is_byte()
        {
            let _ = write!(
                self.out,
                "\tmov\t{},word ptr {}\r\n",
                reg.name(),
                bp_addr(src_off)
            );
            return;
        }
        // Mem-to-reg copy from a global: `<reg> = <global>` where the
        // RHS is a bare identifier (possibly wrapped in a pointer
        // cast) naming an int- or pointer-shaped global. BCC emits
        // `mov <reg>, word ptr DGROUP:_<sym>` (4 bytes) directly,
        // skipping the AX round-trip (a1 mem16 + mov reg, ax = 5
        // bytes). Fixture 2626 (`ip = (int *)vp;` with vp global).
        if let Some(name) = match &expr.kind {
            ExprKind::Ident(n) => Some(n.as_str()),
            ExprKind::Cast { operand, ty } if ty.pointee().is_some() => match &operand.kind {
                ExprKind::Ident(n) => Some(n.as_str()),
                _ => None,
            },
            _ => None,
        }
            && let Some(gty) = self.globals.type_of(name)
            && (gty.is_int_like() || gty.pointee().is_some())
            && !reg.is_byte()
        {
            let _ = write!(
                self.out,
                "\tmov\t{},word ptr DGROUP:_{name}\r\n",
                reg.name(),
            );
            return;
        }
        // Non-constant char init: untested. Best guess would be
        // `<compute to AL> / mov <reg>, al`, but until a fixture pins
        // the load-to-AL path, bail.
        assert!(
            !reg.is_byte(),
            "non-constant char init/assign not yet supported (no fixture)"
        );
        self.emit_expr_to_ax(expr);
        let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
    }

    /// Emit code that leaves the value of `e` in AX.
    fn emit_expr_to_ax(&mut self, e: &Expr) {
        if let Some(v) = try_const_eval(e) {
            // Narrow to 16 bits — BCC writes signed-negative constants
            // as their unsigned-wrapped form (fixture 036: `-5` →
            // `mov ax,65531`).
            let v16 = v & 0xFFFF;
            if v16 == 0 {
                self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{v16}\r\n");
            }
            return;
        }
        match &e.kind {
            ExprKind::IntLit(_) => unreachable!("literals fold via try_const_eval"),
            ExprKind::Ident(name) => {
                // A local shadows a global of the same name (fixture
                // 532), so only take the global path when no local
                // with this name is in scope.
                // Globals first: if this name is file-scope, lower
                // to a `<width> ptr DGROUP:_<name>` reference rather
                // than a stack/register access (fixtures 083–087).
                if !self.locals.has(name)
                    && let Some(gty) = self.globals.type_of(name)
                {
                    if matches!(gty, Type::Array { .. }) {
                        // Global array decay: the value of `arr` is
                        // its address (element 0). Direct
                        // `mov ax, offset DGROUP:_arr` (linker-
                        // resolved). Fixture 3437.
                        let _ = write!(
                            self.out,
                            "\tmov\tax,offset DGROUP:_{name}\r\n",
                        );
                        return;
                    }
                    if gty.is_char_like() {
                        let _ = write!(
                            self.out,
                            "\tmov\tal,byte ptr DGROUP:_{name}\r\n",
                        );
                        if gty.is_unsigned() {
                            // Unsigned char: zero-extend via `mov ah,0`
                            // (B4 00, 2 bytes) — preserves the upper
                            // bits as 0 instead of sign-extending the
                            // 7th bit. Fixture 460.
                            self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                        } else {
                            self.out.extend_from_slice(b"\tcbw\t\r\n");
                        }
                    } else {
                        let _ = write!(
                            self.out,
                            "\tmov\tax,word ptr DGROUP:_{name}\r\n",
                        );
                    }
                    return;
                }
                let ty = self.locals.type_of(name).clone();
                // Array-name decay: when the name refers to a local
                // of array type and we're reading its *value*, the
                // value is the address of element 0. Fixture 090
                // (`int *p = a;`) and fixture 095 (`sum(a)`) both
                // exercise this. Emitted exactly like `&a[0]`.
                if matches!(ty, Type::Array { .. }) {
                    let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                        unreachable!("array `{name}` should be stack-resident");
                    };
                    let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
                    return;
                }
                match self.locals.location_of(name) {
                    LocalLocation::Stack(off) if ty.is_char_like() => {
                        // Char on stack into AX: load AL then widen.
                        // Signed: `cbw` (1 byte). Unsigned:
                        // `mov ah,0` (2 bytes). Fixture 461.
                        let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                        if ty.is_unsigned() {
                            self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                        } else {
                            self.out.extend_from_slice(b"\tcbw\t\r\n");
                        }
                    }
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                    }
                    LocalLocation::Reg(reg) if reg.is_byte() => {
                        // Char in a byte register into AX: copy AL then
                        // widen. Fixture 053 / 461 (register-resident
                        // uchar). Signed picks `cbw`; unsigned picks
                        // `mov ah,0`.
                        let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                        if ty.is_unsigned() {
                            self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                        } else {
                            self.out.extend_from_slice(b"\tcbw\t\r\n");
                        }
                    }
                    LocalLocation::Reg(reg) => {
                        let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    }
                }
            }
            ExprKind::BinOp { op, left, right } => {
                if op.is_comparison() {
                    self.emit_comparison_as_value(e.span.start, *op, left, right);
                } else {
                    // `<char_lvalue> <bitop> <char_lvalue>` — byte op
                    // in AL, single cbw at the end. BCC emits
                    // `mov al, [l]; or al, [r]; cbw` for the
                    // char-or-char case (fixture 1375). Pre-peephole
                    // we widened first and used the word form. Limit
                    // to bitops where C's per-bit semantics are the
                    // same at byte and word width once widened.
                    if matches!(op, BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor)
                        && let Some((l_name, l_off, l_ty)) =
                            self.try_lvalue_chain_addr(left)
                        && let Some((r_name, r_off, r_ty)) =
                            self.try_lvalue_chain_addr(right)
                        && l_ty.is_char_like()
                        && r_ty.is_char_like()
                    {
                        let l_addr = self.resolve_chain_addr(&l_name, l_off);
                        let r_addr = self.resolve_chain_addr(&r_name, r_off);
                        if let (Some(la), Some(ra)) = (l_addr, r_addr) {
                            let mnem = match op {
                                BinOp::BitOr => "or",
                                BinOp::BitAnd => "and",
                                BinOp::BitXor => "xor",
                                _ => unreachable!(),
                            };
                            let _ = write!(self.out, "\tmov\tal,byte ptr {la}\r\n");
                            let _ = write!(self.out, "\t{mnem}\tal,byte ptr {ra}\r\n");
                            self.out.extend_from_slice(b"\tcbw\t\r\n");
                            return;
                        }
                    }
                    // `<ptr-typed lvalue> + K` / `- K` — C scales the
                    // constant by the pointee size. Always route as
                    // Add with the (possibly negative) scaled byte
                    // count so the ±1/±2 inc/dec peephole fires for
                    // small steps (fixture 2922: `p = p + 1` →
                    // `inc ax; inc ax`) and the AX-accumulator add
                    // form fires for larger ones (fixture 3557).
                    // Fixtures 3557, 3256, 3382, 2922.
                    if matches!(op, BinOp::Add | BinOp::Sub)
                        && let ExprKind::Ident(pname) = &left.kind
                        && let Some(pointee) = self.ident_pointee(pname)
                        && let Some(k) = try_const_eval(right)
                        && pointee.size_bytes() > 1
                    {
                        let stride = i32::from(pointee.size_bytes());
                        let sign = if matches!(op, BinOp::Add) { 1i32 } else { -1 };
                        let bytes = sign.wrapping_mul(k as i32).wrapping_mul(stride);
                        let scaled = (bytes as u32) & 0xFFFF;
                        self.emit_expr_to_ax(left);
                        // Pointers compare as unsigned but for the
                        // add-or-inc emission we want the Add form
                        // chosen regardless (no `sub` canonicalization).
                        emit_op_with_source(
                            self.out,
                            BinOp::Add,
                            &OperandSource::Immediate(scaled),
                            true,
                        );
                        return;
                    }
                    // Commutative-op operand swap: BCC prefers the
                    // non-constant operand in AX so the immediate or
                    // simpler operand can be the binop's RHS. Fixture
                    // 200 (`3 + *p` → `*p + 3`).
                    let (left, right) = if op.is_commutative()
                        && try_const_eval(left).is_some()
                        && try_const_eval(right).is_none()
                    {
                        (right.as_ref(), left.as_ref())
                    } else {
                        (left.as_ref(), right.as_ref())
                    };
                    // Associative const-fold for Add/Sub chains:
                    // `((X ± K1) ± K2) ± K3 ...` → `X + (K_total)`.
                    // Walks down the left spine collecting constant
                    // additions/subtractions, then emits the variable
                    // base once with a single combined `add ax, K`.
                    // Lets BCC's smaller form fire for arbitrarily
                    // deep chains. Fixtures 2019, 2075 (`x + 1 + 1
                    // + 1`), 2076.
                    if matches!(op, BinOp::Add | BinOp::Sub)
                        && let Some(k_outer) = try_const_eval(right)
                    {
                        let outer_sign = if matches!(op, BinOp::Add) { 1i32 } else { -1 };
                        let mut total: i32 = outer_sign * (k_outer as i32);
                        let mut base: &Expr = left;
                        loop {
                            if let ExprKind::BinOp { op: bop, left: bl, right: br } = &base.kind
                                && matches!(bop, BinOp::Add | BinOp::Sub)
                                && let Some(k) = try_const_eval(br)
                                && try_const_eval(bl).is_none()
                            {
                                let s = if matches!(bop, BinOp::Add) { 1i32 } else { -1 };
                                total += s * (k as i32);
                                base = bl;
                                continue;
                            }
                            break;
                        }
                        // Only fold if we collapsed at least one nested
                        // const (i.e. base != left).
                        if !std::ptr::eq(base, left) {
                            let unsigned = self.expr_is_unsigned(base);
                            self.emit_expr_to_ax(base);
                            let total_masked = (total as u32) & 0xFFFF;
                            emit_op_with_source(
                                self.out,
                                BinOp::Add,
                                &OperandSource::Immediate(total_masked),
                                unsigned,
                            );
                            return;
                        }
                        // base == left: no inner const to fold; fall
                        // through to the normal binop path.
                    }
                    // Shifts encode the left operand's signedness in
                    // the mnemonic (`shr` vs `sar`); everything else
                    // is signedness-agnostic at the instruction level.
                    // Use the promoted-type variant: char/uchar both
                    // become signed `int` before a shift, so they
                    // get `sar`. Fixture 1015.
                    let unsigned = if matches!(op, BinOp::Shr) {
                        self.expr_shift_is_unsigned(left)
                    } else {
                        self.expr_is_unsigned(left)
                    };
                    // RHS-clobbers-AX path: when the right operand is a
                    // call, a char ident (whose load + cbw widen
                    // clobbers AX), or a nested non-constant BinOp
                    // (which produces its result in AX and so
                    // clobbers any LHS already there), BCC evaluates
                    // RHS first, pushes the result, then evaluates
                    // LHS into AX and pops the saved result into DX
                    // before applying the op. Fixture 593 (`n + sum(n
                    // -1)`), 616 (`a + b` with b a char param), 645
                    // (`x + y * 2`).
                    let rhs_clobbers_ax = matches!(right.kind, ExprKind::Call { .. })
                        || self.expr_is_char_load(right)
                        || matches!(right.kind, ExprKind::Cast { .. } | ExprKind::Ternary { .. })
                        || (matches!(right.kind, ExprKind::BinOp { .. })
                            && try_const_eval(right).is_none());
                    // Callee-preserved register peephole: when the
                    // left operand is a bare ident that lives in
                    // SI or DI (BCC's int register pool sites that
                    // get saved across calls), we can skip the
                    // push/pop dance and apply the op directly with
                    // the register as the source. Fixtures 1697,
                    // 2255 (`n * fact(n-1)` with n in SI → `imul
                    // si` instead of `push ax; mov ax,si; pop dx;
                    // imul dx`).
                    let left_preserved_reg = if let ExprKind::Ident(name) = &left.kind
                        && self.locals.has(name)
                        && let LocalLocation::Reg(reg) = self.locals.location_of(name)
                        && matches!(reg, Reg::Si | Reg::Di)
                    {
                        Some(reg)
                    } else {
                        None
                    };
                    if rhs_clobbers_ax
                        && let Some(reg) = left_preserved_reg
                    {
                        self.emit_expr_to_ax(right);
                        emit_op_with_source(
                            self.out,
                            *op,
                            &OperandSource::Reg(reg),
                            unsigned,
                        );
                    } else if rhs_clobbers_ax
                        && matches!(op, BinOp::Mul)
                        && let Some(left_src) = self.try_memory_source(left)
                    {
                        // `<int_mem> * <char>` shape: BCC emits
                        // `mov al,<char>; cbw; imul word ptr
                        // <int_mem>` directly, avoiding the push/pop
                        // dance. Other commutative ops (Add/Or/And/
                        // Xor) keep the push/pop — BCC specifically
                        // recognizes the mul-mem-direct shape.
                        // Fixture 1228 (`a * c`).
                        self.emit_expr_to_ax(right);
                        emit_op_with_source(self.out, *op, &left_src, unsigned);
                    } else if rhs_clobbers_ax && {
                        // LHS-first when LHS itself clobbers AX
                        // (nested binop / char-load / call / cast /
                        // ternary). RHS-first when LHS is simple
                        // (it can be loaded last for free).
                        // Div/Mod/Mul: always LHS-first (need AX as
                        // accumulator for the implicit operand).
                        let lhs_clobbers_ax =
                            matches!(left.kind, ExprKind::Call { .. })
                            || self.expr_is_char_load(left)
                            || matches!(left.kind,
                                ExprKind::Cast { .. } | ExprKind::Ternary { .. })
                            || (matches!(left.kind, ExprKind::BinOp { .. })
                                && try_const_eval(left).is_none());
                        matches!(op, BinOp::Div | BinOp::Mod | BinOp::Mul)
                            || lhs_clobbers_ax
                    } {
                        // Div/Mod scratch is BX (DX is clobbered by
                        // cwd / xor dx,dx). Mul scratch is DX (imul
                        // writes DX:AX, no other reg-clobbering setup).
                        // Add/Sub/etc with LHS-clobbering RHS-also-
                        // clobbering: DX as scratch is fine. Fixtures
                        // 087 (`a + b + c` w/ char c), 1357, 1625,
                        // 1223, 2006.
                        let scratch = if matches!(op, BinOp::Div | BinOp::Mod) {
                            Reg::Bx
                        } else {
                            Reg::Dx
                        };
                        self.emit_expr_to_ax(left);
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        self.emit_expr_to_ax(right);
                        let _ = write!(self.out, "\tmov\t{},ax\r\n", scratch.name());
                        self.out.extend_from_slice(b"\tpop\tax\r\n");
                        emit_op_with_source(
                            self.out,
                            *op,
                            &OperandSource::Reg(scratch),
                            unsigned,
                        );
                    } else if rhs_clobbers_ax {
                        self.emit_expr_to_ax(right);
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        self.emit_expr_to_ax(left);
                        self.out.extend_from_slice(b"\tpop\tdx\r\n");
                        emit_op_with_source(
                            self.out,
                            *op,
                            &OperandSource::Reg(Reg::Dx),
                            unsigned,
                        );
                    } else {
                        self.emit_expr_to_ax(left);
                        self.emit_binary_right(*op, right, unsigned);
                    }
                }
            }
            ExprKind::Unary { op, operand } => self.emit_unary(*op, operand),
            ExprKind::Logical { op, left, right } => {
                self.emit_logical_to_ax(e.span.start, *op, left, right);
            }
            ExprKind::Update { target, op, position } => {
                self.emit_update_to_ax(target, *op, *position);
            }
            ExprKind::AssignExpr { target, value } => {
                // Chained assignment `a = b = c = 5;` lands here via
                // the outer statement's RHS. Recursively evaluate the
                // inner value into AX, then store AX into `target`.
                // AX still holds the assigned value so the outer
                // store reuses it. Fixture 500.
                self.emit_expr_to_ax(value);
                if self.globals.contains(target) {
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{target},ax\r\n");
                } else {
                    match self.locals.location_of(target) {
                        LocalLocation::Stack(off) => {
                            let _ = write!(
                                self.out,
                                "\tmov\tword ptr {},ax\r\n",
                                bp_addr(off)
                            );
                        }
                        LocalLocation::Reg(reg) => {
                            let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
                        }
                    }
                }
            }
            ExprKind::Call { name, args } => {
                self.emit_call(name, args);
                // Char-returning callee leaves only AL meaningful;
                // widen to AX so the caller sees a full int. Signed
                // char uses cbw; uchar uses `mov ah, 0`. Fixture 562.
                if let Some(ret) = self.signatures.ret_ty_of(name)
                    && ret.is_char_like()
                {
                    let ret = ret.clone();
                    self.emit_widen_al(&ret);
                }
            }
            ExprKind::AddressOf(name) => self.emit_address_of(name),
            ExprKind::AddressOfArrayElem { array, byte_offset } => {
                // `&<arr>[K]` at runtime — for a global array, emit
                // the symbol+offset as an immediate. For a stack-
                // resident local array, emit `lea ax, [bp+off+K]`
                // where `off` is the local's bp-offset. Fixture 486.
                if self.globals.contains(array) {
                    if *byte_offset == 0 {
                        let _ = write!(
                            self.out,
                            "\tmov\tax,offset DGROUP:_{array}\r\n",
                        );
                    } else {
                        let _ = write!(
                            self.out,
                            "\tmov\tax,offset DGROUP:_{array}+{byte_offset}\r\n",
                        );
                    }
                } else {
                    let LocalLocation::Stack(base_off) = self.locals.location_of(array) else {
                        panic!("local array `{array}` should be stack-resident");
                    };
                    let total = base_off + i16::try_from(*byte_offset).unwrap_or(i16::MAX);
                    let _ = write!(
                        self.out,
                        "\tlea\tax,word ptr {}\r\n",
                        bp_addr(total),
                    );
                }
            }
            ExprKind::Deref(operand) => self.emit_deref_to_ax(operand),
            ExprKind::ArrayIndex { array, index } => {
                self.emit_array_index_to_ax(array, index);
            }
            ExprKind::StringLit(bytes) => {
                // A bare string literal in value position is its
                // address (the C decay rule). We don't have a
                // fixture, but `mov ax, offset DGROUP:s@<offset>`
                // is the expected shape.
                let offset = self.strings.intern(bytes);
                if offset == 0 {
                    let _ = write!(self.out, "\tmov\tax,offset DGROUP:s@\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tax,offset DGROUP:s@+{offset}\r\n");
                }
            }
            ExprKind::Member { base, field, kind } => {
                self.emit_member_to_ax(base, field, *kind);
            }
            ExprKind::Ternary { cond, then_value, else_value } => {
                self.emit_ternary_to_ax(e.span.start, cond, then_value, else_value);
            }
            ExprKind::Cast { ty, operand } => {
                self.emit_cast_to_ax(ty, operand);
            }
            ExprKind::InitList { .. } => {
                panic!("initializer list not legal in value position");
            }
            ExprKind::Comma { left, right } => {
                // Comma operator: emit left for side effects (as if
                // it were an expression statement) then emit right's
                // value into AX. Fixture 469.
                self.emit_expr_discard(left);
                self.emit_expr_to_ax(right);
            }
        }
    }

    /// Lower `(<ty>) <operand>` into AX. The narrowing int→char case
    /// (the only one with a fixture today, 170) fuses the load with
    /// the truncate: `mov al, byte ptr [bp-N]; cbw` when the operand
    /// is a stack-int local — exactly what BCC emits for reading a
    /// char-typed local from that offset. Widening / no-op casts just
    /// evaluate the operand into AX.
    fn emit_cast_to_ax(&mut self, ty: &Type, operand: &Expr) {
        if ty.is_char_like() {
            // `(char|uchar) <lvalue>` — byte-load the low byte of
            // the source, then widen per the cast type's signedness.
            // Signed → cbw; unsigned → mov ah, 0. Source can be any
            // int/char lvalue (including char-to-uchar narrowing,
            // fixture 1524). Width-resolving comes from
            // try_lvalue_chain_addr/resolve_chain_addr.
            if let Some((src_name, src_off, _)) = self.try_lvalue_chain_addr(operand)
                && let Some(src_addr) = self.resolve_chain_addr(&src_name, src_off)
            {
                let _ = write!(self.out, "\tmov\tal,byte ptr {src_addr}\r\n");
                if ty.is_unsigned() {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
                return;
            }
        }
        self.emit_expr_to_ax(operand);
    }

    /// Emit a ternary `cond ? then : else` into AX. The shape BCC
    /// produces (fixture 166): test the condition with a reverse
    /// branch to the false-arm label, emit the then-value into AX,
    /// jump to the merge label, emit the false-arm label + else-value,
    /// emit the merge label. Slot layout matches an `if`-`else`:
    /// base+1 is the false arm, base+2 is the merge target.
    fn emit_ternary_to_ax(
        &mut self,
        span_start: u32,
        cond: &Expr,
        then_value: &Expr,
        else_value: &Expr,
    ) {
        let base = self.label_plan.base(span_start);
        // Some compare shapes need an explicit then-entry label so the
        // 3-jump long-vs-K cmp pattern (and `||` short-circuit-to-true)
        // can land at the start of the then-arm. Mirrors the same
        // pre-allocation logic in `emit_if`. Fixture 433 (`g > 0 ? 1
        // : 0` for long `g`).
        let cond_has_top_or = matches!(
            cond.kind,
            ExprKind::Logical { op: LogicalOp::Or, .. }
        );
        let needs_then_entry = cond_has_top_or
            || self.is_long_signed_globals_cmp(cond)
            || self.is_long_signed_const_cmp(cond)
            || self.is_long_vs_int_cmp(cond)
            || self.is_long_vs_int_ne(cond)
            || self.is_long_ne_const(cond);
        let true_slot = if needs_then_entry { Some(base) } else { None };
        let false_slot = base + 1;
        let merge_slot = base + 2;
        self.emit_cond_branch(cond, true_slot, Some(false_slot));
        if let Some(t) = true_slot {
            self.emit_label(t);
        }
        self.emit_expr_to_ax(then_value);
        let _ = write!(
            self.out,
            "\tjmp\tshort {}\r\n",
            self.label_ref(merge_slot),
        );
        self.emit_label(false_slot);
        self.emit_expr_to_ax(else_value);
        self.emit_label(merge_slot);
    }

    fn emit_comparison_as_value(
        &mut self,
        cmp_span_start: u32,
        op: BinOp,
        left: &Expr,
        right: &Expr,
    ) {
        let base = self.label_plan.base(cmp_span_start);
        let false_slot = base + 1;
        let end_slot = base + 2;
        let unsigned = self.cmp_is_unsigned(left, right);
        let inv = op.jump_if_false(unsigned).expect("comparison op has inverse jump");

        self.emit_compare(left, right);
        let _ = write!(self.out, "\t{inv}\tshort {}\r\n", self.label_ref(false_slot));
        self.out.extend_from_slice(b"\tmov\tax,1\r\n");
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(end_slot));
        self.emit_label(false_slot);
        self.out.extend_from_slice(b"\txor\tax,ax\r\n");
        self.emit_label(end_slot);
    }

    /// Emit the right-hand side of a binary op, applying it to AX.
    fn emit_binary_right(&mut self, op: BinOp, e: &Expr, unsigned: bool) {
        // ±1 / +2 peephole: BCC emits `inc ax` / `dec ax` for ±1 (1
        // byte each vs. 3 for `add ax, 1` / `sub ax, 1`), and a pair
        // of `inc ax` for +2 (2 bytes vs. 3). Notably -2 does NOT
        // collapse to `dec ax; dec ax` — BCC keeps `add ax, -2`
        // (3 bytes, AX-accum imm16). Fixtures 027–031 (±1), 076 case 1
        // (+2 → inc/inc), 2074/1277 (-2 → `add ax, -2`).
        if let Some(v) = try_const_eval(e)
            && ((matches!(op, BinOp::Add) && (v == 1 || v == 2))
                || (matches!(op, BinOp::Sub) && v == 1))
        {
            let mnemonic = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            for _ in 0..v {
                let _ = write!(self.out, "\t{mnemonic}\tax\r\n");
            }
            return;
        }
        // Char-on-right widening dance (fixture 087: `a + b + c` with
        // `c` a char global). Loading a char clobbers AX, so the
        // running sum gets pushed, the char loaded + widened to AX,
        // saved to DX, the sum restored, then combined. The same
        // pattern would apply to a char *stack* local but we have no
        // fixture pinning it yet.
        if let ExprKind::Ident(name) = &e.kind
            && self.ident_is_char(name)
        {
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.emit_expr_to_ax(e);
            self.out.extend_from_slice(b"\tmov\tdx,ax\r\n");
            self.out.extend_from_slice(b"\tpop\tax\r\n");
            emit_op_with_source_opts(
                self.out,
                op,
                &OperandSource::Reg(Reg::Dx),
                unsigned,
                self.skip_mod_to_ax,
            );
            return;
        }
        let src = self.resolve_operand_source(e);
        emit_op_with_source_opts(self.out, op, &src, unsigned, self.skip_mod_to_ax);
    }

    /// True iff `name` refers to an identifier (global or local)
    /// whose static type is `char`. Used by `emit_binary_right` to
    /// detect when the right operand needs the widening dance.
    fn ident_is_char(&self, name: &str) -> bool {
        if let Some(ty) = self.globals.type_of(name) {
            return ty.is_char_like();
        }
        // The locals analyzer panics on unknown names, so only ask
        // if there's no global match.
        matches!(self.locals.type_of(name), Type::Char)
    }

    /// True when `e` evaluates to a char-typed value via a memory
    /// load that would clobber AX (`mov al, ...; cbw`). Covers bare
    /// char idents, char array elements, and char struct fields.
    /// Used by the binop RHS path to decide whether to push the LHS
    /// before evaluating the RHS — otherwise the load would
    /// overwrite AX. Fixture 2006 (`a[0] + a[126]` for char a[]).
    fn expr_is_char_load(&self, e: &Expr) -> bool {
        if let ExprKind::Ident(name) = &e.kind {
            return self.ident_is_char(name);
        }
        if let Some((_, _, ty)) = self.try_lvalue_chain_addr(e) {
            return ty.is_char_like();
        }
        false
    }

    /// Pointee type of `name` if it's a pointer-typed identifier;
    /// `None` for non-pointers and unknown names. Used by the
    /// pointer-arithmetic stride scaling (fixture 3557).
    fn ident_pointee(&self, name: &str) -> Option<Type> {
        if let Some(ty) = self.globals.type_of(name) {
            return ty.pointee().cloned();
        }
        if self.locals.has(name) {
            return self.locals.type_of(name).pointee().cloned();
        }
        None
    }

    /// Resolve the right operand to a textual asm source operand. Today
    /// either an immediate (constant-foldable), a register-resident
    /// local, or a `word ptr [bp-N]` stack local.
    /// Best-effort type lookup for the RHS of a long-compound
    /// widening branch. Today only recognizes bare-ident sources
    /// (`g += x`). Returns `None` for compound RHS expressions —
    /// those would need a more general typing pass before they
    /// can pick the right widening shape.
    fn rhs_type_for_long_widening(&self, e: &ExprKind) -> Option<Type> {
        let name = match e {
            ExprKind::Ident(n) => n,
            _ => return None,
        };
        if let Some(t) = self.globals.type_of(name) {
            Some(t.clone())
        } else if self.locals.has(name) {
            Some(self.locals.type_of(name).clone())
        } else {
            None
        }
    }

    /// Resolve an RHS expression to a `byte ptr <addr>` form
    /// pointing at its low byte. Supports `Ident` (global or
    /// stack local), `ArrayIndex` with constant index, and
    /// `Member` of a stack or global struct. Used by the shift
    /// arm to load CL with the shift count. Fixture 826.
    fn rhs_byte_addr(&self, e: &ExprKind) -> Option<String> {
        match e {
            ExprKind::Ident(n) => {
                if self.globals.contains(n) {
                    Some(format!("byte ptr DGROUP:_{n}"))
                } else if self.locals.has(n) {
                    let LocalLocation::Stack(off) = self.locals.location_of(n) else {
                        return None;
                    };
                    Some(format!("byte ptr {}", bp_addr(off)))
                } else {
                    None
                }
            }
            ExprKind::ArrayIndex { array, index } => {
                let ExprKind::Ident(arr_name) = &array.kind else { return None };
                let k = try_const_eval(index)?;
                let arr_ty = if self.globals.contains(arr_name) {
                    self.globals.type_of(arr_name)?.clone()
                } else if self.locals.has(arr_name) {
                    self.locals.type_of(arr_name).clone()
                } else {
                    return None;
                };
                let Type::Array { elem, .. } = arr_ty else { return None };
                let stride = u32::from(elem.size_bytes());
                let off = (k as u32).wrapping_mul(stride);
                if self.globals.contains(arr_name) {
                    let addr = if off == 0 {
                        format!("DGROUP:_{arr_name}")
                    } else {
                        format!("DGROUP:_{arr_name}+{off}")
                    };
                    Some(format!("byte ptr {addr}"))
                } else {
                    let LocalLocation::Stack(base) = self.locals.location_of(arr_name) else {
                        return None;
                    };
                    let total = base + i16::try_from(off).ok()?;
                    Some(format!("byte ptr {}", bp_addr(total)))
                }
            }
            ExprKind::Member { base, field, .. } => {
                let ExprKind::Ident(base_name) = &base.kind else { return None };
                let base_ty = if self.globals.contains(base_name) {
                    self.globals.type_of(base_name)?.clone()
                } else if self.locals.has(base_name) {
                    self.locals.type_of(base_name).clone()
                } else {
                    return None;
                };
                let (field_off, _) = base_ty.field(field)?;
                if self.globals.contains(base_name) {
                    let addr = if field_off == 0 {
                        format!("DGROUP:_{base_name}")
                    } else {
                        format!("DGROUP:_{base_name}+{field_off}")
                    };
                    Some(format!("byte ptr {addr}"))
                } else {
                    let LocalLocation::Stack(base_off) = self.locals.location_of(base_name) else {
                        return None;
                    };
                    let total = base_off + i16::try_from(field_off).ok()?;
                    Some(format!("byte ptr {}", bp_addr(total)))
                }
            }
            _ => None,
        }
    }

    /// Resolve a long-typed RHS expression to its (low, high)
    /// address-string halves plus the result type. Supports
    /// `Ident` (any long global or stack local), `ArrayIndex`
    /// with constant index over a long array, and `Member` of
    /// a struct whose field is long. Used by the long+long arm
    /// to accept array elements and members. Fixture 829.
    fn long_rhs_halves(&self, e: &ExprKind) -> Option<(String, String, Type)> {
        match e {
            ExprKind::Ident(n) => {
                let ty = if let Some(t) = self.globals.type_of(n) {
                    t.clone()
                } else if self.locals.has(n) {
                    self.locals.type_of(n).clone()
                } else {
                    return None;
                };
                if !ty.is_long_like() { return None; }
                let (lo, hi) = self.long_halves_of(n);
                Some((lo, hi, ty))
            }
            ExprKind::ArrayIndex { array, index } => {
                let ExprKind::Ident(arr_name) = &array.kind else { return None };
                let k = try_const_eval(index)?;
                let arr_ty = if self.globals.contains(arr_name) {
                    self.globals.type_of(arr_name)?.clone()
                } else if self.locals.has(arr_name) {
                    self.locals.type_of(arr_name).clone()
                } else {
                    return None;
                };
                let Type::Array { elem, .. } = arr_ty else { return None };
                if !elem.is_long_like() { return None; }
                let stride = u32::from(elem.size_bytes());
                let off = (k as u32).wrapping_mul(stride);
                let (lo, hi) = if self.globals.contains(arr_name) {
                    let lo = if off == 0 {
                        format!("DGROUP:_{arr_name}")
                    } else {
                        format!("DGROUP:_{arr_name}+{off}")
                    };
                    let hi = format!("DGROUP:_{arr_name}+{}", off + 2);
                    (lo, hi)
                } else {
                    let LocalLocation::Stack(base) = self.locals.location_of(arr_name) else {
                        return None;
                    };
                    let lo_off = base + i16::try_from(off).ok()?;
                    let hi_off = lo_off + 2;
                    (bp_addr(lo_off), bp_addr(hi_off))
                };
                Some((lo, hi, (*elem).clone()))
            }
            ExprKind::Member { base, field, .. } => {
                let ExprKind::Ident(base_name) = &base.kind else { return None };
                let base_ty = if self.globals.contains(base_name) {
                    self.globals.type_of(base_name)?.clone()
                } else if self.locals.has(base_name) {
                    self.locals.type_of(base_name).clone()
                } else {
                    return None;
                };
                let (field_off, field_ty) = base_ty.field(field)?;
                if !field_ty.is_long_like() { return None; }
                let off = u32::from(field_off);
                let (lo, hi) = if self.globals.contains(base_name) {
                    let lo = if off == 0 {
                        format!("DGROUP:_{base_name}")
                    } else {
                        format!("DGROUP:_{base_name}+{off}")
                    };
                    let hi = format!("DGROUP:_{base_name}+{}", off + 2);
                    (lo, hi)
                } else {
                    let LocalLocation::Stack(base_off) = self.locals.location_of(base_name) else {
                        return None;
                    };
                    let lo_off = base_off + i16::try_from(off).ok()?;
                    let hi_off = lo_off + 2;
                    (bp_addr(lo_off), bp_addr(hi_off))
                };
                Some((lo, hi, field_ty))
            }
            _ => None,
        }
    }

    /// Resolve an RHS expression to a DGROUP-relative address
    /// string (`DGROUP:_<name>[+<offset>]`) plus the resulting
    /// type, if it lives entirely in one DGROUP slot. Supports
    /// `Ident` (whole global), `ArrayIndex` with constant index
    /// (`a[K]`), and `Member` with `.` (`s.field`). Returns
    /// `None` for stack-resident RHS or non-foldable expressions.
    /// Used by the int-global Mul/Div arm to pick an `imul/idiv
    /// word ptr <addr>` mem operand.
    fn global_int_rhs_addr(&self, e: &ExprKind) -> Option<(String, Type)> {
        match e {
            ExprKind::Ident(n) => {
                let ty = self.globals.type_of(n)?.clone();
                Some((format!("DGROUP:_{n}"), ty))
            }
            ExprKind::ArrayIndex { array, index } => {
                let ExprKind::Ident(arr_name) = &array.kind else { return None };
                if !self.globals.contains(arr_name) { return None; }
                let k = try_const_eval(index)?;
                let arr_ty = self.globals.type_of(arr_name)?.clone();
                let Type::Array { elem, .. } = arr_ty else { return None };
                let stride = u32::from(elem.size_bytes());
                let off = (k as u32).wrapping_mul(stride);
                let addr = if off == 0 {
                    format!("DGROUP:_{arr_name}")
                } else {
                    format!("DGROUP:_{arr_name}+{off}")
                };
                Some((addr, (*elem).clone()))
            }
            ExprKind::Member { base, field, .. } => {
                let ExprKind::Ident(base_name) = &base.kind else { return None };
                if !self.globals.contains(base_name) { return None; }
                let base_ty = self.globals.type_of(base_name)?.clone();
                let (field_off, field_ty) = base_ty.field(field)?;
                let off = u32::from(field_off);
                let addr = if off == 0 {
                    format!("DGROUP:_{base_name}")
                } else {
                    format!("DGROUP:_{base_name}+{off}")
                };
                Some((addr, field_ty))
            }
            _ => None,
        }
    }

    /// Like `rhs_type_for_long_widening` but also resolves
    /// `ArrayIndex` (returning element type), `Deref` (returning
    /// pointee type), `Member` (returning field type), and
    /// `Unary` (returning operand type, since neg/bitnot don't
    /// widen). Used by the int-global compound arm to accept
    /// `g += a[K]`, `g += *p`, `g += s.x`, `g += -y`. Fixtures
    /// 821, 822, 823, 851.
    fn rhs_int_compound_type(&self, e: &ExprKind) -> Option<Type> {
        if let Some(t) = self.rhs_type_for_long_widening(e) {
            return Some(t);
        }
        match e {
            ExprKind::Unary { operand, .. } => self.rhs_int_compound_type(&operand.kind),
            ExprKind::IntLit(_) => Some(Type::Int),
            // Function calls return into AX under BCC's small-
            // model convention. Assume int return; long-returning
            // calls would route through a separate path. Fixture 854.
            ExprKind::Call { .. } => Some(Type::Int),
            // `!y` and `a && b` / `a || b` yield 0/1 in AX, int-
            // typed. Fixture 856 (`g += !y`).
            ExprKind::Logical { .. } => Some(Type::Int),
            // Cast to an int-family type. The cast's target type
            // determines the result. Fixture 857 (`g += (int)c`).
            ExprKind::Cast { ty, .. } => {
                if matches!(ty, Type::Int | Type::UInt | Type::Char | Type::UChar) {
                    Some(ty.clone())
                } else {
                    None
                }
            }
            // Comma operator: type is the last subexpression's
            // type. Fixture 858.
            ExprKind::Comma { right, .. } => self.rhs_int_compound_type(&right.kind),
            // Assignment expression: yields the assigned value
            // in AX. Type comes from the target ident. Fixture 859.
            ExprKind::AssignExpr { target, .. } => {
                if let Some(t) = self.globals.type_of(target) {
                    Some(t.clone())
                } else if self.locals.has(target) {
                    Some(self.locals.type_of(target).clone())
                } else {
                    None
                }
            }
            // Ternary in int-typed branches resolves to int.
            // Fixture 855.
            ExprKind::Ternary { then_value, else_value, .. } => {
                let lt = self.rhs_int_compound_type(&then_value.kind)?;
                let rt = self.rhs_int_compound_type(&else_value.kind)?;
                if lt.is_long_like() || rt.is_long_like() {
                    return None;
                }
                Some(Type::Int)
            }
            ExprKind::BinOp { left, right, .. } => {
                // If both operands resolve to non-long int-family
                // types, the BinOp result is int-typed. Used for
                // sub-expression RHS in int compound (fixture 852).
                let lt = self.rhs_int_compound_type(&left.kind)?;
                let rt = self.rhs_int_compound_type(&right.kind)?;
                if lt.is_long_like() || rt.is_long_like() {
                    return None;
                }
                Some(Type::Int)
            }
            ExprKind::ArrayIndex { array, .. } => {
                let ExprKind::Ident(arr_name) = &array.kind else { return None };
                let ty = if let Some(t) = self.globals.type_of(arr_name) {
                    t.clone()
                } else if self.locals.has(arr_name) {
                    self.locals.type_of(arr_name).clone()
                } else {
                    return None;
                };
                match ty {
                    Type::Array { ref elem, .. } => Some((**elem).clone()),
                    _ => None,
                }
            }
            ExprKind::Deref(inner) => {
                let ExprKind::Ident(p_name) = &inner.kind else { return None };
                let ty = if let Some(t) = self.globals.type_of(p_name) {
                    t.clone()
                } else if self.locals.has(p_name) {
                    self.locals.type_of(p_name).clone()
                } else {
                    return None;
                };
                ty.pointee().cloned()
            }
            ExprKind::Member { base, field, .. } => {
                let ExprKind::Ident(base_name) = &base.kind else { return None };
                let base_ty = if let Some(t) = self.globals.type_of(base_name) {
                    t.clone()
                } else if self.locals.has(base_name) {
                    self.locals.type_of(base_name).clone()
                } else {
                    return None;
                };
                base_ty.field(field).map(|(_, ty)| ty)
            }
            _ => None,
        }
    }

    /// Resolve a long-type lookup by name across globals and
    /// locals. Used by the long-compound-with-long-RHS path
    /// (fixtures 744 / 745) to accept either source kind.
    fn lhs_long_type(&self, name: &str) -> Option<Type> {
        if let Some(t) = self.globals.type_of(name) {
            Some(t.clone())
        } else if self.locals.has(name) {
            Some(self.locals.type_of(name).clone())
        } else {
            None
        }
    }
    fn rhs_long_type_of_ident(&self, name: &str) -> Option<Type> {
        self.lhs_long_type(name)
    }
    /// Format the (low, high) word-pointer address strings for a
    /// long-type identifier (without the `word ptr` prefix —
    /// callers add that themselves so the same helper covers both
    /// load and store).
    fn long_halves_of(&self, name: &str) -> (String, String) {
        if self.globals.contains(name) {
            (
                format!("DGROUP:_{name}"),
                format!("DGROUP:_{name}+2"),
            )
        } else {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long never sits in a register");
            };
            (bp_addr(off), bp_addr(off + 2))
        }
    }

    fn resolve_operand_source(&self, e: &Expr) -> OperandSource {
        if let Some(v) = try_const_eval(e) {
            return OperandSource::Immediate(v);
        }
        match &e.kind {
            ExprKind::Ident(name) => {
                if self.globals.contains(name) {
                    return OperandSource::Global(name.clone());
                }
                match self.locals.location_of(name) {
                    LocalLocation::Stack(off) => OperandSource::Local(off),
                    LocalLocation::Reg(reg) => OperandSource::Reg(reg),
                }
            }
            ExprKind::IntLit(_) => unreachable!("literals fold via try_const_eval"),
            ExprKind::Call { .. } => {
                panic!("call as right operand not yet supported (need to preserve AX)")
            }
            ExprKind::BinOp { .. } => {
                panic!("nested non-constant right operand not yet supported")
            }
            ExprKind::Unary { .. } => {
                panic!("non-constant unary expression as right operand not yet supported")
            }
            ExprKind::Update { .. } => {
                panic!("++/-- as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::Logical { .. } => {
                panic!("`&&`/`||` as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::AssignExpr { .. } => {
                panic!("assignment expression as right operand not yet supported (no fixture)")
            }
            ExprKind::AddressOf(_) | ExprKind::AddressOfArrayElem { .. } => {
                panic!("`&x` as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::Deref(inner) => {
                // `*p` as RHS where `p` is a register-resident local
                // pointer — fold to a `<width> ptr [<reg>]` operand
                // (fixture 201). Other deref shapes (chained, global
                // pointer, post-update) still need materialization.
                if let ExprKind::Ident(name) = &inner.kind {
                    if self.globals.type_of(name).is_none() {
                        if let LocalLocation::Reg(reg) = self.locals.location_of(name) {
                            return OperandSource::DerefReg(reg);
                        }
                    }
                }
                panic!("`*p` as right operand of a binary op only supported for register-resident local pointers (no fixture for {:?})", inner.kind)
            }
            ExprKind::ArrayIndex { array, index } => {
                // `g[K]` where `g` is a file-scope array — fold to
                // `word ptr DGROUP:_g+(K*stride)`. Fixture 189 emits
                // `add ax, word ptr DGROUP:_a+2` for `a[1]`.
                //
                // Also handles member→array chains like `s.a[K]` and
                // global struct field arrays. Fixture 932 (`s.n +
                // s.a[1]` with `struct { int n; int a[3]; } s`).
                //
                // For stack-resident local arrays the same offset
                // arithmetic applies but the operand is a bp-relative
                // `[bp+(base_off+K*stride)]`. Fixture 977.
                //
                // `p[K]` where `p` is a register-resident pointer —
                // fold to `<width> ptr [<reg>+(K*stride)]`. Fixture
                // 1472 (`p[1]` in `sum`: `add ax, [si+2]`).
                if let ExprKind::Ident(pname) = &array.kind
                    && self.locals.has(pname)
                    && let Some(pointee) = self.locals.type_of(pname).pointee()
                    && let LocalLocation::Reg(reg) = self.locals.location_of(pname)
                    && let Some(k) = try_const_eval(index)
                {
                    let stride = i32::from(pointee.size_bytes());
                    let off = (k as i32).wrapping_mul(stride);
                    let off16 = i16::try_from(off).unwrap_or(i16::MAX);
                    return OperandSource::DerefRegOffset { reg, offset: off16 };
                }
                let (name, total_off, _leaf_ty) = self
                    .try_lvalue_chain_addr(e)
                    .unwrap_or_else(|| {
                        panic!("variable-indexed global array rhs not yet supported")
                    });
                if self.globals.contains(&name) {
                    return OperandSource::GlobalOffset { name, offset: total_off };
                }
                if self.locals.has(&name)
                    && let LocalLocation::Stack(base_off) = self.locals.location_of(&name)
                {
                    let elem_off = base_off + i16::try_from(total_off).unwrap_or(i16::MAX);
                    return OperandSource::Local(elem_off);
                }
                panic!("array-indexed rhs not supported on `{name}`");
            }
            ExprKind::StringLit(_) => {
                panic!("string literal as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } => {
                // `a.x` / `pts[1].x` / `a.b.c` / global `g.x` as a
                // right operand: walk the lvalue chain. Local chain
                // → `[bp-N]`; global chain → `DGROUP:_<name>+K`.
                // Fixture 103 (`return p.x + p.y;`),
                // fixture 185 (`pts[1].x + pts[1].y`),
                // fixture 190 (global `g.x + g.y`).
                let (name, total_off, _leaf_ty) = self
                    .try_member_dot_chain(base, field)
                    .unwrap_or_else(|| {
                        panic!("non-const-foldable member base in rhs not yet supported")
                    });
                if self.globals.contains(&name) {
                    return OperandSource::GlobalOffset { name, offset: total_off };
                }
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) else {
                    panic!("struct local `{name}` not stack-resident");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                OperandSource::Local(off)
            }
            ExprKind::Member { base, field, kind: crate::ast::MemberKind::Arrow } => {
                // `<reg_ptr>-><field>` as RHS: fold to `<width> ptr
                // [<reg>+field_off]`. Mirrors the `p[K]` case above.
                // Fixture 2313 (`pp->y` for register-resident
                // struct ptr).
                if let ExprKind::Ident(p_name) = &base.kind
                    && self.locals.has(p_name)
                    && let Some(pointee) = self.locals.type_of(p_name).pointee()
                    && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
                    && let Some((field_off, _field_ty)) = pointee.field(field)
                {
                    return OperandSource::DerefRegOffset {
                        reg,
                        offset: field_off as i16,
                    };
                }
                panic!("`p->x` as right operand not yet supported for non-register pointers (no fixture for {:?})", base.kind)
            }
            ExprKind::Ternary { .. } => {
                panic!("ternary as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::Cast { .. } => {
                panic!("cast as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::InitList { .. } => {
                panic!("initializer list not legal as a binary-op operand")
            }
            ExprKind::Comma { .. } => {
                panic!("comma expression as right operand of a binary op not yet supported (no fixture)")
            }
        }
    }

    /// Emit `;` source-comment block(s). Emits ALL source lines from
    /// `current_line + 1` through `line` (inclusive) as one combined
    /// block — leading blank `;\t`, one `;\t<content>` per line, then
    /// trailing blank `;\t`. This matches what BCC does when multiple
    /// source lines have no asm between them (e.g. a `while` header
    /// followed by its first body statement; the close-brace of a
    /// `while` body followed by a statement after the loop).
    ///
    /// The very first comment block in a function — when
    /// `current_line == 0` — emits only the *target* line, not the
    /// preceding source. Otherwise functions defined later in the file
    /// would carry along all prior content as part of their opening
    /// comment block (fixture 009).
    fn advance_to_line(&mut self, line: u32) {
        if line <= self.current_line {
            return;
        }
        let from = if self.current_line == 0 { line } else { self.current_line + 1 };
        self.out.extend_from_slice(b"   ;\t\r\n");
        for ln in from..=line {
            let content = self.lines.line_content(self.source, ln);
            let _ = write!(self.out, "   ;\t{content}\r\n");
        }
        self.out.extend_from_slice(b"   ;\t\r\n");
        self.current_line = line;
    }
}

/// Does `body` contain a `break;` that targets the enclosing loop?
/// Stops at nested loops — a `break;` inside an inner `while`/`for`
/// targets the inner loop, not the outer one.
fn body_has_break(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_break)
}

fn stmt_has_break(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Break => true,
        StmtKind::If { then_branch, else_branch, .. } => {
            body_has_break(then_branch)
                || else_branch.as_ref().is_some_and(|b| body_has_break(b))
        }
        // Nested loops AND nested switches shadow `break;` — they
        // consume any break in their body, so the enclosing loop
        // doesn't see it.
        _ => false,
    }
}

fn body_has_continue(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_continue)
}

fn stmt_has_continue(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Continue => true,
        StmtKind::If { then_branch, else_branch, .. } => {
            body_has_continue(then_branch)
                || else_branch.as_ref().is_some_and(|b| body_has_continue(b))
        }
        // A switch does NOT consume `continue;` — the inner continue
        // threads past it to the enclosing loop, so we have to look
        // inside the case bodies.
        StmtKind::Switch { cases, .. } => {
            cases.iter().any(|c| body_has_continue(&c.body))
        }
        _ => false,
    }
}

/// Compute the `C<num>` suffix of the data-table label BCC uses for
/// a jump-table or linear-search switch. The formulas below are
/// **empirical fits** through our captured fixtures — they pin the
/// labels for 073 (jump-table, 8 cases), 076 (jump-table, 4 cases)
/// and 074 (linear-search, 4 cases), but we don't yet understand
/// what determines the constants `508` and `442`, or whether they
/// vary with anything other than `case_count` (e.g. function
/// position, function size, surrounding constants). _Fingerprint
/// open question; see `specs/FINGERPRINTS.md`._
fn switch_c_num(strategy: SwitchStrategy, case_count: u32) -> u32 {
    match strategy {
        SwitchStrategy::JumpTable => 92 * case_count + 508,
        SwitchStrategy::LinearSearch => 74 * case_count + 442,
        SwitchStrategy::Chained => unreachable!(
            "chained-compare switch has no data label and no `C<num>` to compute"
        ),
    }
}

/// Width keyword for a `mov ptr [bp-N], K` store of the given type:
/// `"byte"` for `char` (and char arrays), `"word"` for `int`,
/// pointers, and int arrays. Currently used only by initialization
/// of stack-resident locals.
/// The (low-half, high-half) mnemonic pair for long-to-long arithmetic
/// or bitwise ops. Add/Sub propagate carry/borrow into the high half
/// (`adc`/`sbb`); bitwise ops act independently per half so high uses
/// the same mnemonic as low.
fn long_pair_op(op: BinOp) -> Option<(&'static str, &'static str)> {
    match op {
        BinOp::Add => Some(("add", "adc")),
        BinOp::Sub => Some(("sub", "sbb")),
        BinOp::BitAnd => Some(("and", "and")),
        BinOp::BitOr => Some(("or", "or")),
        BinOp::BitXor => Some(("xor", "xor")),
        _ => None,
    }
}

fn ptr_width(ty: &Type) -> &'static str {
    if ty.size_bytes() == 1 { "byte" } else { "word" }
}

/// Walk a deref expression chain (`*p` → `(p, 0)`, `**p` → `(p, 1)`,
/// `***p` → `(p, 2)`) and return the base ident name + the count of
/// visible `*`s. The caller's implicit outer `*` (the one applied
/// when reading/writing) is not counted. Used by both
/// `emit_deref_to_ax` (read) and `emit_deref_assign` (write) so the
/// chain prefix is shared.
fn deref_chain_root(ptr: &Expr) -> (&str, u32) {
    let mut depth = 0u32;
    let mut cur = ptr;
    let name = loop {
        match &cur.kind {
            ExprKind::Deref(inner) => {
                depth += 1;
                cur = inner;
            }
            ExprKind::Ident(name) => break name.as_str(),
            _ => panic!(
                "non-ident base in deref chain not yet supported (no fixture for {:?})",
                cur.kind
            ),
        }
    };
    (name, depth)
}

/// Walk an array-type chain against an index list, summing
/// `stride * k` for each subscript when every index is a compile-time
/// constant. Returns `(byte_offset, leaf_type)`. `None` if any index
/// is non-constant or the type chain stops being `Type::Array` before
/// all indices are consumed. Used by both array-read and array-assign
/// codegen to fold `a[1][2]` into a single `[bp-N]` operand.
fn try_const_array_offset<'a, I>(array_ty: &Type, indices: I) -> Option<(i32, Type)>
where
    I: IntoIterator<Item = &'a Expr>,
{
    let mut ty = array_ty.clone();
    let mut off: i32 = 0;
    for ix in indices {
        let k = try_const_eval(ix)? as i32;
        let Type::Array { elem, .. } = &ty else { return None };
        let stride = i32::from(elem.size_bytes());
        off = off.checked_add(k.checked_mul(stride)?)?;
        ty = (**elem).clone();
    }
    Some((off, ty))
}

/// A resolved right-hand operand.
enum OperandSource {
    Immediate(u32),
    /// Stack-resident local or param at a (signed) bp offset.
    Local(i16),
    Reg(Reg),
    /// File-scope variable — addressed as `<width> ptr DGROUP:_<name>`.
    /// Fixture 087: `add ax, word ptr DGROUP:_b`.
    Global(String),
    /// File-scope array element at a compile-time offset:
    /// `<width> ptr DGROUP:_<name>+<offset>`. Fixture 189 uses
    /// `add ax, word ptr DGROUP:_a+2` for `a[1]`.
    GlobalOffset { name: String, offset: i32 },
    /// `*p` where `p` is a register-resident local pointer —
    /// addressed as `<width> ptr [<reg>]`. Fixture 201:
    /// `sub ax,word ptr [si]` for `10 - *p` with `p` in SI.
    DerefReg(Reg),
    /// `p[K]` where `p` is a register-resident local pointer and
    /// `K` is constant — addressed as `<width> ptr [<reg>+<off>]`.
    /// Fixture 1472 (`sum`: `add ax, [si+2]` for `p[1]`).
    DerefRegOffset { reg: Reg, offset: i16 },
}

impl OperandSource {
    /// Format as a 16-bit source operand.
    fn word(&self) -> String {
        match self {
            Self::Immediate(v) => v.to_string(),
            Self::Local(off) => format!("word ptr {}", bp_addr(*off)),
            Self::Reg(r) => r.name().to_owned(),
            Self::Global(name) => format!("word ptr DGROUP:_{name}"),
            Self::GlobalOffset { name, offset } => {
                if *offset == 0 {
                    format!("word ptr DGROUP:_{name}")
                } else {
                    format!("word ptr DGROUP:_{name}+{offset}")
                }
            }
            Self::DerefReg(r) => format!("word ptr [{}]", r.name()),
            Self::DerefRegOffset { reg, offset } => {
                if *offset == 0 {
                    format!("word ptr [{}]", reg.name())
                } else if *offset > 0 {
                    format!("word ptr [{}+{}]", reg.name(), offset)
                } else {
                    format!("word ptr [{}-{}]", reg.name(), -offset)
                }
            }
        }
    }

    /// Byte form, used for shift counts (`mov cl, byte ptr ...`).
    fn byte(&self) -> String {
        match self {
            Self::Immediate(v) => v.to_string(),
            Self::Local(off) => format!("byte ptr {}", bp_addr(*off)),
            Self::Global(name) => format!("byte ptr DGROUP:_{name}"),
            Self::GlobalOffset { name, offset } => {
                if *offset == 0 {
                    format!("byte ptr DGROUP:_{name}")
                } else {
                    format!("byte ptr DGROUP:_{name}+{offset}")
                }
            }
            // A register holding an int provides the low byte via
            // its `*L` half; we'd need a separate fixture to confirm
            // BCC's exact shape. Panic until we see one.
            Self::Reg(_) => panic!("shift count from a register local not yet supported"),
            Self::DerefReg(r) => format!("byte ptr [{}]", r.name()),
            Self::DerefRegOffset { reg, offset } => {
                if *offset == 0 {
                    format!("byte ptr [{}]", reg.name())
                } else if *offset > 0 {
                    format!("byte ptr [{}+{}]", reg.name(), offset)
                } else {
                    format!("byte ptr [{}-{}]", reg.name(), -offset)
                }
            }
        }
    }
}

/// Emit the operator-specific instruction(s) given an already-loaded AX
/// (left operand) and a source string for the right operand. `unsigned`
/// selects `shr` over `sar` for `Shr` — the left operand's static type
/// drives the choice (right is always the shift count).
fn emit_op_with_source(out: &mut Vec<u8>, op: BinOp, src: &OperandSource, unsigned: bool) {
    emit_op_with_source_opts(out, op, src, unsigned, false);
}

#[allow(clippy::too_many_arguments)]
fn emit_op_with_source_opts(
    out: &mut Vec<u8>,
    op: BinOp,
    src: &OperandSource,
    unsigned: bool,
    skip_mod_to_ax: bool,
) {
    // Identity folds: `a + 0`, `a - 0`, `a | 0`, `a ^ 0`, `a << 0`,
    // `a >> 0` are all just `a`. Skip the emission entirely. BCC
    // collapses these at compile time (fixtures 3370 `x + 0`, 2735
    // `x - 0`).
    if let OperandSource::Immediate(0) = src {
        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr) {
            return;
        }
    }
    // ±1 / +2 peephole: `inc ax` for +1 (1 byte vs 3), `dec ax` for -1,
    // and two `inc ax` for +2 (2 bytes vs 3). Notably -2 is NOT folded
    // to `dec ax; dec ax` — BCC keeps `add ax, -2` (3 bytes, AX-accum
    // imm16 form). Fixture 2074 (`x - 2`), 1277 (`fib(n - 2)`).
    if let OperandSource::Immediate(v) = src
        && ((matches!(op, BinOp::Add) && (*v == 1 || *v == 2))
            || (matches!(op, BinOp::Sub) && *v == 1))
    {
        let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
        for _ in 0..*v {
            let _ = write!(out, "\t{mnem}\tax\r\n");
        }
        return;
    }
    match op {
        BinOp::Add => {
            let _ = write!(out, "\tadd\tax,{}\r\n", src.word());
        }
        BinOp::Sub => {
            // BCC canonicalizes signed `ax - K` (immediate RHS, K !=
            // ±1, ±2 — those go through the inc/dec peephole
            // upstream) as `add ax, -K` (the AX-accumulator imm16
            // form `05 lo hi`) rather than `sub ax, K`. For unsigned
            // operands BCC keeps `sub` (fixture 3578: `unsigned x -
            // 5` → `2d 05 00`). Fixture 630 (signed `x - 5` → `05
            // fb ff`).
            if let OperandSource::Immediate(k) = src
                && !unsigned
            {
                let neg = (0u32.wrapping_sub(*k)) & 0xFFFF;
                let neg_i16 = neg as i16;
                let _ = write!(out, "\tadd\tax,{neg_i16}\r\n");
            } else {
                let _ = write!(out, "\tsub\tax,{}\r\n", src.word());
            }
        }
        BinOp::BitAnd => {
            let _ = write!(out, "\tand\tax,{}\r\n", src.word());
        }
        BinOp::BitOr => {
            let _ = write!(out, "\tor\tax,{}\r\n", src.word());
        }
        BinOp::BitXor => {
            let _ = write!(out, "\txor\tax,{}\r\n", src.word());
        }
        BinOp::Mul => {
            if let OperandSource::Immediate(v) = src {
                // `ax *= K` with K a small power of two unrolls into
                // `shl ax, 1` repeated. Fixture 592 (`return x * 2`
                // → `shl ax, 1`). For non-power-of-2 immediates BCC
                // materializes K in DX and uses the single-operand
                // `imul dx`. Fixture 615 (`return x * 3` → `mov dx,
                // 3; imul dx`).
                let k = *v;
                if k > 0 && k.is_power_of_two() && k.trailing_zeros() <= 15 {
                    let shifts = k.trailing_zeros();
                    if shifts <= 3 {
                        for _ in 0..shifts {
                            out.extend_from_slice(b"\tshl\tax,1\r\n");
                        }
                    } else {
                        let _ = write!(out, "\tmov\tcl,{shifts}\r\n");
                        out.extend_from_slice(b"\tshl\tax,cl\r\n");
                    }
                } else {
                    let v16 = k & 0xFFFF;
                    let _ = write!(out, "\tmov\tdx,{v16}\r\n");
                    out.extend_from_slice(b"\timul\tdx\r\n");
                }
            } else {
                let _ = write!(out, "\timul\t{}\r\n", src.word());
            }
        }
        BinOp::Div => {
            // For unsigned operands BCC uses `xor dx, dx; div <r/m>` —
            // zeros the upper half rather than sign-extending via
            // `cwd`, and uses the unsigned `div` instruction. Fixture
            // 946 (`unsigned a, b; return a / b;`).
            //
            // Unsigned div by power-of-2 constant: strength-reduce to
            // `shr ax, log2(K)` (signed div by pow2 is NOT reducible —
            // signed shift rounds toward -∞ while signed div rounds
            // toward 0). For K ≤ 8, unroll `shr ax, 1` (each 2 bytes);
            // larger K uses `mov cl, log2(K); shr ax, cl`. Fixtures
            // 3369, 2084, 1725.
            if unsigned
                && let OperandSource::Immediate(v) = src
                && *v > 0
                && v.is_power_of_two()
                && v.trailing_zeros() <= 15
            {
                let shifts = v.trailing_zeros();
                if shifts <= 3 {
                    for _ in 0..shifts {
                        out.extend_from_slice(b"\tshr\tax,1\r\n");
                    }
                } else {
                    let _ = write!(out, "\tmov\tcl,{shifts}\r\n");
                    out.extend_from_slice(b"\tshr\tax,cl\r\n");
                }
                return;
            }
            let (widen, mnem) = if unsigned {
                (&b"\txor\tdx,dx\r\n"[..], "div")
            } else {
                (&b"\tcwd\t\r\n"[..], "idiv")
            };
            if let OperandSource::Immediate(v) = src {
                // `idiv`/`div` has no immediate form. BCC materializes
                // the divisor in BX, then `<widen>; <mnem> bx`.
                // Fixture 584 (signed compound `/=`), 946 (unsigned).
                let v16 = v & 0xFFFF;
                let _ = write!(out, "\tmov\tbx,{v16}\r\n");
                out.extend_from_slice(widen);
                let _ = write!(out, "\t{mnem}\tbx\r\n");
            } else {
                out.extend_from_slice(widen);
                let _ = write!(out, "\t{mnem}\t{}\r\n", src.word());
            }
        }
        BinOp::Mod => {
            // Unsigned mod by power-of-2 K: `x % K = x & (K-1)`. Single
            // `and ax, K-1` (3 bytes) vs the full `mov bx; xor dx,dx;
            // div bx; mov ax,dx` (9 bytes).
            if unsigned
                && let OperandSource::Immediate(v) = src
                && *v > 0
                && v.is_power_of_two()
            {
                let mask = (v - 1) & 0xFFFF;
                let _ = write!(out, "\tand\tax,{mask}\r\n");
                return;
            }
            let (widen, mnem) = if unsigned {
                (&b"\txor\tdx,dx\r\n"[..], "div")
            } else {
                (&b"\tcwd\t\r\n"[..], "idiv")
            };
            if let OperandSource::Immediate(v) = src {
                let v16 = v & 0xFFFF;
                let _ = write!(out, "\tmov\tbx,{v16}\r\n");
                out.extend_from_slice(widen);
                let _ = write!(out, "\t{mnem}\tbx\r\n");
            } else {
                out.extend_from_slice(widen);
                let _ = write!(out, "\t{mnem}\t{}\r\n", src.word());
            }
            // `mov ax, dx` materializes the remainder in AX. Skipped
            // when the caller has signaled it'll read the remainder
            // from DX directly (e.g. an immediate `mov [mem], dx`
            // store). Saves 2 bytes for `int r = x % K` shapes.
            if !skip_mod_to_ax {
                out.extend_from_slice(b"\tmov\tax,dx\r\n");
            }
        }
        BinOp::Shl | BinOp::Shr => {
            let mnemonic = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if unsigned => "shr",
                BinOp::Shr => "sar",
                _ => unreachable!(),
            };
            // BCC unrolls expression-context shifts by 1, 2, or 3
            // into repeated `<mn> ax, 1` rather than the `mov cl, K;
            // <mn> ax, cl` pair — even when K=3 (which would be
            // shorter as a CL load). Fixture 627 (`x >> 3` → three
            // `sar ax, 1`). For K >= 4 BCC switches to the CL form.
            if let OperandSource::Immediate(k) = src
                && *k >= 1
                && *k <= 3
            {
                for _ in 0..*k {
                    let _ = write!(out, "\t{mnemonic}\tax,1\r\n");
                }
            } else {
                let _ = write!(out, "\tmov\tcl,{}\r\n", src.byte());
                let _ = write!(out, "\t{mnemonic}\tax,cl\r\n");
            }
        }
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            unreachable!("comparison op should take the cmp-as-value path");
        }
    }
}
