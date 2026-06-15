use super::*;

impl<'a> super::FunctionEmitter<'a> {
    pub(crate) fn emit_stmt(&mut self, stmt: &Stmt) {
        self.emit_stmt_inner(stmt);
        // Statement boundary: flush any deferred postinc/postdec that
        // BCC emits after the consumer of the loaded value. Fixture
        // 2000 (`sum = *p++` chain).
        self.flush_pending_post_update();
    }
    pub(crate) fn emit_stmt_inner(&mut self, stmt: &Stmt) {
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
            StmtKind::Declare { name, init, ty, is_static, is_register: _, is_volatile: _ } => {
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
                // Pseudo-register LHS: `_AX = K;` lowers to a direct
                // `mov ax, K`, no locals/globals involvement. Fixtures
                // 4051 (`_AX = 0xabcd;`), 4053 (`_AH = 0x80;`).
                if is_asm_pseudo_register(name) {
                    self.emit_assign_pseudo_register(name, value);
                    return;
                }
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
                self.emit_if(stmt.span.start, stmt.span.end, cond, then_branch, else_branch.as_deref());
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
            StmtKind::Asm { body } => {
                self.advance_to_stmt_line(stmt);
                self.emit_asm_block(body);
            }
            StmtKind::Block(body) => {
                // Bare `{ ... }` block at statement position — emit
                // the inner statements in order. Block-scope rules
                // are enforced by the locals layout pass; codegen
                // just walks. Fixtures 1743, 1966-1969, 3014.
                for s in body {
                    self.emit_stmt(s);
                }
            }
        }
    }
    pub(crate) fn emit_if(
        &mut self,
        if_span_start: u32,
        if_span_end: u32,
        cond: &Expr,
        then_branch: &[Stmt],
        else_branch: Option<&[Stmt]>,
    ) {
        // `if (K)` with K a constant non-zero — BCC elides the
        // compare/branch entirely and inlines the then-branch (the
        // else, if any, is unreachable and BCC drops it). Fixture
        // 931 (`if (1) { return 7; } return 0;`).
        //
        // `if (0)`: BCC does NOT elide. It still allocates the
        // skip-target label and emits an unconditional `jmp short
        // <skip>` over the dead then-block; the then-block code is
        // emitted verbatim (unreachable). Same shape for `if (0)
        // ... else ...`: jmp over then to the else. Fixture 1585
        // (`if (0) return 5; return 10;`).
        if let Some(v) = try_const_eval(cond)
            && v != 0
            && else_branch.is_none()
        {
            for s in then_branch {
                self.emit_stmt(s);
            }
            return;
        }
        let base = self.label_plan.base(if_span_start, if_span_end);
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
        // `a && (b || c)` — the inner `||` short-circuits to the
        // then-branch entry, which needs an explicit label.
        // Fixture 2615.
        fn has_reachable_or(e: &Expr) -> bool {
            match &e.kind {
                ExprKind::Logical { op: LogicalOp::Or, .. } => true,
                ExprKind::Logical { op: LogicalOp::And, left, right } => {
                    has_reachable_or(left) || has_reachable_or(right)
                }
                _ => false,
            }
        }
        let cond_has_nested_or = matches!(
            cond.kind,
            ExprKind::Logical { op: LogicalOp::And, .. },
        ) && has_reachable_or(cond);
        let needs_then_entry = cond_has_top_or
            || cond_has_nested_or
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
            //
            // Special case: when both branches end in `return`, the
            // merge label is dead (both returns already jumped to
            // exit). BCC still emits a trailing `jmp` after the
            // then-branch but retargets it to the exit slot (a
            // sensible always-valid jump target) and drops the
            // merge label. Fixture 026.
            let else_slot = base + 2;
            let merge_slot = base + 1;
            let then_ends_in_return = then_branch
                .last()
                .is_some_and(|s| matches!(s.kind, StmtKind::Return(_)));
            let else_ends_in_return = else_stmts
                .last()
                .is_some_and(|s| matches!(s.kind, StmtKind::Return(_)));
            let merge_dead = then_ends_in_return && else_ends_in_return;
            self.emit_cond_branch(cond, then_entry_slot, Some(else_slot));
            if let Some(slot) = then_entry_slot {
                self.emit_label(slot);
            }
            for s in then_branch {
                self.emit_stmt(s);
            }
            let trailing_jmp_target = if merge_dead {
                self.label_plan.exit_slot()
            } else {
                merge_slot
            };
            let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(trailing_jmp_target));
            self.emit_label(else_slot);
            for s in else_stmts {
                self.emit_stmt(s);
            }
            if !merge_dead {
                self.emit_label(merge_slot);
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
    pub(crate) fn emit_while(&mut self, while_span_start: u32, cond: &Expr, body: &[Stmt]) {
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
            // Pass `None` as the false slot: the cond's false-path
            // falls through to `break_target_slot` (emitted right
            // after). This mirrors do-while's shape and makes the
            // OR/AND lowering treat fall-through as the FALSE
            // direction (not the TRUE direction it would assume from
            // an `if` cond). Fixture 3233 (`while (i < n || data[0]
            // != 0)`).
            self.emit_cond_branch(
                cond,
                Some(plan.body_slot),
                None,
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
    pub(crate) fn emit_do_while(&mut self, do_span_start: u32, body: &[Stmt], cond: &Expr) {
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
    pub(crate) fn emit_for(
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
            // Logical cond (`&&`/`||`): mirror while/do-while by
            // passing `None` for the false slot. The cond's false
            // path falls through to `break_target_slot`, emitted
            // right after, so the OR/AND lowering can treat fall-
            // through as the FALSE direction (matching the natural
            // layout of a loop's check-then-exit shape). Fixture
            // 3331 (`for (i = 0; i < n && p[i] != 0; i++) ;`).
            let cond_is_logical = matches!(c.kind, ExprKind::Logical { .. });
            self.emit_cond_branch(c, Some(plan.body_slot), None);
            if cond_is_logical {
                self.emit_label(plan.break_target_slot);
                return;
            }
        } else {
            // Missing cond means infinite loop — unconditional back-jump.
            let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(plan.body_slot));
        }
        if body_has_break(body) {
            self.emit_label(plan.break_target_slot);
        }
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
    pub(crate) fn emit_switch(&mut self, switch_span_start: u32, scrutinee: &Expr, cases: &[SwitchCase]) {
        let plan = self.label_plan.switch_plan(switch_span_start).clone();
        self.advance_to_stmt_line_at(switch_span_start);
        // Override the plan-time strategy for long-scrutinee
        // switches — the planner runs without type info, so it
        // defaults to Chained for a 3-case fixture like 1913, but
        // BCC always uses the linear-search-with-both-halves loop
        // for longs because there's no shorter int-style dispatch
        // that can compare a 4-byte value in two parts.
        let strategy = if scrutinee_is_long_typed(scrutinee, &self.locals, self.globals) {
            SwitchStrategy::LongLinearSearch
        } else {
            plan.strategy
        };
        match strategy {
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
            SwitchStrategy::LongLinearSearch => {
                self.emit_switch_long_linear_search(
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
    pub(crate) fn emit_switch_chained(
        &mut self,
        scrutinee: &Expr,
        cases: &[SwitchCase],
        case_slots: &[u32],
        end_slot: u32,
    ) {
        // Empty switch `switch (x) {}` — BCC emits a trampoline jmp
        // to the end-of-switch label (`eb 00`-equivalent) and the
        // end label, nothing else. The scrutinee load is skipped
        // entirely. Fixture 2893.
        if cases.is_empty() {
            let _ = write!(
                self.out,
                "\tjmp\tshort {}\r\n",
                self.label_ref(end_slot),
            );
            return;
        }
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
        //
        // `switch (n % K)` special-cases: idiv already leaves the
        // remainder in DX, so BCC skips the trailing `mov ax, dx`
        // and dispatches against DX. Fixture 1448 (`switch (n % 3)`).
        let scrut_in_dx = matches!(&scrutinee.kind, ExprKind::BinOp { op: BinOp::Mod, .. });
        let scrut_loaded = match &scrutinee.kind {
            ExprKind::Ident(_) => false,
            _ => {
                if scrut_in_dx {
                    self.skip_mod_to_ax = true;
                    self.emit_expr_to_ax(scrutinee);
                    self.skip_mod_to_ax = false;
                } else {
                    self.emit_expr_to_ax(scrutinee);
                }
                true
            }
        };
        let scrut_reg = if scrut_in_dx { "dx" } else { "ax" };
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
        // Compare/branch chain: one cmp+je per non-default case.
        // BCC sorts the chain by ascending (signed) case value, not
        // source order — the case bodies/labels stay in source order,
        // only the dispatch comparisons are reordered (fixture 4206,
        // `case HIGH/MID/LOW` declared 3,2,1 dispatches as 1,2,3).
        // `case 0` uses `or ax,ax` (cf. fixture 072).
        let default_slot = cases
            .iter()
            .zip(case_slots)
            .find_map(|(c, &s)| c.value.is_none().then_some(s));
        let mut chain: Vec<(i32, u32)> = cases
            .iter()
            .zip(case_slots)
            .filter_map(|(c, &s)| c.value.map(|v| (v as i16 as i32, s)))
            .collect();
        chain.sort_by_key(|&(v, _)| v);
        for &(v, slot) in &chain {
            let v16 = v as u32 & 0xFFFF;
            if v16 == 0 {
                let _ = write!(self.out, "\tor\t{scrut_reg},{scrut_reg}\r\n");
            } else if v16 < 256 && scrut_in_dx {
                // Small immediate against DX uses the
                // sign-extended `83 fa ii` form (3 bytes vs
                // 4 for `cmp dx, imm16`). Fixture 1448.
                let _ = write!(self.out, "\tcmp\t{scrut_reg},{v16}\r\n");
            } else {
                let _ = write!(self.out, "\tcmp\t{scrut_reg},{v16}\r\n");
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
    pub(crate) fn emit_switch_jump_table(
        &mut self,
        scrutinee: &Expr,
        cases: &[SwitchCase],
        case_slots: &[u32],
        end_slot: u32,
    ) {
        // Planner picked this strategy only for dense-from-K runs.
        // The default case (if any) is the only None-value case; it
        // becomes the bounds-check failure target. The value cases
        // must form a dense run when sorted — source order doesn't
        // matter, but the address table is laid out in sorted order
        // (so the indexed jump lands at the right body).
        let default_slot = cases
            .iter()
            .zip(case_slots)
            .find_map(|(c, s)| if c.value.is_none() { Some(*s) } else { None });
        let mut value_pairs: Vec<(u32, u32)> = cases
            .iter()
            .zip(case_slots)
            .filter_map(|(c, &slot)| c.value.map(|v| (v, slot)))
            .collect();
        // Sort by SIGNED value so negative cases (e.g. -2, -1, 0, 1)
        // come out in source-numeric order, not unsigned-wraparound
        // order. Fixture 1909 (cases -2..1).
        value_pairs.sort_by_key(|&(v, _)| v as i32 as i64);
        let case_base = value_pairs.first()
            .map(|&(v, _)| v)
            .expect("jump-table needs at least one value case");
        // The table spans `base..=last`. Slots for missing values
        // (gaps) are filled with default-or-end. Fixture 1904
        // (cases 1, 2, 4, 5 — gap at 3 fills with end_slot).
        let last = value_pairs.last().expect("non-empty").0;
        let span = (last as i32).wrapping_sub(case_base as i32);
        let table_len = u32::try_from(span).unwrap_or(0) + 1;
        let max_value = table_len - 1;

        // Load scrutinee into BX. Bare int-typed Ident takes the
        // direct path (mov bx, <addr>/<reg>). `<ident> +/- <const>`
        // folds the constant into a `inc bx`/`dec bx`/`add bx, K`/
        // `sub bx, K` after the load. Char-typed Ident widens via
        // byte-load + cbw to AX, normalizes in AX, then mov bx,
        // ax. Anything else evaluates to AX, then mov bx, ax with
        // normalization on BX. Fixture 3650 (`switch (x - 1)`),
        // 3482 (`switch (c)` for char c).
        let mut effective_base = case_base;
        let mut normalize_in_ax = false;
        let loaded_via_ident = match &scrutinee.kind {
            ExprKind::Ident(name)
                if self.locals.type_of(name).is_char_like() =>
            {
                let unsigned = self.locals.type_of(name).is_unsigned();
                let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                    panic!("char-typed switch scrutinee not in stack — no fixture");
                };
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                if unsigned {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
                normalize_in_ax = true;
                true
            }
            ExprKind::Ident(name)
                if matches!(self.locals.type_of(name), Type::Int) =>
            {
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
                true
            }
            ExprKind::BinOp { op, left, right }
                if (matches!(op, BinOp::Add) || matches!(op, BinOp::Sub))
                    && let ExprKind::Ident(name) = &left.kind
                    && matches!(self.locals.type_of(name), Type::Int)
                    && let Some(k) = try_const_eval(right) =>
            {
                // Fold `<ident> +/- K` into an adjustment on the
                // case_base: subtract from base instead of adding to
                // BX. The load is the same as the bare-ident path.
                match self.locals.location_of(name) {
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                    }
                    LocalLocation::Reg(reg) => {
                        let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                    }
                }
                let sign = if matches!(op, BinOp::Add) { 1i32 } else { -1 };
                let delta = sign.wrapping_mul(k as i32);
                // switch on `x + K` means scrutinee = x + K; case_base
                // is what `value` matches against. Adjust:
                // shifted_base = case_base - K.
                effective_base = (case_base as i32).wrapping_sub(delta) as u32;
                true
            }
            _ => {
                self.emit_expr_to_ax(scrutinee);
                self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                true
            }
        };
        let _ = loaded_via_ident;

        // Normalize scrutinee to 0..N-1 when effective_base != 0.
        // For char scrutinee (loaded into AX), do the sub in AX first
        // (uses the shorter `2D imm16` accumulator form), then
        // `mov bx, ax`. For other paths, BX is already loaded so the
        // sub goes on BX directly. K=±1 collapses to dec/inc.
        if normalize_in_ax {
            if effective_base != 0 {
                let k_signed = effective_base as i32;
                if k_signed == 1 {
                    self.out.extend_from_slice(b"\tdec\tax\r\n");
                } else if k_signed == -1 || (k_signed & 0xFFFF) == 0xFFFF {
                    self.out.extend_from_slice(b"\tinc\tax\r\n");
                } else {
                    let k16 = effective_base & 0xFFFF;
                    let _ = write!(self.out, "\tsub\tax,{k16}\r\n");
                }
            }
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
        } else if effective_base != 0 {
            let k_signed = effective_base as i32;
            if k_signed == 1 {
                self.out.extend_from_slice(b"\tdec\tbx\r\n");
            } else if k_signed == -1 || (k_signed & 0xFFFF) == 0xFFFF {
                self.out.extend_from_slice(b"\tinc\tbx\r\n");
            } else {
                let k16 = effective_base & 0xFFFF;
                let _ = write!(self.out, "\tsub\tbx,{k16}\r\n");
            }
        }

        // Bounds check: anything > max_value (unsigned, since out-of-
        // range negatives also overflow into > max when treated as
        // unsigned) jumps to default (if present) or end-of-switch.
        let out_of_range = default_slot.unwrap_or(end_slot);
        let _ = write!(self.out, "\tcmp\tbx,{max_value}\r\n");
        let _ = write!(self.out, "\tja\tshort {}\r\n", self.label_ref(out_of_range));
        self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
        let c_num = switch_c_num(SwitchStrategy::JumpTable, table_len);
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
        // The table is laid out by VALUE position (0..span) so the
        // jump-by-index lands at the matching body. Missing values
        // (gaps) fill with the out-of-range slot (default or end).
        // Default case doesn't get its own table entry.
        let _ = write!(
            self.post_function_data,
            "@{}@C{c_num}\tlabel\tword\r\n",
            self.func_idx,
        );
        let gap_slot = out_of_range;
        let mut value_iter = value_pairs.iter().peekable();
        for i in 0..table_len {
            let pos = case_base.wrapping_add(i);
            let slot = if let Some(&&(v, s)) = value_iter.peek() {
                if v == pos {
                    value_iter.next();
                    s
                } else {
                    gap_slot
                }
            } else {
                gap_slot
            };
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
    pub(crate) fn emit_switch_linear_search(
        &mut self,
        switch_span_start: u32,
        scrutinee: &Expr,
        cases: &[SwitchCase],
        case_slots: &[u32],
        end_slot: u32,
    ) {
        let default_slot = cases
            .iter()
            .zip(case_slots)
            .find_map(|(c, &s)| if c.value.is_none() { Some(s) } else { None });
        let value_cases: Vec<&SwitchCase> = cases.iter().filter(|c| c.value.is_some()).collect();
        let value_slots: Vec<u32> = cases
            .iter()
            .zip(case_slots)
            .filter_map(|(c, &s)| if c.value.is_some() { Some(s) } else { None })
            .collect();
        let case_count = u32::try_from(value_cases.len()).unwrap_or(u32::MAX);
        // Locals analyzer reserved a stack slot for the spilled
        // scrutinee; look up its offset by this switch's span_start.
        let spill_off = self.locals.switch_spill_offset(switch_span_start);

        // Load scrutinee into AX (any local kind works).
        let ExprKind::Ident(name) = &scrutinee.kind else {
            panic!("non-ident switch scrutinee not yet supported (no fixture)");
        };
        let scrutinee_ty = self.locals.type_of(name).clone();
        let is_char = scrutinee_ty.is_char_like();
        let unsigned = scrutinee_ty.is_unsigned();
        match self.locals.location_of(name) {
            LocalLocation::Stack(off) => {
                if is_char {
                    // `mov al, byte ptr [bp+off]` + widen to AX.
                    // Fixtures 3962 (signed default), 3965-style
                    // multi-case fall-through on char.
                    let _ = write!(
                        self.out,
                        "\tmov\tal,byte ptr {}\r\n",
                        bp_addr(off),
                    );
                    if unsigned {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                }
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
        // Pre-slots are based on the FIRST value case (default
        // doesn't shift them — slot reservation in plan.rs counts
        // total cases including default for the pre-slot budget,
        // but the loop_top/dispatch are computed off the first
        // value case body's slot).
        let first_value_slot = *value_slots.first().expect("at least one value case");
        let loop_top_slot = first_value_slot - 4;
        let dispatch_slot = first_value_slot - 1;

        self.emit_label(loop_top_slot);
        self.out.extend_from_slice(b"\tmov\tax,word ptr cs:[bx]\r\n");
        let _ = write!(self.out, "\tcmp\tax,word ptr {}\r\n", bp_addr(spill_off));
        let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(dispatch_slot));
        self.out.extend_from_slice(b"\tinc\tbx\r\n");
        self.out.extend_from_slice(b"\tinc\tbx\r\n");
        let _ = write!(self.out, "\tloop\tshort {}\r\n", self.label_ref(loop_top_slot));
        // No match: jump to default (if present) or end-of-switch.
        let no_match_slot = default_slot.unwrap_or(end_slot);
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(no_match_slot));
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

        // Stage value table + address table for post-function
        // emission. Only value cases participate; the default
        // (if any) is the no-match `jmp` target, not in the table.
        let _ = write!(
            self.post_function_data,
            "@{}@C{c_num}\tlabel\tword\r\n",
            self.func_idx,
        );
        for case in &value_cases {
            let v = case.value.expect("value-case has value") & 0xFFFF;
            let lo = v & 0xFF;
            let hi = (v >> 8) & 0xFF;
            let _ = write!(self.post_function_data, "\tdb\t{lo}\r\n");
            let _ = write!(self.post_function_data, "\tdb\t{hi}\r\n");
        }
        for &slot in &value_slots {
            let _ = write!(
                self.post_function_data,
                "\tdw\t{}\r\n",
                self.label_ref(slot),
            );
        }
    }
    /// Emit an inline-assembly statement. Splits `body` into
    /// individual lines (by `;` and `\n`), trims whitespace,
    /// substitutes any C-identifier reference against the
    /// function's locals / params / globals (e.g. `x` → `word ptr
    /// [bp-2]`), and emits each line as a tab-indented asm
    /// instruction. The `_AX` / `_BX` / `_CX` / `_DX` pseudo-
    /// registers are dropped on the floor — BCC treats a
    /// `return _AX;` as "AX already holds the value", so no
    /// reload is emitted. Fixtures 2303, 2304, 2120, 2119, 2122.
    pub(crate) fn emit_asm_block(&mut self, body: &str) {
        for raw_line in body.split(|c: char| c == ';' || c == '\n') {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            let translated = self.translate_asm_line(line);
            // Normalize the first whitespace run after the mnemonic
            // to a tab — BCC's emitted asm uses `\tmov\tax,...`,
            // not `\tmov ax, ...`. Also collapse spaces after `,`
            // separators so operand lists become `ax,42` not
            // `ax, 42`.
            let normalized = normalize_asm_line(&translated);
            let _ = write!(self.out, "\t{normalized}\r\n");
        }
    }
    /// Long-scrutinee linear-search dispatch. Spill the 4-byte
    /// scrutinee, then walk a CS-relative table whose layout is
    /// three N-word arrays: case lows, case highs, body offsets.
    /// The loop body compares the low half first (per-iteration
    /// `mov ax, cs:[bx]; cmp ax, spill_lo; jne skip`), then the
    /// high half (`mov ax, cs:[bx+2N]; cmp ax, spill_hi; je
    /// matched`); on no-match it bumps BX by 2, `loop`s, and falls
    /// through to `jmp default`. The matched dispatch is a single
    /// `jmp cs:[bx+4N]`. Fixture 1913.
    pub(crate) fn emit_switch_long_linear_search(
        &mut self,
        switch_span_start: u32,
        scrutinee: &Expr,
        cases: &[SwitchCase],
        case_slots: &[u32],
        end_slot: u32,
    ) {
        let default_slot = cases
            .iter()
            .zip(case_slots)
            .find_map(|(c, &s)| if c.value.is_none() { Some(s) } else { None });
        let value_cases: Vec<&SwitchCase> = cases.iter().filter(|c| c.value.is_some()).collect();
        let value_slots: Vec<u32> = cases
            .iter()
            .zip(case_slots)
            .filter_map(|(c, &s)| if c.value.is_some() { Some(s) } else { None })
            .collect();
        let case_count = u32::try_from(value_cases.len()).unwrap_or(u32::MAX);
        let spill_off = self.locals.switch_spill_offset(switch_span_start);

        // Load the long scrutinee high half into AX then low half
        // into DX (the standard ordering for long memory reads),
        // and spill: low → [spill_off], high → [spill_off+2].
        let ExprKind::Ident(name) = &scrutinee.kind else {
            panic!("non-ident long-switch scrutinee not yet supported");
        };
        match self.locals.location_of(name) {
            LocalLocation::Stack(off) => {
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off));
            }
            LocalLocation::Reg(_) => {
                panic!("long-switch scrutinee in a register is impossible (longs never enregister)");
            }
        }
        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(spill_off));
        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(spill_off + 2));

        // CX = case count, BX = pointer to the value table.
        let _ = write!(self.out, "\tmov\tcx,{case_count}\r\n");
        let c_num = switch_c_num(SwitchStrategy::LongLinearSearch, case_count);
        let _ = write!(self.out, "\tmov\tbx,offset @{}@C{c_num}\r\n", self.func_idx);

        // Three internal labels — loop_top, skip_high, matched —
        // co-locate in the pre-slot range allocated by plan.rs (the
        // Chained-style `non_default_count + 2` budget that the
        // planner reserved before knowing this would become a long-
        // linear-search). The first value case body is at
        // `first_value_slot`; the three labels sit at -3, -2, -1
        // from there, all within the pre-slot range.
        let first_value_slot = *value_slots.first().expect("at least one value case");
        let loop_top_slot = first_value_slot - 3;
        let skip_high_slot = first_value_slot - 2;
        let matched_slot = first_value_slot - 1;

        self.emit_label(loop_top_slot);
        self.out.extend_from_slice(b"\tmov\tax,word ptr cs:[bx]\r\n");
        let _ = write!(self.out, "\tcmp\tax,word ptr {}\r\n", bp_addr(spill_off));
        let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(skip_high_slot));
        let high_table_offset = case_count * 2;
        let _ = write!(
            self.out,
            "\tmov\tax,word ptr cs:[bx+{high_table_offset}]\r\n",
        );
        let _ = write!(self.out, "\tcmp\tax,word ptr {}\r\n", bp_addr(spill_off + 2));
        let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(matched_slot));
        self.emit_label(skip_high_slot);
        self.out.extend_from_slice(b"\tinc\tbx\r\n");
        self.out.extend_from_slice(b"\tinc\tbx\r\n");
        let _ = write!(self.out, "\tloop\tshort {}\r\n", self.label_ref(loop_top_slot));
        let no_match_slot = default_slot.unwrap_or(end_slot);
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(no_match_slot));
        self.emit_label(matched_slot);
        let addr_table_offset = case_count * 4;
        let _ = write!(
            self.out,
            "\tjmp\tword ptr cs:[bx+{addr_table_offset}]\r\n",
        );

        // Case bodies in source order. Same break-target setup.
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

        // Stage the three N-word data arrays in post-function data:
        // case lows, case highs, body offsets.
        let _ = write!(
            self.post_function_data,
            "@{}@C{c_num}\tlabel\tword\r\n",
            self.func_idx,
        );
        for case in &value_cases {
            let v = case.value.expect("value-case has value");
            let lo = v & 0xFFFF;
            let _ = write!(self.post_function_data, "\tdw\t{lo}\r\n");
        }
        for case in &value_cases {
            let v = case.value.expect("value-case has value");
            let hi = (v >> 16) & 0xFFFF;
            let _ = write!(self.post_function_data, "\tdw\t{hi}\r\n");
        }
        for &slot in &value_slots {
            let _ = write!(
                self.post_function_data,
                "\tdw\t{}\r\n",
                self.label_ref(slot),
            );
        }
    }
    pub(crate) fn emit_return_value_load(&mut self, value: Option<&Expr>) {
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
    pub(crate) fn emit_return_value_load_inner(&mut self, e: &Expr) {
        // `return _AX;` and friends route through `emit_expr_to_ax`'s
        // `ExprKind::PseudoReg` arm (the pre-codegen rewrite pass
        // converts pseudo-register `Ident`s to that variant). `_AX`
        // is a no-op there; other pseudos load AX explicitly.
        // Fixtures 2122, 4051–4053.
        //
        // Float / double-returning function: evaluate the value onto
        // the FPU stack. BCC leaves the result on st(0) for the
        // caller; no register transfer needed. Fixture 1684.
        if self.function.ret_ty.is_float_like() {
            self.emit_float_load_to_fpu(e);
            return;
        }
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
        // `char f() { return a + b; }` (no cast — both operands are
        // already char). Same byte-width arithmetic as the Cast(char)
        // path above; the cast is just elided in the source. Limit
        // to Add/Sub/BitAnd/BitOr/BitXor where the low byte is
        // independent of the high half. Fixture 3517.
        if self.function.ret_ty.is_char_like()
            && let ExprKind::BinOp { op: binop, left, right } = &e.kind
            && matches!(binop,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some((l_name, l_off, l_ty)) = self.try_lvalue_chain_addr(left)
            && let Some((r_name, r_off, r_ty)) = self.try_lvalue_chain_addr(right)
            && l_ty.is_char_like()
            && r_ty.is_char_like()
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
            // 4-byte struct returned from a stack-local: load DX:AX
            // directly from the local's two words (high half at +2,
            // low half at +0). Caller picks DX:AX. Fixture 1875
            // (`struct P make_p(void) { ...; return r; }` with
            // `struct P { int x; int y; }`).
            if size == 4
                && let ExprKind::Ident(src_name) = &e.kind
                && self.locals.has(src_name)
                && self.locals.type_of(src_name) == &self.function.ret_ty
                && let LocalLocation::Stack(off) = self.locals.location_of(src_name)
            {
                let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
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
            //
            // Indirect variant: if the callee name is a global
            // function-pointer, emit `call word ptr DGROUP:_<name>`
            // instead. Fixture 2913 (`long (*op)(void); long call()
            // { return op(); }`).
            if let ExprKind::Call { name: fname, args } = &e.kind
                && args.is_empty()
            {
                if let Some(gty) = self.globals.type_of(fname)
                    && gty.pointee().is_some()
                    && self.signatures.params_of(fname).is_none()
                {
                    let _ = write!(
                        self.out,
                        "\tcall\tword ptr DGROUP:_{fname}\r\n",
                    );
                } else {
                    let _ = write!(self.out, "\tcall\tnear ptr _{fname}\r\n");
                }
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
            // `return <a> << 16;` / `>> 16;` for a long lvalue — BCC
            // collapses the helper call to a single half-load. For
            // `<< 16` the result's high half is the source low half
            // and the low half is zero (`mov dx, [lo]; xor ax, ax`).
            // For `>> 16` the result's low half is the source high
            // half (`mov ax, [hi]`) and the high half is the sign
            // extension (`cwd` for signed) or zero (`xor dx, dx` for
            // unsigned). Fixtures 2795 (signed >>), 2801 (unsigned
            // >>), 2875 (<<).
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && matches!(op, BinOp::Shl | BinOp::Shr)
                && let Some((hi_addr, lo_addr)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
                && k == 16
            {
                let unsigned = self.function.ret_ty.is_unsigned();
                if matches!(op, BinOp::Shl) {
                    let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                    self.out.extend_from_slice(b"\txor\tax,ax\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                    if unsigned {
                        self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcwd\t\r\n");
                    }
                }
                return;
            }
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
            // `return <long-lvalue>;` — load DX:AX from the source's
            // (high, low) word pair. Same shape regardless of where
            // the source lives (global / stack local / member /
            // const-indexed array). For a bare long lvalue:
            if let Some((hi, lo)) = self.long_lvalue_addr_pair(e) {
                let _ = write!(self.out, "\tmov\tdx,word ptr {hi}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {lo}\r\n");
                return;
            }
            // `return <long-lvalue> << K`/`>> K` (or by variable n).
            // Load DX:AX from source, set CL to count, call shift
            // helper. Result remains in DX:AX. Fixture 3430
            // (`a << n` for long a, int n).
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && matches!(op, BinOp::Shl | BinOp::Shr)
                && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
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
                if let Some(k) = try_const_eval(right) {
                    let k8 = (k & 0xFF) as u8;
                    let _ = write!(self.out, "\tmov\tcl,{k8}\r\n");
                } else if let ExprKind::Ident(n_name) = &right.kind
                    && self.locals.has(n_name)
                    && let LocalLocation::Stack(n_off) = self.locals.location_of(n_name)
                {
                    let _ = write!(self.out, "\tmov\tcl,byte ptr {}\r\n", bp_addr(n_off));
                } else {
                    // Fall through to panic — RHS shape not supported.
                    panic!("long return shift count from non-stack-local not yet supported");
                }
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                return;
            }
            // `return <long-fn-call>(args);` — the call already
            // returns DX:AX; just emit the call. Args are pushed in
            // the standard way. Fixture 3426 (`return handle(K)`).
            if let ExprKind::Call { name, args } = &e.kind
                && self.signatures.ret_ty_of(name).is_some_and(|t| t.is_long_like())
            {
                self.emit_call(name, args);
                return;
            }
            // `return <(long)int-expr> * <long-const>;` — widen the
            // int via cwd, save it as CX:BX (push then pop the
            // halves), load const RHS as DX:AX, call N_LXMUL@.
            // Fixture 1683 (`return (long)x * 1000L`).
            if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &e.kind
                && let ExprKind::Cast { ty, operand } = &left.kind
                && ty.is_long_like()
                && !self.expr_is_long_like(operand)
                && let Some(k) = try_const_eval(right)
            {
                let unsigned = self.expr_int_is_unsigned(operand) || ty.is_unsigned();
                self.emit_expr_to_ax(operand);
                if unsigned {
                    self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                }
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                self.out.extend_from_slice(b"\tpush\tdx\r\n");
                let lo_k = (k & 0xFFFF) as u16;
                let hi_k = ((k >> 16) & 0xFFFF) as u16;
                if hi_k == 0 {
                    self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tdx,{hi_k}\r\n");
                }
                let _ = write!(self.out, "\tmov\tax,{lo_k}\r\n");
                self.out.extend_from_slice(b"\tpop\tcx\r\n");
                self.out.extend_from_slice(b"\tpop\tbx\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                self.helpers.insert("N_LXMUL@".to_string());
                return;
            }
            // `return <long-lvalue> * <long-const>;` — long * long
            // helper. CX:BX = long lvalue, DX:AX = const RHS, call
            // N_LXMUL@ (returns DX:AX). Fixture 3303 (`a * 10L`).
            if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &e.kind
                && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
            {
                let lo_k = (k & 0xFFFF) as u16;
                let hi_k = ((k >> 16) & 0xFFFF) as u16;
                let _ = write!(self.out, "\tmov\tcx,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tbx,word ptr {a_lo}\r\n");
                if hi_k == 0 {
                    self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tdx,{hi_k}\r\n");
                }
                let _ = write!(self.out, "\tmov\tax,{lo_k}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                self.helpers.insert("N_LXMUL@".to_string());
                return;
            }
            // `return <long-lvalue> * <long-lvalue>;` — long * long
            // helper, both operands from memory.
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
            // `return <long-lvalue> +/-/&/|/^ <long-lvalue>;` — long
            // pair op, both operands from memory. Load a into DX:AX,
            // apply <op> with b's halves. Sibling of the long-init
            // path's lvalue-lvalue case. Also handles chained
            // `<long-lvalue> + <long-lvalue> + <long-lvalue>` — the
            // outermost `+`'s LHS is itself a long-pair-op binop
            // chain rooted at a long lvalue. Fixtures 3301 (chained
            // long add on stack array).
            if let ExprKind::BinOp { op: _, .. } = &e.kind
                && let Some(addrs) = self.collect_long_lvalue_chain(e)
            {
                let first = &addrs[0];
                let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", first.hi);
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", first.lo);
                for step in &addrs[1..] {
                    let _ = write!(self.out, "\t{}\tax,word ptr {}\r\n", step.lo_op, step.lo);
                    let _ = write!(self.out, "\t{}\tdx,word ptr {}\r\n", step.hi_op, step.hi);
                }
                return;
            }
            // `return <long-lvalue> +/-/&/|/^ <int-expr>;` — widen
            // the int via cwd, push, load long lvalue into DX:AX,
            // pop the widened-int into CX:BX, then long pair-op.
            // Fixture 3291 (`a + b` for long a, int b).
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && let Some((lo_op, hi_op)) = long_pair_op(*op)
            {
                let (int_expr, long_addr): (&Expr, Option<(String, String)>) =
                    if let Some(pair) = self.long_lvalue_addr_pair(left)
                        && !self.expr_is_long_like(right)
                    {
                        (right, Some(pair))
                    } else if let Some(pair) = self.long_lvalue_addr_pair(right)
                        && !self.expr_is_long_like(left)
                        && matches!(op, BinOp::Add | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
                    {
                        (left, Some(pair))
                    } else {
                        (left, None)
                    };
                if let Some((a_hi, a_lo)) = long_addr {
                    let unsigned = self.expr_int_is_unsigned(int_expr);
                    self.emit_expr_to_ax(int_expr);
                    if unsigned {
                        self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcwd\t\r\n");
                    }
                    self.out.extend_from_slice(b"\tpush\tax\r\n");
                    self.out.extend_from_slice(b"\tpush\tdx\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {a_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr {a_lo}\r\n");
                    self.out.extend_from_slice(b"\tpop\tbx\r\n");
                    self.out.extend_from_slice(b"\tpop\tcx\r\n");
                    let _ = write!(self.out, "\t{lo_op}\tax,cx\r\n");
                    let _ = write!(self.out, "\t{hi_op}\tdx,bx\r\n");
                    return;
                }
            }
            // `return <long-lvalue> +/-/&/|/^ <long-const>;`
            if let ExprKind::BinOp { op, left, right } = &e.kind
                && (matches!(op, BinOp::Add) || matches!(op, BinOp::Sub))
                && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
            {
                let signed = k as i32;
                let (delta, carry) = if matches!(op, BinOp::Add) {
                    (signed, 0i16)
                } else {
                    (-signed, -1i16)
                };
                let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
                if let Ok(delta_i8) = i8::try_from(delta) {
                    let _ = write!(self.out, "\tadd\tdx,{delta_i8}\r\n");
                } else {
                    let delta_u16 = (delta as i32) as u16;
                    let _ = write!(self.out, "\tadd\tdx,{delta_u16}\r\n");
                }
                let _ = write!(self.out, "\tadc\tax,{carry}\r\n");
                // Pattern produces (hi=AX, lo=DX); swap to return
                // convention (DX=hi, AX=lo).
                self.out.extend_from_slice(b"\txchg\tax,dx\r\n");
                return;
            }
            // `return <long-global>++ / --;` — load current DX:AX
            // from g, then memory-direct inc/dec the long. Fixture
            // 3294 (`return g++` for long global).
            if let ExprKind::Update { target, op, position } = &e.kind
                && let Some(gty) = self.globals.type_of(target)
                && gty.is_long_like()
            {
                let (lo_op, hi_op) = match op {
                    crate::ast::UpdateOp::Inc => ("add", "adc"),
                    crate::ast::UpdateOp::Dec => ("sub", "sbb"),
                };
                match position {
                    crate::ast::UpdatePosition::Pre => {
                        let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{target},1\r\n");
                        let _ = write!(self.out, "\t{hi_op}\tword ptr DGROUP:_{target}+2,0\r\n");
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{target}+2\r\n");
                    }
                    crate::ast::UpdatePosition::Post => {
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{target}+2\r\n");
                        let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{target},1\r\n");
                        let _ = write!(self.out, "\t{hi_op}\tword ptr DGROUP:_{target}+2,0\r\n");
                    }
                }
                return;
            }
            // Fallback: an int-typed return expression in a long-
            // returning function — widen via cwd / xor dx,dx.
            // `return <cond> ? <long-arm-a> : <long-arm-b>;` Each
            // arm can be a long lvalue (load DX:AX from memory) or
            // a constant (emit DX/AX immediates, preferring `xor
            // dx, dx` for zero hi). Plan reserves 3 slots for the
            // ternary; base+1 is the false-arm label, base+2 is
            // the merge. Fixtures 3304 (`c ? a : b` for long
            // lvalues), 3225 (`flag ? 100L : 200L` for long
            // consts). Check before the int-widening fallback so
            // we get per-arm widening (BCC's actual shape) rather
            // than a single trailing cwd.
            if let ExprKind::Ternary { cond, then_value, else_value } = &e.kind
                && let Some(then_load) = self.long_arm_load(then_value)
                && let Some(else_load) = self.long_arm_load(else_value)
            {
                let base = self.label_plan.base(e.span.start, e.span.end);
                let false_slot = base + 1;
                let merge_slot = base + 2;
                let (_t, inv) = self.emit_cond_test(cond);
                let _ = write!(self.out, "\t{inv}\tshort {}\r\n", self.label_ref(false_slot));
                self.out.extend_from_slice(then_load.as_bytes());
                let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(merge_slot));
                self.emit_label(false_slot);
                self.out.extend_from_slice(else_load.as_bytes());
                self.emit_label(merge_slot);
                return;
            }
            if !self.expr_is_long_like(e) {
                let unsigned = self.expr_int_is_unsigned(e);
                self.emit_expr_to_ax(e);
                if unsigned {
                    self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                }
                return;
            }
            // `return <global-long-array>[<var-idx>]` — scale the
            // index into BX (×4 for long stride), then load both
            // halves through `[_arr[bx]]` / `[_arr[bx+2]]`. Fixture
            // 3288.
            if let ExprKind::ArrayIndex { array, index } = &e.kind
                && let ExprKind::Ident(arr_name) = &array.kind
                && let Some(gty) = self.globals.type_of(arr_name)
                && let Some(elem) = gty.array_elem()
                && elem.is_long_like()
            {
                let elem_ty = elem.clone();
                self.emit_index_into_bx(index, &elem_ty);
                // High half first, then low — matches BCC's long-
                // return load order (DX=hi, AX=lo with DX written
                // before AX). Fixture 3288.
                let _ = write!(
                    self.out,
                    "\tmov\tdx,word ptr DGROUP:_{arr_name}[bx+2]\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tmov\tax,word ptr DGROUP:_{arr_name}[bx]\r\n",
                );
                return;
            }
            // Same shape for a stack-local long array: compute
            // &arr[i] address (lea-then-add via BX), then load
            // both halves through `[bx]` / `[bx+2]`. Fixture 2798.
            if let ExprKind::ArrayIndex { array, index } = &e.kind
                && let ExprKind::Ident(arr_name) = &array.kind
                && self.locals.has(arr_name)
                && let Some(elem) = self.locals.type_of(arr_name).array_elem()
                && elem.is_long_like()
                && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
            {
                let elem_size = elem.size_bytes();
                self.emit_array_addr_to_bx(arr_name, index, base_off, elem_size);
                self.out.extend_from_slice(b"\tmov\tdx,word ptr [bx+2]\r\n");
                self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
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
}
