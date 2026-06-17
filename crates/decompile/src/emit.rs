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
use crate::lo_ir::{BinOp, Reg};

/// How an offset pointer access — a `Deref` of `base + k` — is *spelled*. Both
/// forms are semantically identical and (where the compiler supports them)
/// recompile to the same bytes, so the choice is pure presentation, not
/// correctness. The recovery stays form-neutral (it produces `Deref(base + k)`);
/// this picks the surface syntax, and the recompile verifier is the gate on the
/// choice ([`crate::render_idiomatic`]). The seam a second pass — or a human, or
/// a UI toggle — would use to retune the output.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccessForm {
    /// `base[k]` — array/pointer subscript. The idiomatic first choice.
    Subscript,
    /// `*(base + k)` — explicit pointer arithmetic. The widest-coverage form
    /// (our `bcc` compiles some accesses only in this shape), and the safe
    /// default.
    PointerArith,
}

/// The name binding for a function's variables. Lookups are by identity (stack
/// slot, parameter, or register), so emitted references stay consistent.
struct Names {
    bindings: Vec<(Var, String)>,
    /// Variables accessed at byte width — declared `char` rather than `int`.
    chars: Vec<Var>,
    /// Variables that are pointers — declared `int *`.
    ptrs: Vec<Var>,
    /// Pointers dereferenced at byte width — declared `char *`.
    char_ptrs: Vec<Var>,
    /// Variables loaded as a `dx:ax` pair — declared `long`.
    longs: Vec<Var>,
    /// Variables compared/shifted as unsigned — declared `unsigned`.
    unsigneds: Vec<Var>,
    /// The function's parameter list, pre-rendered (`int p1, long p2`).
    signature: String,
    /// The number of file-scope globals to declare — likewise sized by the
    /// highest global offset used, so each access lands at the right offset.
    global_count: usize,
    /// How offset pointer accesses are spelled (subscript vs pointer arithmetic).
    form: AccessForm,
}

/// `<type> <name>` for a variable — `int *p` (pointer), `unsigned long l`,
/// `unsigned char c`, `unsigned u`, `long l`, `char c`, else `int x`.
fn decl_str(
    var: Var,
    name: &str,
    chars: &[Var],
    ptrs: &[Var],
    char_ptrs: &[Var],
    longs: &[Var],
    unsigneds: &[Var],
) -> String {
    if char_ptrs.contains(&var) {
        return format!("char *{name}");
    }
    if ptrs.contains(&var) {
        return format!("int *{name}");
    }
    let u = if unsigneds.contains(&var) { "unsigned " } else { "" };
    if longs.contains(&var) {
        format!("{u}long {name}")
    } else if chars.contains(&var) {
        format!("{u}char {name}")
    } else if u.is_empty() {
        format!("int {name}")
    } else {
        format!("unsigned {name}")
    }
}

/// The 1-based index of a word global at data-segment offset `off`.
fn global_index(off: u16) -> usize {
    usize::from(off / 2 + 1)
}

