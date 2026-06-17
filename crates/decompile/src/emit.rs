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

use crate::hi_ir::{
    recover_program, ArraySpec, Expr, Function, LValue, RelOp, Stmt, Type, Var,
};
use crate::lo_ir::{BinOp, Reg};

/// How an offset pointer access — a `Deref` of `base + k` — is *spelled*. Both
/// forms are semantically identical and (where the compiler supports them)
/// recompile to the same bytes, so the choice is pure presentation, not
/// correctness. The recovery stays form-neutral (it produces `Deref(base + k)`);
/// this picks the surface syntax, and the recompile verifier is the gate on the
/// choice ([`crate::render_idiomatic`]). The seam a second pass — or a human, or
/// a UI toggle — would use to retune the output.
///
/// Neither form is universally compilable, which is *why* the verifier decides:
/// our `bcc` builds a constant-offset store either way, but a *variable*-index
/// store only as a subscript (`p[i] = v`), while some other shapes only build as
/// pointer arithmetic. [`Subscript`](AccessForm::Subscript) is the default
/// because it covers the most recovered cases today.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccessForm {
    /// `base[k]` — array/pointer subscript. The idiomatic default and the
    /// first choice the verifier tries.
    Subscript,
    /// `*(base + k)` — explicit pointer arithmetic. The fallback for shapes the
    /// subscript form can't express or the compiler won't build as a subscript.
    PointerArith,
}

/// The name binding for a function's variables. Lookups are by identity (stack
/// slot, parameter, or register), so emitted references stay consistent.
struct Names {
    bindings: Vec<(Var, String)>,
    /// Local arrays reconstructed from the frame; a `Slot` on one renders `aN[k]`
    /// and the slot is *not* declared as a scalar.
    arrays: Vec<ArraySpec>,
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
    /// Local callees by `_TEXT` start offset → name. A recovered call whose
    /// target is in here names that function; one that isn't is an external
    /// (`g0`). Empty for single-function emit, so every call is external there.
    callees: Vec<(usize, String)>,
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
        arrays: &[ArraySpec],
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
            arrays: arrays.to_vec(),
            form: AccessForm::Subscript,
            callees: Vec::new(),
        }
    }

    /// The name a call to `target` resolves to: a local function if `target`
    /// names one, else the opaque external `g0`.
    fn callee(&self, target: usize) -> &str {
        self.callees
            .iter()
            .find(|(off, _)| *off == target)
            .map_or("g0", |(_, name)| name.as_str())
    }

    /// The 1-based array number and element index a stack slot maps to, if it
    /// lies on a reconstructed local array.
    fn array_index(&self, off: i16) -> Option<(usize, u16)> {
        self.arrays.iter().enumerate().find_map(|(i, a)| a.index_of(off).map(|k| (i + 1, k)))
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
    ///
    /// Order matters: BCC lays out locals in declaration order top-down from
    /// `bp`, so the recompiled offsets only match if stack locals are declared
    /// closest-to-`bp` (least-negative base) first. Register variables (no stack
    /// base) lead; then arrays and scalars are interleaved by base. A slot that
    /// lands on an array is an element, declared via the array, not a scalar.
    fn local_decls(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        // Register variables first — they don't occupy the stack frame.
        for (v, n) in &self.bindings {
            if matches!(v, Var::Reg(_) | Var::ByteReg(_)) {
                out.push(self.decl(*v, n));
            }
        }
        // Stack locals (arrays + scalars), ordered top-down by base offset.
        let mut stack: Vec<(i16, String)> = self
            .arrays
            .iter()
            .enumerate()
            .map(|(i, a)| (a.base, format!("{} a{}[{}]", a.c_type(), i + 1, a.len)))
            .collect();
        for (v, n) in &self.bindings {
            if let Var::Slot(off) = v
                && self.array_index(*off).is_none()
            {
                stack.push((*off, self.decl(*v, n)));
            }
        }
        stack.sort_by_key(|&(base, _)| std::cmp::Reverse(base)); // closest to bp first
        out.extend(stack.into_iter().map(|(_, d)| d));
        out
    }

    /// Render a variable reference — a reconstructed array element spells `aN[k]`,
    /// everything else its bound name.
    fn var_str(&self, v: Var) -> String {
        if let Var::Slot(off) = v
            && let Some((i, k)) = self.array_index(off)
        {
            return format!("a{i}[{k}]");
        }
        self.of(v).to_string()
    }
}

/// Decompile `_TEXT` bytes to C, or `None` if it isn't fully recovered yet (some
/// op the lift/fold can't model, or a program shape not yet supported). A `Some`
/// result is the candidate to hand to [`crate::verify`].
///
/// Handles a multi-function segment: each function is recovered independently and
/// a local `call` between them names its callee (see [`decompile_program`]). A
/// lone function takes the single-function path unchanged.
#[must_use]
pub fn decompile(code: &[u8]) -> Option<String> {
    let funcs = recover_program(code);
    match funcs.as_slice() {
        [] => None,
        [one] => to_c(one),
        many => emit_program(many),
    }
}

