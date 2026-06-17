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

use crate::hi_ir::{recover, Expr, Function, LValue, RelOp, Stmt, Type, Var};
use crate::lo_ir::Reg;

/// The declaration order → name binding for a function's variables. Lookups are
/// by identity (stack slot or register), so emitted references stay consistent.
struct Names(Vec<(Var, String)>);

impl Names {
    /// Build names in declaration order: register variables first (`si` before
    /// `di`, the order BCC allocates them), then stack slots closest-to-bp
    /// first (BCC assigns slots from bp downward in declaration order). Naming
    /// every variable `v1`, `v2`, … in that order makes recompilation reproduce
    /// the same storage assignment.
    fn build(vars: &[Var]) -> Names {
        let mut order: Vec<Var> = vars.iter().filter(|v| matches!(v, Var::Reg(_))).copied().collect();
        order.sort_by_key(|v| match v {
            Var::Reg(Reg::Si) => 0,
            Var::Reg(_) => 1,
            Var::Slot(_) => 2,
        });
        let mut slots: Vec<Var> = vars.iter().filter(|v| matches!(v, Var::Slot(_))).copied().collect();
        slots.sort_by(|a, b| match (a, b) {
            (Var::Slot(x), Var::Slot(y)) => y.cmp(x), // descending disp (closest to bp first)
            _ => std::cmp::Ordering::Equal,
        });
        order.extend(slots);
        Names(order.into_iter().enumerate().map(|(i, v)| (v, format!("v{}", i + 1))).collect())
    }

    fn of(&self, var: Var) -> &str {
        self.0.iter().find(|(v, _)| *v == var).map_or("v?", |(_, n)| n.as_str())
    }

    fn decls(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(_, n)| n.as_str())
    }
}

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

    let names = Names::build(&f.vars);
    for decl in names.decls() {
        let _ = writeln!(s, "  int {decl};");
    }

    emit_block(&f.body, 1, true, &names, &mut s);
    s.push_str("}\n");
    Some(s)
}

/// Emit a statement list at indent depth `depth`. `top` marks the function's
/// outermost block, where a trailing valueless `return` is implicit and dropped
/// (keeping the body identical to an empty one).
fn emit_block(stmts: &[Stmt], depth: usize, top: bool, names: &Names, out: &mut String) {
    let n = stmts.len();
    for (i, stmt) in stmts.iter().enumerate() {
        if top && i + 1 == n && matches!(stmt, Stmt::Return(None)) {
            continue;
        }
        emit_stmt(stmt, depth, names, out);
    }
}

fn indent(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn emit_stmt(stmt: &Stmt, depth: usize, names: &Names, out: &mut String) {
    indent(depth, out);
    match stmt {
        Stmt::Assign(lv, e) => {
            let _ = writeln!(out, "{} = {};", lvalue_str(lv, names), expr_str(e, names));
        }
        Stmt::Return(None) => out.push_str("return;\n"),
        Stmt::Return(Some(e)) => {
            let _ = writeln!(out, "return {};", expr_str(e, names));
        }
        Stmt::If(cond, then, els) => {
            let _ = writeln!(out, "if ({}) {{", expr_str(cond, names));
            emit_block(then, depth + 1, false, names, out);
            if els.is_empty() {
                indent(depth, out);
                out.push_str("}\n");
            } else {
                indent(depth, out);
                out.push_str("} else {\n");
                emit_block(els, depth + 1, false, names, out);
                indent(depth, out);
                out.push_str("}\n");
            }
        }
        Stmt::While(cond, body) => {
            let _ = writeln!(out, "while ({}) {{", expr_str(cond, names));
            emit_block(body, depth + 1, false, names, out);
            indent(depth, out);
            out.push_str("}\n");
        }
    }
}

fn lvalue_str(lv: &LValue, names: &Names) -> String {
    match lv {
        LValue::Var(v) => names.of(*v).to_string(),
    }
}

fn expr_str(e: &Expr, names: &Names) -> String {
    match e {
        Expr::Const(v) => v.to_string(),
        Expr::Var(v) => names.of(*v).to_string(),
        // Fully parenthesized so the printed tree matches the recovered one.
        Expr::Binary(op, l, r) => {
            format!("({} {} {})", expr_str(l, names), binop_token(*op), expr_str(r, names))
        }
        Expr::Rel(op, l, r) => {
            format!("({} {} {})", expr_str(l, names), relop_token(*op), expr_str(r, names))
        }
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
    fn register_variable_control_flow_roundtrips() {
        // Default options — BCC promotes `x` to the `si` register variable. This
        // is the payoff: decompiling *default* BCC output (not just `-r-`), so
        // the reg-var data-flow (mov ax,si / mov si,ax / xor si,si / or si,si)
        // recovers and recompiles byte-exact.
        assert_roundtrips("int f() { int x; x = 0; if (x) { x = 1; } return x; }\n");
        assert_roundtrips("int f() { int x; x = 3; if (x == 5) { x = 7; } return x; }\n");
        assert_roundtrips("int f() { int x; x = 0; while (x < 10) { x = x + 1; } return x; }\n");
        assert_roundtrips(
            "int f() { int x; x = 0; while (x < 10) { if (x == 5) { x = 8; } x = x + 1; } return x; }\n",
        );
        // Two register variables (si and di): the declaration ordering must
        // reproduce BCC's allocation of each.
        assert_roundtrips(
            "int f() { int i; int s; s = 0; i = 0; while (i < 4) { s = s + i; i = i + 1; } return s; }\n",
        );
    }

    #[test]
    fn incomplete_function_emits_nothing() {
        let opts = CompileOpts::default();
        let code = recompile_text("int g(); int f() { return g(); }\n", &opts).expect("compiles");
        assert!(decompile(&code).is_none(), "an incomplete recovery emits no C");
    }
}
