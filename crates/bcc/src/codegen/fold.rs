//! Constant folding. BCC evaluates compile-time-known sub-expressions
//! and emits them as immediates rather than runtime arithmetic — see
//! fixture 005 (`return 1 + 2;` → `mov ax,3`). We do the same by
//! attempting a recursive constant-evaluation on every expression before
//! emitting it.

use crate::ast::{BinOp, Expr, ExprKind, UnaryOp};

/// If `e` is a constant expression under our current set of operators
/// and atom forms, return its `u32` value. Otherwise return `None`.
///
/// The value-width matches `ExprKind::IntLit` (32 bits internally) even
/// though BCC's `int` is 16 bits — we'll narrow at emission time when
/// the destination's size is known.
pub fn try_const_eval(e: &Expr) -> Option<u32> {
    match &e.kind {
        ExprKind::IntLit(n) => Some(*n),
        // Identifiers, calls, ++/--, and the short-circuit operators
        // have side effects or unknown runtime values — never fold.
        // (`&& / ||` *could* fold if both operands are constants, but
        // BCC doesn't emit a fixture where that matters; defer.)
        // `&x`, `*p`, `a[i]`, and string literals all evaluate at
        // runtime — addresses and memory loads aren't compile-time
        // constants in our model. (String literals decay to their
        // address, which a future codegen pass could fold to an
        // immediate, but there's no fixture for it yet.)
        ExprKind::Ident(_)
        | ExprKind::Call { .. }
        | ExprKind::Update { .. }
        | ExprKind::Logical { .. }
        | ExprKind::AssignExpr { .. }
        | ExprKind::AddressOf(_)
        | ExprKind::AddressOfArrayElem { .. }
        | ExprKind::Deref(_)
        | ExprKind::ArrayIndex { .. }
        | ExprKind::StringLit(_)
        | ExprKind::Member { .. }
        | ExprKind::Ternary { .. }
        | ExprKind::InitList { .. } => None,
        // `(a, b)` const-folds to `b`'s value when BOTH sides fold —
        // the left side has no observable effect when it's a pure
        // constant, and the comma's value is always the right. Lets
        // `int x = (5, 7);` (fixture 1662) collapse to `int x = 7`
        // and use the immediate-store init shape.
        ExprKind::Comma { left, right } => {
            let _ = try_const_eval(left)?;
            try_const_eval(right)
        }
        ExprKind::Cast { ty, operand } => {
            let v = try_const_eval(operand)?;
            // Truncate to the target type's width, then sign-extend
            // back to a u32 so subsequent folding sees the same value
            // BCC's runtime cast would produce. (We currently only
            // emit int/char/pointer-sized types.)
            Some(match ty {
                crate::ast::Type::Char => {
                    let b = (v & 0xFF) as i8;
                    i32::from(b) as u32
                }
                _ => v & 0xFFFF,
            })
        }
        ExprKind::Unary { op, operand } => {
            let v = try_const_eval(operand)?;
            Some(match op {
                // Two's-complement negation; emit-time truncation to
                // u16 makes `-5` land as `65531` (fixture 036).
                UnaryOp::Neg => 0u32.wrapping_sub(v),
                UnaryOp::BitNot => !v,
                // `!0` → 1; anything-non-zero → 0.
                UnaryOp::Not => u32::from(v == 0),
            })
        }
        ExprKind::BinOp { op, left, right } => {
            let l = try_const_eval(left)?;
            let r = try_const_eval(right)?;
            Some(match op {
                BinOp::Add => l.wrapping_add(r),
                BinOp::Sub => l.wrapping_sub(r),
                BinOp::Mul => l.wrapping_mul(r),
                BinOp::Div => {
                    if r == 0 {
                        return None;
                    }
                    // Signed division to match BCC's `idiv`. Our values
                    // are small u32 today; the i32 round-trip preserves
                    // them and gives signed semantics for the eventual
                    // negative-literal case.
                    l.cast_signed().wrapping_div(r.cast_signed()).cast_unsigned()
                }
                BinOp::Mod => {
                    if r == 0 {
                        return None;
                    }
                    l.cast_signed().wrapping_rem(r.cast_signed()).cast_unsigned()
                }
                BinOp::BitAnd => l & r,
                BinOp::BitOr => l | r,
                BinOp::BitXor => l ^ r,
                BinOp::Shl => l.wrapping_shl(r & 0x1F),
                // Signed (arithmetic) shift-right, matching BCC's `sar`.
                BinOp::Shr => l.cast_signed().wrapping_shr(r & 0x1F).cast_unsigned(),
                // Comparisons fold to 0/1 (C bool-result). Signed compare
                // to match BCC's signed-int semantics.
                BinOp::Eq => u32::from(l == r),
                BinOp::Ne => u32::from(l != r),
                BinOp::Lt => u32::from(l.cast_signed() < r.cast_signed()),
                BinOp::Le => u32::from(l.cast_signed() <= r.cast_signed()),
                BinOp::Gt => u32::from(l.cast_signed() > r.cast_signed()),
                BinOp::Ge => u32::from(l.cast_signed() >= r.cast_signed()),
            })
        }
    }
}
