//! Pre-emission pass that assigns label slots to control-flow constructs
//! in a function. BCC numbers labels as `50 + 24 * slot`; each control
//! construct reserves a fixed number of slots (see
//! `specs/bcc/ASM_OUTPUT.md`), and the function exit gets the next slot
//! after all body reservations.
//!
//! By doing this before codegen we can emit the function exit label with
//! its correct number even though it's referenced from `jmp short
//! <exit>` calls that appear earlier in the output.

use std::collections::HashMap;

use crate::ast::{Expr, ExprKind, Function, Stmt, StmtKind};

/// Compiled label assignments for one function.
#[derive(Debug)]
pub struct LabelPlan {
    /// `span.start` of a control construct → its base slot. Each construct
    /// reserves a contiguous run of slots starting from this number.
    bases: HashMap<u32, u32>,
    /// Slot for the function exit label. Numerically: `50 + 24 * exit_slot`.
    exit_slot: u32,
}

impl LabelPlan {
    #[must_use]
    pub fn build(function: &Function) -> Self {
        let mut counter = 0u32;
        let mut bases = HashMap::new();
        plan_stmts(&function.body, &mut counter, &mut bases);
        Self { bases, exit_slot: counter }
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

    /// Base slot reserved for the construct whose span starts at `start`.
    /// Panics if the planner didn't reserve one — that's a bug.
    #[must_use]
    pub fn base(&self, span_start: u32) -> u32 {
        *self.bases.get(&span_start).unwrap_or_else(|| {
            panic!("no label plan entry for span starting at byte {span_start}")
        })
    }
}

fn plan_stmts(stmts: &[Stmt], counter: &mut u32, bases: &mut HashMap<u32, u32>) {
    for stmt in stmts {
        plan_stmt(stmt, counter, bases);
    }
}

fn plan_stmt(stmt: &Stmt, counter: &mut u32, bases: &mut HashMap<u32, u32>) {
    match &stmt.kind {
        StmtKind::Return(value) => {
            if let Some(e) = value {
                plan_expr_value(e, counter, bases);
            }
        }
        StmtKind::Declare { init, .. } => {
            if let Some(e) = init {
                plan_expr_value(e, counter, bases);
            }
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            // The condition is in jump position — its top-level comparison
            // is consumed by a conditional jump, not as a 0/1 value. Sub-
            // expressions of the condition are still in value position.
            plan_expr_condition(cond, counter, bases);
            // Reserve slots for the `if` construct itself before walking
            // its branches: that way the inner content's labels (e.g. a
            // cmp-as-value in a branch's `return`) get later numbers.
            let base = *counter;
            bases.insert(stmt.span.start, base);
            *counter += if else_branch.is_some() { 3 } else { 2 };
            plan_stmts(then_branch, counter, bases);
            if let Some(else_branch) = else_branch {
                plan_stmts(else_branch, counter, bases);
            }
        }
        StmtKind::Assign { value, .. } => {
            plan_expr_value(value, counter, bases);
        }
        StmtKind::ExprStmt(e) => plan_expr_value(e, counter, bases),
        StmtKind::While { cond, body } => {
            // While reserves its slots up-front (3: body, check, unused)
            // before walking either condition or body, so the construct's
            // own labels come first. The condition itself is in jump
            // position; sub-expressions revert to value position.
            let base = *counter;
            bases.insert(stmt.span.start, base);
            *counter += 3;
            plan_expr_condition(cond, counter, bases);
            plan_stmts(body, counter, bases);
        }
    }
}

/// Walk an expression in value position. Each comparison reserves 3 slots
/// (its base goes into the map keyed by `expr.span.start`).
fn plan_expr_value(e: &Expr, counter: &mut u32, bases: &mut HashMap<u32, u32>) {
    match &e.kind {
        ExprKind::BinOp { op, left, right } => {
            plan_expr_value(left, counter, bases);
            plan_expr_value(right, counter, bases);
            if op.is_comparison() {
                let base = *counter;
                bases.insert(e.span.start, base);
                *counter += 3;
            }
        }
        ExprKind::Unary { operand, .. } => {
            plan_expr_value(operand, counter, bases);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                plan_expr_value(a, counter, bases);
            }
        }
        ExprKind::IntLit(_) | ExprKind::Ident(_) | ExprKind::Update { .. } => {}
    }
}

/// Walk an expression in condition position. The top-level comparison (if
/// any) doesn't reserve cmp-as-value slots — it becomes a conditional
/// jump. Sub-expressions revert to value position.
fn plan_expr_condition(e: &Expr, counter: &mut u32, bases: &mut HashMap<u32, u32>) {
    match &e.kind {
        ExprKind::BinOp { op, left, right } if op.is_comparison() => {
            plan_expr_value(left, counter, bases);
            plan_expr_value(right, counter, bases);
        }
        _ => plan_expr_value(e, counter, bases),
    }
}
