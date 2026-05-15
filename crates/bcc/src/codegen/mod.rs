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

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }
}

/// Accumulator for string literals encountered during codegen of a
/// translation unit. Each unique literal gets a stable byte offset
/// within the `s@` block; identical literals deduplicate. Emission
/// of the actual `db 'string' / db 0` block happens in the tail of
/// the file (`emit_s.rs::write_tail`).
#[derive(Debug, Default)]
pub struct StringPool {
    /// Source bytes of each unique literal, in insertion order. The
    /// running total of `bytes.len() + 1` (NUL terminator) is the
    /// next available offset.
    entries: Vec<Vec<u8>>,
}

impl StringPool {
    /// Intern a literal and return its byte offset within `s@`.
    /// Identical literals return the same offset.
    pub fn intern(&mut self, bytes: &[u8]) -> u32 {
        let mut offset: u32 = 0;
        for existing in &self.entries {
            if existing.as_slice() == bytes {
                return offset;
            }
            offset += u32::try_from(existing.len() + 1).expect("string offset fits in u32");
        }
        self.entries.push(bytes.to_vec());
        offset
    }

    /// True when no literals have been interned. Tail emission can
    /// skip the `db` lines entirely in that case (matching the
    /// "empty s@ block" we used to always emit).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The interned literals in insertion order. Tail emission writes
    /// each as `db '<contents>'` (and an explicit terminating `db 0`).
    #[must_use]
    pub fn entries(&self) -> &[Vec<u8>] {
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
) {
    let mut emitter = FunctionEmitter::new(
        out, source, function, func_idx, signatures, globals, strings,
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
                if self.globals.contains(name) {
                    self.emit_assign_global(name, value);
                } else {
                    let loc = self.locals.location_of(name);
                    self.emit_assign_local(loc, value);
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
            StmtKind::ArrayCompoundAssign { array, indices, op, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_array_compound_assign(array, indices, *op, value);
            }
            StmtKind::DerefAssign { target, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_deref_assign(target, value);
            }
            StmtKind::DerefCompoundAssign { target, op, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_deref_compound_assign(target, *op, value);
            }
            StmtKind::MemberAssign { base, field, kind, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_member_assign(base, field, *kind, value);
            }
            StmtKind::MemberCompoundAssign { base, field, kind, op, value } => {
                self.advance_to_stmt_line(stmt);
                self.emit_member_compound_assign(base, field, *kind, *op, value);
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
        }
    }

    /// Emit `expr` for its side effects, discarding the value. The
    /// special case is `Update` (`++x;` / `x++;`): BCC emits just the
    /// increment, no `mov ax, ...` afterward (fixture 040). Likewise
    /// for an assignment expression in a `for`-clause: emit the
    /// side-effect store, no value-load afterward.
    fn emit_expr_discard(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Update { target, op, .. } => {
                self.emit_update_in_place(target, *op);
            }
            ExprKind::AssignExpr { target, value } => {
                let loc = self.locals.location_of(target);
                self.emit_assign_local(loc, value);
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
    fn emit_update_in_place(&mut self, name: &str, op: UpdateOp) {
        let mnemonic = match op {
            UpdateOp::Inc => "inc",
            UpdateOp::Dec => "dec",
        };
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
                let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            }
            LocalLocation::Reg(reg) => {
                // Pointer stride > 1: repeat inc/dec stride times.
                // (Matches the BCC +2 peephole; for stride > 2 BCC
                // probably switches to `add reg, K` but no fixture
                // pins the crossover yet.)
                for _ in 0..stride {
                    let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                }
            }
            LocalLocation::Stack(off) => {
                // Stack-resident ++/-- on a char uses the AL round-trip
                // (fixture 055). Stack ints are still unobserved — keep
                // the panic until a fixture forces us there.
                let ty = self.locals.type_of(name);
                assert!(
                    matches!(ty, Type::Char),
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
        let base = self.label_plan.base(if_span_start);
        // When the cond's outermost operator is `||`, the operands may
        // short-circuit-to-true; we need a label at the start of the
        // then-branch for them to land at. The if's base+0 slot —
        // unused for plain conds — serves as that "then-entry".
        let cond_has_top_or = matches!(
            cond.kind,
            ExprKind::Logical { op: LogicalOp::Or, .. }
        );
        let then_entry_slot = if cond_has_top_or { Some(base) } else { None };

        if let Some(else_stmts) = else_branch {
            // if/else reserves 3 slots; the else label lives at +2.
            let else_slot = base + 2;
            self.emit_cond_branch(cond, then_entry_slot, Some(else_slot));
            if let Some(slot) = then_entry_slot {
                self.emit_label(slot);
            }
            for s in then_branch {
                self.emit_stmt(s);
            }
            let exit_n = self.exit_label_num();
            let _ = write!(self.out, "\tjmp\tshort @{}@{exit_n}\r\n", self.func_idx);
            self.emit_label(else_slot);
            for s in else_stmts {
                self.emit_stmt(s);
            }
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
        assert!(
            !matches!(cond.kind, ExprKind::Logical { .. }),
            "logical condition (`&&`/`||`) in a `while` not yet supported (no fixture)"
        );
        let plan = self.label_plan.loop_plan(while_span_start);
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
        // Trampoline jump to the check.
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
        // Load scrutinee into AX. Today only an ident-as-int — chars
        // or non-trivial scrutinee expressions need fixtures to pin
        // the exact shape (e.g. byte-register-then-cbw).
        let ExprKind::Ident(name) = &scrutinee.kind else {
            panic!("non-ident switch scrutinee not yet supported (no fixture)");
        };
        let ty = self.locals.type_of(name);
        assert!(
            matches!(ty, Type::Int),
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
    fn emit_cond_branch(
        &mut self,
        cond: &Expr,
        true_slot: Option<u32>,
        false_slot: Option<u32>,
    ) {
        // `<long_global> == K` for non-zero K — BCC emits a chained
        // cmp+jne pair: high half against (K>>16), low half against
        // (K&0xFFFF). Both halves use Grp1 imm8sx form, so each half
        // must fit a sign-extended i8. Only the false-slot-only shape
        // shows up in fixture 223 (`if (g == K) ...`); a true-slot
        // form would invert to `je` and pick up later.
        if let ExprKind::BinOp { op: BinOp::Eq, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(Type::Long) = self.globals.type_of(name)
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
            // Restricted to top-level binary `&&` / `||`. Chained or
            // nested logical operators need a more careful target
            // tracking (each non-final operand's short-circuit must
            // still jump rather than fall through); we'll lift this
            // when a fixture forces a choice.
            assert!(
                !matches!(left.kind, ExprKind::Logical { .. })
                    && !matches!(right.kind, ExprKind::Logical { .. }),
                "nested `&&`/`||` operators not yet supported (no fixture)"
            );
            match op {
                LogicalOp::And => {
                    // a false → false_slot; a true → fall through to b.
                    // b carries the outer true/false targets.
                    self.emit_cond_branch(left, None, false_slot);
                    self.emit_cond_branch(right, true_slot, false_slot);
                }
                LogicalOp::Or => {
                    // a true → true_slot (jump); a false → fall through to b.
                    // b: true → fall through (caller emits true_slot label
                    // right after this call); false → false_slot.
                    self.emit_cond_branch(left, true_slot, None);
                    self.emit_cond_branch(right, None, false_slot);
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
            (Some(_), Some(_)) => panic!(
                "emit_cond_branch with both true and false targets not yet supported \
                 (nested mixed && / || requires this case)"
            ),
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
        // `<long_global> == 0` / `<long_global> != 0` — BCC folds the
        // 32-bit comparison into `mov ax,low / or ax,high`, which
        // sets ZF iff both halves are zero. Fixture 215.
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && matches!(op, BinOp::Eq | BinOp::Ne)
            && let ExprKind::Ident(name) = &left.kind
            && let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Long)
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
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && op.is_comparison()
        {
            let unsigned = self.cmp_is_unsigned(left, right);
            self.emit_compare(left, right);
            return (
                op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
            );
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
        if let Some(gt) = self.globals.type_of(name) {
            return gt.is_unsigned();
        }
        self.locals.type_of(name).is_unsigned()
    }

    /// Resolve a stack-resident lvalue chain (`Ident`, `ArrayIndex`
    /// with constant subscripts, `Member` via `Dot`, or any
    /// composition of those) to `(base_name, total_byte_offset,
    /// leaf_type)`. Returns `None` if the chain includes a
    /// non-constant subscript, a pointer dereference, or anything
    /// outside this lvalue shape. Used by the member/array codegen
    /// to fold `pts[1].x` and friends into a single `[bp-N]` operand
    /// (fixture 185).
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

    /// Emit a "test against zero" instruction for a non-comparison
    /// expression — used in boolean contexts (`if (x)`, `x && y`).
    /// Today only `Ident`s are supported; other expressions panic.
    fn emit_zero_test(&mut self, cond: &Expr) {
        let ExprKind::Ident(name) = &cond.kind else {
            panic!("non-ident boolean condition not yet supported (no fixture)");
        };
        match self.locals.location_of(name) {
            LocalLocation::Stack(off) => {
                let ty = self.locals.type_of(name);
                let width = if matches!(ty, Type::Char) { "byte" } else { "word" };
                let _ = write!(self.out, "\tcmp\t{width} ptr {},0\r\n", bp_addr(off));
            }
            LocalLocation::Reg(reg) => {
                let _ = write!(self.out, "\tor\t{0},{0}\r\n", reg.name());
            }
        }
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
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            let _ = write!(self.out, "\tcmp\tword ptr {},{rhs}\r\n", bp_addr(off));
            return;
        }
        self.emit_expr_to_ax(left);
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
        let reg = match self.locals.location_of(target) {
            LocalLocation::Reg(r) => r,
            LocalLocation::Stack(_) => {
                panic!("++/-- in expression on a stack-resident local not yet supported (no fixture)");
            }
        };
        let mnemonic = match op {
            UpdateOp::Inc => "inc",
            UpdateOp::Dec => "dec",
        };
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
        let mut total_bytes: u32 = 0;
        for (i, arg) in args.iter().enumerate().rev() {
            // Param type for the i-th arg, defaulting to int when the
            // signature isn't known (extern function — no fixture yet).
            let arg_ty = param_tys
                .and_then(|tys| tys.get(i))
                .cloned()
                .unwrap_or(Type::Int);
            if matches!(arg_ty, Type::Long) {
                // Long arg: materialize (AX=high, DX=low), push
                // high then low. 4 bytes per arg. Fixture 216.
                self.emit_long_arg_push(arg);
                total_bytes += 4;
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

    /// Materialize a long argument into AX (high) and DX (low), then
    /// push both halves (high first, low second) per BCC's calling
    /// convention. Fixture 216.
    fn emit_long_arg_push(&mut self, arg: &Expr) {
        let Some(v) = try_const_eval(arg) else {
            panic!("non-constant long argument not yet supported (no fixture)");
        };
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
    }

    /// Place an argument into AX (the low byte of which is `al`) for
    /// the subsequent `push ax`. For a `char` param the load uses the
    /// 8-bit form so only AL is touched; AH is whatever happened to
    /// be there. For `int`, the standard 16-bit load.
    fn emit_arg_into_ax(&mut self, arg: &Expr, param_ty: Type) {
        if !matches!(param_ty, Type::Char) {
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
                matches!(ty, Type::Char),
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
        match self.locals.location_of(name) {
            LocalLocation::Reg(r) => Some(r),
            LocalLocation::Stack(_) => None,
        }
    }

    fn emit_return_value_load(&mut self, value: Option<&Expr>) {
        let Some(e) = value else { return };
        // Long return: standard 8086 32-bit return-value convention
        // puts the high word in DX and the low word in AX. (Note
        // BCC swaps the AX/DX roles when doing in-memory long
        // arithmetic — see fixture 207 — but the boundary at
        // `return` uses the ABI-standard layout.) Fixture 212.
        if matches!(self.function.ret_ty, Type::Long) {
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
            // `return g;` for a long global — load high to DX (8B
            // form, no AX-specific short opcode for non-AX dst) then
            // low to AX (A1 short form). Fixture 213.
            if let ExprKind::Ident(name) = &e.kind
                && let Some(src_ty) = self.globals.type_of(name)
                && matches!(src_ty, Type::Long)
            {
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}+2\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                return;
            }
            // `return x;` for a long parameter or stack-local. Layout:
            // `off` points to the low word (the lower stack address);
            // the high word lives at `off + 2`. Load high to DX, low
            // to AX per the ABI. Fixture 217.
            if let ExprKind::Ident(name) = &e.kind
                && matches!(self.locals.type_of(name), Type::Long)
            {
                let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                    panic!("register-resident long not yet supported (no fixture)");
                };
                let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                return;
            }
            panic!("non-constant long return value not yet supported (no fixture)");
        }
        self.emit_expr_to_ax(e);
    }

    /// Initialize a freshly-declared local with `init`.
    fn emit_init_local(&mut self, loc: LocalLocation, ty: &Type, init: &Expr) {
        match loc {
            LocalLocation::Stack(off) => {
                // `long x = K;` stack local — two word stores, high
                // word at the upper slot offset then low word at the
                // lower slot. Mirrors fixture 205's global-long shape.
                // Fixture 210.
                if matches!(ty, Type::Long) {
                    let Some(v) = try_const_eval(init) else {
                        panic!("non-constant long local init not yet supported (no fixture)");
                    };
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
                // Stack init: prefer the immediate-store form when the
                // initializer folds to a constant. For `char` we emit
                // `byte ptr` (fixture 011); for `int`, `word ptr`.
                if let Some(v) = try_const_eval(init) {
                    let width = ptr_width(ty);
                    let _ = write!(self.out, "\tmov\t{width} ptr {},{v}\r\n", bp_addr(off));
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
                // Non-constant init for a char would need a different
                // shape (load to AL, store AL); no fixture yet.
                // Pointers and ints share the int-like word-sized
                // path: compute into AX, then store as `word ptr`.
                assert!(
                    ty.is_int_like(),
                    "non-constant init for non-int-like type {:?} not yet supported",
                    ty
                );
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
        let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
            panic!(
                "compound assignment on stack-resident `{name}` not yet supported (no fixture)"
            );
        };
        assert!(
            !reg.is_byte(),
            "compound assignment on a char (byte-register) target not yet supported (no fixture)"
        );
        match op {
            BinOp::Add | BinOp::Sub => {
                if let Some(v) = try_const_eval(value) {
                    let v16 = v & 0xFFFF;
                    if v16 == 1 {
                        let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                        let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
                        return;
                    }
                    let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
                    let _ = write!(self.out, "\t{mnem}\t{},{v16}\r\n", reg.name());
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
                // `imul <src>` with AX, materializing the RHS in DX
                // first (fixture 069).
                if let Some(v) = try_const_eval(value) {
                    let v16 = v & 0xFFFF;
                    let _ = write!(self.out, "\tmov\tdx,{v16}\r\n");
                } else {
                    let src = self.resolve_operand_source(value);
                    let _ = write!(self.out, "\tmov\tdx,{}\r\n", src.word());
                }
                let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                self.out.extend_from_slice(b"\timul\tdx\r\n");
                let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
            }
            BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr => {
                panic!(
                    "compound `{op:?}` not yet supported (no fixture); expected to route through AX with cwd+idiv or cl-loaded shifts"
                );
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
        // `*(p + offset)` shapes go through a shared helper that
        // builds the addressing mode.
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &ptr.kind
            && let ExprKind::Ident(name) = &left.kind
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
            if matches!(*pointee, Type::Char) {
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
            if matches!(*pointee, Type::Char) {
                let _ = write!(self.out, "\tmov\tal,byte ptr [{addr_reg}]\r\n");
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{width} ptr [{addr_reg}]\r\n");
            }
            return;
        }
        // Chain path: land the address-to-be-deref'd-once-more in BX,
        // then do the final load. Fixture 195 (`int **p` → `**p`)
        // hits depth=1; fixture 193 hits depth=0 on a global.
        let final_ty = self.emit_chain_to_bx(base_name, depth);
        if matches!(final_ty, Type::Char) {
            self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
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
        if is_global {
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{base_name}\r\n");
        } else {
            match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) if reg.name() == "bx" => {}
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                }
                LocalLocation::Stack(_) => {
                    panic!("stack-resident pointer chain root not yet supported (no fixture)");
                }
            }
        }
        for _ in 0..depth {
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
        let load_byte = matches!(pointee, Type::Char);
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
                if matches!(leaf_ty, Type::Char) {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
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
            if matches!(leaf_ty, Type::Char) {
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                self.out.extend_from_slice(b"\tcbw\t\r\n");
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
            if matches!(leaf_ty, Type::Char) {
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
        if matches!(*elem, Type::Char) {
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
        if matches!(pointee, Type::Char) {
            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        } else {
            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
        }
    }

    fn emit_pointer_index_to_ax(&mut self, ptr_name: &str, pointee: Type, index: &Expr) {
        let Some(k) = try_const_eval(index) else {
            panic!("variable-indexed pointer access not yet supported (no fixture)");
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
        if matches!(pointee, Type::Char) {
            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
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
        let array_ty = self.locals.type_of(array).clone();
        let LocalLocation::Stack(base_off) = self.locals.location_of(array) else {
            panic!("array `{array}` should be stack-resident");
        };
        if let Some((const_off, leaf_ty)) = try_const_array_offset(&array_ty, indices.iter()) {
            let off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
            let width = ptr_width(&leaf_ty);
            if let Some(v) = try_const_eval(value) {
                let v_masked =
                    if matches!(leaf_ty, Type::Char) { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(
                    self.out,
                    "\tmov\t{width} ptr {},{v_masked}\r\n",
                    bp_addr(off),
                );
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
                if matches!(leaf_ty, Type::Char) { v & 0xFF } else { v & 0xFFFF };
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
            let v_masked = if matches!(*elem, Type::Char) { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
            return;
        }
        panic!("non-constant rhs in variable-indexed array assign not yet supported (no fixture)");
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
    ) {
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
        let store_byte = matches!(leaf_ty, Type::Char);
        let width = if store_byte { "byte" } else { "word" };
        let Some(v) = try_const_eval(value) else {
            panic!("non-constant rhs in array compound assign not yet supported (no fixture)");
        };
        let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
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
            "\t{mnemonic}\t{width} ptr {},{v_masked}\r\n",
            bp_addr(off),
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
                    let load_byte = matches!(leaf_ty, Type::Char);
                    let width = if load_byte { "byte" } else { "word" };
                    let addr = if total_off == 0 {
                        format!("DGROUP:_{name}")
                    } else {
                        format!("DGROUP:_{name}+{total_off}")
                    };
                    if load_byte {
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    } else {
                        let _ = write!(self.out, "\tmov\tax,{width} ptr {addr}\r\n");
                    }
                    return;
                }
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) else {
                    panic!("struct local `{name}` not stack-resident (unexpected)");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                if matches!(leaf_ty, Type::Char) {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                }
                return;
            }
        }
        // Arrow path (or Dot whose base isn't a const-chain lvalue):
        // base must be a bare Ident referring to a pointer.
        let ExprKind::Ident(name) = &base.kind else {
            panic!("non-ident base in member access not yet supported (no fixture)");
        };
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
        let load_byte = matches!(field_ty, Type::Char);
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
                self.out.extend_from_slice(b"\tcbw\t\r\n");
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
        let store_byte = matches!(leaf_ty, Type::Char);
        let width = if store_byte { "byte" } else { "word" };
        if let Some(v) = try_const_eval(value) {
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr {dest},{v_masked}\r\n");
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
    fn emit_member_compound_assign(
        &mut self,
        base: &Expr,
        field: &str,
        kind: crate::ast::MemberKind,
        op: BinOp,
        value: &Expr,
    ) {
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
        let store_byte = matches!(field_ty, Type::Char);
        let width = if store_byte { "byte" } else { "word" };
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
        let Some(v) = try_const_eval(value) else {
            panic!("non-constant rhs in member compound assign not yet supported (no fixture)");
        };
        let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
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
            let width = ptr_width(pointee);
            let addr_reg = match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) => reg.name(),
                LocalLocation::Stack(_) => {
                    panic!("stack-resident pointer in `*p = v` not yet supported (no fixture)");
                }
            };
            let Some(v) = try_const_eval(value) else {
                panic!("non-constant rhs in `*p = v` not yet supported (no fixture)");
            };
            let v_masked = if matches!(*pointee, Type::Char) { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr [{addr_reg}],{v_masked}\r\n");
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
        let v_masked = if matches!(final_ty, Type::Char) { v & 0xFF } else { v & 0xFFFF };
        let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
    }

    /// `*<target> <op>= <value>;` — read-modify-write through a
    /// dereferenced pointer. Same shape as `emit_deref_assign` for
    /// address resolution, then emits `<op> <width> ptr [reg],imm`
    /// directly (fixture 183).
    fn emit_deref_compound_assign(&mut self, target: &Expr, op: BinOp, value: &Expr) {
        let (base_name, depth) = deref_chain_root(target);
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
            let store_byte = matches!(*pointee, Type::Char);
            let width = if store_byte { "byte" } else { "word" };
            let addr_reg = match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) => reg.name(),
                LocalLocation::Stack(_) => {
                    panic!(
                        "stack-resident pointer in `*p <op>= v` not yet supported (no fixture)"
                    );
                }
            };
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
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
        let store_byte = matches!(final_ty, Type::Char);
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
        // low word (fixture 205).
        if matches!(ty, Type::Long) {
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
                && matches!(src_ty, Type::Long)
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
                && matches!(self.locals.type_of(src_name), Type::Long)
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
                && self.signatures.ret_ty_of(fname).map_or(false, |t| matches!(t, Type::Long))
            {
                self.emit_call(fname, args);
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // Long-to-long arithmetic/bitwise between two long globals:
            // `g = a + b;` / `g = a - b;` / `g = a & b;` (and similar
            // for `|`, `^` once fixtured). Same skeleton: load a into
            // (AX=high, DX=low), apply the op's pair to b's halves,
            // store back. Add/Sub need carry/borrow (adc/sbb on the
            // high half); bitwise ops are independent per-half so the
            // high-half op is the same mnemonic. Fixtures 219 (add),
            // 220 (sub), 221 (and).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && let Some((lo_op, hi_op)) = long_pair_op(*op)
                && let ExprKind::Ident(a) = &left.kind
                && let ExprKind::Ident(b) = &right.kind
                && self.globals.type_of(a).map_or(false, |t| matches!(t, Type::Long))
                && self.globals.type_of(b).map_or(false, |t| matches!(t, Type::Long))
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\t{lo_op}\tdx,word ptr DGROUP:_{b}\r\n");
                let _ = write!(self.out, "\t{hi_op}\tax,word ptr DGROUP:_{b}+2\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = g + K;` or `g = g - K;` for a long global, K small
            // (fits in i8 — after sign flip for sub). BCC reuses one
            // add/adc pattern for both: load (high→AX, low→DX), add
            // the low half (`add dx,delta`), carry-propagate into the
            // high half (`adc ax,carry`), then writeback (A3 + 89/16).
            // For Add: delta = +K, carry = 0. For Sub: delta = -K,
            // carry = -1 (sign-extension of the negative delta).
            // Fixtures 207 (add) and 208 (sub).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && (matches!(op, BinOp::Add) || matches!(op, BinOp::Sub))
                && let ExprKind::Ident(rhs_name) = &left.kind
                && rhs_name == name
                && let Some(k) = try_const_eval(right)
            {
                let signed = k as i32;
                let (delta, carry) = if matches!(op, BinOp::Add) {
                    (signed, 0i16)
                } else {
                    (-signed, -1i16)
                };
                if let Ok(delta_i8) = i8::try_from(delta) {
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}\r\n");
                    let _ = write!(self.out, "\tadd\tdx,{delta_i8}\r\n");
                    let _ = write!(self.out, "\tadc\tax,{carry}\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                    return;
                }
            }
            panic!("non-constant long assignment to global not yet supported (no fixture)");
        }
        let width = if matches!(ty, Type::Char) { "byte" } else { "word" };
        if let Some(v) = try_const_eval(value) {
            let v_masked = if matches!(ty, Type::Char) { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(
                self.out,
                "\tmov\t{width} ptr DGROUP:_{name},{v_masked}\r\n",
            );
            return;
        }
        // Non-constant: compute into AX, then store.
        self.emit_expr_to_ax(value);
        if matches!(ty, Type::Char) {
            let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
        } else {
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
        }
    }

    fn emit_assign_local(&mut self, loc: LocalLocation, value: &Expr) {
        match loc {
            LocalLocation::Stack(off) => {
                // No fixture yet for "assign constant to stack local" —
                // mirror the init form (immediate-store) when possible.
                if let Some(v) = try_const_eval(value) {
                    let _ = write!(self.out, "\tmov\tword ptr {},{v}\r\n", bp_addr(off));
                    return;
                }
                self.emit_expr_to_ax(value);
                let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
            }
            LocalLocation::Reg(reg) => self.emit_store_reg(reg, value),
        }
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
                // Globals first: if this name is file-scope, lower
                // to a `<width> ptr DGROUP:_<name>` reference rather
                // than a stack/register access (fixtures 083–087).
                if let Some(gty) = self.globals.type_of(name) {
                    if matches!(gty, Type::Char) {
                        let _ = write!(
                            self.out,
                            "\tmov\tal,byte ptr DGROUP:_{name}\r\n",
                        );
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
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
                    LocalLocation::Stack(off) if matches!(ty, Type::Char) => {
                        // Char on stack into AX: load AL then sign-extend.
                        let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                    }
                    LocalLocation::Reg(reg) if reg.is_byte() => {
                        // Char in a byte register into AX: copy AL then
                        // sign-extend (fixture 053).
                        let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
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
                    // Shifts encode the left operand's signedness in
                    // the mnemonic (`shr` vs `sar`); everything else
                    // is signedness-agnostic at the instruction level.
                    let unsigned = self.expr_is_unsigned(left);
                    self.emit_expr_to_ax(left);
                    self.emit_binary_right(*op, right, unsigned);
                }
            }
            ExprKind::Unary { op, operand } => self.emit_unary(*op, operand),
            ExprKind::Logical { op, left, right } => {
                self.emit_logical_to_ax(e.span.start, *op, left, right);
            }
            ExprKind::Update { target, op, position } => {
                self.emit_update_to_ax(target, *op, *position);
            }
            ExprKind::AssignExpr { .. } => {
                // No fixture yet exercises an assignment-expression
                // in value position (we don't materialize its value).
                // `for`-init/step go through `emit_expr_discard`.
                panic!("AssignExpr in value position not yet supported (no fixture)");
            }
            ExprKind::Call { name, args } => self.emit_call(name, args),
            ExprKind::AddressOf(name) => self.emit_address_of(name),
            ExprKind::AddressOfArrayElem { .. } => {
                panic!("`&arr[K]` in value position not yet supported (no fixture)")
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
        }
    }

    /// Lower `(<ty>) <operand>` into AX. The narrowing int→char case
    /// (the only one with a fixture today, 170) fuses the load with
    /// the truncate: `mov al, byte ptr [bp-N]; cbw` when the operand
    /// is a stack-int local — exactly what BCC emits for reading a
    /// char-typed local from that offset. Widening / no-op casts just
    /// evaluate the operand into AX.
    fn emit_cast_to_ax(&mut self, ty: &Type, operand: &Expr) {
        if matches!(ty, Type::Char) {
            if let ExprKind::Ident(name) = &operand.kind
                && !self.globals.contains(name)
            {
                let op_ty = self.locals.type_of(name).clone();
                if matches!(op_ty, Type::Int)
                    && let LocalLocation::Stack(off) = self.locals.location_of(name)
                {
                    let _ =
                        write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                    return;
                }
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
        let false_slot = base + 1;
        let merge_slot = base + 2;
        self.emit_cond_branch(cond, None, Some(false_slot));
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
        // ±1 / ±2 peephole: BCC emits `inc ax` / `dec ax` for ±1 (1
        // byte each vs. 3 for `add ax, 1` / `sub ax, 1`), and a *pair*
        // of `inc` / `dec` for ±2 (2 bytes vs. 3). At ±3 the cost of
        // three inc/dec ties with `add/sub ax, K`, and BCC switches
        // to the `add` / `sub` form. Confirmed on fixtures 027–031
        // (±1) and 076 case 1 (`r = r + 2` → `inc ax / inc ax`).
        if matches!(op, BinOp::Add | BinOp::Sub)
            && let Some(v) = try_const_eval(e)
            && (v == 1 || v == 2)
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
            emit_op_with_source(self.out, op, &OperandSource::Reg(Reg::Dx), unsigned);
            return;
        }
        let src = self.resolve_operand_source(e);
        emit_op_with_source(self.out, op, &src, unsigned);
    }

    /// True iff `name` refers to an identifier (global or local)
    /// whose static type is `char`. Used by `emit_binary_right` to
    /// detect when the right operand needs the widening dance.
    fn ident_is_char(&self, name: &str) -> bool {
        if let Some(ty) = self.globals.type_of(name) {
            return matches!(ty, Type::Char);
        }
        // The locals analyzer panics on unknown names, so only ask
        // if there's no global match.
        matches!(self.locals.type_of(name), Type::Char)
    }

    /// Resolve the right operand to a textual asm source operand. Today
    /// either an immediate (constant-foldable), a register-resident
    /// local, or a `word ptr [bp-N]` stack local.
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
            ExprKind::ArrayIndex { .. } => {
                // `g[K]` where `g` is a file-scope array — fold to
                // `word ptr DGROUP:_g+(K*stride)`. Fixture 189 emits
                // `add ax, word ptr DGROUP:_a+2` for `a[1]`.
                let mut indices: Vec<&Expr> = Vec::new();
                let mut cur = e;
                let name = loop {
                    match &cur.kind {
                        ExprKind::ArrayIndex { array, index } => {
                            indices.push(index);
                            cur = array;
                        }
                        ExprKind::Ident(n) => break n.clone(),
                        _ => panic!("array-index rhs: non-ident base not supported"),
                    }
                };
                indices.reverse();
                let Some(gty) = self.globals.type_of(&name) else {
                    panic!("array-indexed rhs only supported on globals so far");
                };
                let gty = gty.clone();
                let (const_off, _leaf_ty) =
                    try_const_array_offset(&gty, indices.iter().copied())
                        .unwrap_or_else(|| {
                            panic!("variable-indexed global array rhs not yet supported")
                        });
                OperandSource::GlobalOffset { name, offset: const_off }
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
            ExprKind::Member { kind: crate::ast::MemberKind::Arrow, .. } => {
                // `p->x` as a right operand would need a register-
                // indirect operand source. No fixture yet.
                panic!("`p->x` as right operand not yet supported (no fixture)")
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
        }
    }
}

/// Emit the operator-specific instruction(s) given an already-loaded AX
/// (left operand) and a source string for the right operand. `unsigned`
/// selects `shr` over `sar` for `Shr` — the left operand's static type
/// drives the choice (right is always the shift count).
fn emit_op_with_source(out: &mut Vec<u8>, op: BinOp, src: &OperandSource, unsigned: bool) {
    match op {
        BinOp::Add => {
            let _ = write!(out, "\tadd\tax,{}\r\n", src.word());
        }
        BinOp::Sub => {
            let _ = write!(out, "\tsub\tax,{}\r\n", src.word());
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
            if matches!(src, OperandSource::Immediate(_)) {
                panic!("imul with immediate not yet supported (80186+ only)");
            }
            let _ = write!(out, "\timul\t{}\r\n", src.word());
        }
        BinOp::Div => {
            if matches!(src, OperandSource::Immediate(_)) {
                panic!("idiv with immediate not supported (no such encoding)");
            }
            out.extend_from_slice(b"\tcwd\t\r\n");
            let _ = write!(out, "\tidiv\t{}\r\n", src.word());
        }
        BinOp::Mod => {
            if matches!(src, OperandSource::Immediate(_)) {
                panic!("idiv with immediate not supported (no such encoding)");
            }
            out.extend_from_slice(b"\tcwd\t\r\n");
            let _ = write!(out, "\tidiv\t{}\r\n", src.word());
            out.extend_from_slice(b"\tmov\tax,dx\r\n");
        }
        BinOp::Shl | BinOp::Shr => {
            let _ = write!(out, "\tmov\tcl,{}\r\n", src.byte());
            let mnemonic = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if unsigned => "shr",
                BinOp::Shr => "sar",
                _ => unreachable!(),
            };
            let _ = write!(out, "\t{mnemonic}\tax,cl\r\n");
        }
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            unreachable!("comparison op should take the cmp-as-value path");
        }
    }
}
