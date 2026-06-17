//! Hi-IR: expression and statement recovery — §5 of `specs/decompiler/IR.md`.
//!
//! Where Lo-IR is a mechanical decode, this is the actual *recovery*: folding
//! the accumulator chains Lo-IR exposes back into expression trees and C
//! statements. The driving fact is BCC's non-optimizing codegen — it evaluates
//! every expression into `ax` and then stores it back to a slot, never keeping a
//! value live in a register across a statement. So **expression folding** is a
//! symbolic execution of `ax` across a basic block:
//!
//! ```text
//! Load(ax, b)            acc := b
//! Bin(ax, +, ax, c)      acc := (b + c)
//! Store(a, ax)           emit  a = (b + c)   and flush acc
//! ```
//!
//! A `Store` (or a `Ret`) flushes the current `ax` expression into a statement;
//! a fresh `Load(ax, …)` starts the next one. Because the accumulator resets at
//! every statement boundary, the analysis is bounded to one statement at a time.
//!
//! This first increment recovers **straight-line** functions over `int` locals
//! and constants — assignments, arithmetic chains, and `return`. Anything it
//! can't yet fold (control flow, calls, params, globals, byte/long widths,
//! pointers) sets [`Function::complete`] to `false`, so the harness never claims
//! a half-recovered function is done. Control-flow structuring (`if`/`while`)
//! is the next increment; the recompile-verify loop adjudicates each as it lands.

use crate::lo_ir::{lift, BinOp, LoOp, Place, Reg};

/// A recovered expression — a subset of the §5 grammar (the rest arrives with
/// later increments). `Local` is a frame slot; it's named at emit time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// An integer constant.
    Const(i32),
    /// A local variable, identified by its `[bp+disp]` slot (`disp < 0`).
    Local(i16),
    /// `lhs op rhs` (left-associative, as the accumulator chain produces).
    Binary(BinOp, Box<Expr>, Box<Expr>),
}

/// An assignable location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LValue {
    /// A local variable slot.
    Local(i16),
}

/// A recovered statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `lvalue = expr;`
    Assign(LValue, Expr),
    /// `return expr;` (or `return;` when the accumulator holds no value).
    Return(Option<Expr>),
}

/// A recovered value type. Minimal for now — the first increment is all `int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Int,
    Void,
}

/// A recovered function body. The signature is intentionally thin: recompilation
/// doesn't depend on the function's *name* (it isn't in `_TEXT`), so we emit a
/// placeholder name and focus on the body and the slots it touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    /// The return type, inferred from whether any `return` carries a value.
    pub ret: Type,
    /// The local slots the body touches, in first-appearance order. All `int`
    /// in this increment.
    pub locals: Vec<i16>,
    /// The recovered statements.
    pub body: Vec<Stmt>,
    /// `false` if any Lo-IR op couldn't be folded — the function is only
    /// partially recovered and must not be presented as done.
    pub complete: bool,
}

/// Record a local slot the first time it's seen (preserving order).
fn note(locals: &mut Vec<i16>, slot: i16) {
    if !locals.contains(&slot) {
        locals.push(slot);
    }
}

/// The arithmetic ops the single-accumulator fold can model as a C binary
/// expression. `cmp`/`test` (flag-only), the carry/borrow forms, rotates, and
/// the `dx:ax` multiplicatives are excluded — they show up in control flow or
/// wider-than-`int` code this increment doesn't recover yet.
fn is_foldable(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Add | BinOp::Sub | BinOp::Or | BinOp::And | BinOp::Xor | BinOp::Shl | BinOp::Shr | BinOp::Sar
    )
}

/// A source operand as an [`Expr`], or `None` if it's outside this increment
/// (a param, global, register, pointer, …) — the caller then marks incomplete.
fn operand(place: Place, locals: &mut Vec<i16>) -> Option<Expr> {
    match place {
        Place::Imm(v) => Some(Expr::Const(v)),
        // A negative bp-offset is a local; a non-negative one is a parameter,
        // which needs a signature we don't recover yet.
        Place::Local(d) if d < 0 => {
            note(locals, d);
            Some(Expr::Local(d))
        }
        _ => None,
    }
}

/// A destination operand as an [`LValue`], or `None` if unsupported yet.
fn dest(place: Place, locals: &mut Vec<i16>) -> Option<LValue> {
    match place {
        Place::Local(d) if d < 0 => {
            note(locals, d);
            Some(LValue::Local(d))
        }
        _ => None,
    }
}

