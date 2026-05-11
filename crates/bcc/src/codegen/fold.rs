//! Constant folding. BCC evaluates compile-time-known sub-expressions
//! and emits them as immediates rather than runtime arithmetic — see
//! fixture 005 (`return 1 + 2;` → `mov ax,3`). We do the same by
//! attempting a recursive constant-evaluation on every expression before
//! emitting it.

use crate::ast::{BinOp, Expr, ExprKind};

/// If `e` is a constant expression under our current set of operators
/// and atom forms, return its `u32` value. Otherwise return `None`.
///
/// The value-width matches `ExprKind::IntLit` (32 bits internally) even
/// though BCC's `int` is 16 bits — we'll narrow at emission time when
/// the destination's size is known.
pub fn try_const_eval(e: &Expr) -> Option<u32> {
    match &e.kind {
        ExprKind::IntLit(n) => Some(*n),
        ExprKind::Ident(_) | ExprKind::Call { .. } => None,
        ExprKind::BinOp { op, left, right } => {
            let l = try_const_eval(left)?;
            let r = try_const_eval(right)?;
            Some(match op {
                BinOp::Add => l.wrapping_add(r),
                BinOp::Sub => l.wrapping_sub(r),
                BinOp::Mul => l.wrapping_mul(r),
            })
        }
    }
}
