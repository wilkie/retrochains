//! Pre-emission pass that assigns label slots to control-flow constructs
//! in a function. BCC numbers labels as `50 + 24 * slot`; each control
//! construct reserves a fixed number of slots (see
//! `specs/bcc/ASM_OUTPUT.md`), and the function exit gets the next slot
//! after all body reservations.
//!
//! Loop constructs (while/do-while/for) maintain enough state to let
//! codegen know:
//! - where the body label is (where the loop iterates back to)
//! - where the check label is (where the cond is tested)
//! - where the break-target is (where `break;` jumps)
//! - where `continue;` lands (for while/do-while it's the check label;
//!   for `for` it's a separate "continue-target" slot when reserved)

use std::collections::HashMap;

use crate::ast::{BinOp, Expr, ExprKind, Function, Stmt, StmtKind, SwitchCase};
use crate::codegen::fold::try_const_eval;

/// True iff `body` contains a `continue;` that targets the
/// enclosing loop (walks into nested `if`/`switch` since they
/// don't consume `continue`). Mirrors the helper in
/// `codegen/mod.rs`.
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
        StmtKind::Switch { cases, .. } => {
            cases.iter().any(|c| body_has_continue(&c.body))
        }
        _ => false,
    }
}

/// Compiled label assignments for one function.
#[derive(Debug)]
pub struct LabelPlan {
    /// `(span.start, span.end)` of a non-loop control construct →
    /// its base slot. Used by `if`, `comparison-as-value`, and
    /// `&&`/`||`. Both endpoints are part of the key because the
    /// parser sets a BinOp's `span.start` equal to its left
    /// operand's start, so an outer `(a == b) == c` and the inner
    /// `a == b` both start at `a` — only span.end distinguishes
    /// them. Fixture 2479 (nested == used as value).
    bases: HashMap<(u32, u32), u32>,
    /// `span.start` of a loop construct → its named slot assignments.
    loops: HashMap<u32, LoopPlan>,
    /// `span.start` of a `switch` → its slot assignments.
    switches: HashMap<u32, SwitchPlan>,
    /// Slot for the function exit label.
    exit_slot: u32,
}

/// Strategy BCC uses to dispatch a `switch`. The choice is made at
/// plan time (it affects how many slots get reserved) and re-checked
/// by codegen.
///
/// - **Chained**: a linear chain of `cmp / je` per case, with a
///   trailing `jmp` to the default body (or end-of-switch if no
///   default). BCC reserves `#non-default-cases + 2` pre-slots
///   before the first case body — those slots are unused as code
///   labels but get burned by the slot counter (fixtures 072, 075).
///
/// - **JumpTable**: bounds-check then `shl bx,1 / jmp word ptr cs:@<func>@C<n>[bx]`.
///   Reserves 3 pre-slots, regardless of case count (fixtures 073, 076).
///   Not yet implemented in codegen — planner panics for now.
///
/// - **LinearSearch**: spill scrutinee, walk a `dw` value table with
///   `mov / cmp / je / inc / inc / loop`, indirect-jmp through a
///   parallel address table. Reserves `#cases + 2` pre-slots
///   (fixture 074). Not yet implemented.
#[derive(Debug, Clone, Copy)]
pub enum SwitchStrategy {
    Chained,
    JumpTable,
    LinearSearch,
}

/// Slot assignments for one `switch` statement.
///
/// `case_slots` is parallel to the AST's `cases` vector, so the
/// codegen can iterate `cases.iter().zip(case_slots)` to find each
/// arm's body label. The `end_slot` is what `break;` targets.
#[derive(Debug, Clone)]
pub struct SwitchPlan {
    pub strategy: SwitchStrategy,
    pub case_slots: Vec<u32>,
    pub end_slot: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct LoopPlan {
    pub body_slot: u32,
    pub check_slot: u32,
    pub break_target_slot: u32,
    /// Where `continue;` should jump. For while/do-while this equals
    /// `check_slot`; for `for` it's the continue-target slot that
    /// sits between the body and the step.
    pub continue_target_slot: u32,
}

impl LabelPlan {
    #[must_use]
    pub fn build(function: &Function) -> Self {
        // Collect names of long-typed params + local declares so the
        // condition-planner can recognize the `<long-lvalue> !=
        // <long-lvalue>` shape that needs an extra label slot
        // reserved. Fixture 2869.
        let mut long_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for p in &function.params {
            if p.ty.is_long_like() {
                long_names.insert(p.name.clone());
            }
        }
        collect_long_decl_names(
            function.body.as_deref().unwrap_or(&[]),
            &mut long_names,
        );
        let mut ctx = PlanCtx {
            counter: 0,
            bases: HashMap::new(),
            loops: HashMap::new(),
            switches: HashMap::new(),
            long_names,
        };
        plan_stmts(function.body.as_deref().unwrap_or(&[]), &mut ctx);
        Self {
            bases: ctx.bases,
            loops: ctx.loops,
            switches: ctx.switches,
            exit_slot: ctx.counter,
        }
    }

