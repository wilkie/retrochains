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
    BinOp, Expr, ExprKind, Function, LogicalOp, Stmt, StmtKind, Type, UnaryOp, Unit, UpdateOp,
    UpdatePosition,
};

/// Maps each function's name to the declared types of its parameters,
/// in source order. Built once per translation unit and consulted at
/// call sites so we know whether to push each argument as a byte or
/// a word (fixture 052: `f(1)` where `f` takes `char` becomes
/// `mov al,1 / push ax`, not `mov ax,1 / push ax`).
#[derive(Debug, Default)]
pub struct Signatures {
    map: HashMap<String, Vec<Type>>,
}

impl Signatures {
    #[must_use]
    pub fn from_unit(unit: &Unit) -> Self {
        let map = unit
            .functions
            .iter()
            .map(|f| (f.name.clone(), f.params.iter().map(|p| p.ty).collect()))
            .collect();
        Self { map }
    }

    /// Look up the declared parameter types of a function. Returns
    /// `None` if the name isn't defined in this TU (extern function).
    /// Callers should default to `int` widths for missing signatures —
    /// we have no fixture for extern char-arg calls yet.
    #[must_use]
    pub fn params_of(&self, name: &str) -> Option<&[Type]> {
        self.map.get(name).map(Vec::as_slice)
    }
}

mod fold;
mod line_map;
mod locals;
mod plan;

use fold::try_const_eval;
use line_map::LineMap;
use locals::{LocalLocation, Locals, ParamLoad, Reg};

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
use plan::LabelPlan;

/// Emit the per-function chunk of an `-S` file for one function.
pub fn emit_function(
    out: &mut Vec<u8>,
    source: &str,
    function: &Function,
    func_idx: u32,
    signatures: &Signatures,
) {
    let mut emitter = FunctionEmitter::new(out, source, function, func_idx, signatures);
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
}