/// Decompile a `_TEXT` segment as a multi-function program (always the program
/// path, even for one function — names it `f0`). Exposed for callers that want
/// the program framing explicitly; [`decompile`] picks it automatically.
#[must_use]
pub fn decompile_program(code: &[u8]) -> Option<String> {
    emit_program(&recover_program(code))
}

/// Emit a recovered multi-function program. Functions are named `f0, f1, …` in
/// `_TEXT` (definition) order — the order BCC lays them out, so reproducing it
/// reproduces each intra-module call's forward/backward resolution. A local call
/// resolves to its callee's name; an external stays `g0`.
///
/// Declines (returns `None`) when any function is incomplete, or when the program
/// touches file-scope globals (their shared data-segment layout across functions
/// isn't modelled yet) — both sound, not mis-shaped.
fn emit_program(funcs: &[Function]) -> Option<String> {
    if funcs.is_empty() || funcs.iter().any(|f| !f.complete) {
        return None;
    }
    // Globals are file-scope and shared across functions; per-function emission
    // can't yet reconcile one data-segment layout. Decline if any are used.
    if funcs.iter().any(|f| f.vars.iter().any(|v| matches!(v, Var::Global(_)))) {
        return None;
    }
    let callees: Vec<(usize, String)> =
        funcs.iter().enumerate().map(|(i, f)| (f.start, format!("f{i}"))).collect();

    let mut s = String::new();
    // One K&R prototype covers every external callee (a local call resolves to a
    // name and needs none). Emit it only if some call is actually external.
    if funcs.iter().any(|f| body_has_external_call(&f.body, &callees)) {
        s.push_str("extern int g0();\n");
    }
    for (i, f) in funcs.iter().enumerate() {
        emit_function(f, &format!("f{i}"), &callees, AccessForm::Subscript, &mut s)?;
    }
    Some(s)
}

/// Emit one function of a program into `out` under `name`, resolving local calls
/// via `callees`. Returns `None` if the function isn't complete.
fn emit_function(
    f: &Function,
    name: &str,
    callees: &[(usize, String)],
    form: AccessForm,
    out: &mut String,
) -> Option<()> {
    if !f.complete {
        return None;
    }
    let ret = type_str(f.ret);
    let mut names = Names::build(
        &f.vars,
        &f.char_vars,
        &f.ptr_vars,
        &f.char_ptr_vars,
        &f.long_vars,
        &f.unsigned_vars,
        &f.arrays,
    );
    names.form = form;
    names.callees = callees.to_vec();
    let _ = writeln!(out, "{ret} {name}({}) {{", names.signature());
    for decl in names.local_decls() {
        let _ = writeln!(out, "  {decl};");
    }
    emit_block(&f.body, 1, true, &names, out);
    out.push_str("}\n");
    Some(())
}

/// Render a recovered function as C, or `None` if it isn't
/// [`complete`](Function::complete). Uses the default form
/// ([`AccessForm::Subscript`]); [`to_c_with_form`] selects another, and
/// [`crate::render_idiomatic`] picks the first form that recompiles.
#[must_use]
pub fn to_c(f: &Function) -> Option<String> {
    to_c_with_form(f, AccessForm::Subscript)
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

    let ret = type_str(f.ret);
    let mut names = Names::build(
        &f.vars,
        &f.char_vars,
        &f.ptr_vars,
        &f.char_ptr_vars,
        &f.long_vars,
        &f.unsigned_vars,
        &f.arrays,
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

/// The C keyword for a return/declaration type.
fn type_str(ty: Type) -> &'static str {
    match ty {
        Type::Int => "int",
        Type::Char => "char",
        Type::Long => "long",
        Type::Void => "void",
    }
}

/// Does the body hold a call to an *external* (not one of `callees`)? Such a call
/// emits `g0`, so the program needs the K&R extern prototype.
fn body_has_external_call(stmts: &[Stmt], callees: &[(usize, String)]) -> bool {
    let ext = |e: &Expr| expr_has_external_call(e, callees);
    stmts.iter().any(|s| match s {
        Stmt::Assign(_, e) | Stmt::Compound(_, _, e) | Stmt::ExprStmt(e) | Stmt::Return(Some(e)) => {
            ext(e)
        }
        Stmt::Return(None) | Stmt::Break => false,
        Stmt::If(c, t, e) => ext(c) || has_ext(t, callees) || has_ext(e, callees),
        Stmt::While(c, b) | Stmt::Do(c, b) => ext(c) || has_ext(b, callees),
        Stmt::For(init, c, step, b) => {
            has_ext(std::slice::from_ref(init), callees)
                || ext(c)
                || has_ext(std::slice::from_ref(step), callees)
                || has_ext(b, callees)
        }
        Stmt::Switch(scrut, arms, def) => {
            ext(scrut)
                || arms.iter().any(|(_, b)| has_ext(b, callees))
                || has_ext(def, callees)
        }
    })
}

/// Alias kept short for the recursive calls above.
fn has_ext(stmts: &[Stmt], callees: &[(usize, String)]) -> bool {
    body_has_external_call(stmts, callees)
}

fn expr_has_external_call(e: &Expr, callees: &[(usize, String)]) -> bool {
    match e {
        Expr::Call(target, args) => {
            !callees.iter().any(|(off, _)| off == target)
                || args.iter().any(|a| expr_has_external_call(a, callees))
        }
        Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => {
            expr_has_external_call(a, callees) || expr_has_external_call(b, callees)
        }
        Expr::Ternary(a, b, c) => {
            expr_has_external_call(a, callees)
                || expr_has_external_call(b, callees)
                || expr_has_external_call(c, callees)
        }
        Expr::Not(a) | Expr::Deref(a) | Expr::Cast(_, a) | Expr::Unary(_, a) => expr_has_external_call(a, callees),
        Expr::Const(_) | Expr::LongConst(_) | Expr::Var(_) | Expr::AddrOf(_) => false,
    }
}

/// Does the recovered body contain a call anywhere (so it needs the extern)?
fn body_has_call(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| match s {
        Stmt::Assign(_, e) | Stmt::Compound(_, _, e) | Stmt::ExprStmt(e) | Stmt::Return(Some(e)) => {
            expr_has_call(e)
        }
        Stmt::Return(None) | Stmt::Break => false,
        Stmt::If(c, t, e) => expr_has_call(c) || body_has_call(t) || body_has_call(e),
        Stmt::While(c, b) | Stmt::Do(c, b) => expr_has_call(c) || body_has_call(b),
        Stmt::For(init, c, step, b) => {
            body_has_call(std::slice::from_ref(init))
                || expr_has_call(c)
                || body_has_call(std::slice::from_ref(step))
                || body_has_call(b)
        }
        Stmt::Switch(scrut, arms, def) => {
            expr_has_call(scrut)
                || arms.iter().any(|(_, b)| body_has_call(b))
                || body_has_call(def)
        }
    })
}