    /// Numeric label corresponding to a slot index.
    #[must_use]
    pub fn label_number(slot: u32) -> u32 {
        50 + 24 * slot
    }

    /// Slot for the function exit (the `jmp short` target for `return`).
    #[must_use]
    pub fn exit_slot(&self) -> u32 {
        self.exit_slot
    }

    /// Base slot reserved for a non-loop control construct.
    /// Panics if the planner didn't reserve one.
    #[must_use]
    pub fn base(&self, span_start: u32, span_end: u32) -> u32 {
        *self.bases.get(&(span_start, span_end)).unwrap_or_else(|| {
            panic!("no label plan entry for span {span_start}..{span_end}")
        })
    }

    /// Slot assignments for a loop construct.
    #[must_use]
    pub fn loop_plan(&self, span_start: u32) -> LoopPlan {
        *self.loops.get(&span_start).unwrap_or_else(|| {
            panic!("no loop plan entry for span starting at byte {span_start}")
        })
    }

    /// Slot assignments for a `switch` statement.
    #[must_use]
    pub fn switch_plan(&self, span_start: u32) -> &SwitchPlan {
        self.switches.get(&span_start).unwrap_or_else(|| {
            panic!("no switch plan entry for span starting at byte {span_start}")
        })
    }
}

/// Per-function planner state, threaded through `plan_stmts` /
/// `plan_stmt`. Folding the four pieces into one struct keeps the
/// recursive call sites short.
struct PlanCtx {
    counter: u32,
    bases: HashMap<(u32, u32), u32>,
    loops: HashMap<u32, LoopPlan>,
    switches: HashMap<u32, SwitchPlan>,
    long_names: std::collections::HashSet<String>,
}

fn collect_long_decl_names(stmts: &[Stmt], set: &mut std::collections::HashSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Declare { name, ty, .. } if ty.is_long_like() => {
                set.insert(name.clone());
            }
            StmtKind::If { then_branch, else_branch, .. } => {
                collect_long_decl_names(then_branch, set);
                if let Some(b) = else_branch {
                    collect_long_decl_names(b, set);
                }
            }
            StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
                collect_long_decl_names(body, set);
            }
            StmtKind::For { body, .. } => collect_long_decl_names(body, set),
            StmtKind::Switch { cases, .. } => {
                for c in cases {
                    collect_long_decl_names(&c.body, set);
                }
            }
            StmtKind::Block(stmts) => collect_long_decl_names(stmts, set),
            _ => {}
        }
    }
}

fn is_long_lvalue_expr_in(expr: &Expr, long_names: &std::collections::HashSet<String>) -> bool {
    matches!(&expr.kind, ExprKind::Ident(n) if long_names.contains(n))
}

fn plan_stmts(stmts: &[Stmt], ctx: &mut PlanCtx) {
    for stmt in stmts {
        plan_stmt(stmt, ctx);
    }
}