impl Names {
    /// Build names. Parameters are `p1, p2, …` by stack offset. Locals are
    /// `v1, v2, …` in BCC's allocation order — register variables first (`si`
    /// before `di`), then stack slots closest-to-bp first — so recompiling a
    /// plain `int` reproduces the same storage assignment.
    fn build(
        vars: &[Var],
        char_vars: &[Var],
        ptr_vars: &[Var],
        char_ptr_vars: &[Var],
        long_vars: &[Var],
        unsigned_vars: &[Var],
    ) -> Names {
        let mut bindings = Vec::new();

        let mut global_count = 0;
        for &v in vars {
            if let Var::Global(off) = v {
                let idx = global_index(off);
                global_count = global_count.max(idx);
                bindings.push((v, format!("gv{idx}")));
            }
        }

        // Parameters: walk offsets from `[bp+4]`, sizing a `long` parameter at 4
        // bytes (it occupies two slots) and filling unread gaps with `int`, so
        // the positional names in the body and the signature agree.
        let max_param =
            vars.iter().filter_map(|v| if let Var::Param(o) = v { Some(*o) } else { None }).max();
        let mut sig_parts = Vec::new();
        if let Some(max) = max_param {
            let mut off = 4i16;
            let mut pidx = 1usize;
            while off <= max {
                let var = Var::Param(off);
                let name = format!("p{pidx}");
                if vars.contains(&var) {
                    sig_parts.push(decl_str(
                        var,
                        &name,
                        char_vars,
                        ptr_vars,
                        char_ptr_vars,
                        long_vars,
                        unsigned_vars,
                    ));
                    bindings.push((var, name));
                    off += if long_vars.contains(&var) { 4 } else { 2 };
                } else {
                    sig_parts.push(format!("int {name}"));
                    off += 2;
                }
                pidx += 1;
            }
        }

        let mut regs: Vec<Var> = vars.iter().filter(|v| matches!(v, Var::Reg(_))).copied().collect();
        regs.sort_by_key(|v| usize::from(matches!(v, Var::Reg(Reg::Di))));
        let byteregs: Vec<Var> = vars.iter().filter(|v| matches!(v, Var::ByteReg(_))).copied().collect();
        let mut slots: Vec<Var> = vars.iter().filter(|v| matches!(v, Var::Slot(_))).copied().collect();
        slots.sort_by(|a, b| match (a, b) {
            (Var::Slot(x), Var::Slot(y)) => y.cmp(x), // descending disp (closest to bp first)
            _ => std::cmp::Ordering::Equal,
        });
        for (i, v) in regs.into_iter().chain(byteregs).chain(slots).enumerate() {
            bindings.push((v, format!("v{}", i + 1)));
        }

        Names {
            bindings,
            chars: char_vars.to_vec(),
            ptrs: ptr_vars.to_vec(),
            char_ptrs: char_ptr_vars.to_vec(),
            longs: long_vars.to_vec(),
            unsigneds: unsigned_vars.to_vec(),
            signature: sig_parts.join(", "),
            global_count,
            form: AccessForm::PointerArith,
        }
    }

    fn of(&self, var: Var) -> &str {
        self.bindings.iter().find(|(v, _)| *v == var).map_or("v?", |(_, n)| n.as_str())
    }

    /// A full typed declaration `<type> <name>`.
    fn decl(&self, var: Var, name: &str) -> String {
        decl_str(var, name, &self.chars, &self.ptrs, &self.char_ptrs, &self.longs, &self.unsigneds)
    }

    /// The pre-rendered parameter list (`int p1, long p2`).
    fn signature(&self) -> &str {
        &self.signature
    }

    /// The file-scope global declarations — `gv1, gv2, …` in offset order, so
    /// recompiling re-derives the same data-segment offsets.
    fn global_decls(&self) -> impl Iterator<Item = String> + '_ {
        (1..=self.global_count).map(|i| {
            let off = u16::try_from(i - 1).unwrap_or(0) * 2;
            self.decl(Var::Global(off), &format!("gv{i}"))
        })
    }

    /// The local-variable declarations (parameters and globals excluded — those
    /// are the signature and file scope respectively), each typed.
    fn local_decls(&self) -> impl Iterator<Item = String> + '_ {
        self.bindings
            .iter()
            .filter(|(v, _)| matches!(v, Var::Slot(_) | Var::Reg(_) | Var::ByteReg(_)))
            .map(|(v, n)| self.decl(*v, n))
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
/// [`complete`](Function::complete). Uses the safe default form
/// ([`AccessForm::PointerArith`]); [`to_c_with_form`] selects another.
#[must_use]
pub fn to_c(f: &Function) -> Option<String> {
    to_c_with_form(f, AccessForm::PointerArith)
}

