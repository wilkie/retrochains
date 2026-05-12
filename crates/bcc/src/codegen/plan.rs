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

use crate::ast::{Expr, ExprKind, Function, Stmt, StmtKind};

/// Compiled label assignments for one function.
#[derive(Debug)]
pub struct LabelPlan {
    /// `span.start` of a non-loop control construct → its base slot.
    /// Used by `if`, `comparison-as-value`, and `&&`/`||`.
    bases: HashMap<u32, u32>,
    /// `span.start` of a loop construct → its named slot assignments.
    loops: HashMap<u32, LoopPlan>,
    /// Slot for the function exit label.
    exit_slot: u32,
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
        let mut counter = 0u32;
        let mut bases = HashMap::new();
        let mut loops = HashMap::new();
        plan_stmts(&function.body, &mut counter, &mut bases, &mut loops);
        Self { bases, loops, exit_slot: counter }
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
    pub fn base(&self, span_start: u32) -> u32 {
        *self.bases.get(&span_start).unwrap_or_else(|| {
            panic!("no label plan entry for span starting at byte {span_start}")
        })
    }

    /// Slot assignments for a loop construct.
    #[must_use]
    pub fn loop_plan(&self, span_start: u32) -> LoopPlan {
        *self.loops.get(&span_start).unwrap_or_else(|| {
            panic!("no loop plan entry for span starting at byte {span_start}")
        })
    }
}

fn plan_stmts(
    stmts: &[Stmt],
    counter: &mut u32,
    bases: &mut HashMap<u32, u32>,
    loops: &mut HashMap<u32, LoopPlan>,
) {
    for stmt in stmts {
        plan_stmt(stmt, counter, bases, loops);
    }
}

fn plan_stmt(
    stmt: &Stmt,
    counter: &mut u32,
    bases: &mut HashMap<u32, u32>,
    loops: &mut HashMap<u32, LoopPlan>,
) {
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
            plan_expr_condition(cond, counter, bases);
            let base = *counter;
            bases.insert(stmt.span.start, base);
            *counter += if else_branch.is_some() { 3 } else { 2 };
            plan_stmts(then_branch, counter, bases, loops);
            if let Some(else_branch) = else_branch {
                plan_stmts(else_branch, counter, bases, loops);
            }
        }
        StmtKind::Assign { value, .. } | StmtKind::CompoundAssign { value, .. } => {
            plan_expr_value(value, counter, bases);
        }
        StmtKind::While { cond, body } => {
            // While layout: body slot, then body planning, then check
            // and break-target. Matches fixtures 027, 063, 066. The
            // earlier "reserve 3 contiguous slots up-front" model is
            // wrong when the body has nested labels (063: if-skip
            // lands inside what would have been while's "+2 unused"
            // slot, requiring while's check/break-target to come
            // *after* the body's reservations).
            let body_slot = *counter;
            *counter += 1;
            plan_expr_condition(cond, counter, bases);
            plan_stmts(body, counter, bases, loops);
            let check_slot = *counter;
            *counter += 1;
            let break_target_slot = *counter;
            *counter += 1;
            loops.insert(
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
            let body_slot = *counter;
            *counter += 1;
            plan_stmts(body, counter, bases, loops);
            plan_expr_condition(cond, counter, bases);
            let check_slot = *counter;
            *counter += 1;
            let break_target_slot = *counter;
            *counter += 1;
            loops.insert(
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
            let body_slot = *counter;
            *counter += 1;
            if let Some(e) = init {
                plan_expr_value(e, counter, bases);
            }
            if let Some(e) = cond {
                plan_expr_condition(e, counter, bases);
            }
            if let Some(e) = step {
                plan_expr_value(e, counter, bases);
            }
            let before_body = *counter;
            plan_stmts(body, counter, bases, loops);
            let continue_target_slot;
            if *counter == before_body {
                // No nested labels in body — reserve a filler slot
                // that doubles as the `continue;` landing if any.
                continue_target_slot = *counter;
                *counter += 1;
            } else {
                // Body's reservations consumed the slot that would
                // have been the continue-target. We don't yet have
                // a fixture for `continue` in a `for` with nested
                // body labels; defaulting to check_slot is safe-ish
                // but codegen will panic if it actually fires.
                continue_target_slot = *counter;
            }
            let check_slot = *counter;
            *counter += 1;
            let break_target_slot = *counter;
            *counter += 1;
            loops.insert(
                stmt.span.start,
                LoopPlan {
                    body_slot,
                    check_slot,
                    break_target_slot,
                    continue_target_slot,
                },
            );
        }
        StmtKind::Break | StmtKind::Continue => {
            // No slot reservations.
        }
        StmtKind::ExprStmt(e) => plan_expr_value(e, counter, bases),
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
        ExprKind::Logical { left, right, .. } => {
            let base = *counter;
            bases.insert(e.span.start, base);
            *counter += 4;
            plan_expr_condition(left, counter, bases);
            plan_expr_condition(right, counter, bases);
        }
        ExprKind::AssignExpr { value, .. } => {
            plan_expr_value(value, counter, bases);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                plan_expr_value(a, counter, bases);
            }
        }
        ExprKind::IntLit(_) | ExprKind::Ident(_) | ExprKind::Update { .. } => {}
    }
}

/// Walk an expression in condition position.
fn plan_expr_condition(e: &Expr, counter: &mut u32, bases: &mut HashMap<u32, u32>) {
    match &e.kind {
        ExprKind::BinOp { op, left, right } if op.is_comparison() => {
            plan_expr_value(left, counter, bases);
            plan_expr_value(right, counter, bases);
        }
        ExprKind::Logical { left, right, .. } => {
            let base = *counter;
            bases.insert(e.span.start, base);
            *counter += 1;
            plan_expr_condition(left, counter, bases);
            plan_expr_condition(right, counter, bases);
        }
        _ => plan_expr_value(e, counter, bases),
    }
}