fn plan_stmt(stmt: &Stmt, ctx: &mut PlanCtx) {
    match &stmt.kind {
        StmtKind::Return(value) => {
            if let Some(e) = value {
                plan_expr_value(e, ctx);
            }
        }
        StmtKind::Declare { init, is_static, .. } => {
            // Static-local initializers run at module load, not as
            // part of this function's label plan.
            if !*is_static {
                if let Some(e) = init {
                    plan_expr_value(e, ctx);
                }
            }
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            plan_expr_condition(cond, ctx);
            let base = ctx.counter;
            ctx.bases.insert((stmt.span.start, stmt.span.end), base);
            ctx.counter += if else_branch.is_some() { 3 } else { 2 };
            plan_stmts(then_branch, ctx);
            if let Some(else_branch) = else_branch {
                plan_stmts(else_branch, ctx);
            }
        }
        StmtKind::Assign { value, .. } | StmtKind::CompoundAssign { value, .. } => {
            plan_expr_value(value, ctx);
        }
        StmtKind::ArrayAssign { indices, value, .. }
        | StmtKind::ArrayCompoundAssign { indices, value, .. }
        | StmtKind::MemberArrayAssign { indices, value, .. } => {
            for ix in indices {
                plan_expr_value(ix, ctx);
            }
            plan_expr_value(value, ctx);
        }
        StmtKind::DerefAssign { target, value }
        | StmtKind::DerefCompoundAssign { target, value, .. } => {
            plan_expr_value(target, ctx);
            plan_expr_value(value, ctx);
        }
        StmtKind::MemberAssign { base, value, .. }
        | StmtKind::MemberCompoundAssign { base, value, .. } => {
            plan_expr_value(base, ctx);
            plan_expr_value(value, ctx);
        }
        StmtKind::While { cond, body } => {
            // While layout: body slot, then body planning, then check
            // and break-target. Matches fixtures 027, 063, 066. The
            // earlier "reserve 3 contiguous slots up-front" model is
            // wrong when the body has nested labels (063: if-skip
            // lands inside what would have been while's "+2 unused"
            // slot, requiring while's check/break-target to come
            // *after* the body's reservations).
            let body_slot = ctx.counter;
            ctx.counter += 1;
            plan_expr_condition(cond, ctx);
            plan_stmts(body, ctx);
            let check_slot = ctx.counter;
            ctx.counter += 1;
            let break_target_slot = ctx.counter;
            ctx.counter += 1;
            ctx.loops.insert(
                stmt.span.start,
                LoopPlan {
                    body_slot,
                    check_slot,
                    break_target_slot,
                    continue_target_slot: check_slot,
                },
            );
        }
        StmtKind::DoWhile { body, cond } => {
            // Do-while: same shape as while, just no trampoline jmp at
            // the top. Same slot reservation.
            let body_slot = ctx.counter;
            ctx.counter += 1;
            plan_stmts(body, ctx);
            plan_expr_condition(cond, ctx);
            let check_slot = ctx.counter;
            ctx.counter += 1;
            let break_target_slot = ctx.counter;
            ctx.counter += 1;
            ctx.loops.insert(
                stmt.span.start,
                LoopPlan {
                    body_slot,
                    check_slot,
                    break_target_slot,
                    continue_target_slot: check_slot,
                },
            );
        }
        StmtKind::For { init, cond, step, body } => {
            // For: body slot, plan init/cond/step (typically 0 slots),
            // plan body, then if body planning reserved nothing emit
            // an extra "continue-target / step" slot before check +
            // break-target. Fixture 061 reserves 4 slots for a body
            // with 0 nested labels; 065 reserves 5 (3 + 2 nested).
            let body_slot = ctx.counter;
            ctx.counter += 1;
            if let Some(exprs) = init {
                for e in exprs {
                    plan_expr_value(e, ctx);
                }
            }
            if let Some(e) = cond {
                plan_expr_condition(e, ctx);
            }
            if let Some(exprs) = step {
                for e in exprs {
                    plan_expr_value(e, ctx);
                }
            }
            let before_body = ctx.counter;
            plan_stmts(body, ctx);
            // Reserve a separate continue-target slot only when the
            // body actually uses `continue;`. Without continue, the
            // slot would just be unused chrome that shifts all
            // downstream label numbers (regressing prior for-loop
            // fixtures). Fixture 558 (`for (...) { if (...) continue;
            // }`) is the first to need the distinct slot. The old
            // "shares with check_slot when body had nested labels"
            // branch collided when continue *was* present.
            let continue_target_slot;
            if body_has_continue(body) {
                continue_target_slot = ctx.counter;
                ctx.counter += 1;
            } else if ctx.counter == before_body {
                // No body reservations and no continue — match the
                // historical filler-slot behavior for label-number
                // stability with all prior for-loop fixtures.
                continue_target_slot = ctx.counter;
                ctx.counter += 1;
            } else {
                continue_target_slot = ctx.counter;
            }
            let check_slot = ctx.counter;
            ctx.counter += 1;
            let break_target_slot = ctx.counter;
            ctx.counter += 1;
            ctx.loops.insert(
                stmt.span.start,
                LoopPlan {
                    body_slot,
                    check_slot,
                    break_target_slot,
                    continue_target_slot,
                },
            );
        }
        StmtKind::Switch { scrutinee, cases } => {
            plan_switch(stmt.span.start, scrutinee, cases, ctx);
        }
        StmtKind::Break | StmtKind::Continue => {
            // No slot reservations.
        }
        StmtKind::Goto { .. } | StmtKind::Label { .. } => {
            // No slot reservations — goto/label use user_<name>
            // labels not numbered BCC slots.
        }
        StmtKind::Empty => {
            // `;` placeholder body — no slots, no expressions.
        }
        StmtKind::ExprStmt(e) => plan_expr_value(e, ctx),
        StmtKind::Block(body) => plan_stmts(body, ctx),
    }
}