/// Render a recovered function as C with a chosen access [`form`](AccessForm),
/// or `None` if it isn't [`complete`](Function::complete). Both forms recompile
/// identically where the compiler supports them, so this is presentation only —
/// the verifier gates the choice.
#[must_use]
pub fn to_c_with_form(f: &Function, form: AccessForm) -> Option<String> {
    if !f.complete {
        return None;
    }

    let ret = match f.ret {
        Type::Int => "int",
        Type::Char => "char",
        Type::Long => "long",
        Type::Void => "void",
    };
    let mut names = Names::build(
        &f.vars,
        &f.char_vars,
        &f.ptr_vars,
        &f.char_ptr_vars,
        &f.long_vars,
        &f.unsigned_vars,
    );
    names.form = form;

    let mut s = String::new();
    // The callee of every recovered call is an opaque external (its identity
    // isn't in `_TEXT`); one K&R prototype lets us call it with any arguments.
    if body_has_call(&f.body) {
        s.push_str("extern int g0();\n");
    }
    // File-scope globals, in offset order, so they get the same offsets.
    for g in names.global_decls() {
        let _ = writeln!(s, "{g};");
    }
    let _ = writeln!(s, "{ret} f({}) {{", names.signature());
    for decl in names.local_decls() {
        let _ = writeln!(s, "  {decl};");
    }

    emit_block(&f.body, 1, true, &names, &mut s);
    s.push_str("}\n");
    Some(s)
}

/// Does the recovered body contain a call anywhere (so it needs the extern)?
fn body_has_call(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| match s {
        Stmt::Assign(_, e) | Stmt::ExprStmt(e) | Stmt::Return(Some(e)) => expr_has_call(e),
        Stmt::Return(None) => false,
        Stmt::If(c, t, e) => expr_has_call(c) || body_has_call(t) || body_has_call(e),
        Stmt::While(c, b) | Stmt::Do(c, b) => expr_has_call(c) || body_has_call(b),
        Stmt::For(init, c, step, b) => {
            body_has_call(std::slice::from_ref(init))
                || expr_has_call(c)
                || body_has_call(std::slice::from_ref(step))
                || body_has_call(b)
        }
    })
}

fn expr_has_call(e: &Expr) -> bool {
    match e {
        Expr::Call(_) => true,
        Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => expr_has_call(a) || expr_has_call(b),
        Expr::Not(a) | Expr::Deref(a) => expr_has_call(a),
        Expr::Const(_) | Expr::LongConst(_) | Expr::Var(_) | Expr::AddrOf(_) => false,
    }
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
        Stmt::ExprStmt(e) => {
            let _ = writeln!(out, "{};", expr_str(e, names));
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
        Stmt::Do(cond, body) => {
            out.push_str("do {\n");
            emit_block(body, depth + 1, false, names, out);
            indent(depth, out);
            let _ = writeln!(out, "}} while ({});", expr_str(cond, names));
        }
        Stmt::For(init, cond, step, body) => {
            let _ = writeln!(
                out,
                "for ({}; {}; {}) {{",
                assign_inline(init, names),
                expr_str(cond, names),
                assign_inline(step, names),
            );
            emit_block(body, depth + 1, false, names, out);
            indent(depth, out);
            out.push_str("}\n");
        }
    }
}

/// Render an `Assign` statement inline (no indent, no trailing `;`) for a `for`
/// header clause.
fn assign_inline(stmt: &Stmt, names: &Names) -> String {
    match stmt {
        Stmt::Assign(lv, e) => format!("{} = {}", lvalue_str(lv, names), expr_str(e, names)),
        _ => String::new(),
    }
}

fn lvalue_str(lv: &LValue, names: &Names) -> String {
    match lv {
        LValue::Var(v) => names.of(*v).to_string(),
        LValue::Deref(e) => deref_str(e, names),
    }
}

/// Spell a dereference of `inner`. A plain `*p` always renders `*p`, but an
/// *offset* deref `*(base + k)` can be spelled as a subscript `base[k]` — the
/// two are equivalent and recompile to the same bytes where the compiler
/// supports both, so the [`Names::form`] policy chooses. This is the single seam
/// where the surface form of an indexed access is decided.
fn deref_str(inner: &Expr, names: &Names) -> String {
    if names.form == AccessForm::Subscript
        && let Expr::Binary(BinOp::Add, base, idx) = inner
        && let Expr::Const(k) = **idx
    {
        return format!("{}[{}]", expr_str(base, names), k);
    }
    format!("*{}", expr_str(inner, names))
}