/// Recover a function body from `_TEXT` bytes: lift to Lo-IR, then fold the
/// accumulator across the (single) basic block into statements.
#[must_use]
pub fn recover(code: &[u8]) -> Function {
    let lo = lift(code);
    let mut locals = Vec::new();
    let mut body = Vec::new();
    let mut complete = true;
    // The symbolic value currently in `ax`.
    let mut acc: Option<Expr> = None;

    for insn in &lo {
        match insn.op {
            // Frame setup/teardown and the stereotyped exit jump carry no value.
            LoOp::Enter { .. } | LoOp::Leave | LoOp::Jump { .. } => {}

            // A return flushes whatever the accumulator holds.
            LoOp::Ret { .. } => body.push(Stmt::Return(acc.take())),

            // Load into ax starts a fresh accumulator expression.
            LoOp::Load { dst: Place::Reg(Reg::Ax), src } => match operand(src, &mut locals) {
                Some(e) => acc = Some(e),
                None => complete = false,
            },

            // ALU against ax extends the accumulator: acc := acc op rhs.
            LoOp::Bin { dst: Place::Reg(Reg::Ax), op, lhs: Place::Reg(Reg::Ax), rhs }
                if is_foldable(op) =>
            {
                match (acc.take(), operand(rhs, &mut locals)) {
                    (Some(l), Some(r)) => {
                        acc = Some(Expr::Binary(op, Box::new(l), Box::new(r)));
                    }
                    _ => complete = false,
                }
            }

            // Store ax flushes the accumulator into an assignment.
            LoOp::Store { dst, src: Place::Reg(Reg::Ax) } => {
                match (dest(dst, &mut locals), acc.take()) {
                    (Some(lv), Some(e)) => body.push(Stmt::Assign(lv, e)),
                    _ => complete = false,
                }
            }

            // A direct store of an immediate (`mov [slot], imm`) is its own
            // assignment and doesn't disturb the accumulator.
            LoOp::Store { dst, src: Place::Imm(v) } => match dest(dst, &mut locals) {
                Some(lv) => body.push(Stmt::Assign(lv, Expr::Const(v))),
                None => complete = false,
            },

            // Anything else is beyond this increment.
            _ => complete = false,
        }
    }

    let ret = if body.iter().any(|s| matches!(s, Stmt::Return(Some(_)))) {
        Type::Int
    } else {
        Type::Void
    };
    Function { ret, locals, body, complete }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{recompile_text, CompileOpts};

    fn recover_c(src: &str) -> Function {
        let code = recompile_text(src, &CompileOpts::default()).expect("compiles");
        recover(&code)
    }

    #[test]
    fn return_const_folds_to_return_expr() {
        let f = recover_c("int f() { return 42; }\n");
        assert!(f.complete);
        assert_eq!(f.ret, Type::Int);
        assert_eq!(f.body, vec![Stmt::Return(Some(Expr::Const(42)))]);
    }

    #[test]
    fn assign_then_return_folds_through_the_slot() {
        // `x = 5; return x;` → an assignment to a local, then a return reading it.
        let f = recover_c("int f() { int x; x = 5; return x; }\n");
        assert!(f.complete, "straight-line int code is fully recovered");
        assert_eq!(f.locals.len(), 1, "one local slot");
        let slot = f.locals[0];
        assert!(slot < 0, "a local sits below bp");
        assert_eq!(
            f.body,
            vec![
                Stmt::Assign(LValue::Local(slot), Expr::Const(5)),
                Stmt::Return(Some(Expr::Local(slot))),
            ],
        );
    }

    #[test]
    fn arithmetic_chain_folds_left_associative() {
        // `y = x + 3;` → the accumulator loads x, adds 3, stores y.
        let f = recover_c("int f() { int x; int y; x = 5; y = x + 3; return y; }\n");
        assert!(f.complete);
        let assign_y = f.body.iter().find_map(|s| match s {
            Stmt::Assign(_, e @ Expr::Binary(..)) => Some(e.clone()),
            _ => None,
        });
        let Some(Expr::Binary(op, lhs, rhs)) = assign_y else {
            panic!("expected a binary assignment for y");
        };
        assert_eq!(op, BinOp::Add);
        assert!(matches!(*lhs, Expr::Local(_)), "lhs is x");
        assert_eq!(*rhs, Expr::Const(3), "rhs is the literal 3");
    }

    #[test]
    fn a_call_marks_the_function_incomplete() {
        // A call is beyond this increment; recovery must flag it rather than
        // emit a body missing the call.
        let f = recover_c("int g(); int f() { return g(); }\n");
        assert!(!f.complete, "an unfolded call leaves the function incomplete");
    }
}