impl<'a> FunctionEmitter<'a> {
    fn new(
        out: &'a mut Vec<u8>,
        source: &'a str,
        function: &'a Function,
        func_idx: u32,
        signatures: &'a Signatures,
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
        for stmt in &self.function.body {
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
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Return(value) => {
                self.advance_to_stmt_line(stmt);
                self.emit_return_value_load(value.as_ref());
                let exit = self.exit_label_num();
                let _ = write!(self.out, "\tjmp\tshort @{}@{exit}\r\n", self.func_idx);
            }
            StmtKind::Declare { name, init, ty } => {
                self.advance_to_stmt_line(stmt);
                if let Some(init) = init {
                    let loc = self.locals.location_of(name);
                    self.emit_init_local(loc, *ty, init);
                }
            }
            StmtKind::Assign { name, value } => {
                self.advance_to_stmt_line(stmt);
                let loc = self.locals.location_of(name);
                self.emit_assign_local(loc, value);
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
            StmtKind::ExprStmt(expr) => {
                self.advance_to_stmt_line(stmt);
                self.emit_expr_discard(expr);
            }
        }
    }

    /// Emit `expr` for its side effects, discarding the value. The
    /// special case is `Update` (`++x;` / `x++;`): BCC emits just the
    /// increment, no `mov ax, ...` afterward (fixture 040).
    fn emit_expr_discard(&mut self, expr: &Expr) {
        if let ExprKind::Update { target, op, .. } = &expr.kind {
            self.emit_update_in_place(target, *op);
            return;
        }
        // Other expressions: compute into AX, drop the result.
        self.emit_expr_to_ax(expr);
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
        match self.locals.location_of(name) {
            LocalLocation::Reg(reg) if reg.is_byte() => {
                let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            }
            LocalLocation::Reg(reg) => {
                let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
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
        // While reserves 3 slots: +0 body, +1 check, +2 unused.
        // Logical conditions (`&& / ||`) in a while are unobserved —
        // they'd need an explicit "past-loop" label since the
        // standard `cond / j-true body / fall-through` shape only
        // has *one* labeled target. Bail until a fixture appears.
        assert!(
            !matches!(cond.kind, ExprKind::Logical { .. }),
            "logical condition (`&&`/`||`) in a `while` not yet supported (no fixture)"
        );
        let base = self.label_plan.base(while_span_start);
        let body_slot = base;
        let check_slot = base + 1;
        // Trampoline jump to the check, then body label.
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(check_slot));
        self.emit_label(body_slot);
        for s in body {
            self.emit_stmt(s);
        }
        self.emit_label(check_slot);
        // Condition: jump back to body if true; fall through if false.
        self.emit_cond_branch(cond, Some(body_slot), None);
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
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && op.is_comparison()
        {
            self.emit_compare(left, right);
            return (
                op.jump_if_true().expect("comparison op has true mnemonic"),
                op.jump_if_false().expect("comparison op has false mnemonic"),
            );
        }
        self.emit_zero_test(cond);
        ("jne", "je")
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
        for (i, arg) in args.iter().enumerate().rev() {
            // Param type for the i-th arg, defaulting to int when the
            // signature isn't known (extern function — no fixture yet).
            let arg_ty = param_tys.and_then(|tys| tys.get(i)).copied().unwrap_or(Type::Int);
            self.emit_arg_into_ax(arg, arg_ty);
            self.out.extend_from_slice(b"\tpush\tax\r\n");
        }
        let _ = write!(self.out, "\tcall\tnear ptr _{name}\r\n");
        match args.len() {
            0 => {}
            1 | 2 => {
                for _ in args {
                    self.out.extend_from_slice(b"\tpop\tcx\r\n");
                }
            }
            n => {
                let _ = write!(self.out, "\tadd\tsp,{}\r\n", n * 2);
            }
        }
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
        self.emit_expr_to_ax(e);
    }

    /// Initialize a freshly-declared local with `init`.
    fn emit_init_local(&mut self, loc: LocalLocation, ty: Type, init: &Expr) {
        match loc {
            LocalLocation::Stack(off) => {
                // Stack init: prefer the immediate-store form when the
                // initializer folds to a constant. For `char` we emit
                // `byte ptr` (fixture 011); for `int`, `word ptr`.
                if let Some(v) = try_const_eval(init) {
                    let width = ptr_width(ty);
                    let _ = write!(self.out, "\tmov\t{width} ptr {},{v}\r\n", bp_addr(off));
                    return;
                }
                // Non-constant init for a char would need a different
                // shape (load to AL, store AL); no fixture yet.
                assert!(
                    matches!(ty, Type::Int),
                    "non-constant init for `char` not yet supported (no fixture)"
                );
                self.emit_expr_to_ax(init);
                let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
            }
            LocalLocation::Reg(reg) => self.emit_store_reg(reg, init),
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
                let ty = self.locals.type_of(name);
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
                    self.emit_expr_to_ax(left);
                    self.emit_binary_right(*op, right);
                }
            }
            ExprKind::Unary { op, operand } => self.emit_unary(*op, operand),
            ExprKind::Logical { op, left, right } => {
                self.emit_logical_to_ax(e.span.start, *op, left, right);
            }
            ExprKind::Update { target, op, position } => {
                self.emit_update_to_ax(target, *op, *position);
            }
            ExprKind::Call { name, args } => self.emit_call(name, args),
        }
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
        let inv = op.jump_if_false().expect("comparison op has inverse jump");

        self.emit_compare(left, right);
        let _ = write!(self.out, "\t{inv}\tshort {}\r\n", self.label_ref(false_slot));
        self.out.extend_from_slice(b"\tmov\tax,1\r\n");
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(end_slot));
        self.emit_label(false_slot);
        self.out.extend_from_slice(b"\txor\tax,ax\r\n");
        self.emit_label(end_slot);
    }

    /// Emit the right-hand side of a binary op, applying it to AX.
    fn emit_binary_right(&mut self, op: BinOp, e: &Expr) {
        // +1 / -1 peephole: BCC emits `inc ax` for "AX = AX + 1" and
        // `dec ax` for "AX = AX - 1" (one-byte encodings, smaller than
        // the corresponding `add ax,1` / `sub ax,1`). Confirmed on
        // fixtures 027–031 (all in `<reg> = <reg> + 1` form).
        if matches!(op, BinOp::Add | BinOp::Sub)
            && let Some(1) = try_const_eval(e)
        {
            let mnemonic = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnemonic}\tax\r\n");
            return;
        }
        let src = self.resolve_operand_source(e);
        emit_op_with_source(self.out, op, &src);
    }

    /// Resolve the right operand to a textual asm source operand. Today
    /// either an immediate (constant-foldable), a register-resident
    /// local, or a `word ptr [bp-N]` stack local.
    fn resolve_operand_source(&self, e: &Expr) -> OperandSource {
        if let Some(v) = try_const_eval(e) {
            return OperandSource::Immediate(v);
        }
        match &e.kind {
            ExprKind::Ident(name) => match self.locals.location_of(name) {
                LocalLocation::Stack(off) => OperandSource::Local(off),
                LocalLocation::Reg(reg) => OperandSource::Reg(reg),
            },
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

/// Width keyword for a `mov ptr [bp-N], K` store of the given type:
/// `"byte"` for `char`, `"word"` for `int`. Currently used only by
/// initialization of stack-resident locals.
fn ptr_width(ty: Type) -> &'static str {
    match ty {
        Type::Int => "word",
        Type::Char => "byte",
    }
}

/// A resolved right-hand operand.
enum OperandSource {
    Immediate(u32),
    /// Stack-resident local or param at a (signed) bp offset.
    Local(i16),
    Reg(Reg),
}

impl OperandSource {
    /// Format as a 16-bit source operand.
    fn word(&self) -> String {
        match self {
            Self::Immediate(v) => v.to_string(),
            Self::Local(off) => format!("word ptr {}", bp_addr(*off)),
            Self::Reg(r) => r.name().to_owned(),
        }
    }

    /// Byte form, used for shift counts (`mov cl, byte ptr ...`).
    fn byte(&self) -> String {
        match self {
            Self::Immediate(v) => v.to_string(),
            Self::Local(off) => format!("byte ptr {}", bp_addr(*off)),
            // A register holding an int provides the low byte via
            // its `*L` half; we'd need a separate fixture to confirm
            // BCC's exact shape. Panic until we see one.
            Self::Reg(_) => panic!("shift count from a register local not yet supported"),
        }
    }
}

/// Emit the operator-specific instruction(s) given an already-loaded AX
/// (left operand) and a source string for the right operand.
fn emit_op_with_source(out: &mut Vec<u8>, op: BinOp, src: &OperandSource) {
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