/// Reserve slots for a `switch`. Selects the dispatch strategy,
/// burns the pre-dispatch slots (BCC's slot counter advances past
/// labels the dispatch code could have but didn't end up needing),
/// then walks each case body in source order — each gets one body
/// slot, then its nested labels reserve normally. Finally the
/// end-of-switch slot (the `break;` target).
///
/// Pre-slot counts (from fixtures 072–076):
/// - chained: `non_default_count + 2`
/// - jump-table: 3 (fixed)
/// - linear-search: `non_default_count + 2`
fn plan_switch(span_start: u32, scrutinee: &Expr, cases: &[SwitchCase], ctx: &mut PlanCtx) {
    plan_expr_value(scrutinee, ctx);
    let strategy = pick_switch_strategy(cases);
    let non_default_count: u32 = cases
        .iter()
        .filter(|c| c.value.is_some())
        .count()
        .try_into()
        .expect("case count fits in u32");
    let pre_slots = match strategy {
        SwitchStrategy::Chained | SwitchStrategy::LinearSearch => non_default_count + 2,
        SwitchStrategy::JumpTable => 3,
    };
    ctx.counter += pre_slots;
    let mut case_slots = Vec::with_capacity(cases.len());
    for case in cases {
        let body_slot = ctx.counter;
        ctx.counter += 1;
        case_slots.push(body_slot);
        plan_stmts(&case.body, ctx);
    }
    let end_slot = ctx.counter;
    ctx.counter += 1;
    ctx.switches.insert(
        span_start,
        SwitchPlan { strategy, case_slots, end_slot },
    );
}

/// Pick a dispatch strategy for a switch given its case list. Rules
/// (heuristics, will be refined as more fixtures land):
///
/// - 0..N contiguous from 0 with N ≥ 4 → **JumpTable**
///   (fixtures 073 with 8, 076 with 4)
/// - Otherwise with ≥ 4 cases → **LinearSearch** (fixture 074)
/// - Otherwise → **Chained** (fixtures 072 with 3, 075 with 2 + default)
///
/// Exposed beyond the planner so the locals analyzer can decide
/// whether to reserve a stack-slot for the scrutinee-spill that
/// linear-search needs.
#[must_use]
pub fn pick_switch_strategy(cases: &[SwitchCase]) -> SwitchStrategy {
    let values: Vec<u32> = cases.iter().filter_map(|c| c.value).collect();
    if values.len() < 4 {
        return SwitchStrategy::Chained;
    }
    // Sort by signed value so negative bases work and source-order
    // is irrelevant. The jump-table strategy fires when the
    // values' span+1 is at most `2 * count`: tolerates small gaps
    // (filled with default/end slots in the table) but doesn't
    // pay 2 bytes per slot when sparsity gets large. Span =
    // (last - first) as signed. Fixtures 1605 (K=5), 1909 (K=-2),
    // 1609 (out-of-source-order), 1904 (one-gap dense).
    let mut sorted = values.clone();
    sorted.sort_by_key(|&v| v as i32 as i64);
    let first = sorted[0] as i32;
    let last = (*sorted.last().expect("non-empty by len check")) as i32;
    let span = (last as i64) - (first as i64);
    let count = sorted.len() as i64;
    if span >= 0 && span + 1 <= 2 * count {
        SwitchStrategy::JumpTable
    } else {
        SwitchStrategy::LinearSearch
    }
}

