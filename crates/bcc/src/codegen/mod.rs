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

use std::io::Write as _;

use crate::ast::{BinOp, Expr, ExprKind, Function, Stmt, StmtKind};

mod fold;
mod line_map;
mod locals;
mod plan;

use fold::try_const_eval;
use line_map::LineMap;
use locals::{LocalLocation, Locals, Reg};
use plan::LabelPlan;

/// Emit the per-function chunk of an `-S` file for one function.
pub fn emit_function(out: &mut Vec<u8>, source: &str, function: &Function, func_idx: u32) {
    let mut emitter = FunctionEmitter::new(out, source, function, func_idx);
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
}

impl<'a> FunctionEmitter<'a> {
    fn new(
        out: &'a mut Vec<u8>,
        source: &'a str,
        function: &'a Function,
        func_idx: u32,
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
            StmtKind::Declare { name, init, .. } => {
                self.advance_to_stmt_line(stmt);
                if let Some(init) = init {
                    let loc = self.locals.location_of(name);
                    self.emit_init_local(loc, init);
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
        if let Some(else_stmts) = else_branch {
            // if/else reserves 3 slots; the else label lives at +2.
            let else_slot = base + 2;
            self.emit_cond_jump_if_false(cond, else_slot);
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
            self.emit_cond_jump_if_false(cond, skip_slot);
            for s in then_branch {
                self.emit_stmt(s);
            }
            self.emit_label(skip_slot);
        }
    }

    fn emit_while(&mut self, while_span_start: u32, cond: &Expr, body: &[Stmt]) {
        // While reserves 3 slots: +0 body, +1 check, +2 unused.
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
        // Condition emits a true-mnemonic jump back to the body.
        self.emit_cond_jump_if_true(cond, body_slot);
    }

    /// Emit a conditional jump that jumps to `target_slot` when `cond`
    /// is FALSE. Used by `if`. Picks the best emission shape based on
    /// where the LHS lives and whether the RHS folds to a constant; see
    /// `specs/bcc/ASM_OUTPUT.md` for the priority order.
    fn emit_cond_jump_if_false(&mut self, cond: &Expr, target_slot: u32) {
        let (op, left, right) = expect_comparison(cond);
        let inv = op.jump_if_false().expect("comparison op has inverse jump");
        self.emit_compare(left, right);
        let target = self.label_ref(target_slot);
        let _ = write!(self.out, "\t{inv}\tshort {target}\r\n");
    }

    /// Same as `emit_cond_jump_if_false` but uses the *true* mnemonic
    /// — for `while`, where the loop-back jump fires when the
    /// condition holds.
    fn emit_cond_jump_if_true(&mut self, cond: &Expr, target_slot: u32) {
        let (op, left, right) = expect_comparison(cond);
        let mnemonic = op.jump_if_true().expect("comparison op has true jump");
        self.emit_compare(left, right);
        let target = self.label_ref(target_slot);
        let _ = write!(self.out, "\t{mnemonic}\tshort {target}\r\n");
    }

    /// Emit just the `cmp` instruction (no jump). Three shapes,
    /// matching what BCC produces:
    ///
    /// 1. LHS in a register: `cmp <reg>, <rhs>`
    /// 2. LHS is a stack local and RHS is a constant: `cmp word ptr [bp-N], K`
    /// 3. Otherwise: `mov ax, <lhs>` then `cmp ax, <rhs>`
    fn emit_compare(&mut self, left: &Expr, right: &Expr) {
        if let Some(reg) = self.ident_in_register(left) {
            let src = self.resolve_operand_source(right);
            let _ = write!(self.out, "\tcmp\t{},{}\r\n", reg.name(), src.word());
            return;
        }
        if let (ExprKind::Ident(name), Some(rhs)) = (&left.kind, try_const_eval(right))
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            let _ = write!(self.out, "\tcmp\tword ptr [bp-{off}],{rhs}\r\n");
            return;
        }
        self.emit_expr_to_ax(left);
        let src = self.resolve_operand_source(right);
        let _ = write!(self.out, "\tcmp\tax,{}\r\n", src.word());
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
    fn emit_init_local(&mut self, loc: LocalLocation, init: &Expr) {
        match loc {
            LocalLocation::Stack(off) => {
                // Stack init: prefer the immediate-store form when the
                // initializer folds to a constant.
                if let Some(v) = try_const_eval(init) {
                    let _ = write!(self.out, "\tmov\tword ptr [bp-{off}],{v}\r\n");
                    return;
                }
                self.emit_expr_to_ax(init);
                let _ = write!(self.out, "\tmov\tword ptr [bp-{off}],ax\r\n");
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
                    let _ = write!(self.out, "\tmov\tword ptr [bp-{off}],{v}\r\n");
                    return;
                }
                self.emit_expr_to_ax(value);
                let _ = write!(self.out, "\tmov\tword ptr [bp-{off}],ax\r\n");
            }
            LocalLocation::Reg(reg) => self.emit_store_reg(reg, value),
        }
    }

    /// Store `expr`'s value into register `reg`. For constants we use
    /// the same special cases as AX (`xor reg,reg` for zero, otherwise
    /// `mov reg,K`). For everything else we compute into AX and copy.
    fn emit_store_reg(&mut self, reg: Reg, expr: &Expr) {
        if let Some(v) = try_const_eval(expr) {
            if v == 0 {
                let _ = write!(self.out, "\txor\t{0},{0}\r\n", reg.name());
            } else {
                let _ = write!(self.out, "\tmov\t{},{v}\r\n", reg.name());
            }
            return;
        }
        self.emit_expr_to_ax(expr);
        let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
    }

    /// Emit code that leaves the value of `e` in AX.
    fn emit_expr_to_ax(&mut self, e: &Expr) {
        if let Some(v) = try_const_eval(e) {
            if v == 0 {
                self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{v}\r\n");
            }
            return;
        }
        match &e.kind {
            ExprKind::IntLit(_) => unreachable!("literals fold via try_const_eval"),
            ExprKind::Ident(name) => match self.locals.location_of(name) {
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tax,word ptr [bp-{off}]\r\n");
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                }
            },
            ExprKind::BinOp { op, left, right } => {
                if op.is_comparison() {
                    self.emit_comparison_as_value(e.span.start, *op, left, right);
                } else {
                    self.emit_expr_to_ax(left);
                    self.emit_binary_right(*op, right);
                }
            }
            ExprKind::Call { name } => {
                let _ = write!(self.out, "\tcall\tnear ptr _{name}\r\n");
            }
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

/// Destructure an expression that should be a comparison BinOp, or
/// panic. Used at the entry to `emit_cond_jump_*` and friends.
fn expect_comparison(e: &Expr) -> (BinOp, &Expr, &Expr) {
    let ExprKind::BinOp { op, left, right } = &e.kind else {
        panic!("non-comparison if/while condition not yet supported");
    };
    assert!(
        op.is_comparison(),
        "non-comparison binop in if/while condition not yet supported"
    );
    (*op, left, right)
}

/// A resolved right-hand operand.
enum OperandSource {
    Immediate(u32),
    Local(u16),
    Reg(Reg),
}

impl OperandSource {
    /// Format as a 16-bit source operand.
    fn word(&self) -> String {
        match self {
            Self::Immediate(v) => v.to_string(),
            Self::Local(off) => format!("word ptr [bp-{off}]"),
            Self::Reg(r) => r.name().to_owned(),
        }
    }

    /// Byte form, used for shift counts (`mov cl, byte ptr ...`).
    fn byte(&self) -> String {
        match self {
            Self::Immediate(v) => v.to_string(),
            Self::Local(off) => format!("byte ptr [bp-{off}]"),
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