fn expr_str(e: &Expr, names: &Names) -> String {
    match e {
        Expr::Const(v) => v.to_string(),
        Expr::LongConst(v) => format!("{v}L"),
        Expr::Var(v) => names.of(*v).to_string(),
        // Fully parenthesized so the printed tree matches the recovered one.
        Expr::Binary(op, l, r) => {
            format!("({} {} {})", expr_str(l, names), binop_token(*op), expr_str(r, names))
        }
        Expr::Rel(op, l, r) => {
            format!("({} {} {})", expr_str(l, names), relop_token(*op), expr_str(r, names))
        }
        Expr::Not(e) => format!("!{}", expr_str(e, names)),
        Expr::Deref(e) => deref_str(e, names),
        Expr::AddrOf(v) => format!("&{}", names.of(*v)),
        Expr::Call(args) => {
            let list = args.iter().map(|a| expr_str(a, names)).collect::<Vec<_>>().join(", ");
            format!("g0({list})")
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
        BinOp::Imul | BinOp::Mul => "*",
        BinOp::Idiv | BinOp::Div => "/",
        BinOp::Mod => "%",
        // The fold only produces the operators above; the rest never reach here.
        _ => "?",
    }
}

fn relop_token(op: RelOp) -> &'static str {
    // Unsigned comparisons print the same token as their signed peers; the
    // operands' `unsigned` declarations make the compare re-emit unsigned.
    match op {
        RelOp::Eq => "==",
        RelOp::Ne => "!=",
        RelOp::Lt | RelOp::ULt => "<",
        RelOp::Le | RelOp::ULe => "<=",
        RelOp::Gt | RelOp::UGt => ">",
        RelOp::Ge | RelOp::UGe => ">=",
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
    fn pointers_roundtrip() {
        // `*p` is `mov bx,p; mov ax,[bx]`; `&x` is `lea ax,[bp+disp]`. Pointer
        // params/locals/globals, deref in arithmetic, address-of, and a pointer
        // copy all recover and recompile.
        assert_roundtrips_stack("int f(int *p) { return *p; }\n");
        assert_roundtrips_stack("int f(int *p) { return *p + 1; }\n");
        assert_roundtrips_stack("int f() { int x; int *p; x = 3; p = &x; return *p; }\n");
        assert_roundtrips_stack("int f(int *p) { int *q; q = p; return *q; }\n");
        assert_roundtrips_stack("int *gp; int f() { return *gp; }\n");
        // Pointer writes (`*p = v` / `*p = const`) and a two-deref expression
        // (`*p + *q`) — store/ALU through a stack-resident pointer in bx.
        assert_roundtrips_stack("void f(int *p, int v) { *p = v; }\n");
        assert_roundtrips_stack("void f(int *p) { *p = 5; }\n");
        assert_roundtrips_stack("int f(int *p, int *q) { return *p + *q; }\n");
        assert_roundtrips_stack("int f(int *p) { *p = 7; return *p; }\n");
        // A dereference in a condition — the test value (`*p`) is the accumulator
        // at the `cmp`/`or`, recovered from the test region's fold.
        assert_roundtrips_stack("int f(int *p) { if (*p > 0) { return 1; } return 0; }\n");
        assert_roundtrips_stack("int f(int *p) { if (*p == 5) { return 1; } return 0; }\n");
        assert_roundtrips_stack("int f(int *p) { if (*p) { return 1; } return 0; }\n");
        assert_roundtrips_stack(
            "int f(int *p) { int s; s = 0; while (*p > s) { s = s + 1; } return s; }\n",
        );
        // A constant-offset deref (`p[K]` / `*(p+K)`) reads `[bx+K*stride]` — the
        // byte displacement divided by the pointee stride recovers the element
        // index, rendered as `*(p + K)` (which BCC recompiles to the same code).
        // `p[0]` collapses to a plain `*p`. Int (stride 2) and char (stride 1).
        assert_roundtrips_stack("int f(int *p) { return p[2]; }\n");
        assert_roundtrips_stack("int f(int *p) { return p[1]; }\n");
        assert_roundtrips_stack("int f(int *p) { return *(p + 3); }\n");
        assert_roundtrips_stack("int f(char *p) { return p[3]; }\n");
        assert_roundtrips_stack("int f(int *p) { return p[2] + 1; }\n");
        // The write side: a store at a constant offset (`*(p+K) = value`) is
        // `mov [bx+K*stride],<imm|ax>`. A constant and a variable RHS, and two
        // writes in sequence. Recovered as `*(p + K) = …`.
        assert_roundtrips_stack("void f(int *p) { p[2] = 5; }\n");
        assert_roundtrips_stack("void f(int *p, int v) { *(p + 2) = v; }\n");
        assert_roundtrips_stack("void f(int *p) { p[1] = 10; p[2] = 20; }\n");
    }

    #[test]
    fn char_pointers_roundtrip() {
        // A `char *` derefs at byte width (`mov al,[bx]`, vs `mov ax,[bx]` for an
        // `int *`), so the pointer is declared `char *`. Read (with the usual
        // `cbw` promotion to `int`), arithmetic, a write of a `char` value, and a
        // `char` immediate store (`mov byte ptr [bx],imm8`) all recover.
        assert_roundtrips_stack("int f(char *p) { return *p; }\n");
        assert_roundtrips_stack("int f(char *p) { return *p + 1; }\n");
        assert_roundtrips_stack("void f(char *p, char v) { *p = v; }\n");
        assert_roundtrips_stack("void f(char *p) { *p = 5; }\n");
        assert_roundtrips_stack("int f(char *p) { *p = 7; return *p; }\n");
    }

    #[test]
    fn char_return_roundtrips() {
        // A `char`-returning function leaves the value in `al` (a byte) with no
        // `cbw` — detectable as a byte write right before the return-jump. A
        // `char` parameter, a `char *` deref, a byte constant, and a `char`
        // local all recover with a `char` return type.
        assert_roundtrips_stack("char f(char c) { return c; }\n");
        assert_roundtrips_stack("char f(char *p) { return *p; }\n");
        assert_roundtrips_stack("char f() { return 5; }\n");
        assert_roundtrips_stack("char f() { char c; c = 3; return c; }\n");
        // Returning an `int` value from a `char` function truncates in `al`, but
        // the codegen (`mov ax,[a]`) is identical to an `int` return, so it
        // recovers as `int` — and recompiles byte-exact either way.
        assert_roundtrips_stack("int f(int a) { return a; }\n");
    }

    #[test]
    fn unsigned_comparisons_and_shifts_roundtrip() {
        // Unsigned compares (jb/ja → operands declared unsigned), an unsigned
        // loop bound, and an unsigned right shift (shr, collapsed from shift-by-1s).
        assert_roundtrips_stack("int f(unsigned a) { if (a > 5) { return 1; } return 0; }\n");
        assert_roundtrips_stack("int f(unsigned a) { if (a < 5) { return 1; } return 0; }\n");
        assert_roundtrips_stack("int f(unsigned a, unsigned b) { if (a < b) { return 1; } return 0; }\n");
        assert_roundtrips_stack("unsigned f(unsigned a) { return a >> 2; }\n");
        // unsigned char zero-extends with `mov ah,0`; a char only ever compared
        // (a byte `cmp`) is recovered as `char`, signed or unsigned.
        assert_roundtrips_stack("int f(unsigned char c) { return c; }\n");
        assert_roundtrips_stack("int f(unsigned char c) { if (c > 5) { return 1; } return 0; }\n");
        assert_roundtrips_stack("int f(char c) { if (c > 5) { return 1; } return 0; }\n");
        assert_roundtrips_stack(
            "int f(unsigned n) { unsigned i; int s; s = 0; for (i = 0; i < n; i = i + 1) { s = s + 1; } return s; }\n",
        );
    }

    #[test]
    fn multiply_divide_modulo_roundtrip() {
        // imul (memory or via dx for a constant), idiv quotient, and the idiv
        // remainder (`mov ax,dx`) → `%`.
        assert_roundtrips_stack("int f(int a, int b) { return a * b; }\n");
        assert_roundtrips_stack("int f(int a) { return a * 3; }\n");
        assert_roundtrips_stack("int f(int a, int b) { return a / b; }\n");
        assert_roundtrips_stack("int f(int a, int b) { return a % b; }\n");
        assert_roundtrips_stack("int f(int a, int b, int c) { return a * b + c; }\n");
        // Division by a constant lowers to `mov bx,K; cwd; idiv bx` — the bx
        // tracker resolves the divisor. Signed and unsigned, quotient and
        // remainder.
        assert_roundtrips_stack("int f(int a) { return a / 2; }\n");
        assert_roundtrips_stack("unsigned f(unsigned a) { return a / 2; }\n");
        assert_roundtrips_stack("int f(int a) { return a % 2; }\n");
    }

    #[test]
    fn do_while_roundtrips() {
        // A `do { } while` is a backward conditional branch with no header jump —
        // the body runs once before the test. Recovered as `Stmt::Do`.
        assert_roundtrips_stack(
            "int f(int a) { int s; s = 0; do { s = s + 1; a = a - 1; } while (a > 0); return s; }\n",
        );
        assert_roundtrips_stack(
            "int f(int n) { int i; i = 0; do { i = i + 1; } while (i < n); return i; }\n",
        );
    }

    #[test]
    fn longs_roundtrip() {
        // 32-bit `long` via dx:ax — a constant (high word zero or not) and a
        // long parameter pass-through.
        assert_roundtrips_stack("long f() { return 5; }\n");
        assert_roundtrips_stack("long f() { return 0; }\n");
        assert_roundtrips_stack("long f() { return 100000; }\n");
        assert_roundtrips_stack("long f(long a) { return a; }\n");
        // long arithmetic: add/adc and sub/sbb (low/high), with a negative long
        // constant normalized to a subtraction, and multiple long params.
        assert_roundtrips_stack("long f(long a, long b) { return a + b; }\n");
        assert_roundtrips_stack("long f(long a, long b) { return a - b; }\n");
        assert_roundtrips_stack("long f(long a) { return a + 1; }\n");
        assert_roundtrips_stack("long f(long a) { return a - 100000; }\n");
        // A `long` *local* constant assignment is a store pair (high word, then
        // low); it folds to one `long` assignment. The disambiguation from two
        // adjacent `int` locals (identical store shape) is the `dx:ax` read-back.
        assert_roundtrips_stack("long f() { long x; x = 5; return x; }\n");
        assert_roundtrips_stack("long f() { long x; x = 100000; return x; }\n");
        assert_roundtrips_stack("int f() { int x; int y; x = 3; y = 4; return x + y; }\n");
    }

    #[test]
    fn early_returns_roundtrip() {
        // Multi-exit functions: each `return <expr>` is `mov ax,val; jmp
        // epilogue`. An early return inside an if, sequential guards, an
        // if/else where both arms return, and a return inside a loop.
        assert_roundtrips("int f(int a) { if (a > 0) { return a; } return 0; }\n");
        assert_roundtrips("int f(int a) { if (a == 0) { return 1; } return a; }\n");
        assert_roundtrips("int f(int a) { if (a > 0) { return 1; } else { return 2; } }\n");
        assert_roundtrips("int f(int a) { if (a < 0) { return 0; } if (a > 9) { return 9; } return a; }\n");
        assert_roundtrips(
            "int f(int a) { int i; for (i = 0; i < 10; i = i + 1) { if (i == a) { return i; } } return 0; }\n",
        );
    }

    #[test]
    fn for_loops_roundtrip() {
        // `for` recovers as `for` and recompiles byte-exact, including a
        // parameter or global as the loop bound (a two-memory-operand compare).
        assert_roundtrips_stack(
            "int f() { int s; int i; s = 0; for (i = 0; i < 10; i = i + 1) { s = s + i; } return s; }\n",
        );
        assert_roundtrips_stack(
            "int f(int n) { int i; int s; s = 0; for (i = 0; i < n; i = i + 1) { s = s + i; } return s; }\n",
        );
        assert_roundtrips_stack(
            "int g; int f() { int i; i = 0; while (i < g) { i = i + 1; } return i; }\n",
        );
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
    fn parameters_and_calls_roundtrip() {
        // Parameters (`[bp+4]`, `[bp+6]`), a call returning into the result, a
        // parameter passed as an argument, and discarded calls as statements.
        assert_roundtrips("int f(int a) { return a; }\n");
        assert_roundtrips("int f(int a, int b) { return a + b; }\n");
        assert_roundtrips("extern int g(); int f() { return g(5); }\n");
        assert_roundtrips("extern int g(); int f(int a) { return g(a); }\n");
        assert_roundtrips("extern int g(); int f() { return g(3, 4); }\n");
        assert_roundtrips("extern void g(); void f() { g(3); g(4); }\n");
    }

    #[test]
    fn globals_roundtrip() {
        // Near globals: a scalar read/write, two distinct globals (distinguished
        // by their data-segment offset), a read-modify-write, and a global in an
        // `if` condition (`cmp [global], imm`).
        assert_roundtrips("int gv; int f() { return gv; }\n");
        assert_roundtrips("int gv; void f() { gv = 5; }\n");
        assert_roundtrips("int a; int b; int f() { a = b; return a; }\n");
        assert_roundtrips("int gv; int f() { gv = gv + 1; return gv; }\n");
        assert_roundtrips("int gv; int f(int a) { gv = a; if (gv > 0) { gv = gv - 1; } return gv; }\n");
    }

    #[test]
    fn chars_roundtrip() {
        // char globals (read/write/RMW/condition), a stack char local, and a
        // char parameter — byte loads/stores, the cbw promotion, and the byte
        // group-1 compare.
        assert_roundtrips("char cv; int f() { return cv; }\n");
        assert_roundtrips("char cv; void f() { cv = 5; }\n");
        assert_roundtrips("char cv; void f() { cv = cv + 1; }\n");
        assert_roundtrips("char cv; int f() { cv = cv - 1; return cv; }\n");
        assert_roundtrips("char cv; int f() { if (cv > 0) { cv = 0; } return cv; }\n");
        // A char in a loop — the `c = c + 1` body stays byte-wide (`inc al`).
        assert_roundtrips("int f() { char c; c = 0; while (c < 9) { c = c + 1; } return c; }\n");
        assert_roundtrips("int f() { char c; c = 3; return c; }\n");
        assert_roundtrips("int f(char a) { return a; }\n");
    }

    #[test]
    fn byte_register_variables_roundtrip() {
        // BCC promotes a used char local to a byte register variable (dl). The
        // data-flow (mov dl,imm / mov al,dl), the byte compare (cmp dl,imm), and
        // the byte truthiness test (or dl,dl) all recover and recompile.
        assert_roundtrips("int f() { char c; c = 0; if (c == 0) { c = 1; } return c; }\n");
        assert_roundtrips("int f() { char c; c = 0; if (c) { c = 1; } return c; }\n");
        assert_roundtrips("int f() { char c; c = 5; if (c > 3) { c = 0; } return c; }\n");
        assert_roundtrips("int f() { char c; c = 1; if (c == 1) { c = 2; } else { c = 3; } return c; }\n");
    }

    #[test]
    fn incomplete_function_emits_nothing() {
        // A multi-case switch isn't structured yet — the recovery declines
        // rather than emit a wrong body.
        let opts = CompileOpts::default();
        let code = recompile_text(
            "int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; } return 0; }\n",
            &opts,
        )
        .expect("compiles");
        assert!(decompile(&code).is_none(), "an incomplete recovery emits no C");
    }
}