/// Walk an expression in value position. Each comparison reserves 3 slots
/// (its base goes into the map keyed by `expr.span.start`).
fn plan_expr_value(e: &Expr, ctx: &mut PlanCtx) {
    match &e.kind {
        ExprKind::BinOp { op, left, right } => {
            plan_expr_value(left, ctx);
            plan_expr_value(right, ctx);
            if op.is_comparison() {
                let base = ctx.counter;
                ctx.bases.insert((e.span.start, e.span.end), base);
                ctx.counter += 3;
            }
        }
        ExprKind::Unary { operand, .. } => {
            plan_expr_value(operand, ctx);
        }
        ExprKind::Logical { left, right, .. } => {
            let base = ctx.counter;
            ctx.bases.insert((e.span.start, e.span.end), base);
            ctx.counter += 4;
            plan_expr_condition(left, ctx);
            plan_expr_condition(right, ctx);
        }
        ExprKind::AssignExpr { value, .. }
        | ExprKind::CompoundAssignExpr { value, .. } => {
            plan_expr_value(value, ctx);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                plan_expr_value(a, ctx);
            }
        }
        ExprKind::CallVia { addr, args } => {
            plan_expr_value(addr, ctx);
            for a in args {
                plan_expr_value(a, ctx);
            }
        }
        ExprKind::Deref(operand) => plan_expr_value(operand, ctx),
        ExprKind::ArrayIndex { array, index } => {
            plan_expr_value(array, ctx);
            plan_expr_value(index, ctx);
        }
        ExprKind::Member { base, .. } => plan_expr_value(base, ctx),
        ExprKind::Cast { operand, .. } => plan_expr_value(operand, ctx),
        ExprKind::Ternary { cond, then_value, else_value } => {
            // Constant cond: codegen folds to the surviving arm only,
            // so no slots are needed. Match the codegen by walking
            // only that arm here too. Fixture 2965 / 2318.
            if let Some(v) = try_const_eval(cond) {
                if v != 0 {
                    plan_expr_value(then_value, ctx);
                } else {
                    plan_expr_value(else_value, ctx);
                }
                return;
            }
            // Same skeleton as `if`-`else`: reserve 3 slots (base+0
            // stays unused to match BCC's numbering, base+1 is the
            // false-arm label, base+2 is the merge label after both
            // arms have written AX).
            plan_expr_condition(cond, ctx);
            let base = ctx.counter;
            ctx.bases.insert((e.span.start, e.span.end), base);
            ctx.counter += 3;
            plan_expr_value(then_value, ctx);
            plan_expr_value(else_value, ctx);
        }
        ExprKind::InitList { items } => {
            for item in items {
                plan_expr_value(item, ctx);
            }
        }
        ExprKind::Comma { left, right } => {
            plan_expr_value(left, ctx);
            plan_expr_value(right, ctx);
        }
        ExprKind::UpdateLvalue { target, .. } => plan_expr_value(target, ctx),
        ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::DoubleLit(_)
        | ExprKind::Ident(_)
        | ExprKind::Update { .. }
        | ExprKind::AddressOf(_)
        | ExprKind::AddressOfArrayElem { .. } | ExprKind::AddressOfArrayElemVar { .. }
        | ExprKind::StringLit(_) => {}
    }
}

/// Walk an expression in condition position.
fn plan_expr_condition(e: &Expr, ctx: &mut PlanCtx) {
    match &e.kind {
        // `<long-lvalue> != <long-lvalue>` needs a fresh label for
        // the codegen's fall-through-on-true case (the EQ-cmp skips
        // it via short-circuit jne). Reserve one slot keyed to the
        // BinOp's span. Fixture 2869.
        ExprKind::BinOp { op: BinOp::Ne, left, right }
            if is_long_lvalue_expr_in(left, &ctx.long_names)
                && is_long_lvalue_expr_in(right, &ctx.long_names) =>
        {
            ctx.bases.insert((e.span.start, e.span.end), ctx.counter);
            ctx.counter += 1;
            plan_expr_value(left, ctx);
            plan_expr_value(right, ctx);
        }
        ExprKind::BinOp { op, left, right } if op.is_comparison() => {
            plan_expr_value(left, ctx);
            plan_expr_value(right, ctx);
        }
        ExprKind::Logical { left, right, .. } => {
            let base = ctx.counter;
            ctx.bases.insert((e.span.start, e.span.end), base);
            ctx.counter += 1;
            plan_expr_condition(left, ctx);
            plan_expr_condition(right, ctx);
        }
        _ => plan_expr_value(e, ctx),
    }
}
