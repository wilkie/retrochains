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

use crate::hi_ir::{recover, Expr, Function, LValue, RelOp, Stmt, Type};

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

    emit_block(&f.body, 1, true, &mut s);
    s.push_str("}\n");
    Some(s)
}

/// Emit a statement list at indent depth `depth`. `top` marks the function's
/// outermost block, where a trailing valueless `return` is implicit and dropped
/// (keeping the body identical to an empty one).
fn emit_block(stmts: &[Stmt], depth: usize, top: bool, out: &mut String) {
    let n = stmts.len();
    for (i, stmt) in stmts.iter().enumerate() {
        if top && i + 1 == n && matches!(stmt, Stmt::Return(None)) {
            continue;
        }
        emit_stmt(stmt, depth, out);
    }
}

fn indent(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn emit_stmt(stmt: &Stmt, depth: usize, out: &mut String) {
    indent(depth, out);
    match stmt {
        Stmt::Assign(lv, e) => {
            let _ = writeln!(out, "{} = {};", lvalue_str(lv), expr_str(e));
        }
        Stmt::Return(None) => out.push_str("return;\n"),
        Stmt::Return(Some(e)) => {
            let _ = writeln!(out, "return {};", expr_str(e));
        }
        Stmt::If(cond, then, els) => {
            let _ = writeln!(out, "if ({}) {{", expr_str(cond));
            emit_block(then, depth + 1, false, out);
            if els.is_empty() {
                indent(depth, out);
                out.push_str("}\n");
            } else {
                indent(depth, out);
                out.push_str("} else {\n");
                emit_block(els, depth + 1, false, out);
                indent(depth, out);
                out.push_str("}\n");
            }
        }
        Stmt::While(cond, body) => {
            let _ = writeln!(out, "while ({}) {{", expr_str(cond));
            emit_block(body, depth + 1, false, out);
            indent(depth, out);
            out.push_str("}\n");
        }
    }
}

fn lvalue_str(lv: &LValue) -> String {
    match lv {
        LValue::Local(slot) => local_name(*slot),
    }
}

/// `v1` for `[bp-2]`, `v2` for `[bp-4]`, … — a stable name per slot.
fn local_name(slot: i16) -> String {
    format!("v{}", (-slot) / 2)
}

fn expr_str(e: &Expr) -> String {
    match e {
        Expr::Const(v) => v.to_string(),
        Expr::Local(slot) => local_name(*slot),
        // Fully parenthesized so the printed tree matches the recovered one.
        Expr::Binary(op, l, r) => format!("({} {} {})", expr_str(l), binop_token(*op), expr_str(r)),
        Expr::Rel(op, l, r) => format!("({} {} {})", expr_str(l), relop_token(*op), expr_str(r)),
    }
}

/// The C token for a foldable binary operator (the `is_foldable` set in
/// [`crate::hi_ir`]).
fn binop_token(op: crate::lo_ir::BinOp) -> &'static str {
    use crate::lo_ir::BinOp;
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Or => "|",
        BinOp::And => "&",
        BinOp::Xor => "^",
        BinOp::Shl => "<<",
        // Arithmetic/logical right shift both print as `>>`; the operand's
        // signedness decides which the compiler re-emits.
        BinOp::Shr | BinOp::Sar => ">>",
        // The fold only produces the operators above; the rest never reach here.
        _ => "?",
    }
}

fn relop_token(op: RelOp) -> &'static str {
    match op {
        RelOp::Eq => "==",
        RelOp::Ne => "!=",
        RelOp::Lt => "<",
        RelOp::Le => "<=",
        RelOp::Gt => ">",
        RelOp::Ge => ">=",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recompile_text;
    use crate::verify::{verify, CompileOpts};

    /// The whole loop in one shot: compile `src` with `opts`, decompile its
    /// `_TEXT` back to C purely from the bytes, and require that C recompile
    /// (under the same `opts`) to the *same* bytes — the §8 contract closing.
    fn assert_roundtrips_with(src: &str, opts: &CompileOpts) {
        let code = recompile_text(src, opts).expect("the sample compiles");
        let recovered = decompile(&code).unwrap_or_else(|| panic!("not recovered: {src:?}"));
        let outcome = verify(&recovered, opts, &code).expect("recovered C compiles");
        assert!(
            outcome.is_match(),
            "decompiled C must recompile to the original bytes.\nsource:  {src}\nrecovered:\n{recovered}",
        );
    }

    fn assert_roundtrips(src: &str) {
        assert_roundtrips_with(src, &CompileOpts::default());
    }

    /// Stack-local options — control flow this increment recovers uses stack
    /// locals, not BCC's default `si`/`di` register variables.
    fn assert_roundtrips_stack(src: &str) {
        assert_roundtrips_with(src, &CompileOpts { no_reg_vars: true, ..CompileOpts::default() });
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
        assert_roundtrips("int f() { int x; x = 9; return x - 2; }\n");
        assert_roundtrips("int f() { int x; x = 12; return x & 6; }\n");
        assert_roundtrips("int f() { int x; x = 1; return x | 8; }\n");
    }

    #[test]
    fn void_body_roundtrips() {
        assert_roundtrips("void f() { }\n");
    }

    #[test]
    fn if_roundtrips() {
        assert_roundtrips_stack("int f() { int x; x = 0; if (x) { x = 1; } return x; }\n");
        assert_roundtrips_stack("int f() { int x; x = 3; if (x == 5) { x = 7; } return x; }\n");
    }

    #[test]
    fn if_else_roundtrips() {
        assert_roundtrips_stack(
            "int f() { int x; x = 3; if (x == 5) { x = 7; } else { x = 9; } return x; }\n",
        );
    }

    #[test]
    fn while_roundtrips() {
        assert_roundtrips_stack("int f() { int x; x = 0; while (x < 10) { x = x + 1; } return x; }\n");
    }

    #[test]
    fn nested_and_sequential_control_flow_roundtrips() {
        // Recursive structuring: an if nested inside a while, an accumulation
        // loop, sequential ifs, and an if/else with arithmetic bodies.
        assert_roundtrips_stack(
            "int f() { int x; x = 0; while (x < 10) { if (x == 5) { x = 8; } x = x + 1; } return x; }\n",
        );
        assert_roundtrips_stack(
            "int f() { int i; int s; s = 0; i = 0; while (i < 4) { s = s + i; i = i + 1; } return s; }\n",
        );
        assert_roundtrips_stack(
            "int f() { int x; int y; x = 0; y = 0; if (x == 1) { y = 2; } if (x == 0) { y = 3; } return y; }\n",
        );
        assert_roundtrips_stack(
            "int f() { int x; x = 7; if (x > 3) { x = x - 1; } else { x = x + 1; } return x; }\n",
        );
    }

    #[test]
    fn incomplete_function_emits_nothing() {
        let opts = CompileOpts::default();
        let code = recompile_text("int g(); int f() { return g(); }\n", &opts).expect("compiles");
        assert!(decompile(&code).is_none(), "an incomplete recovery emits no C");
    }
}
