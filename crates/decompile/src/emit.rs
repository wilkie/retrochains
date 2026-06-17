//! Hi-IR → C. The back half of the decompiler: render a recovered [`Function`]
//! as C source that the recompile-verify harness can check against the original
//! bytes.
//!
//! The emitter is deliberately literal. It fully parenthesizes binary
//! expressions so the printed tree is exactly the one the fold recovered —
//! `(a + b) + c` and `a + (b + c)` can lower to different code, and the
//! accumulator chain is left-associative, so preserving the shape is what keeps
//! the recompile byte-exact. Names are cosmetic (recompilation doesn't depend on
//! them): the function is `f`, locals are `v1`, `v2`, … by slot.

use std::fmt::Write as _;

use crate::hi_ir::{recover, Expr, Function, LValue, Stmt, Type};

/// Decompile `_TEXT` bytes to C, or `None` if the function isn't fully
/// recovered yet (it still holds ops the lift/fold can't model). A `Some` result
/// is the candidate to hand to [`crate::verify`].
#[must_use]
pub fn decompile(code: &[u8]) -> Option<String> {
    to_c(&recover(code))
}

/// Render a recovered function as C, or `None` if it isn't
/// [`complete`](Function::complete).
#[must_use]
pub fn to_c(f: &Function) -> Option<String> {
    if !f.complete {
        return None;
    }

    let ret = match f.ret {
        Type::Int => "int",
        Type::Void => "void",
    };
    let mut s = format!("{ret} f() {{\n");

    // Declare locals closest-to-bp first; BCC allocates locals from bp downward
    // in declaration order, so this order makes it re-assign the same slots.
    let mut locals = f.locals.clone();
    locals.sort_by(|a, b| b.cmp(a));
    for slot in &locals {
        let _ = writeln!(s, "  int {};", local_name(*slot));
    }

    // A trailing valueless return is implicit in C; dropping it keeps the
    // emitted body identical to an empty one (and recompiles the same).
    let n = f.body.len();
    for (i, stmt) in f.body.iter().enumerate() {
        let last = i + 1 == n;
        if last && matches!(stmt, Stmt::Return(None)) {
            continue;
        }
        s.push_str("  ");
        emit_stmt(stmt, &mut s);
        s.push('\n');
    }

    s.push_str("}\n");
    Some(s)
}

/// `v1` for `[bp-2]`, `v2` for `[bp-4]`, … — a stable name per slot.
fn local_name(slot: i16) -> String {
    format!("v{}", (-slot) / 2)
}

fn emit_stmt(stmt: &Stmt, out: &mut String) {
    match stmt {
        Stmt::Assign(lv, e) => {
            out.push_str(&lvalue_str(lv));
            out.push_str(" = ");
            out.push_str(&expr_str(e));
            out.push(';');
        }
        Stmt::Return(None) => out.push_str("return;"),
        Stmt::Return(Some(e)) => {
            out.push_str("return ");
            out.push_str(&expr_str(e));
            out.push(';');
        }
    }
}

fn lvalue_str(lv: &LValue) -> String {
    match lv {
        LValue::Local(slot) => local_name(*slot),
    }
}

fn expr_str(e: &Expr) -> String {
    match e {
        Expr::Const(v) => v.to_string(),
        Expr::Local(slot) => local_name(*slot),
        // Fully parenthesized so the printed tree matches the recovered one.
        Expr::Binary(op, l, r) => {
            format!("({} {} {})", expr_str(l), binop_token(*op), expr_str(r))
        }
    }
}

/// The C token for a foldable binary operator (the [`is_foldable`] set).
///
/// [`is_foldable`]: crate::hi_ir
fn binop_token(op: crate::lo_ir::BinOp) -> &'static str {
    use crate::lo_ir::BinOp;
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Or => "|",
        BinOp::And => "&",
        BinOp::Xor => "^",
        BinOp::Shl => "<<",
        // Arithmetic/logical right shift both print as `>>`; signedness of the
        // operand decides which the compiler re-emits.
        BinOp::Shr | BinOp::Sar => ">>",
        // The fold only produces the operators above; the rest never reach here.
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{verify, CompileOpts};
    use crate::recompile_text;

    /// The whole loop in one shot: compile `src`, decompile its `_TEXT` back to
    /// C purely from the bytes, and require that C recompile to the *same*
    /// bytes. This is the §8 correctness contract closing end to end.
    fn assert_roundtrips(src: &str) {
        let opts = CompileOpts::default();
        let code = recompile_text(src, &opts).expect("the sample compiles");
        let recovered = decompile(&code).unwrap_or_else(|| panic!("not recovered: {src:?}"));
        let outcome = verify(&recovered, &opts, &code).expect("recovered C compiles");
        assert!(
            outcome.is_match(),
            "decompiled C must recompile to the original bytes.\nsource:  {src}\nrecovered:\n{recovered}",
        );
    }

    #[test]
    fn return_constant_roundtrips() {
        assert_roundtrips("int f() { return 0; }\n");
        assert_roundtrips("int f() { return 42; }\n");
        assert_roundtrips("int f() { return 1234; }\n");
    }

    #[test]
    fn local_assignment_roundtrips() {
        assert_roundtrips("int f() { int x; x = 5; return x; }\n");
    }

    #[test]
    fn arithmetic_chain_roundtrips() {
        assert_roundtrips("int f() { int x; int y; x = 5; y = x + 3; return y; }\n");
    }

    #[test]
    fn subtraction_and_bitwise_roundtrip() {
        // Exercises the `sub ax,imm` / `and ax,imm` / `or ax,imm` accumulator
        // forms — and `- 2`, which BCC encodes as `add ax,-2` (a sign asymmetry
        // the lift's sign-extended immediate has to reproduce).
        assert_roundtrips("int f() { int x; x = 9; return x - 2; }\n");
        assert_roundtrips("int f() { int x; x = 12; return x & 6; }\n");
        assert_roundtrips("int f() { int x; x = 1; return x | 8; }\n");
    }

    #[test]
    fn void_body_roundtrips() {
        assert_roundtrips("void f() { }\n");
    }

    #[test]
    fn incomplete_function_emits_nothing() {
        // A call isn't recovered yet, so decompile must decline rather than emit
        // a body that silently drops it.
        let opts = CompileOpts::default();
        let code = recompile_text("int g(); int f() { return g(); }\n", &opts).expect("compiles");
        assert!(decompile(&code).is_none(), "an incomplete recovery emits no C");
    }
}