fn expr_has_call(e: &Expr) -> bool {
    match e {
        Expr::Call(..) => true,
        Expr::Binary(_, a, b) | Expr::Rel(_, a, b) => expr_has_call(a) || expr_has_call(b),
        Expr::Ternary(a, b, c) => expr_has_call(a) || expr_has_call(b) || expr_has_call(c),
        Expr::Not(a) | Expr::Deref(a) | Expr::Cast(_, a) | Expr::Unary(_, a) => expr_has_call(a),
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
        Stmt::Compound(lv, op, e) => {
            let _ = writeln!(out, "{};", compound_str(lv, *op, e, names));
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
        Stmt::Switch(scrut, arms, def) => {
            let _ = writeln!(out, "switch ({}) {{", expr_str(scrut, names));
            for (value, body) in arms {
                indent(depth, out);
                let _ = writeln!(out, "case {value}:");
                emit_block(body, depth + 1, false, names, out);
            }
            if !def.is_empty() {
                indent(depth, out);
                out.push_str("default:\n");
                emit_block(def, depth + 1, false, names, out);
            }
            indent(depth, out);
            out.push_str("}\n");
        }
        Stmt::Break => {
            out.push_str("break;\n");
        }
    }
}

/// Render an `Assign` statement inline (no indent, no trailing `;`) for a `for`
/// header clause.
fn assign_inline(stmt: &Stmt, names: &Names) -> String {
    match stmt {
        Stmt::Assign(lv, e) => format!("{} = {}", lvalue_str(lv, names), expr_str(e, names)),
        Stmt::Compound(lv, op, e) => compound_str(lv, *op, e, names),
        _ => String::new(),
    }
}

/// `&v` — the address of a variable. A reconstructed array slot needs its array
/// name: `&a[0]` is just `a` (the array decays to a pointer), and `&a[k]` keeps
/// the element address; everything else is `&name`. (Rendering a bare slot name
/// would print a non-existent scalar, since the slot is declared as the array.)
fn addr_of_str(v: Var, names: &Names) -> String {
    if let Var::Slot(off) = v
        && let Some((n, k)) = names.array_index(off)
    {
        return if k == 0 { format!("a{n}") } else { format!("&a{n}[{k}]") };
    }
    format!("&{}", names.of(v))
}

fn lvalue_str(lv: &LValue, names: &Names) -> String {
    match lv {
        LValue::Var(v) => names.var_str(*v),
        LValue::Deref(e) => deref_str(e, names),
    }
}

/// Spell an in-place compound modification. A `±1` step renders as `++`/`--`
/// (BCC codes `x += 1` and `x++` identically — both `inc`); any other step is
/// `lv op= rhs`.
fn compound_str(lv: &LValue, op: BinOp, rhs: &Expr, names: &Names) -> String {
    let target = lvalue_str(lv, names);
    if matches!(op, BinOp::Add | BinOp::Sub) && matches!(rhs, Expr::Const(1)) {
        let pp = if op == BinOp::Add { "++" } else { "--" };
        return format!("{target}{pp}");
    }
    format!("{target} {}= {}", binop_token(op), expr_str(rhs, names))
}

/// Spell a dereference of `inner`. A plain `*p` always renders `*p`, but an
/// *offset* deref `*(base + k)` can be spelled as a subscript `base[k]` — the
/// two are equivalent and recompile to the same bytes where the compiler
/// supports both, so the [`Names::form`] policy chooses. This is the single seam
/// where the surface form of an indexed access is decided.
fn deref_str(inner: &Expr, names: &Names) -> String {
    if let Expr::Binary(BinOp::Add, base, idx) = inner {
        // A local *array* indexed: the base is `&a[0]` (the `lea` of the array's
        // element 0), so it spells the array's name — `a[i]`, not `(&a[0])[i]`.
        if let Expr::AddrOf(Var::Slot(off)) = **base
            && let Some((n, 0)) = names.array_index(off)
        {
            return match names.form {
                AccessForm::Subscript => format!("a{n}[{}]", expr_str(idx, names)),
                AccessForm::PointerArith => format!("*(a{n} + {})", expr_str(idx, names)),
            };
        }
        // A pointer indexed: `base[k]` for a constant index, `base[i]` for a
        // variable one.
        if names.form == AccessForm::Subscript {
            return format!("{}[{}]", expr_str(base, names), expr_str(idx, names));
        }
    }
    format!("*{}", expr_str(inner, names))
}

fn expr_str(e: &Expr, names: &Names) -> String {
    match e {
        Expr::Const(v) => v.to_string(),
        Expr::LongConst(v) => format!("{v}L"),
        Expr::Var(v) => names.var_str(*v),
        // Fully parenthesized so the printed tree matches the recovered one.
        Expr::Binary(op, l, r) => {
            format!("({} {} {})", expr_str(l, names), binop_token(*op), expr_str(r, names))
        }
        Expr::Rel(op, l, r) => {
            format!("({} {} {})", expr_str(l, names), relop_token(*op), expr_str(r, names))
        }
        Expr::Not(e) => format!("!{}", expr_str(e, names)),
        Expr::Deref(e) => deref_str(e, names),
        Expr::AddrOf(v) => addr_of_str(*v, names),
        Expr::Call(target, args) => {
            let list = args.iter().map(|a| expr_str(a, names)).collect::<Vec<_>>().join(", ");
            format!("{}({list})", names.callee(*target))
        }
        Expr::Cast(ty, e) => format!("({}){}", type_str(*ty), expr_str(e, names)),
        Expr::Unary(op, e) => {
            let sym = match op {
                crate::hi_ir::UnaryOp::Neg => "-",
                crate::hi_ir::UnaryOp::BitNot => "~",
            };
            format!("{sym}{}", expr_str(e, names))
        }
        Expr::Ternary(c, t, f) => format!(
            "({} ? {} : {})",
            expr_str(c, names),
            expr_str(t, names),
            expr_str(f, names)
        ),
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
    fn compare_chain_switch_roundtrips() {
        // A small switch is a compare-chain (`cmp ax,K; je case`) — recovered as
        // a `switch` with the no-match path as the post-switch code. Two and
        // three cases, with sparse and dense values.
        assert_roundtrips_stack(
            "int f(int a) { switch (a) { case 1: return 10; case 2: return 20; case 3: return 30; } return 0; }\n",
        );
        assert_roundtrips_stack(
            "int f(int a) { switch (a) { case 5: return 1; case 9: return 2; } return 0; }\n",
        );
        // `break` cases: a body ending in a jump to the post-switch code is a
        // `break`. All-break, and break mixed with an early `return`.
        assert_roundtrips_stack(
            "int f(int a) { int r; r = 0; switch (a) { case 1: r = 10; break; case 2: r = 20; break; } return r; }\n",
        );
        assert_roundtrips_stack(
            "int f(int a) { int r; r = 0; switch (a) { case 1: r = 1; break; case 2: return 99; case 3: r = 3; break; } return r; }\n",
        );
    }

    #[test]
    fn jump_table_switch_roundtrips() {
        // A dense switch (≥ 4 contiguous cases) BCC lowers to a jump table:
        // `cmp bx,N; ja default; jmp cs:[bx+table]` with the case-body offsets in
        // an embedded table. Recovered by reading the table. Base 0, base 1, a
        // non-1 base (`sub bx,K`), and break cases.
        assert_roundtrips_stack(
            "int f(int a) { switch (a) { case 0: return 1; case 1: return 2; case 2: return 3; case 3: return 4; } return 99; }\n",
        );
        assert_roundtrips_stack(
            "int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; case 4: return 4; } return 99; }\n",
        );
        assert_roundtrips_stack(
            "int f(int a) { switch (a) { case 5: return 1; case 6: return 2; case 7: return 3; case 8: return 4; } return 99; }\n",
        );
        assert_roundtrips_stack(
            "int f(int a) { int r; r = 0; switch (a) { case 1: r = 10; break; case 2: r = 20; break; case 3: r = 30; break; case 4: r = 40; break; } return r; }\n",
        );
        // Fall-through (case values sharing a body → empty lead cases), a gap
        // within the dense range (`case 5` after a missing 4 → a table entry to
        // the no-match block), and a `default:` (recovered as post-switch code).
        assert_roundtrips_stack(
            "int f(int a) { switch (a) { case 1: case 2: return 5; case 3: return 6; case 4: return 7; } return 99; }\n",
        );
        assert_roundtrips_stack(
            "int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; case 5: return 5; } return 99; }\n",
        );
        assert_roundtrips_stack(
            "int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; case 4: return 4; default: return 9; } }\n",
        );
    }

    #[test]
    fn switch_default_with_break_roundtrips() {
        // A `default:` whose body ends in `break` (a jump to the post-switch code,
        // not the epilogue) is a real `default` arm — distinct from a `default`
        // that returns (recovered as post-switch code). Compare-chain and jump
        // table.
        assert_roundtrips_stack(
            "int f(int a) { int r; r = 0; switch (a) { case 1: r = 1; break; case 2: r = 2; break; default: r = 9; break; } return r; }\n",
        );
        assert_roundtrips_stack(
            "int f(int a) { int r; r = 0; switch (a) { case 1: r = 1; break; case 2: r = 2; break; case 3: r = 3; break; case 4: r = 4; break; default: r = 9; break; } return r; }\n",
        );
    }

    #[test]
    fn local_int_array_roundtrips() {
        // A constant array index folds to a direct `[bp+disp]` slot, so the
        // `int a[M]` surfaces as scalar slots — but only the accessed ones, which
        // under-allocates the frame. The frame is modelled as one array spanning
        // it, which reproduces the same accesses byte-exact. (Before this it was
        // a silent MISMATCH: the recovered scalars produced the wrong frame.)
        assert_roundtrips_stack("int f() { int a[4]; a[0] = 1; return a[2]; }\n");
        assert_roundtrips_stack("int f() { int a[4]; a[0] = 10; a[1] = 20; return a[0] + a[1]; }\n");
        assert_roundtrips_stack("int f() { int a[8]; a[3] = 7; return a[3]; }\n");
        // Variable index — `a[i]` is a `lea` of the array base plus a scaled
        // index (the base's provenance, `lea` vs a loaded pointer, is what makes
        // it `a[i]` not `p[i]`). A read, a write, and a fully-dynamic array (no
        // constant access — the `lea` and frame alone reveal it) all recover.
        assert_roundtrips_stack("int f(int i) { int a[4]; a[0] = 1; return a[i]; }\n");
        assert_roundtrips_stack("void f(int i, int v) { int a[4]; a[i] = v; }\n");
        assert_roundtrips_stack("int f(int i) { int a[8]; return a[i]; }\n");
    }

    #[test]
    fn mixed_frame_partitions_into_scalar_and_array() {
        // A `lea` of the array base anchors the partition, so a scalar + an array
        // recover as `int x; int a[M]` (in BCC's declaration-order top-down
        // layout), not one merged array — and round-trip. Scalar above the array,
        // scalar below it, both work.
        assert_roundtrips_stack("int f(int i) { int x; int a[4]; x = 9; return x + a[i]; }\n");
        assert_roundtrips_stack("int f(int i) { int a[4]; int x; a[i] = 1; x = 9; return x; }\n");
    }

    #[test]
    fn local_char_array_roundtrips() {
        // A `char` array is stride 1 (byte accesses, any offset parity), declared
        // `char a[M]`. A constant-index read, a variable index (read + write), and
        // a variable-only-indexed array (typed `char` from the byte deref alone,
        // no constant access) all recover.
        assert_roundtrips_stack("int f() { char a[4]; a[0] = 65; return a[2]; }\n");
        assert_roundtrips_stack("int f(int i) { char a[4]; a[0] = 65; return a[i]; }\n");
        assert_roundtrips_stack("void f(int i, int v) { char a[4]; a[i] = v; }\n");
        assert_roundtrips_stack("int f(int i) { char a[8]; return a[i]; }\n");
    }

    #[test]
    fn local_long_array_roundtrips() {
        // A `long` array is stride 4, each element a `dx:ax` word pair. A
        // variable index scales by `<<2` and reads the pair through the element
        // address (`mov dx,[bx+2]; mov ax,[bx]`); the array is declared `long`
        // and the element type comes from that pair deref alone.
        assert_roundtrips_stack("long f(int i) { long a[8]; return a[i]; }\n");
        assert_roundtrips_stack("long f(int i) { long a[4]; return a[i]; }\n");
    }

    #[test]
    fn char_arithmetic_via_spill_roundtrips() {
        // Two `char` operands each need a `cbw`, so neither can be a memory
        // operand: BCC spills the left (`push ax`), evaluates the right into `dx`,
        // pops the left back, and `add ax,dx`. The fold recovers the binary op,
        // preserving operand order (so `x - y` isn't `y - x`). Locals, a `char`
        // array's elements, and `char` parameters all work.
        assert_roundtrips_stack("int f() { char x; char y; x = 1; y = 2; return x + y; }\n");
        assert_roundtrips_stack("int f() { char x; char y; x = 9; y = 2; return x - y; }\n");
        assert_roundtrips_stack("int f() { char a[4]; a[0] = 65; a[1] = 66; return a[0] + a[1]; }\n");
        assert_roundtrips_stack("int f(char a, char b) { return a + b; }\n");
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
        // Variable index — `p[i]` scales the index by the pointee stride
        // (`i << 1` for `int`) and adds it to the pointer in bx. The base's
        // provenance (a loaded pointer, `mov bx,[p]`) makes it a pointer index;
        // a read, a use in a larger expression, and a write all recover.
        assert_roundtrips_stack("int f(int *p, int i) { return p[i]; }\n");
        assert_roundtrips_stack("int f(int *p, int i) { return p[i] + 1; }\n");
        assert_roundtrips_stack("void f(int *p, int i, int v) { p[i] = v; }\n");
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
        // Variable shift counts (`shl ax,cl` / `shr ax,cl`) — the count loaded
        // into `cl` from another variable, distinct from the constant-unrolled
        // shift-by-1s. `cl` here is the shift register, not a `char` variable.
        assert_roundtrips_stack("int f(int x, int y) { return x << y; }\n");
        assert_roundtrips_stack("int f(int x, int y) { return x >> y; }\n");
        assert_roundtrips_stack("unsigned f(unsigned x, int n) { return x >> n; }\n");
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
    fn multi_function_programs_roundtrip() {
        // Two functions share one `_TEXT`. A local call (`main` → `f`) is a near
        // call with a real displacement, so it must resolve to the local callee's
        // name — recovered as `f0`/`f1` in definition order, `f1` calling `f0`.
        assert_roundtrips("int f(int x) { return x; }\nint main(void) { return f(7); }\n");
        assert_roundtrips("int f(int a, int b) { return a + b; }\nint main(void){ return f(3, 5); }\n");
        // Two independent functions, no calls between them.
        assert_roundtrips("int f(void) { return 1; }\nint g(void) { return 2; }\n");
        // A chain of three: f2 calls f1 calls f0.
        assert_roundtrips(
            "int a(int x){ return x; }\nint b(int x){ return a(x); }\nint main(void){ return b(9); }\n",
        );
        // A mix: the callee is local, but it also calls an undefined external
        // (`g0` stays an extern; the local call still resolves).
        assert_roundtrips(
            "extern int g0();\nint f(void){ return g0(); }\nint main(void){ return f(); }\n",
        );
    }

    #[test]
    fn in_place_compound_modifications_roundtrip() {
        // A register variable / global / loop variable updated in place codes as
        // a single instruction (`inc si`, `add si,5`, `inc word [g]`), distinct
        // from the load-op-store `x = x op y` — so it recovers as `x op= y` / `++`.
        assert_roundtrips("int f(int x){ x++; return x; }\n");
        assert_roundtrips("int f(int x){ --x; return x; }\n");
        assert_roundtrips("int f(int x){ x += 5; return x; }\n");
        assert_roundtrips("int f(int x){ x -= 3; return x; }\n");
        // Globals: in-place `inc word [g]` and `add word [g],imm`.
        assert_roundtrips("int g; void f(){ g++; }\n");
        assert_roundtrips("int g; void f(){ g += 3; }\n");
        // A register-variable `for` loop, whose step `i++` and body `s += i` are
        // both in-place compounds (the load-op-store form would be longer).
        assert_roundtrips(
            "int f(int n){ int s; int i; s=0; for(i=0;i<n;i++){ s+=i; } return s; }\n",
        );
    }

    #[test]
    fn widening_conversions_roundtrip() {
        // `char * char` promotes both to `int` (each `cbw`) and multiplies via
        // the register spill — `imul dx` reads the spilled right operand, not a
        // constant. `int → long` is a `cwd` that isn't feeding an `idiv` (which
        // would just be the dividend setup), so the value widens to `long`.
        assert_roundtrips_stack("int f(char a, char b){ return a * b; }\n");
        assert_roundtrips_stack("int f(){ char a; char b; a=5; b=3; return a * b; }\n");
        assert_roundtrips_stack("long f(int i){ return i; }\n");
        assert_roundtrips_stack("long f(int i){ return (long)i; }\n");
        // The `cwd; idiv` dividend setup must stay a no-op (not a widening).
        assert_roundtrips_stack("int f(int a, int b){ return a / b; }\n");
        assert_roundtrips_stack("int f(int a){ return a % 2; }\n");
        // A `long`-local store from `dx:ax`: the widened int is written as a
        // high/low slot pair. A `long` shift via a runtime helper (its result in
        // `dx:ax` too) must *not* fold this way — the call clears `acc_long`, so
        // it declines instead of folding a stale value.
        assert_roundtrips_stack("int f(int i){ long r; r = (long)i; return (int)r; }\n");
        let opts = CompileOpts { no_reg_vars: true, ..CompileOpts::default() };
        let code =
            recompile_text("int f(long a, int n){ long r; r = a << n; return (int)r; }\n", &opts)
                .unwrap();
        assert!(decompile(&code).is_none(), "a helper-based long shift store declines");
    }

    #[test]
    fn long_arithmetic_stored_to_a_local_roundtrips() {
        // A returned `long` keeps its high word in `dx` (calling convention); a
        // `long` arithmetic result *stored to a local* keeps it in `ax` instead.
        // The reversed load (`mov ax,[hi]; mov dx,[lo]`) and add (`add dx,lo;
        // adc ax,hi`) recover the same way as the dx-high return form.
        assert_roundtrips_stack("long f(long a, long b){ long r; r = a + b; return r; }\n");
        assert_roundtrips_stack("long f(long a, long b){ long r; r = a - b; return r; }\n");
        assert_roundtrips_stack("long f(long a){ long r; r = a + 1; return r; }\n");
        // The dx-high return form still works.
        assert_roundtrips_stack("long f(long a, long b){ return a + b; }\n");
    }

    #[test]
    fn array_decay_to_pointer_recovers_the_array_name() {
        // `p = arr` (array decays to a pointer) is `lea ax,[arr]; mov si,ax`. The
        // `&` of the array's base slot must render as the array name `a1` (which
        // decays), not a stray scalar — emitting `&v?` made invalid C that
        // crashed our `bcc`. With `p++` and `*p` it round-trips end to end.
        assert_roundtrips("int f(){ char s[3]; char *p; s[0]=97; p=s; p++; return *p; }\n");
        assert_roundtrips("int f(){ int a[4]; int *p; a[0]=1; p=a; return *p; }\n");
    }

    #[test]
    fn register_variable_pointer_deref_roundtrips() {
        // BCC keeps a pointer parameter/local in a register variable (si/di), so
        // `*p` is `mov ax,[si]` — not the `mov bx,p; mov ax,[bx]` stack form. The
        // deref, store, and the char-width variants all recover (default opts, so
        // the pointer is a reg var). `p->x` (offset 0) is the same as `*p`.
        assert_roundtrips("int f(int *p){ return *p; }\n");
        assert_roundtrips("int f(int *p){ return *p + 1; }\n");
        assert_roundtrips("void f(int *p, int v){ *p = v; }\n");
        assert_roundtrips("int f(char *p){ return *p; }\n");
        assert_roundtrips("void f(char *p, char v){ *p = v; }\n");
        assert_roundtrips("struct S { int x; int y; }; int f(struct S *p){ return p->x; }\n");
    }

    #[test]
    fn address_of_global_recovers() {
        // `&g` is `mov si,<offset>` (a relocated immediate) — byte-identical to a
        // literal, but the address forces a fixup'd 3-byte `mov` (a literal 0
        // would be `xor`). A pointer reg var (it's dereferenced) loaded with an
        // immediate is recovered as `&global` at that data-segment offset.
        assert_roundtrips("int g; int main(void){ int *p = &g; return *p; }\n");
        assert_roundtrips("int a; int b; int main(void){ int *p = &b; return *p; }\n");
        assert_roundtrips("int g; int main(void){ int *p; p = &g; return p[0]; }\n");
        // The recovered RHS is an address-of, not a constant.
        let f = crate::hi_ir::recover(
            &recompile_text("int g; int main(void){ int *p = &g; return *p; }\n", &CompileOpts::default())
                .unwrap(),
        );
        assert!(
            f.body.iter().any(|s| matches!(s, Stmt::Assign(_, Expr::AddrOf(Var::Global(_))))),
            "the pointer is assigned &global, not a literal",
        );
    }

    #[test]
    fn register_variable_pointer_offset_deref_roundtrips() {
        // A field at a non-zero offset (`p->y` = `mov ax,[si+2]`) — the reg-var
        // analog of `[bx+disp]`. Recovers as `*(p + K)` / `p[K]` (struct fields
        // aren't reconstructed; the byte-identical pointer form round-trips). Int
        // read/write at offsets 2 and 4, and a char field read.
        assert_roundtrips("struct S { int x; int y; }; int f(struct S *p){ return p->y; }\n");
        assert_roundtrips("struct S { int x; int y; }; void f(struct S *p, int v){ p->y = v; }\n");
        assert_roundtrips("struct S { int a; int b; int c; }; int f(struct S *p){ return p->c; }\n");
        assert_roundtrips("int f(int *p){ return p[1]; }\n");
        assert_roundtrips("struct S { char a; char b; }; int f(struct S *p){ return p->b; }\n");
    }

    #[test]
    fn ternary_expressions_roundtrip() {
        // `cond ? t : f` is a diamond whose both arms leave a value in `ax` and
        // converge; recovered as `Expr::Ternary` and seeded into the consumer
        // (a `return`/store). Abs (`a>0 ? a : -a`), a plain select, a stored
        // result, and a truthiness condition.
        assert_roundtrips("int f(int a){ return a > 0 ? a : -a; }\n");
        assert_roundtrips("int f(int a, int b, int c){ return a ? b : c; }\n");
        assert_roundtrips("int f(int a){ int r; r = a > 5 ? 1 : 2; return r; }\n");
        assert_roundtrips_stack("int f(int a, int b){ return a < b ? a : b; }\n");
    }

    #[test]
    fn unary_operators_roundtrip() {
        // Prefix `-` (`neg`), `~` (`not`), and `!` (the `neg; sbb ax,ax; inc ax`
        // idiom that leaves 0/1). The `!` peephole consumes its `sbb`/`inc` tail;
        // a bare `neg` opening it must not be mistaken for `-x`.
        assert_roundtrips("int f(int x){ return -x; }\n");
        assert_roundtrips("int f(int x){ return ~x; }\n");
        assert_roundtrips("int f(int x){ return !x; }\n");
        assert_roundtrips("int f(int a, int b){ return -a + b; }\n");
        assert_roundtrips("int f(int x){ return ~x & 15; }\n");
        assert_roundtrips_stack("int f(){ int x; x=5; return !x; }\n");
    }

    #[test]
    fn int_to_char_narrowing_recovers_a_cast() {
        // Storing an `int` into a `char` reads the low byte (`mov al,[x]`); a
        // plain `c = x` would re-evaluate `x` at word width, so the narrowing
        // recovers an explicit `(char)` cast. The byte-load typing also keeps the
        // frame from being mis-modelled as a `char` array (which crashed `bcc`).
        // Whichever load width BCC used — an explicit `(char)x` reads the low
        // byte (`mov al,[x]`), an implicit `c = x` re-evaluates at word width —
        // the recovery distinguishes them and round-trips both.
        assert_roundtrips("int f(){ int x; char c; x=300; c=(char)x; return c; }\n");
        assert_roundtrips("int f(){ int x; char c; x=300; c=x; return c; }\n");
        assert_roundtrips("int f(){ int x; char c; x=70; c=x; return c; }\n");
        assert_roundtrips("int g; char c; void f(){ c = g; }\n");
    }

    #[test]
    fn char_in_place_compound_roundtrips() {
        // `a op= b` on a `char` register variable is an in-place byte op
        // (`add dl,al`) — the byte analog of the word compound.
        assert_roundtrips("int f(){ char a; char b; a=10; b=3; a+=b; return a; }\n");
        assert_roundtrips("int f(){ char a; char b; a=20; b=5; a-=b; return a; }\n");
        assert_roundtrips("int f(){ char a; char b; a=1; b=4; a|=b; return a; }\n");
        assert_roundtrips("int f(){ char a; char b; a=7; b=3; a&=b; return a; }\n");
        assert_roundtrips("int f(char c, char d){ c += d; return c; }\n");
        // `char op= int`: the rhs is an `int` whose low byte is read into `al`;
        // its word stores keep it typed `int` (not mis-typed `char`).
        assert_roundtrips("int f(){ char c; int n; c=5; n=240; c|=n; return c; }\n");
    }

    #[test]
    fn parameter_promotion_recovers_direct_mutation() {
        // A mutated parameter is copied into a register variable at entry; the
        // register *is* the parameter, so the recovery rewrites it back to direct
        // parameter mutation rather than a spurious local + copy. Decisive for
        // `char` (the extra local would cost a 2-byte frame), clean for `int`.
        assert_roundtrips("int f(char c){ c++; return c; }\n");
        assert_roundtrips("int f(char c){ c--; return c; }\n");
        assert_roundtrips("int f(int x){ x++; return x; }\n");
        assert_roundtrips("int f(int x){ x += 5; return x; }\n");
        // The recovered `char` parameter mutation has no spurious local.
        let f = crate::hi_ir::recover(
            &recompile_text("int f(char c){ c++; return c; }\n", &CompileOpts::default()).unwrap(),
        );
        assert!(f.vars.iter().all(|v| matches!(v, Var::Param(_))), "no local — just the param");
    }

    #[test]
    fn a_program_touching_a_global_declines() {
        // File-scope globals are shared across functions; their one data-segment
        // layout isn't reconciled across per-function recovery yet, so a
        // multi-function program that reads a global declines (sound, not wrong).
        let code = recompile_text(
            "int g; int f(void){ return g; }\nint main(void){ return f(); }\n",
            &CompileOpts::default(),
        )
        .unwrap();
        assert!(decompile(&code).is_none(), "globals across functions aren't modelled yet");
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
        // A jump-table switch whose source cases are out of value order lays the
        // bodies out non-monotonically, which the table reader declines — so the
        // recovery emits nothing rather than a wrong body. (In-order dense cases,
        // fall-through, and gaps all recover.)
        let opts = CompileOpts::default();
        let code = recompile_text(
            "int f(int a) { switch (a) { case 4: return 4; case 1: return 1; case 2: return 2; case 3: return 3; } return 0; }\n",
            &opts,
        )
        .expect("compiles");
        assert!(decompile(&code).is_none(), "an incomplete recovery emits no C");
    }
}
