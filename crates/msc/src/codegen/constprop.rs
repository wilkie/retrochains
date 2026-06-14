use crate::*;

/// `e` is a direct or indirect call.
fn cp_is_call(e: &Expr) -> bool { matches!(e, Expr::Call { .. } | Expr::CallPtr { .. }) }
/// A call-chain leaf: a bare call or `<call> * <const>` (mirrors `call_leaf_mul`).
fn cp_call_leaf(e: &Expr) -> bool {
    cp_is_call(e)
        || matches!(e, Expr::BinOp { op: BinOp::Mul, left, right }
            if cp_is_call(left) && matches!(right.as_ref(), Expr::IntLit(_)))
}
/// `e` is a left-assoc add/sub chain whose every operand is a call leaf (the
/// leftmost may be `<call>*K`; inner rights must be bare calls). Mirrors
/// `collect_call_chain` so const-prop's first-eval suppression engages exactly
/// when the codegen SI/DI call-chain scheduler will.
fn cp_left_call_chain(e: &Expr) -> bool {
    match e {
        Expr::BinOp { op: BinOp::Add | BinOp::Sub, left, right } =>
            cp_left_call_chain(left) && cp_is_call(right),
        _ => cp_call_leaf(e),
    }
}

/// Dead-code elimination around `goto`: (1) statements after an unconditional
/// top-level `goto` are unreachable until the next label, so drop them; (2) a
/// `goto L` whose target `L` is reachable from that point through only labels
/// (no intervening code) is a jump to the next instruction — MSC elides it.
pub(crate) fn drop_dead_after_goto(stmts: Vec<Stmt>) -> Vec<Stmt> {
    // Pass 0: a constant `if (cond) goto L;` becomes an unconditional goto
    // (cond true) or vanishes (cond false). Operands are already substituted to
    // literals by const-prop, so fold_cond_raw with an empty view resolves them.
    let stmts: Vec<Stmt> = stmts.into_iter().map(|s| {
        if let Stmt::If { cond, then_branch, else_branch } = &s
            && else_branch.is_none()
            && let Stmt::Goto(l) = then_branch.as_ref()
            && let Some(k) = crate::codegen::statements::fold_cond_raw(cond, &[])
        {
            if k != 0 { Stmt::Goto(l.clone()) } else { Stmt::Empty }
        } else {
            s
        }
    }).collect();
    // Pass 1: drop unreachable statements after an unconditional goto.
    let mut live: Vec<Stmt> = Vec::with_capacity(stmts.len());
    let mut dropping = false;
    for s in stmts {
        match &s {
            Stmt::Label(_) => { dropping = false; live.push(s); }
            _ if dropping => {}
            Stmt::Goto(_) => { dropping = true; live.push(s); }
            _ => live.push(s),
        }
    }
    // Pass 2: drop a goto whose target label is reachable through only labels.
    let mut out: Vec<Stmt> = Vec::with_capacity(live.len());
    for (i, s) in live.iter().enumerate() {
        if let Stmt::Goto(l) = s {
            let mut reaches = false;
            for next in &live[i + 1..] {
                match next {
                    Stmt::Label(m) if m == l => { reaches = true; break; }
                    Stmt::Label(_) => {}
                    _ => break,
                }
            }
            if reaches { continue; } // jump to the next instruction — elide
        }
        out.push(s.clone());
    }
    out
}
/// True when float/double local `idx` is read (anywhere in its subtree) as an
/// argument to a call — printf-style variadic float promotion or any callee
/// expecting a float. Such reads must stay `Local` (no int-fold). A read only
/// through an `(int)` cast (e.g. `return (int)f;`) is NOT a call arg, so a
/// literal-init float there still folds to its truncated int. Fixtures
/// 2198/3999 (keep float) vs 1670/1672 (fold cast).
fn float_local_used_as_call_arg(stmts: &[Stmt], idx: usize) -> bool {
    fn reads_local(e: &Expr, idx: usize) -> bool {
        match e {
            Expr::Local(i) => *i == idx,
            Expr::BinOp { left, right, .. } | Expr::Index2D { row: left, col: right, .. } =>
                reads_local(left, idx) || reads_local(right, idx),
            Expr::Ternary { cond, then_arm, else_arm } =>
                reads_local(cond, idx) || reads_local(then_arm, idx) || reads_local(else_arm, idx),
            Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => reads_local(ptr, idx),
            Expr::Index { index, .. } | Expr::IndexByte { index, .. }
            | Expr::LocalIndex { index, .. } | Expr::LocalIndexByte { index, .. }
            | Expr::ParamIndex { index, .. } | Expr::PtrIndexByte { index, .. } => reads_local(index, idx),
            Expr::AssignExpr { value, .. } => reads_local(value, idx),
            Expr::CastChar { value, .. } | Expr::CastLong { value, .. } => reads_local(value, idx),
            Expr::Call { args, .. } | Expr::CallStructField { args, .. } =>
                args.iter().any(|a| reads_local(a, idx)),
            Expr::CallPtr { target, args } =>
                reads_local(target, idx) || args.iter().any(|a| reads_local(a, idx)),
            Expr::Seq { value, .. } => reads_local(value, idx),
            _ => false,
        }
    }
    fn scan_expr(e: &Expr, idx: usize) -> bool {
        match e {
            Expr::Call { args, .. } | Expr::CallStructField { args, .. } =>
                args.iter().any(|a| reads_local(a, idx) || scan_expr(a, idx)),
            Expr::CallPtr { target, args } =>
                args.iter().any(|a| reads_local(a, idx)) || scan_expr(target, idx)
                    || args.iter().any(|a| scan_expr(a, idx)),
            Expr::BinOp { left, right, .. } | Expr::Index2D { row: left, col: right, .. } =>
                scan_expr(left, idx) || scan_expr(right, idx),
            Expr::Ternary { cond, then_arm, else_arm } =>
                scan_expr(cond, idx) || scan_expr(then_arm, idx) || scan_expr(else_arm, idx),
            Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => scan_expr(ptr, idx),
            Expr::Index { index, .. } | Expr::IndexByte { index, .. }
            | Expr::LocalIndex { index, .. } | Expr::LocalIndexByte { index, .. }
            | Expr::ParamIndex { index, .. } | Expr::PtrIndexByte { index, .. } => scan_expr(index, idx),
            Expr::AssignExpr { value, .. } => scan_expr(value, idx),
            Expr::CastChar { value, .. } | Expr::CastLong { value, .. } => scan_expr(value, idx),
            Expr::Seq { sides, value } => sides.iter().any(|s| scan_stmt(s, idx)) || scan_expr(value, idx),
            _ => false,
        }
    }
    fn scan_cond(c: &Cond, idx: usize) -> bool {
        match c {
            Cond::Truthy(e) => scan_expr(e, idx),
            Cond::Cmp { left, right, .. } => scan_expr(left, idx) || scan_expr(right, idx),
            Cond::And(a, b) | Cond::Or(a, b) => scan_cond(a, idx) || scan_cond(b, idx),
        }
    }
    fn scan_stmt(s: &Stmt, idx: usize) -> bool {
        match s {
            Stmt::Return(e) | Stmt::ExprStmt(e) => scan_expr(e, idx),
            Stmt::Assign { value, .. } => scan_expr(value, idx),
            Stmt::If { cond, then_branch, else_branch } =>
                scan_cond(cond, idx) || scan_stmt(then_branch, idx)
                    || else_branch.as_ref().is_some_and(|e| scan_stmt(e, idx)),
            Stmt::While { cond, body } => scan_cond(cond, idx) || scan_stmt(body, idx),
            Stmt::DoWhile { body, cond } => scan_stmt(body, idx) || scan_cond(cond, idx),
            Stmt::For { init, cond, step, body } =>
                scan_stmt(init, idx) || scan_cond(cond, idx) || scan_stmt(step, idx) || scan_stmt(body, idx),
            Stmt::Block(ss) => ss.iter().any(|s| scan_stmt(s, idx)),
            Stmt::Switch { scrutinee, cases } =>
                scan_expr(scrutinee, idx) || cases.iter().any(|a| a.body.iter().any(|s| scan_stmt(s, idx))),
            _ => false,
        }
    }
    stmts.iter().any(|s| scan_stmt(s, idx))
}
/// Pre-pass: rewrite `while (C) BODY` → `do BODY while (C)` when an assignment
/// in the straight-line code reaching the loop makes the entry test C true, so
/// the body provably runs at least once and MSC drops the while form's initial
/// jmp (and alignment NOP). The discriminator is that only `Stmt::Assign`
/// values count: a declarator literal (`int i = 0;`) is folded into the local's
/// spec.init and emits no statement, so it stays in the while form for
/// emit_loop's alignment-gated do-while elision (fixture 1182). A plain
/// assignment (`int i; i = 0; while (i < 4)`, or fixture 1452's nested
/// `j = 0; while (j < 2)`, or fixture 922's `g = 3; while (g)`) is a statement,
/// so it elides unconditionally — confirmed against the MSC oracle. Sound
/// because the rewrite fires only when C is true on first entry, so
/// `while`≡`do-while` regardless of later iterations; emit re-folds per-pass.
mod runonce {
    use crate::{Cond, Expr, RelOp, Stmt, SwitchArm};
    use crate::AssignTarget;
    use std::collections::HashMap;

    type Recent = HashMap<(bool, usize), i32>;

    fn val(e: &Expr, recent: &Recent) -> Option<i32> {
        match e {
            Expr::IntLit(k) => Some(*k),
            Expr::Local(i) => recent.get(&(false, *i)).copied(),
            Expr::Global(g) => recent.get(&(true, *g)).copied(),
            _ => None,
        }
    }
    fn while_entry_true(cond: &Cond, recent: &Recent) -> bool {
        match cond {
            Cond::Truthy(e) => val(e, recent).is_some_and(|v| v != 0),
            Cond::Cmp { op, left, right } => {
                let (Some(l), Some(r)) = (val(left, recent), val(right, recent)) else {
                    return false;
                };
                match op {
                    RelOp::Eq => l == r,
                    RelOp::Ne => l != r,
                    RelOp::Lt => l < r,
                    RelOp::Gt => l > r,
                    RelOp::Le => l <= r,
                    RelOp::Ge => l >= r,
                }
            }
            // And/Or entry tests are left to the generic emit path.
            Cond::And(_, _) | Cond::Or(_, _) => false,
        }
    }
    /// Recurse into a statement's nested bodies (so inner loops are rewritten),
    /// without carrying any straight-line knowledge across the boundary.
    fn descend(s: &mut Stmt) {
        match s {
            Stmt::Block(v) => run(v),
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::For { body, .. } => {
                descend(body);
            }
            Stmt::If { then_branch, else_branch, .. } => {
                descend(then_branch);
                if let Some(e) = else_branch {
                    descend(e);
                }
            }
            Stmt::Switch { cases, .. } => {
                for SwitchArm { body, .. } in cases {
                    run(body);
                }
            }
            _ => {}
        }
    }
    /// The literal scalar value a statement establishes for a single variable,
    /// or `None` if it's not a `var = <int-literal>` assignment.
    fn stmt_establishes(stmt: &Stmt) -> Option<((bool, usize), i32)> {
        let Stmt::Assign { target, value } = stmt else { return None };
        let Expr::IntLit(k) = value else { return None };
        match target {
            AssignTarget::Local(i) => Some(((false, *i), *k)),
            AssignTarget::Global(g) => Some(((true, *g), *k)),
            _ => None,
        }
    }
    /// Process a straight-line statement list, rewriting a `while` to do-while
    /// when its entry test is made true by the IMMEDIATELY preceding statement
    /// (an intervening statement defeats it — MSC keeps the while form, fixture
    /// 559) and the body has no loop-level break/continue (a break keeps the
    /// while form too — confirmed against the oracle).
    pub(super) fn run(stmts: &mut [Stmt]) {
        let mut prev: Recent = HashMap::new();
        for stmt in stmts.iter_mut() {
            if let Stmt::While { cond, body } = stmt
                && while_entry_true(cond, &prev)
                && !crate::codegen::statements::stmt_has_loop_break(body)
            {
                let owned = std::mem::replace(stmt, Stmt::Empty);
                if let Stmt::While { cond, body } = owned {
                    *stmt = Stmt::DoWhile { body, cond };
                }
            }
            descend(stmt);
            // `prev` reflects ONLY the immediately preceding statement.
            prev.clear();
            if let Some((key, k)) = stmt_establishes(stmt) {
                prev.insert(key, k);
            }
        }
    }
}
/// Pre-pass: store-to-load forward a just-written variable-indexed array
/// element to the read immediately following it. MSC recomputes the stored
/// EXPRESSION rather than reloading the slot: `arr[i] = i+1; sum += arr[i]` →
/// `sum += (i+1)` (`mov ax,[i]; inc ax`); `arr[i] = i*2` forwards `i*2`
/// (oracle-confirmed, in and out of loops). Sound because the rewrite only fires
/// when the forwarded value depends solely on the index variable and neither the
/// index nor the array is written (nor a call made) between the store and the
/// read — so re-evaluating it yields the just-stored element.
mod store_fwd {
    use crate::{AssignTarget, Expr, Stmt, SwitchArm};

    /// `e` references only the index local `idx` (plus integer constants) and has
    /// no call/assignment side effect — safe to re-evaluate at the read site.
    fn depends_only_on(e: &Expr, idx: usize) -> bool {
        match e {
            Expr::IntLit(_) => true,
            Expr::Local(i) => *i == idx,
            Expr::BinOp { left, right, .. } => depends_only_on(left, idx) && depends_only_on(right, idx),
            _ => false,
        }
    }
    /// Whether statement `s` writes local `idx`, writes any element of local
    /// array `arr`, or makes a call — any of which invalidates the forward.
    fn invalidates(s: &Stmt, arr: usize, idx: usize) -> bool {
        // A call anywhere clobbers memory; reuse the existing detector.
        if crate::codegen::statements::stmt_call_count(s) > 0 {
            return true;
        }
        match s {
            Stmt::Assign { target, value: _ } => match target {
                AssignTarget::Local(i) => *i == idx || *i == arr,
                AssignTarget::IndexedLocal { local, .. }
                | AssignTarget::IndexedLocalByte { local, .. }
                | AssignTarget::IndexedLocalVar { local, .. }
                | AssignTarget::IndexedLocalByteVar { local, .. }
                | AssignTarget::LocalField { local, .. } => *local == arr || *local == idx,
                _ => true, // deref / unknown target — be conservative
            },
            _ => false,
        }
    }
    /// Replace every `arr[idx]` word read in `e` with `repl`.
    fn replace(e: &mut Expr, arr: usize, idx: usize, repl: &Expr) {
        if let Expr::LocalIndex { local, index } = e
            && *local == arr
            && matches!(index.as_ref(), Expr::Local(i) if *i == idx)
        {
            *e = repl.clone();
            return;
        }
        match e {
            Expr::BinOp { left, right, .. } => { replace(left, arr, idx, repl); replace(right, arr, idx, repl); }
            Expr::AssignExpr { value, .. } => replace(value, arr, idx, repl),
            Expr::Ternary { cond, then_arm, else_arm } => {
                replace(cond, arr, idx, repl); replace(then_arm, arr, idx, repl); replace(else_arm, arr, idx, repl);
            }
            _ => {}
        }
    }
    fn replace_in_stmt(s: &mut Stmt, arr: usize, idx: usize, repl: &Expr) {
        match s {
            Stmt::Assign { value, .. } => replace(value, arr, idx, repl),
            Stmt::Return(e) | Stmt::ExprStmt(e) => replace(e, arr, idx, repl),
            _ => {}
        }
    }
    fn descend(s: &mut Stmt) {
        match s {
            Stmt::Block(v) => run(v),
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::For { body, .. } => descend(body),
            Stmt::If { then_branch, else_branch, .. } => {
                descend(then_branch);
                if let Some(e) = else_branch { descend(e); }
            }
            Stmt::Switch { cases, .. } => for SwitchArm { body, .. } in cases { run(body); },
            _ => {}
        }
    }
    pub(super) fn run(stmts: &mut [Stmt]) {
        for s in stmts.iter_mut() {
            descend(s);
        }
        for k in 0..stmts.len().saturating_sub(1) {
            // Extract the store's (arr, idx, value) from statement k.
            let info = if let Stmt::Assign { target: AssignTarget::IndexedLocalVar { local, index }, value } = &stmts[k]
                && let Expr::Local(idx) = index.as_ref()
                && depends_only_on(value, *idx)
            {
                Some((*local, *idx, value.clone()))
            } else {
                None
            };
            if let Some((arr, idx, e)) = info
                && !invalidates(&stmts[k + 1], arr, idx)
            {
                replace_in_stmt(&mut stmts[k + 1], arr, idx, &e);
            }
        }
    }
}
/// Pre-pass: when a call has 2+ comma-expression args `(side, value)`, MSC runs
/// ALL the side-effects first (in the cdecl right-to-left push order) and only
/// then reads the values — `sum2((x=10,x),(x=20,x))` runs `x=20; x=10` then pushes
/// the final `x` (=10) for BOTH args (oracle fixture 2315), rather than
/// interleaving side+push per arg. Hoist each comma arg's sides (RTL) to
/// statements before the call and replace the arg with its value; const-prop then
/// folds the deferred value reads against the post-side state.
mod comma_hoist {
    use crate::{Expr, Stmt, SwitchArm};

    /// If `e` is a direct `Call`/`CallPtr` with ≥2 comma (`Seq`) args, return the
    /// RTL-ordered side statements to hoist and rewrite the args to their values.
    fn hoist_call(e: &mut Expr) -> Option<Vec<Stmt>> {
        let args = match e {
            Expr::Call { args, .. } | Expr::CallPtr { args, .. } => args,
            _ => return None,
        };
        if args.iter().filter(|a| matches!(a, Expr::Seq { .. })).count() < 2 {
            return None;
        }
        let mut sides: Vec<Stmt> = Vec::new();
        // cdecl pushes right-to-left, so the rightmost arg's side-effects run first.
        for a in args.iter_mut().rev() {
            if let Expr::Seq { sides: s, value } = a {
                sides.append(s);
                *a = (**value).clone();
            }
        }
        Some(sides)
    }
    fn descend(s: &mut Stmt) {
        match s {
            Stmt::Block(v) => run(v),
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::For { body, .. } => descend(body),
            Stmt::If { then_branch, else_branch, .. } => {
                descend(then_branch);
                if let Some(e) = else_branch { descend(e); }
            }
            Stmt::Switch { cases, .. } => for SwitchArm { body, .. } in cases { run(body); },
            _ => {}
        }
    }
    pub(super) fn run(stmts: &mut Vec<Stmt>) {
        let mut out: Vec<Stmt> = Vec::with_capacity(stmts.len());
        for mut s in stmts.drain(..) {
            descend(&mut s);
            let call = match &mut s {
                Stmt::Assign { value, .. } => Some(value),
                Stmt::Return(e) | Stmt::ExprStmt(e) => Some(e),
                _ => None,
            };
            if let Some(c) = call
                && let Some(sides) = hoist_call(c)
            {
                out.extend(sides);
            }
            out.push(s);
        }
        *stmts = out;
    }
}
pub(crate) fn const_prop_globals(
    stmts: &[Stmt],
    local_specs: &[LocalSpec],
    long_globals: &[bool],
    global_elem_sizes: &[usize],
    global_array_lens: &[usize],
    struct_is_union: &[bool],
    union_globals: &std::collections::HashSet<usize>,
    structs: &[StructDef],
    global_struct_idxs: &[Option<usize>],
) -> (
    Vec<Stmt>,
    std::collections::HashSet<usize>,
    std::collections::HashSet<usize>,
    std::collections::HashSet<usize>,
) {
    // Locals whose struct type is a union — their field writes stay out of the
    // const-prop tables so punned sibling reads aren't folded (fixtures 177/919).
    let union_locals: std::collections::HashSet<usize> = local_specs.iter().enumerate()
        .filter(|(_, s)| s.struct_idx.map(|si| struct_is_union.get(si).copied().unwrap_or(false)).unwrap_or(false))
        .map(|(i, _)| i)
        .collect();
    let mut cp = ConstProp {
        local_specs: local_specs.to_vec(),
        global_elem_sizes: global_elem_sizes.to_vec(),
        global_array_lens: global_array_lens.to_vec(),
        union_locals,
        union_globals: union_globals.clone(),
        structs: structs.to_vec(),
        global_struct_idxs: global_struct_idxs.to_vec(),
        ..ConstProp::default()
    };
    for (i, &is_long) in long_globals.iter().enumerate() {
        if is_long { cp.long_globals.insert(i); }
    }
    // Pre-seed l_known with the locals' constant inits so the
    // const-fold pass sees `int x = 1; switch(x)` as having x=1
    // without re-deriving it from prologue stores.
    for (i, spec) in local_specs.iter().enumerate() {
        // Skip long locals: substituting `Local(c)` → `IntLit(K)` would make
        // `return (int)c;` emit a const load instead of a slot read (fixture
        // 1037), and would corrupt LONG-context reads — `printf("%ld",a)`,
        // int→long promotion (1638/1639), long shifts (2183) — by dropping the
        // high word. MSC *does* fold the `(int)<long>` truncation for a literal-
        // equivalent long (`(int)42L`, `a*1L` → `mov ax,2a`; fixture 1783), but
        // its discriminator (fold ×1/copy/pure-literal, NOT shifts/casts/adds)
        // isn't captured by `init_is_literal` — which is also true for 1638/2183
        // — so it's left unhandled. The emit-time fold_cond path still sees the
        // init via `locals.inits` for cond elision (1632).
        if spec.is_long { continue; }
        // A `float`/`double` local consumed by a call argument keeps its float
        // value: substituting it to an IntLit would truncate (`printf("%f", f)`
        // would push `3` for `3.14f`) and defeat the double-vararg promotion. Its
        // reads stay `Local`. Fixtures 2198/3999. But a literal-init float read
        // ONLY through an `(int)` cast (`return (int)f;`) must still fold to its
        // truncated int — fixtures 1670/1672 — so don't blanket-skip floats.
        if spec.is_float {
            if float_local_used_as_call_arg(stmts, i) { continue; }
        }
        // `register` locals live in SI/DI — their reads go through the register,
        // never a propagated immediate, so keep them out of the const tables.
        if spec.is_register { continue; }
        // Only literal-init locals (pure compile-time constant) get
        // substituted. Locals whose init came from another local —
        // `int n = c;` (1043), `char c = a + b;` (1046) — stay as
        // `Local` so reads go through the slot. Literal-init keeps
        // (`int x = 5;` 4081, `char c = 'A'+1;` 1023).
        if !spec.init_is_literal { continue; }
        if let Some(k) = spec.init {
            // A signed `char` local holds a sign-extended byte: `char c = 200`
            // reads as -56 when promoted to int (`(int)c`, `c > 100`, `n = c`).
            // The byte store still writes the low byte (0xC8) via spec.init, so
            // only the const-prop fold view is sign-corrected. Fixtures 2130/2284/4005.
            let k = if spec.size == 1 && !spec.is_unsigned { (k as i8) as i32 } else { k };
            cp.l_known.insert(i, k);
        }
    }
    let mut stmts = drop_dead_after_goto(stmts.to_vec());
    // Rewrite provably-run-once `while`s to do-while form before const-prop.
    runonce::run(&mut stmts);
    // Hoist multi-comma-arg side-effects so values are read after all sides.
    comma_hoist::run(&mut stmts);
    // Store-to-load forward a just-written variable-indexed array element.
    store_fwd::run(&mut stmts);
    let new_stmts: Vec<Stmt> = stmts.iter().map(|s| {
        let mut new_stmt = s.clone();
        prop_stmt(&mut new_stmt, &mut cp);
        let used: Vec<usize> = cp.aliases_used.drain().collect();
        for p in used { cp.ptr_alias.remove(&p); }
        new_stmt
    }).collect();
    // Re-run after prop so a now-constant `if (cond) goto L;` folds and its
    // newly-dead tail is dropped.
    let new_stmts = drop_dead_after_goto(new_stmts);
    // Lower return-terminated no-step loops with a leading `if(c)break;` to the
    // if/else+goto form MSC emits; mark the loop-mutated locals so the emit-time
    // fold view keeps them runtime.
    let (new_stmts, extra_mut) = crate::codegen::statements::fold_break_loops(new_stmts);
    for m in extra_mut {
        cp.mutated_locals.insert(m);
    }
    // Goto flow restructuring: invert `if (c) goto L` and inline the label
    // block at the branch site (fixtures 3306, 2230, 1701).
    let new_stmts = crate::codegen::statements::fold_goto_restructure(new_stmts);
    (new_stmts, cp.mutated_locals, cp.loop_mutated_locals, cp.mutated_globals)
}
/// Rewrite `*p` (DerefWord/DerefByte over an aliased pointer local) to the
/// aliased lvalue `Local(x)`/`Global(g)` WITHOUT const-folding it. Used on a
/// compound-assign RHS so the self-assign peephole sees `x op K` (and emits an
/// in-place `add [x],K`) rather than a folded constant store.
fn alias_rewrite_derefs(e: &mut Expr, cp: &ConstProp) {
    match e {
        Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => {
            alias_rewrite_derefs(ptr, cp);
            if let Expr::Local(p) = ptr.as_ref()
                && let Some(&a) = cp.ptr_alias.get(p)
            {
                if let Some(rewritten) = match a {
                    AliasTarget::Local(x) => Some(Expr::Local(x)),
                    AliasTarget::Global(g) => Some(Expr::Global(g)),
                    AliasTarget::String(_) => None,
                } {
                    *e = rewritten;
                }
            }
        }
        Expr::DerefLocalField { ptr_local, byte_off, size } => {
            // `p->f` where p aliases &g / &x → the direct field of g / x.
            if let Some(&a) = cp.ptr_alias.get(ptr_local) {
                match a {
                    AliasTarget::Global(g) => *e = Expr::GlobalField { global: g, byte_off: *byte_off, size: *size },
                    AliasTarget::Local(x) => *e = Expr::LocalField { local: x, byte_off: *byte_off, size: *size },
                    AliasTarget::String(_) => {}
                }
            }
        }
        Expr::BinOp { left, right, .. } => {
            alias_rewrite_derefs(left, cp);
            alias_rewrite_derefs(right, cp);
        }
        _ => {}
    }
}
/// Resolve `*p` / `*(p + byte_off)` (a DerefWord/DerefByte read) through a
/// pointer whose base address is known. Two sources of base:
///   - `ptr_alias` (offset-0 base from a bare `&x` / `&g` init), single-use.
///   - `ptr_addr` (base + byte offset from `p = &a[K]` / `p = a + K`), where the
///     pointer carries a non-zero base offset into an array. Single-use too.
/// Fold rule keys on the DEREF-SITE offset (`*p` vs `*(p+K)`):
///   - deref offset 0  → resolve the pointee (scalar `Local(x)` when the base
///     offset is also 0, else the array element `base[off/elem]`) and const-fold
///     it to an immediate (MSC folds `*p` reads — 596, 1047, 1049).
///   - deref offset K≠0 → the array element `base[total/elem]`, left as a runtime
///     read (MSC does NOT fold these — fixtures 1019, 888, 1152).
fn fold_aliased_deref(e: &mut Expr, cp: &mut ConstProp) {
    let is_byte = matches!(e, Expr::DerefByte { .. });
    let (p, deref_off) = {
        let ptr = match e {
            Expr::DerefWord { ptr } | Expr::DerefByte { ptr } => ptr,
            _ => return,
        };
        match ptr.as_ref() {
            Expr::Local(p) => (*p, 0i32),
            Expr::BinOp { op: BinOp::Add, left, right } => match (left.as_ref(), right.as_ref()) {
                (Expr::Local(p), Expr::IntLit(k)) => (*p, *k),
                _ => return,
            },
            _ => return,
        }
    };
    // Prefer the offset-0 alias map; fall back to the offset-carrying ptr_addr
    // ONLY for a genuine non-zero base offset (`p = &a[K]` / `p = a + K`, which
    // never gets a ptr_alias entry). An offset-0 pointer (`p = &g`) is handled by
    // ptr_alias with single-use semantics — falling back to ptr_addr there would
    // re-fold a later deref MSC reloads (711/714) and mis-fold far pointers
    // (1649/1652/2250, which populate ptr_addr but not ptr_alias).
    let is_far = cp.local_specs.get(p).map(|s| s.is_far_ptr).unwrap_or(false);
    let (base, base_off, from_alias) = if let Some(&b) = cp.ptr_alias.get(&p) {
        (b, 0i32, true)
    } else if let Some(&(b, off)) = cp.ptr_addr.get(&p)
        && (off != 0 || deref_off != 0)
        && !is_far
    {
        (b, off, false)
    } else {
        return;
    };
    // `*s` / `s[K]` through a CONST-string pointer → a direct CONST byte load
    // `mov al, $SG+K; cbw` (strings are char, so always a byte read).
    if let AliasTarget::String(idx) = base {
        if is_byte && base_off + deref_off >= 0 {
            cp.aliases_used.insert(p);
            *e = Expr::StrLitByte { string_idx: idx, byte_off: (base_off + deref_off) as u16 };
        }
        return;
    }
    // Single-use: drain the alias so a later deref reloads at runtime. A pointer
    // into a GLOBAL array (`p = &a[K]`, ptr_addr base = Global) holds a link-time
    // constant address, so MSC resolves EVERY deref — keep that alias alive for
    // multiple reads. Fixture 2584 (`p[-1] + p[0]`).
    if from_alias {
        cp.aliases_used.insert(p);
    } else if !matches!(base, AliasTarget::Global(_)) {
        cp.ptr_addr.remove(&p);
    }
    // An INTERIOR pointer into a LOCAL array (`p = &a[K]`, base_off≠0) indexed at
    // a NON-zero deref offset is NOT folded by MSC — it keeps the runtime pointer
    // deref `[bx + J*elem]` (reusing the live address), unlike `p[0]` / a
    // base-pointer `p = a` which both fold to a direct element. A pointer into a
    // GLOBAL array holds a link-time-constant address, so MSC resolves every
    // deref (fixture 2584) — keep folding those. Fixtures 2377 (`p[-1]`), and the
    // P0/P1/Pb oracle probes. The `from_alias`/ptr_addr consume above already ran.
    if matches!(base, AliasTarget::Local(_)) && base_off != 0 && deref_off != 0 {
        return;
    }
    let elem = if is_byte { 1 } else { 2 };
    let total = base_off + deref_off;
    if total < 0 || total % elem != 0 { return; }
    if deref_off == 0 && total == 0 {
        // A char-pointer deref of an aliased WIDER object reads the LOW byte at
        // runtime (`mov al,[x]; cbw`) — MSC does NOT fold the partial read to the
        // full word value (fixture 1562, `*(char*)&int_x`). When the aliased
        // object is itself a 1-byte char, the byte read IS the whole value, so it
        // folds like the word case (fixture 610, `*(&char_c)`).
        let base_is_byte_wide = match base {
            AliasTarget::Local(a) => cp.local_specs.get(a).map(|s| s.size == 1).unwrap_or(false),
            AliasTarget::Global(g) => cp.global_elem_sizes.get(g) == Some(&1),
            AliasTarget::String(_) => true, // string bytes are 1-wide
        };
        if is_byte && !base_is_byte_wide {
            let idx = Box::new(Expr::IntLit(0));
            *e = match base {
                AliasTarget::Local(a) => Expr::LocalIndexByte { local: a, index: idx },
                AliasTarget::Global(g) => Expr::IndexByte { array: g, index: idx },
                AliasTarget::String(_) => return,
            };
            return;
        }
        *e = match base {
            AliasTarget::Local(x) => Expr::Local(x),
            AliasTarget::Global(g) => Expr::Global(g),
            AliasTarget::String(_) => return,
        };
        prop_expr(e, cp);
        return;
    }
    let idx = Box::new(Expr::IntLit(total / elem));
    *e = match (base, is_byte) {
        (AliasTarget::Local(a), false) => Expr::LocalIndex { local: a, index: idx },
        (AliasTarget::Local(a), true) => Expr::LocalIndexByte { local: a, index: idx },
        (AliasTarget::Global(g), false) => Expr::Index { array: g, index: idx },
        (AliasTarget::Global(g), true) => Expr::IndexByte { array: g, index: idx },
        (AliasTarget::String(_), _) => return,
    };
    // A `*p` (deref offset 0) at a non-zero base offset still folds to an
    // immediate; an explicit `*(p+K)` stays a runtime element read.
    if deref_off == 0 {
        prop_expr(e, cp);
    }
}
/// `*p = v` through a ptr_addr offset alias (`p = &a[K]`/`a + K`, no ptr_alias):
/// record the written element value in the la/ga known-value table so a later
/// DIRECT read `a[K]` folds (1062), then drop the pointer's address so a later
/// POINTER read `*p` reloads at runtime (1066). No-op for far/aliased pointers.
fn record_deref_ptr_addr_write(p: usize, is_byte: bool, value: &Expr, cp: &mut ConstProp) {
    if cp.ptr_alias.contains_key(&p)
        || !cp.ptr_addr.contains_key(&p)
        || cp.local_specs.get(p).map(|s| s.is_far_ptr).unwrap_or(false)
    {
        return;
    }
    let (base, off) = cp.ptr_addr[&p];
    let folded = value.fold(&[]).map(|k| if is_byte { (k as i8) as i32 } else { k });
    match (base, folded) {
        (AliasTarget::Local(a), Some(k)) => { cp.la_known.insert((a, off as u16), k); }
        (AliasTarget::Local(a), None) => { cp.la_known.remove(&(a, off as u16)); }
        (AliasTarget::Global(g), Some(k)) => { cp.ga_known.insert((g, off as u16), k); }
        (AliasTarget::Global(g), None) => { cp.ga_known.remove(&(g, off as u16)); }
        (AliasTarget::String(_), _) => {}
    }
    cp.ptr_addr.remove(&p);
}
/// The address value an init expression denotes, as (base lvalue, byte offset):
/// `&x` / `&g` (offset 0) and `&base[K]` (lowered to `AddrOf(base) + K*elem`).
fn addr_value_of(e: &Expr) -> Option<(AliasTarget, i32)> {
    match e {
        Expr::AddrOfLocal(x) => Some((AliasTarget::Local(*x), 0)),
        Expr::AddrOfGlobal(g) => Some((AliasTarget::Global(*g), 0)),
        Expr::BinOp { op: BinOp::Add, left, right } => match (left.as_ref(), right.as_ref()) {
            (Expr::AddrOfLocal(x), Expr::IntLit(k)) => Some((AliasTarget::Local(*x), *k)),
            (Expr::AddrOfGlobal(g), Expr::IntLit(k)) => Some((AliasTarget::Global(*g), *k)),
            _ => None,
        },
        _ => None,
    }
}
/// Build the address expression for a known base + byte offset:
/// `&base` (offset 0) or `&base + off`.
fn addr_expr(base: AliasTarget, off: i32) -> Expr {
    let a = match base {
        AliasTarget::Local(x) => Expr::AddrOfLocal(x),
        AliasTarget::Global(g) => Expr::AddrOfGlobal(g),
        AliasTarget::String(_) => return Expr::IntLit(0), // unreachable in callers
    };
    if off == 0 {
        a
    } else {
        Expr::BinOp { op: BinOp::Add, left: Box::new(a), right: Box::new(Expr::IntLit(off)) }
    }
}
/// Fold a pointer-arithmetic init `q = (cast)(p + K)` (or `q = p`) where `p`
/// holds a KNOWN base address (`p = a` / `p = &a[J]`) into a link-time address
/// constant `&base + (off+K)`. MSC folds `(char*)p + 4` to `OFFSET a+4` and
/// stores it directly, rather than loading p and adding at runtime. Fixture
/// 2328. Only fires when the pointer's address is statically known (ptr_alias
/// or ptr_addr); a runtime pointer stays a load+add. Skips the self-compound
/// case (`p += K` → `p = p + K`, target == source), which MSC emits as an
/// in-place `add word[p], K` (fixtures 542/564/577/313/1651/1778).
fn fold_ptr_value_init(target: usize, value: &mut Expr, cp: &ConstProp) -> bool {
    let resolve = |p: usize| -> Option<(AliasTarget, i32)> {
        if let Some(&b) = cp.ptr_alias.get(&p) {
            Some((b, 0))
        } else {
            cp.ptr_addr.get(&p).copied()
        }
    };
    if let Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } = value
        && let Expr::Local(p) = left.as_ref()
        && *p != target
        && let Expr::IntLit(k) = right.as_ref()
        && let Some((base, off)) = resolve(*p)
        && !matches!(base, AliasTarget::String(_))
    {
        // `k` is already a byte offset (the parser scales pointer-init
        // arithmetic by the pointee size). `p - K` subtracts it. Fixture 2269.
        let delta = if matches!(op, BinOp::Sub) { -*k } else { *k };
        *value = addr_expr(base, off + delta);
        return true;
    }
    false
}
pub(crate) fn prop_stmt(stmt: &mut Stmt, cp: &mut ConstProp) {
    prop_stmt_inner(stmt, cp);
    // Statement boundary: a call anywhere in this statement invalidates
    // memory knowledge for FOLLOWING statements (see kill_if_called).
    // Compound statements (blocks, loops, ifs) recurse through prop_stmt,
    // so their inner statements hit their own boundaries first.
    kill_if_called(cp);
}

fn prop_stmt_inner(stmt: &mut Stmt, cp: &mut ConstProp) {
    match stmt {
        Stmt::Return(e) => prop_expr(e, cp),
        Stmt::ExprStmt(e) => {
            prop_expr(e, cp);
        }
        Stmt::Assign { target, value } => {
            // Set when a `p[K]=v` pointer store is rewritten to a direct array
            // element store: MSC's element table does NOT track writes that went
            // through a pointer, so the element stays unknown (later direct reads
            // must NOT fold). Fixture 1017.
            let mut from_ptr_store = false;
            // Set when this store's array index was a VARIABLE that folded to a
            // constant (`a[i] = v`, i known) — vs a source literal (`a[0] = v`).
            // Drives `la_var_written` so a later variable-indexed read forwards
            // only from a variable-indexed write. Fixtures 144/1620 vs 1090/1428.
            let mut var_indexed_write = false;
            // A source-literal-cond ternary RHS (`p = 1 ? &a : &b`) is compile-
            // time — collapse it to the chosen arm NOW, before the pointer-alias
            // and ptr_addr analysis below, so `p = &a` aliasing is recorded (the
            // generic Ternary collapse in prop_expr runs too late, after those
            // checks). The arm is left un-prop'd so an `&a`/`&g` arm reaches the
            // tracked-alias path (which keeps a's value) rather than escaping it.
            // Fixture 2318.
            if let Expr::Ternary { cond, then_arm, else_arm } = value
                && let Expr::IntLit(k) = cond.as_ref()
            {
                let chosen = if *k != 0 { (**then_arm).clone() } else { (**else_arm).clone() };
                let mut dropped = if *k != 0 { (**else_arm).clone() } else { (**then_arm).clone() };
                // The DEAD arm still has its source-level escape effect — `&b` in
                // `1 ? &a : &b` escapes b even though that arm is dropped, so a
                // later read of b must reload (not fold its init). Fixture 2318.
                prop_expr(&mut dropped, cp);
                *value = chosen;
            }
            // Runtime 2-D store `a[i][j] = v`: fold the indices; when both are
            // known, rewrite to a flat IndexedGlobal/IndexedLocal store (matching
            // MSC's const folding). Otherwise it stays Index2D for SI/BX codegen.
            if let AssignTarget::Index2D { is_global, base, row, col, cols, elem } = target {
                prop_expr(row, cp);
                prop_expr(col, cp);
                if let (Expr::IntLit(r), Expr::IntLit(c)) = (row.as_ref(), col.as_ref()) {
                    let byte_off = ((r * *cols as i32 + c) * *elem as i32) as u16;
                    *target = match (*is_global, *elem == 1) {
                        (true, false) => AssignTarget::IndexedGlobal { array: *base, byte_off },
                        (true, true) => AssignTarget::IndexedGlobalByte { array: *base, byte_off },
                        (false, false) => AssignTarget::IndexedLocal { local: *base, byte_off },
                        (false, true) => AssignTarget::IndexedLocalByte { local: *base, byte_off },
                    };
                }
            }
            // Runtime local struct-array store `a[i].f = v`: fold the index; when
            // known, rewrite to a flat LocalField store (matching MSC's const
            // folding — fixture 2438 with `i=2`). Otherwise it stays
            // LocalStructArrayField for si-scaling codegen (1821/1914).
            if let AssignTarget::LocalStructArrayField { local, index, stride, field_off, size } = target {
                prop_expr(index, cp);
                if let Expr::IntLit(k) = index.as_ref()
                    && let Ok(byte_off) = u16::try_from(*k as i64 * *stride as i64 + *field_off as i64)
                {
                    *target = AssignTarget::LocalField { local: *local, byte_off, size: *size };
                }
            }
            // Runtime-indexed local store `a[i] = v` whose index folds: rewrite
            // to a direct IndexedLocal store (MSC emits a direct store for a known
            // index) and record that the slot was written via a VARIABLE index, so
            // a later variable-indexed read may forward from it. A genuine runtime
            // index stays IndexedLocalVar for SI codegen. Fixtures 144/1620.
            if let AssignTarget::IndexedLocalVar { local, index } = target {
                prop_expr(index, cp);
                if let Expr::IntLit(k) = index.as_ref() {
                    let esz = cp.local_specs.get(*local).map(|l| l.size as i64).unwrap_or(2);
                    if let Ok(byte_off) = u16::try_from(*k as i64 * esz) {
                        var_indexed_write = true;
                        *target = AssignTarget::IndexedLocal { local: *local, byte_off };
                    }
                }
            }
            if let AssignTarget::IndexedLocalByteVar { local, index } = target {
                prop_expr(index, cp);
                if let Expr::IntLit(k) = index.as_ref()
                    && let Ok(byte_off) = u16::try_from(*k as i64)
                {
                    var_indexed_write = true;
                    *target = AssignTarget::IndexedLocalByte { local: *local, byte_off };
                }
            }
            // Pointer-arith init from a known-address pointer: `q = (char*)p + K`
            // folds to `&base + (off+K)` (a link-time address constant) so the
            // init stores directly instead of loading p and adding. MSC
            // constant-propagates p's value into the init STORE but does not
            // alias-track q for later derefs — so q gets no ptr_addr/ptr_alias
            // entry and a following `*q` stays a runtime pointer deref. Fixture
            // 2328.
            if let AssignTarget::Local(q) = target
                && fold_ptr_value_init(*q, value, cp)
            {
                cp.l_known.remove(q);
                cp.mutated_locals.insert(*q);
                cp.ptr_addr.remove(q);
                cp.ptr_alias.remove(q);
                return;
            }
            // Pointer-value tracking: record/clear p's known address (&g[K]).
            if let AssignTarget::Local(p) = target {
                if let Some(av) = addr_value_of(value) {
                    cp.ptr_addr.insert(*p, av);
                } else {
                    cp.ptr_addr.remove(p);
                }
            }
            // Global-pointer aliasing: `p = a` (p a global pointer, a a global
            // array → AddrOfGlobal) records `p -> Global(a)` so `p[K]` resolves to
            // direct `a[K]` addressing. Any other store to a global pointer clears
            // its alias.
            if let AssignTarget::Global(p) = target {
                // `p = a` (off 0) or `p = &a[J]` / `p = a + J` (byte offset J).
                if let Some((a @ AliasTarget::Global(_), off)) = addr_value_of(value) {
                    cp.ptr_alias_g.insert(*p, (a, off));
                } else {
                    cp.ptr_alias_g.remove(p);
                }
            }
            // Pointer aliasing: `int *p = &x` (p a NEAR pointer) records `p->x`
            // and leaves x's known value intact — unlike a bare `&x` (which
            // escapes and invalidates x), writes through p are tracked via the
            // `*p` rewrite below. The `p = &x` store still emits normally.
            if let AssignTarget::Local(p) = target
                && !cp.local_specs.get(*p).map(|s| s.is_far_ptr).unwrap_or(false)
                && matches!(value, Expr::AddrOfLocal(_) | Expr::AddrOfGlobal(_))
            {
                match value {
                    Expr::AddrOfLocal(x) => { cp.ptr_alias.insert(*p, AliasTarget::Local(*x)); }
                    Expr::AddrOfGlobal(g) => { cp.ptr_alias.insert(*p, AliasTarget::Global(*g)); }
                    _ => unreachable!(),
                }
                cp.l_known.remove(p);
                cp.mutated_locals.insert(*p);
                return;
            }
            // Element-level aliasing: `arr[K] = &x` (arr a local array of
            // pointers) records `(arr, byte_off) -> x`, leaving x intact (a
            // tracked alias, not an escape). The store still emits. Fixture 1565.
            if let AssignTarget::IndexedLocal { local, byte_off } = target
                && matches!(value, Expr::AddrOfLocal(_) | Expr::AddrOfGlobal(_))
            {
                match value {
                    Expr::AddrOfLocal(x) => { cp.elem_ptr_alias.insert((*local, *byte_off), AliasTarget::Local(*x)); }
                    Expr::AddrOfGlobal(g) => { cp.elem_ptr_alias.insert((*local, *byte_off), AliasTarget::Global(*g)); }
                    _ => unreachable!(),
                }
                return;
            }
            // `arr[K] = "lit"` (arr a local array of char pointers) records the
            // element as a string alias so a later `arr[K][j]` folds to a CONST
            // byte. The `OFFSET str` store still emits. Fixtures 1710, 1921.
            if let AssignTarget::IndexedLocal { local, byte_off } = target
                && let Expr::StrLit(idx) = value
            {
                cp.elem_ptr_alias.insert((*local, *byte_off), AliasTarget::String(*idx));
                return;
            }
            // Struct-field pointer aliasing: `o.p = &i` (o.p a pointer field)
            // records `(o, byte_off) -> i`, so a later chain read `o.p->v` folds
            // to the direct field `i.v`. Same key shape (local, byte_off) as the
            // array-of-pointers case above. The init store still emits. Fixture
            // 1873.
            if let AssignTarget::LocalField { local, byte_off, .. } = target
                && matches!(value, Expr::AddrOfLocal(_) | Expr::AddrOfGlobal(_))
            {
                match value {
                    Expr::AddrOfLocal(x) => { cp.elem_ptr_alias.insert((*local, *byte_off), AliasTarget::Local(*x)); }
                    Expr::AddrOfGlobal(g) => { cp.elem_ptr_alias.insert((*local, *byte_off), AliasTarget::Global(*g)); }
                    _ => unreachable!(),
                }
                return;
            }
            // Pointer aliasing with a base offset: `int *p = &a[K]` / `p = a + K`
            // (already recorded in ptr_addr above). Like the bare `&x` case this
            // is a TRACKED alias, not a genuine escape — keep the base array's
            // known element values (la_known/ga_known) intact so `*p` folds to
            // the element value (fixture 2376). The init store still emits; skip
            // prop_expr(value), which would invalidate the array via AddrOfLocal.
            if let AssignTarget::Local(p) = target
                && !cp.local_specs.get(*p).map(|s| s.is_far_ptr).unwrap_or(false)
                && let Expr::BinOp { op: BinOp::Add, left, right } = value
                && matches!(left.as_ref(), Expr::AddrOfLocal(_) | Expr::AddrOfGlobal(_))
                && matches!(right.as_ref(), Expr::IntLit(_))
            {
                cp.l_known.remove(p);
                cp.mutated_locals.insert(*p);
                return;
            }
            // `char *s = "lit"` — the pointer holds a CONST string address, so a
            // later `s[K]`/`*s` (while s is unmodified) reads the string directly.
            // The `s = OFFSET str` init store still emits. Fixtures 157, 1464.
            if let AssignTarget::Local(p) = target
                && !cp.local_specs.get(*p).map(|s| s.is_far_ptr).unwrap_or(false)
                && let Expr::StrLit(idx) = value
            {
                cp.ptr_alias.insert(*p, AliasTarget::String(*idx));
                cp.l_known.remove(p);
                cp.mutated_locals.insert(*p);
                return;
            }
            // `**pp = ...` where pp aliases a local p → resolve one level to
            // `*p = ...`; the DerefLocal rewrite below finishes the second level.
            if let AssignTarget::DoubleDerefLocal(pp) = target
                && let Some(&AliasTarget::Local(p)) = cp.ptr_alias.get(pp)
            {
                cp.aliases_used.insert(*pp);
                *target = AssignTarget::DerefLocal(p);
            }
            // `*p = ...` where p aliases x/g → rewrite to a direct store, and
            // rewrite any `*p` in the RHS to the aliased lvalue too, so a
            // compound `*p += K` becomes `x += K` (in-place add) not a fold.
            let dlb_is_byte = matches!(target, AssignTarget::DerefLocalByte(_));
            if let (AssignTarget::DerefLocal(p) | AssignTarget::DerefLocalByte(p)) = target
                && let Some(&a) = cp.ptr_alias.get(p)
                && !matches!(a, AliasTarget::String(_))
            {
                cp.aliases_used.insert(*p);
                // A byte deref of a char ARRAY resolves to a byte ELEMENT store
                // (`mov byte [g],imm`). A scalar keeps Local(x)/Global(g) so its
                // known value still PROPAGATES (a later `return x` folds) — the
                // element-table store would lose that. Fixture 465 (array) vs
                // 1247/3956/3960 (scalars).
                let is_array = match a {
                    AliasTarget::Local(x) => cp.local_specs.get(x).map(|s| s.array_len > 1).unwrap_or(false),
                    AliasTarget::Global(g) => cp.global_array_lens.get(g).copied().unwrap_or(1) > 1,
                    AliasTarget::String(_) => false,
                };
                *target = match (a, dlb_is_byte && is_array) {
                    (AliasTarget::Local(x), true) => AssignTarget::IndexedLocalByte { local: x, byte_off: 0 },
                    (AliasTarget::Local(x), false) => AssignTarget::Local(x),
                    (AliasTarget::Global(g), true) => AssignTarget::IndexedGlobalByte { array: g, byte_off: 0 },
                    (AliasTarget::Global(g), false) => AssignTarget::Global(g),
                    (AliasTarget::String(_), _) => unreachable!(),
                };
                alias_rewrite_derefs(value, cp);
            }
            // `p->f = ...` where p aliases &g / &x → a direct field store, and
            // rewrite any `p->f` self-read in the RHS to the same field so a
            // compound `p->f += K` becomes `g.f += K` (in-place) not a pointer
            // deref. Fixtures 842/843.
            if let AssignTarget::DerefLocalField { ptr_local, byte_off, size } = target
                && let Some(&a) = cp.ptr_alias.get(ptr_local)
                && !matches!(a, AliasTarget::String(_))
            {
                let (bo, sz) = (*byte_off, *size);
                cp.aliases_used.insert(*ptr_local);
                *target = match a {
                    AliasTarget::Local(x) => AssignTarget::LocalField { local: x, byte_off: bo, size: sz },
                    AliasTarget::Global(g) => AssignTarget::GlobalField { global: g, byte_off: bo, size: sz },
                    AliasTarget::String(_) => unreachable!(),
                };
                alias_rewrite_derefs(value, cp);
            }
            // `*arr[K] = ...` where the array element `arr[K]` aliases &x → a
            // direct store to x. The parser lowers this to DerefExpr{ ptr:
            // LocalIndex{arr, K} }. Fixture 1565.
            if let AssignTarget::DerefExpr { ptr, .. } = target
                && let Expr::LocalIndex { local, index } = ptr.as_ref()
                && let Expr::IntLit(k) = index.as_ref()
                && let Some(&a) = cp.elem_ptr_alias.get(&(*local, (*k * 2) as u16))
            {
                from_ptr_store = true;
                *target = match a {
                    AliasTarget::Local(x) => AssignTarget::Local(x),
                    AliasTarget::Global(g) => AssignTarget::Global(g),
                    AliasTarget::String(_) => unreachable!(),
                };
                // MSC's array-of-pointers alias is SINGLE-USE: only the FIRST
                // `*arr[K]` store is resolved to a direct store; later ones route
                // through the pointer (oracle-confirmed by swapping/repeating the
                // deref). It also treats even the resolved store conservatively for
                // value-tracking — every OTHER address-taken known value is dropped
                // (the store could alias them) and all elem aliases are consumed.
                // The resolved target's new value is re-established by the normal
                // store path below. Fixtures 2470, 1565.
                cp.l_known.clear();
                cp.g_known.clear();
                cp.la_known.clear();
                cp.ga_known.clear();
                cp.elem_ptr_alias.clear();
            }
            // `p[K] = ...` (constant K≠0) where p aliases a base array → a direct
            // element store `base[K] = ...`. byte_off is already in pointee bytes.
            if let AssignTarget::DerefLocalOffset { local: p, byte_off, is_byte } = target
                && let Some(&a) = cp.ptr_alias.get(p)
                && !matches!(a, AliasTarget::String(_))
            {
                cp.aliases_used.insert(*p);
                from_ptr_store = true;
                let (byte_off, is_byte) = (*byte_off, *is_byte);
                *target = match (a, is_byte) {
                    (AliasTarget::Local(x), false) => AssignTarget::IndexedLocal { local: x, byte_off },
                    (AliasTarget::Local(x), true) => AssignTarget::IndexedLocalByte { local: x, byte_off },
                    (AliasTarget::Global(g), false) => AssignTarget::IndexedGlobal { array: g, byte_off },
                    (AliasTarget::Global(g), true) => AssignTarget::IndexedGlobalByte { array: g, byte_off },
                    (AliasTarget::String(_), _) => unreachable!(),
                };
            }
            // `p[K] = ...` through a GLOBAL pointer aliased to array `a` → direct
            // `a[K]` store. The parser lowers this to PtrIndexByte{ptr:p, disp:K}.
            if let AssignTarget::PtrIndexByte { ptr: p, disp } = target
                && let Some(&(AliasTarget::Global(a), base_off)) = cp.ptr_alias_g.get(p)
            {
                from_ptr_store = true;
                let elem = cp.global_elem_sizes.get(a).copied().unwrap_or(2);
                let k = *disp as i64;
                let byte_off = (base_off as i64 + k * elem as i64) as u16;
                *target = if elem == 1 {
                    AssignTarget::IndexedGlobalByte { array: a, byte_off }
                } else {
                    AssignTarget::IndexedGlobal { array: a, byte_off }
                };
            }
            // `x = x op RHS` preserves the `Local(x)` on the left so
            // emit_assign can hit the in-place inc/dec/add/sub-mem
            // peepholes (fixtures 1029, 1116). Substituting `x` to its
            // known IntLit defeats those.
            // For `x = x ± RHS` (Local or Global self-assign on the
            // left), leave the LHS unsubstituted so emit_assign sees
            // `BinOp(self, Add|Sub, ...)` and hits the in-place
            // inc/dec/add/sub peephole. Other ops (Shl, Mul, etc.)
            // don't have a peephole shape, so we substitute normally
            // (fixtures 1022, 1024).
            // EXCEPTION — `t = ~t` (parsed as `t ^ -1`) of a NON-long target is
            // a plain assignment of a unary complement, NOT a compound op: MSC
            // propagates the target's known value through the complement and
            // stores the folded immediate (`mov word _g,-16` for g=15), unlike
            // `t op= K` which stays in-place (fixtures 557, 1556). Long `~` keeps
            // its self-read for the `not ax; not dx` codegen path, and `^= 0xFFFF`
            // parses to IntLit(65535) (not -1), so it is unaffected.
            let int_self_complement = matches!(value,
                    Expr::BinOp { op: BinOp::BitXor, right, .. }
                        if matches!(right.as_ref(), Expr::IntLit(-1)))
                && match target {
                    AssignTarget::Global(g) => !cp.long_globals.contains(g),
                    AssignTarget::Local(l) => !cp.local_specs.get(*l).map(|s| s.is_long).unwrap_or(false),
                    _ => false,
                };
            let self_assign_addsub = !int_self_complement && match (target.clone(), value.clone()) {
                (AssignTarget::Local(t), Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, left, .. }) => {
                    matches!(left.as_ref(), Expr::Local(l) if *l == t)
                }
                (AssignTarget::Global(t), Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, left, .. }) => {
                    matches!(left.as_ref(), Expr::Global(g) if *g == t)
                }
                // Indexed local compound assigns: `a[k] op= rhs`. Prevent
                // substituting a[k] on the LHS so the emit peephole sees the
                // BinOp shape and emits `add/sub/imul mem, imm` instead of
                // const-folding the whole expression (fixtures 1001, 1210, 1211).
                (AssignTarget::IndexedLocalByte { local: t, byte_off },
                 Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, ref left, .. }) => {
                    if let Expr::LocalIndexByte { local: lx, index } = left.as_ref() {
                        *lx == t && matches!(index.as_ref(), Expr::IntLit(k) if *k as u16 == byte_off)
                    } else { false }
                }
                (AssignTarget::IndexedLocal { local: t, byte_off },
                 Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, ref left, .. }) => {
                    if let Expr::LocalIndex { local: lx, index } = left.as_ref() {
                        *lx == t && matches!(index.as_ref(), Expr::IntLit(k) if (*k as u16 * 2) == byte_off)
                    } else { false }
                }
                // Indexed global compound assigns: `a[k] op= rhs` — keep the
                // Index self-read on the LHS so the in-place mem-op peephole fires.
                (AssignTarget::IndexedGlobalByte { array: t, byte_off },
                 Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, ref left, .. }) => {
                    if let Expr::IndexByte { array: lx, index } = left.as_ref() {
                        *lx == t && matches!(index.as_ref(), Expr::IntLit(k) if *k as u16 == byte_off)
                    } else { false }
                }
                (AssignTarget::IndexedGlobal { array: t, byte_off },
                 Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, ref left, .. }) => {
                    if let Expr::Index { array: lx, index } = left.as_ref() {
                        *lx == t && matches!(index.as_ref(), Expr::IntLit(k) if (*k as u16 * 2) == byte_off)
                    } else { false }
                }
                // Struct-field compound `s.f op= rhs` — keep the field self-read so
                // the in-place mem-op fires (MSC does not const-fold these).
                (AssignTarget::GlobalField { global: t, byte_off, .. },
                 Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, ref left, .. }) => {
                    matches!(left.as_ref(), Expr::GlobalField { global: g, byte_off: bo, .. } if *g == t && *bo == byte_off)
                }
                (AssignTarget::LocalField { local: t, byte_off, .. },
                 Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, ref left, .. }) => {
                    matches!(left.as_ref(), Expr::LocalField { local: l, byte_off: bo, .. } if *l == t && *bo == byte_off)
                }
                _ => false,
            };
            // A ternary RHS may fold to a constant store, but MSC does NOT
            // propagate that value to later reads of the target — the read
            // reloads from the slot (fixture 551 `x=a>0?100:200; return x` →
            // `mov [x],100; ...; mov ax,[x]`). Remember so the known-value
            // recording below is suppressed for ternary-derived stores.
            let value_was_ternary = matches!(value, Expr::Ternary { .. });
            // The pre-prop RHS, when it is a bare scalar LOCAL read. MSC reloads
            // (rather than propagating the folded value) when assigning across a
            // type CONVERSION — a char/long/signedness change — even though the
            // source local is known (`int n; char c=5; n=c; return n` reloads n).
            // Captured here to suppress the known-value recording below for such
            // converting copies. Fixtures 604/1312/2386/2491/2387.
            let rhs_src_local: Option<usize> = if let Expr::Local(i) = value { Some(*i) } else { None };
            // Pre-substitution: an all-SOURCE-literal arithmetic RHS
            // (`x = ~0` → `0^-1`, `x = 30000+30000`). Only this form
            // propagates below — computed values from substituted locals
            // (`x = a*b` with known a,b) store the folded imm but the next
            // read reloads (543/1556/546 vs 2457/2427).
            // (purely — an embedded side effect like `(a = 7) + 3` must keep
            // its statement form; fixture 1217.)
            fn pure_literal_tree(e: &Expr) -> bool {
                match e {
                    Expr::IntLit(_) => true,
                    Expr::BinOp { left, right, .. } => pure_literal_tree(left) && pure_literal_tree(right),
                    _ => false,
                }
            }
            let rhs_source_lit = matches!(value, Expr::BinOp { .. })
                && pure_literal_tree(value)
                && value.fold(&[]).is_some();
            // A struct-field/array element assigned a value COMPUTED from
            // substituted variables (`s.x = a + b`) stores its folded immediate
            // but is NOT propagated to later reads — MSC reloads it. Only direct
            // literals and pure-literal trees propagate. Captured before prop_expr
            // folds the BinOp away. Fixture 1084 (vs `s.x = 5+3` source-literal).
            let rhs_is_computed = matches!(value, Expr::BinOp { .. }) && !rhs_source_lit;
            if self_assign_addsub
                && let Expr::BinOp { right, .. } = value
            {
                // Long globals are normally never substituted (the LHS self-read
                // must stay `Global(g)` for the long-assign codegen). But here the
                // self-read LHS is preserved separately — only the RHS operand is
                // prop'd — so a KNOWN long-global RHS may fold to a literal, letting
                // a long compound `g op= h` lower to immediates / `mov ax,K; cwd`.
                // Fixtures 734/735/741/742. A long-ARRAY element RHS (`g += la[K]`)
                // folds the same way — its value lives in ga_known at byte_off
                // K*4 (long element size). Fixture 830.
                let known_long_global = match right.as_ref() {
                    Expr::Global(gi) if cp.long_globals.contains(gi) => cp.g_known.get(gi).copied(),
                    Expr::Index { array, index }
                        if cp.long_globals.contains(array)
                            && let Expr::IntLit(k) = index.as_ref() =>
                    {
                        u16::try_from(*k * 4).ok().and_then(|bo| cp.ga_known.get(&(*array, bo)).copied())
                    }
                    _ => None,
                };
                // A long-global compound with an int-GLOBAL RHS keeps that RHS a
                // runtime read: MSC loads + sign-extends an int global operand
                // (`mov ax,_i; cwd; add [g],ax; adc [g+2],dx`) rather than folding
                // it — UNLIKE an int LOCAL RHS, which does fold (fixture 755 vs
                // 257). Only int globals are preserved here. 257/258/259/269/270.
                let int_global_rhs = matches!(target, AssignTarget::Global(g) if cp.long_globals.contains(g))
                    && matches!(right.as_ref(), Expr::Global(r) if !cp.long_globals.contains(r));
                if let Some(k) = known_long_global {
                    **right = Expr::IntLit(k);
                } else if int_global_rhs {
                    // preserve the int-global RHS unsubstituted
                } else {
                    prop_expr(right, cp);
                }
            } else {
                prop_expr(value, cp);
            }
            // `v = (int)p` where p aliases `&x` (a stack local, offset 0): MSC
            // re-materializes the address with `lea` rather than reusing the live
            // AX or reloading the slot, so substitute the pointer value with its
            // address expression. Oracle-confirmed (1779 + probes): every
            // value-read of an aliased pointer re-leas. Gated to a non-pointer
            // target (an int conversion); deref reads `*p` are handled by
            // fold_aliased_deref and are not bare `Local(p)` values, so untouched.
            if let Expr::Local(p) = value
                && let Some(&AliasTarget::Local(x)) = cp.ptr_alias.get(p)
                && let AssignTarget::Local(t) = target
                && cp.local_specs.get(*t).map(|s| s.pointee_size).unwrap_or(0) == 0
            {
                *value = Expr::AddrOfLocal(x);
            }
            // A runtime-indexed store whose index carries a side effect
            // (`a[i++] = v`) mutates the index variable — propagate the index
            // so the `i++` registers as a mutation and a later `return i`
            // does not fold to i's stale init. Fixture 2499.
            match target {
                AssignTarget::IndexedGlobalVar { index, .. }
                | AssignTarget::IndexedGlobalByteVar { index, .. }
                | AssignTarget::IndexedLocalVar { index, .. }
                | AssignTarget::IndexedLocalByteVar { index, .. } => prop_expr(index, cp),
                _ => {}
            }
            // Mark the assign target as mutated so the emit-time fold
            // view ignores its `spec.init` (fixture 1029, 1154).
            match target {
                AssignTarget::Local(l) => {
                    cp.mutated_locals.insert(*l);
                    // Reassigning a pointer to a non-address drops its alias.
                    // (An `&x` init was handled by the early return above.)
                    cp.ptr_alias.remove(l);
                }
                AssignTarget::Global(g) => { cp.mutated_globals.insert(*g); }
                AssignTarget::IndexedLocal { local, .. }
                | AssignTarget::IndexedLocalByte { local, .. }
                | AssignTarget::IndexedLocalVar { local, .. }
                | AssignTarget::IndexedLocalByteVar { local, .. }
                | AssignTarget::LocalField { local, .. } => { cp.mutated_locals.insert(*local); }
                AssignTarget::IndexedGlobal { array, .. }
                | AssignTarget::IndexedGlobalByte { array, .. }
                | AssignTarget::GlobalField { global: array, .. } => { cp.mutated_globals.insert(*array); }
                // `*p++ = v` mutates the pointer p itself — drop its known value
                // and alias so a later `*p` reloads p at runtime (fixture 1299).
                AssignTarget::DerefPostMutateLocal { local_idx, .. } => {
                    cp.mutated_locals.insert(*local_idx);
                    cp.ptr_alias.remove(local_idx);
                    cp.l_known.remove(local_idx);
                    cp.ptr_addr.remove(local_idx);
                }
                _ => {}
            }
            // Access size of a struct/union field write, captured before the
            // mutable `match target` below (used to size-gate union folding).
            let write_field_size: Option<u8> = match &*target {
                AssignTarget::LocalField { size, .. }
                | AssignTarget::GlobalField { size, .. } => Some(*size),
                _ => None,
            };
            match target {
                AssignTarget::Global(g) => {
                    if let Expr::IntLit(k) = value
                        && !value_was_ternary
                    {
                        cp.g_known.insert(*g, *k);
                    } else if let Expr::FloatLit(bits, _) = value {
                        // Float-global store: a later `(int)g` read folds to the
                        // truncated value (`mov ax,K`), while the float store
                        // itself still emits. Fixture 1757.
                        cp.g_known.insert(*g, f64::from_bits(*bits) as i32);
                    } else {
                        cp.g_known.remove(g);
                    }
                }
                AssignTarget::Local(l) => {
                    // A converting copy from another local (char/long/signedness
                    // mismatch with the target) stores the folded value but is NOT
                    // propagated — MSC reloads the target. Fixtures 604/1312/2491.
                    let converting = rhs_src_local.is_some_and(|s| {
                        match (cp.local_specs.get(s), cp.local_specs.get(*l)) {
                            (Some(src), Some(tgt)) => {
                                let sz = |x: &LocalSpec| if x.is_long { 4 } else { x.size };
                                sz(src) != sz(tgt) || src.is_unsigned != tgt.is_unsigned
                            }
                            _ => false,
                        }
                    });
                    // `x = <source-literal arithmetic>` (`~0`, `30000+30000`):
                    // MSC propagates — the store emits the folded immediate
                    // AND a later `return x` folds (2457/2427). Values
                    // computed from SUBSTITUTED locals (`x = a*b`) do NOT
                    // propagate — the return reloads (543/1556/546).
                    if rhs_source_lit
                        && !value_was_ternary
                        && !cp.local_specs.get(*l).is_some_and(|s| s.is_long || s.is_float)
                        && let Some(k) = value.fold(&[])
                    {
                        *value = Expr::IntLit(k);
                    }
                    if let Expr::IntLit(k) = value
                        && !value_was_ternary
                        && !converting
                    {
                        // Register targets ARE tracked (for arithmetic folding);
                        // the bare-read protection (`in_arith`) keeps `mov ax,si`.
                        cp.l_known.insert(*l, *k);
                    } else if let Some(src) = rhs_src_local
                        && !converting
                        && cp.local_specs.get(src).is_some_and(|s| s.is_register)
                        && !cp.local_specs.get(*l).is_some_and(|s| s.is_register)
                        && let Some(&k) = cp.l_known.get(&src)
                    {
                        // `a = <register local n>` keeps the register store (the
                        // RHS stays `Local(n)`, not substituted), but the target
                        // inherits n's known constant so later arithmetic folds.
                        // Fixture 1763.
                        cp.l_known.insert(*l, k);
                    } else {
                        cp.l_known.remove(l);
                    }
                    // Function-address propagation: `fp = two` records fp→_two; a
                    // later plain copy `vp = fp` rewrites to `vp = FuncAddr(two)`
                    // so the store materializes `mov [slot], OFFSET _two` directly
                    // (MSC propagates the address through near-pointer copies, incl.
                    // identity casts `(void*)`/`(int(*)())`). Fixture 2332.
                    match value {
                        Expr::FuncAddr(name) => {
                            cp.func_addr.insert(*l, name.clone());
                        }
                        Expr::Local(j) if cp.func_addr.contains_key(j) => {
                            let name = cp.func_addr[j].clone();
                            *value = Expr::FuncAddr(name.clone());
                            cp.func_addr.insert(*l, name);
                        }
                        _ => { cp.func_addr.remove(l); }
                    }
                }
                AssignTarget::IndexedLocal { local, byte_off }
                | AssignTarget::IndexedLocalByte { local, byte_off }
                | AssignTarget::LocalField { local, byte_off, .. } => {
                    // Try to fold the value once more — after prop_expr's leaf
                    // substitution the BinOp may have two literal operands ready
                    // to collapse. NOT for a float array, whose element store
                    // must keep its FloatLit for x87 codegen (fixture 1679).
                    let is_float_arr = cp.local_specs.get(*local).map(|l| l.is_float).unwrap_or(false);
                    if !is_float_arr && let Some(k) = value.fold(&[]) {
                        *value = Expr::IntLit(k);
                    }
                    // Record the (truncated) element value so later reads fold —
                    // UNLESS this store came through a pointer, which MSC's element
                    // table does not track (the element stays unknown). Fixture 1017.
                    if !from_ptr_store && !rhs_is_computed && let Some(k) = value.fold(&[]) {
                        cp.la_known.insert((*local, *byte_off), k);
                        if let Some(sz) = write_field_size {
                            cp.la_field_size.insert((*local, *byte_off), sz);
                        }
                        // Record/clear the slot's variable-write status (a literal
                        // index write clears it, so a later var read won't forward).
                        if var_indexed_write {
                            cp.la_var_written.insert((*local, *byte_off));
                        } else {
                            cp.la_var_written.remove(&(*local, *byte_off));
                        }
                    } else {
                        cp.la_known.remove(&(*local, *byte_off));
                        cp.la_field_size.remove(&(*local, *byte_off));
                    }
                }
                AssignTarget::IndexedGlobal { array, byte_off }
                | AssignTarget::IndexedGlobalByte { array, byte_off }
                | AssignTarget::GlobalField { global: array, byte_off, .. } => {
                    if let Some(k) = value.fold(&[]) {
                        *value = Expr::IntLit(k);
                    }
                    if !from_ptr_store && !rhs_is_computed && let Expr::IntLit(k) = value {
                        cp.ga_known.insert((*array, *byte_off), *k);
                        if let Some(sz) = write_field_size {
                            cp.ga_field_size.insert((*array, *byte_off), sz);
                        }
                    } else {
                        cp.ga_known.remove(&(*array, *byte_off));
                        cp.ga_field_size.remove(&(*array, *byte_off));
                    }
                }
                // `*p = v` where p points into an array element via a ptr_addr
                // offset alias (`p = &a[K]` / `p = a + K`, ptr_alias absent).
                // MSC's element table DOES track this write so a later DIRECT
                // read `a[K]` folds (1062); but it then drops the pointer's
                // address so a later POINTER read `*p` reloads at runtime (1066).
                // The store itself stays pointer-routed (target unchanged).
                AssignTarget::DerefLocal(p) => record_deref_ptr_addr_write(*p, false, value, cp),
                AssignTarget::DerefLocalByte(p) => record_deref_ptr_addr_write(*p, true, value, cp),
                _ => {}
            }
            // A store THROUGH a pointer (not rewritten to a direct global/array
            // element) may hit any address-taken global, so known global values
            // become unknown — a later global read must reload. MSC reloads `g`
            // after `++(*p)` even when p = &g. Fixture 1302.
            if matches!(target,
                AssignTarget::DerefLocal(_) | AssignTarget::DerefLocalByte(_)
                | AssignTarget::DerefParam(_) | AssignTarget::DerefGlobal(_)
                | AssignTarget::DerefExpr { .. } | AssignTarget::PtrIndexByte { .. })
            {
                cp.g_known.clear();
                cp.ga_known.clear();
            }
            // A store through an unresolved `*arr[K]` (the alias was already
            // consumed — see the single-use rule above) routes through the
            // pointer and can hit any address-taken LOCAL, so drop local known
            // values too. MSC reloads both operands after the second `*arr[J]`
            // store in fixture 2470. Scoped to DerefExpr (the array-element form)
            // so scalar-pointer derefs keep their ptr_addr-tracked locals.
            if matches!(target, AssignTarget::DerefExpr { .. }) {
                cp.l_known.clear();
                cp.la_known.clear();
            }
            // A call in the assigned value clobbers ALL known values — handled
            // by the statement-boundary `kill_if_called` (which also marks the
            // killed entries mutated so the emit-time init fold view agrees).
        }
        Stmt::Empty => {}
        Stmt::If { cond, then_branch, else_branch } => {
            // Fold the cond using current knowledge, then propagate
            // into each branch with an isolated copy so writes don't
            // leak across paths. After the if, conservatively clear.
            cp.substituted = false;
            // Pre-substitution shape: a BARE truthy variable test (`if (x)`),
            // unlike a comparison (`if (x != 0)`), consumes its knowledge even
            // on a TRUE fold — the next test of x emits a runtime cmp
            // (fixture 2024 pins the asymmetry between the two forms).
            let bare_truthy_var: Option<(bool, usize)> = match cond {
                Cond::Truthy(Expr::Local(i)) => Some((false, *i)),
                Cond::Truthy(Expr::Global(g)) => Some((true, *g)),
                _ => None,
            };
            // Snapshot the cond's variable reads BEFORE prop_cond substitutes
            // them to literals — a false fold consumes these (see below) and the
            // substituted cond would otherwise carry no Local/Global to mark.
            let cond_before = cond.clone();
            prop_cond(cond, cp);
            if cp.substituted
                && matches!(crate::codegen::statements::fold_cond_raw(cond, &[]), Some(k) if k != 0)
                && let Some((is_global, idx)) = bare_truthy_var
            {
                cp.l_known.clear();
                cp.g_known.clear();
                cp.la_known.clear();
            cp.la_var_written.clear();
                cp.ga_known.clear();
                if is_global {
                    cp.mutated_globals.insert(idx);
                } else {
                    cp.mutated_locals.insert(idx);
                }
            }
            // A condition that became fully constant-FALSE through
            // substitution consumes the knowledge it used: the arm is elided
            // at emit time and MSC re-tests the same variable at runtime in
            // the surviving else-if (509: `x=2; if(x==1)... else if(x==2)...`
            // drops the first arm but emits `cmp [x],2; jne` for the second).
            // A TRUE fold keeps values — the taken then-arm continues folding
            // (1687, 2354, 1986) — and a literal cond substitutes nothing.
            if cp.substituted
                && crate::codegen::statements::fold_cond_raw(cond, &[]) == Some(0)
            {
                cp.l_known.clear();
                cp.g_known.clear();
                cp.la_known.clear();
            cp.la_var_written.clear();
                cp.ga_known.clear();
                // Init-seeded knowledge also lives in the EMIT-time fold view
                // (locals.inits) — mark the cond's own reads, plus the surviving
                // else-if chain's cond reads, as mutated so the emitter re-tests
                // them at runtime, mirroring the ternary-chain rule. `if (a>0)...
                // else if (a<0)` with `int a = 0;` elides the first arm but emits
                // a real `cmp [a],0` for the second (fixture 1201). For two
                // SEPARATE ifs on the same variable, the dropped-false first if
                // likewise consumes its var so the next sibling `if` re-tests at
                // runtime (fixture 2001: `if(x==500)...; if(x==1000)...` with
                // x=1000 drops the first but keeps the second's `cmp`).
                mark_cond_reads(&cond_before, cp);
                let mut chain: Option<&Stmt> = else_branch.as_deref();
                while let Some(Stmt::If { cond, else_branch, .. }) = chain {
                    mark_cond_reads(cond, cp);
                    chain = else_branch.as_deref();
                }
            }
            // A cond folded to a literal selects a live branch — only that
            // branch is walked (the dead one is elided at emit and its writes
            // never happen). Each walked branch propagates through an isolated
            // clone so value knowledge doesn't leak across paths, but its
            // MUTATION marks merge back: a (maybe-)taken write must strip the
            // emit-time fold view, else `if (x) a = 1; return a + b` folds the
            // return from a's stale decl init (fixture 2024).
            let live = crate::codegen::statements::fold_cond_raw(cond, &[]);
            let mut walk = |branch: &mut Stmt, cp: &mut ConstProp| {
                let mut sub = cp_clone(cp);
                prop_stmt(branch, &mut sub);
                cp.mutated_locals.extend(sub.mutated_locals.iter().copied());
                cp.mutated_globals.extend(sub.mutated_globals.iter().copied());
                cp.loop_mutated_locals.extend(sub.loop_mutated_locals.iter().copied());
            };
            if live.is_none_or(|k| k != 0) {
                walk(then_branch, cp);
            }
            if live.is_none_or(|k| k == 0)
                && let Some(eb) = else_branch
            {
                walk(eb, cp);
            }
            // After a branch we don't know which path was taken.
            cp.g_known.clear();
            cp.l_known.clear();
            cp.la_known.clear();
            cp.la_var_written.clear();
            cp.ptr_alias.clear();
            cp.func_addr.clear();
            cp.elem_ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
            cp.ga_known.clear();
        }
        Stmt::Block(stmts) => {
            for s in stmts {
                prop_stmt(s, cp);
            }
            // Exiting a nested block scope flushes MSC's known-value cache:
            // a read after the block reloads from its slot rather than folding
            // a value learned inside (or before) the block. Mirrors the loop /
            // if boundary clears. Fixtures 2258, 2316, 2467 (shadowed locals).
            cp.g_known.clear();
            cp.l_known.clear();
            cp.la_known.clear();
            cp.la_var_written.clear();
            cp.ptr_alias.clear();
            cp.func_addr.clear();
            cp.elem_ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
            cp.ga_known.clear();
        }
        Stmt::Switch { scrutinee, cases } => {
            prop_expr(scrutinee, cp);
            // A scrutinee like `x + 1` (x known) folds to a constant after
            // substitution but `prop_expr` leaves the BinOp shape intact;
            // collapse an all-literal scrutinee so the partial/foldable-switch
            // detection (which keys on `Expr::IntLit`) fires. Fixture 544.
            if let Some(k) = eval_const_int(scrutinee) {
                *scrutinee = Expr::IntLit(k);
            }
            // A long-local scrutinee with a known literal init folds to its
            // low word — MSC dispatches the switch on a plain int chain
            // (`mov ax,2` for `long x = 2L; switch (x)`). Long locals are
            // deliberately absent from l_known (reads stay slot loads), so
            // recover the value from the spec. Fixture 1913.
            if let Expr::Local(i) = scrutinee
                && let Some(spec) = cp.local_specs.get(*i)
                && spec.is_long
                && spec.init_is_literal
                && !cp.mutated_locals.contains(i)
                && let Some(k) = spec.init
            {
                *scrutinee = Expr::IntLit(k);
            }
            if let Expr::IntLit(k) = scrutinee
                && !crate::codegen::statements::switch_is_table(cases)
            {
                let k = *k;
                // Resolve the compare chain against K. Tests survive unless
                // they compare against exactly K (or the case-0 `or ax,ax`):
                // the switch only folds away entirely when NO tests survive
                // — i.e. everything before the matched arm resolved.
                let fold = crate::codegen::statements::fold_chain_ops(
                    crate::codegen::statements::build_chain_ops(cases),
                    k,
                );

                if !fold.ops.is_empty() {
                    // Surviving runtime tests: emit_function picks the partial
                    // (truncated) or plain chain layout.
                    cp.g_known.clear();
                    cp.l_known.clear();
                    cp.la_known.clear();
            cp.la_var_written.clear();
            cp.ptr_alias.clear();
            cp.func_addr.clear();
            cp.elem_ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
                    cp.ga_known.clear();
                } else {
                    // No NFC: fold to matched body block. MSC clears the
                    // const-prop tables BEFORE processing the body so that
                    // any inner switches inside the folded arm use runtime
                    // scrutinee loads rather than const-prop'd literals.
                    cp.g_known.clear();
                    cp.l_known.clear();
                    cp.la_known.clear();
            cp.la_var_written.clear();
            cp.ptr_alias.clear();
            cp.func_addr.clear();
            cp.elem_ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
                    cp.ga_known.clear();
                    let mut chosen: Option<usize> = None;
                    let mut default: Option<usize> = None;
                    for (i, arm) in cases.iter().enumerate() {
                        match arm.value {
                            Some(v) if v == k => { chosen = Some(i); break; }
                            Some(_) => {}
                            None => { default = Some(i); }
                        }
                    }
                    let pick = chosen.or(default);
                    let body: Vec<Stmt> = match pick {
                        Some(i) => {
                            let mut out = Vec::new();
                            let mut j = i;
                            'outer: while j < cases.len() {
                                for s in &cases[j].body {
                                    if matches!(s, Stmt::Break) { break 'outer; }
                                    out.push(s.clone());
                                }
                                j += 1;
                            }
                            out
                        }
                        None => Vec::new(),
                    };
                    *stmt = Stmt::Block(body);
                    if let Stmt::Block(stmts) = stmt {
                        for s in stmts { prop_stmt(s, cp); }
                    }
                    cp.g_known.clear();
                    cp.l_known.clear();
                    cp.la_known.clear();
            cp.la_var_written.clear();
            cp.ptr_alias.clear();
            cp.func_addr.clear();
            cp.elem_ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
                    cp.ga_known.clear();
                }
            } else {
                // Runtime scrutinee (not folded): leave as Switch.
                cp.g_known.clear();
                cp.l_known.clear();
                cp.la_known.clear();
            cp.la_var_written.clear();
            cp.ptr_alias.clear();
            cp.func_addr.clear();
            cp.elem_ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
                cp.ga_known.clear();
            }
        }
        Stmt::Break | Stmt::Continue => {
            // These are flow-control markers; the const-folded
            // switch walker handles Break. Outside that path they
            // signal that subsequent statements may be unreachable.
        }
        _ => {
            // While / for / do-while: fold any cond / step we can
            // reach via a shallow walk, then drop everything.
            // Locals mutated in the loop go to `loop_mutated_locals` — the
            // emit fold view drops them (a post-loop `return s + p` reads
            // the slots, fixture 3478) while the loop entry-fold view keeps
            // them (the while → do-while elision still folds the entry test
            // from the declared init).
            if matches!(stmt, Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. }) {
                for m in crate::codegen::statements::collect_loop_body_mutations(&[stmt]) {
                    cp.loop_mutated_locals.insert(m);
                }
            }
            cp.g_known.clear();
            cp.l_known.clear();
            cp.la_known.clear();
            cp.la_var_written.clear();
            cp.ptr_alias.clear();
            cp.func_addr.clear();
            cp.elem_ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
            cp.ga_known.clear();
        }
    }
}
pub(crate) fn cp_clone(cp: &ConstProp) -> ConstProp {
    ConstProp {
        g_known: cp.g_known.clone(),
        l_known: cp.l_known.clone(),
        la_known: cp.la_known.clone(),
        ga_known: cp.ga_known.clone(),
        long_globals: cp.long_globals.clone(),
        mutated_locals: cp.mutated_locals.clone(),
        mutated_globals: cp.mutated_globals.clone(),
        loop_mutated_locals: cp.loop_mutated_locals.clone(),
        ptr_alias: cp.ptr_alias.clone(),
        elem_ptr_alias: cp.elem_ptr_alias.clone(),
        ptr_alias_g: cp.ptr_alias_g.clone(),
        aliases_used: cp.aliases_used.clone(),
        ptr_addr: cp.ptr_addr.clone(),
        local_specs: cp.local_specs.clone(),
        global_elem_sizes: cp.global_elem_sizes.clone(),
        global_array_lens: cp.global_array_lens.clone(),
        union_locals: cp.union_locals.clone(),
        union_globals: cp.union_globals.clone(),
        func_addr: cp.func_addr.clone(),
        structs: cp.structs.clone(),
        global_struct_idxs: cp.global_struct_idxs.clone(),
        la_field_size: cp.la_field_size.clone(),
        la_var_written: cp.la_var_written.clone(),
        ga_field_size: cp.ga_field_size.clone(),
        in_cond: cp.in_cond,
        saw_call: cp.saw_call,
        substituted: cp.substituted,
        // Transient within a single prop_expr recursion; never carried across
        // the branch snapshots cp_clone serves.
        suppress_subst: false,
        in_parked_call: false,
        in_arith: false,
    }
}
/// If `e` is a pointer local holding `&x`/`&g` (offset 0), the address
/// expression `AddrOfLocal(x)`/`AddrOfGlobal(g)` — used to lower `if (p)` /
/// `if (p == 0)` to `cmp bp,K` / `mov ax,&g; or ax,ax`.
fn ptr_local_addr(e: &Expr, cp: &ConstProp) -> Option<Expr> {
    if let Expr::Local(p) = e
        && let Some(&(base, 0)) = cp.ptr_addr.get(p)
    {
        match base {
            AliasTarget::Local(x) => Some(Expr::AddrOfLocal(x)),
            AliasTarget::Global(g) => Some(Expr::AddrOfGlobal(g)),
            AliasTarget::String(_) => None,
        }
    } else {
        None
    }
}
/// Mark every Local/Global read in a condition as mutated, so the EMIT-time
/// fold view (locals.inits) misses them and the emitter re-tests at runtime.
/// Companion of the knowledge-consume rules (FALSE folds through
/// substitution). Fixtures 1201 (else-if chain), 1358 (`a&&b || c`).
fn mark_cond_reads(c: &Cond, cp: &mut ConstProp) {
    fn mark_expr(e: &Expr, cp: &mut ConstProp) {
        match e {
            Expr::Local(i) => { cp.mutated_locals.insert(*i); }
            Expr::Global(g) => { cp.mutated_globals.insert(*g); }
            Expr::BinOp { left, right, .. } => {
                mark_expr(left, cp);
                mark_expr(right, cp);
            }
            Expr::CastChar { value, .. } | Expr::CastLong { value, .. } => mark_expr(value, cp),
            _ => {}
        }
    }
    match c {
        Cond::Truthy(e) => mark_expr(e, cp),
        Cond::Cmp { left, right, .. } => {
            mark_expr(left, cp);
            mark_expr(right, cp);
        }
        Cond::And(a, b) | Cond::Or(a, b) => {
            mark_cond_reads(a, cp);
            mark_cond_reads(b, cp);
        }
    }
}
pub(crate) fn prop_cond(cond: &mut Cond, cp: &mut ConstProp) {
    let saved_in_cond = cp.in_cond;
    cp.in_cond = true;
    prop_cond_inner(cond, cp);
    cp.in_cond = saved_in_cond;
}
fn prop_cond_inner(cond: &mut Cond, cp: &mut ConstProp) {
    match cond {
        Cond::Truthy(e) => {
            if let Some(addr) = ptr_local_addr(e, cp) {
                *e = addr;
                return;
            }
            prop_expr(e, cp)
        }
        Cond::Cmp { op, left, right } => {
            // `p == q` / `p != q` over two pointer locals each aliasing a string
            // literal: materialize each string's address and compare. Distinct
            // literals get distinct CONST offsets, so the compare stays runtime
            // (`mov ax,OFFSET sp; cmp ax,OFFSET sq`). Fixture 1431.
            if matches!(op, RelOp::Eq | RelOp::Ne)
                && let (Expr::Local(lp), Expr::Local(rp)) = (&*left, &*right)
                && let (Some(AliasTarget::String(li)), Some(AliasTarget::String(ri)))
                    = (cp.ptr_alias.get(lp).copied(), cp.ptr_alias.get(rp).copied())
            {
                *left = Expr::StrLit(li);
                *right = Expr::StrLit(ri);
                return;
            }
            // `p == q` / `p != q` over two same-GLOBAL-base address values folds
            // to a constant condition (fixture 601). Local-base stays runtime.
            if matches!(op, RelOp::Eq | RelOp::Ne)
                && let (Expr::Local(lp), Expr::Local(rp)) = (&*left, &*right)
                && let (Some(&(lb, lo)), Some(&(rb, ro))) = (cp.ptr_addr.get(lp), cp.ptr_addr.get(rp))
                && let (AliasTarget::Global(lg), AliasTarget::Global(rg)) = (lb, rb)
                && lg == rg
            {
                let eq = lo == ro;
                let truthy = if matches!(op, RelOp::Eq) { eq } else { !eq };
                *cond = Cond::Truthy(Expr::IntLit(truthy as i32));
                return;
            }
            // `p == 0` / `p != 0` (p a pointer holding `&x`/`&g`): rewrite the
            // pointer to its address so emit lowers it (cmp bp,K / or ax,ax).
            if matches!(op, RelOp::Eq | RelOp::Ne) {
                if matches!(right, Expr::IntLit(0)) && let Some(addr) = ptr_local_addr(left, cp) {
                    *left = addr;
                    return;
                }
                if matches!(left, Expr::IntLit(0)) && let Some(addr) = ptr_local_addr(right, cp) {
                    *right = addr;
                    return;
                }
            }
            // `if (p == q)` / `if (p < q)` over two pointer locals holding
            // `&x`/`&g` (offset 0) whose bases aren't a foldable same-global
            // pair: substitute each with its address so emit materializes both
            // and compares them (unsigned). Distinct globals have a
            // link-time-unknown relative order, so the relational compare stays
            // runtime. Fixtures 1235 (==, locals), 3944-3947 (==, globals),
            // 3938-3943 (relational, globals).
            if let (Some(la), Some(ra)) = (ptr_local_addr(left, cp), ptr_local_addr(right, cp)) {
                *left = la;
                *right = ra;
                return;
            }
            // `<int-word local> OP <call>` — MSC evaluates the call first (into
            // AX) and compares against the var's SLOT, NOT a folded immediate.
            // Suppress the var's const-fold (mark it mutated so the emit view
            // misses it) and rewrite so the call becomes the LEFT (first-
            // evaluated) operand, swapping the relop. The emitter's `call OP mem`
            // arm then produces `cmp ax,[var]`. Fixture 2044 (`if (x > f())`).
            let int_word_local = |e: &Expr, cp: &ConstProp| matches!(e,
                Expr::Local(i) if cp.local_specs.get(*i)
                    .is_some_and(|s| s.size == 2 && !s.is_long && !s.is_float));
            if matches!(right, Expr::Call { .. }) && int_word_local(left, cp) {
                if let Expr::Local(i) = left { cp.mutated_locals.insert(*i); }
                prop_expr(right, cp);
                *op = crate::codegen::statements::swap_relop(*op);
                std::mem::swap(left, right);
                return;
            }
            if matches!(left, Expr::Call { .. }) && int_word_local(right, cp) {
                if let Expr::Local(i) = right { cp.mutated_locals.insert(*i); }
                prop_expr(left, cp);
                return;
            }
            prop_expr(left, cp);
            prop_expr(right, cp);
        }
        Cond::And(a, b) => {
            prop_cond(a, cp);
            // Left side of `&&` folded TRUE through substitution: the AND
            // reduces to its right side (a TRUE fold keeps knowledge —
            // mirroring the if/ternary rules). `if (a && ++b)` with a=1
            // emits just the `++b` test (fixture 1238). But when the right
            // side ALSO folds (the whole group resolves), keep the And node:
            // an enclosing `||` keys its knowledge-consume on a failed `&&`
            // GROUP (1358/1862).
            if cp.substituted
                && matches!(crate::codegen::statements::fold_cond_raw(a, &[]), Some(k) if k != 0)
            {
                let mut survivor = (**b).clone();
                prop_cond(&mut survivor, cp);
                if crate::codegen::statements::fold_cond_raw(&survivor, &[]).is_some() {
                    **b = survivor;
                } else {
                    *cond = survivor;
                }
                return;
            }
            prop_cond(b, cp);
        }
        Cond::Or(a, b) => {
            prop_cond(a, cp);
            // Left side of `||` folded FALSE through substitution: the OR
            // reduces to its right side. When the dropped side was an `&&`
            // GROUP, the failed group CONSUMES the knowledge it used — the
            // surviving side re-tests at runtime (`if (a && b || c)` with
            // a=1,b=0,c=2 emits a real `cmp [c],0`; fixtures 1358/1862). A
            // bare/cmp/or left keeps knowledge, so the survivor goes on
            // folding (572/621/1176/2615) — only the structural drop applies
            // (`if (a || ++b)` emits just the `++b` test, fixture 1237).
            if cp.substituted
                && crate::codegen::statements::fold_cond_raw(a, &[]) == Some(0)
            {
                if matches!(**a, Cond::And(..)) {
                    cp.l_known.clear();
                    cp.g_known.clear();
                    cp.la_known.clear();
            cp.la_var_written.clear();
                    cp.ga_known.clear();
                    mark_cond_reads(b, cp);
                }
                let mut survivor = (**b).clone();
                prop_cond(&mut survivor, cp);
                *cond = survivor;
                return;
            }
            prop_cond(b, cp);
        }
    }
}
/// Evaluate an expression made entirely of integer literals (after const-prop
/// has substituted known locals/globals) to a single 16-bit constant. Returns
/// `None` for anything not statically computable. Used to fold a switch
/// scrutinee like `x + 1` (x known) so the partial/foldable-switch path fires
/// — MSC loads the folded value (`mov ax,2`) and truncates the case chain.
pub(crate) fn eval_const_int(e: &Expr) -> Option<i32> {
    match e {
        Expr::IntLit(k) => Some(*k),
        Expr::CastChar { value, .. } | Expr::CastLong { value, .. } => eval_const_int(value),
        Expr::BinOp { op, left, right } => {
            let a = eval_const_int(left)? as i16;
            let b = eval_const_int(right)? as i16;
            let r: i16 = match op {
                BinOp::Add => a.wrapping_add(b),
                BinOp::Sub => a.wrapping_sub(b),
                BinOp::Mul => a.wrapping_mul(b),
                BinOp::Div => if b == 0 { return None } else { a.wrapping_div(b) },
                BinOp::Mod => if b == 0 { return None } else { a.wrapping_rem(b) },
                BinOp::Shl => a.wrapping_shl(b as u32 & 0xF),
                BinOp::Shr => a.wrapping_shr(b as u32 & 0xF),
                BinOp::BitAnd => a & b,
                BinOp::BitOr => a | b,
                BinOp::BitXor => a ^ b,
                BinOp::Eq => (a == b) as i16,
                BinOp::Ne => (a != b) as i16,
                BinOp::Lt => (a < b) as i16,
                BinOp::Gt => (a > b) as i16,
                BinOp::Le => (a <= b) as i16,
                BinOp::Ge => (a >= b) as i16,
                BinOp::LogAnd => ((a != 0) && (b != 0)) as i16,
                BinOp::LogOr => ((a != 0) || (b != 0)) as i16,
            };
            Some(r as i32)
        }
        _ => None,
    }
}

/// MSC's optimizer drops all memory value knowledge at the END of any
/// statement containing a call: the callee may write any global, and it is
/// conservative about locals too — gold reloads pre-call constants in the
/// NEXT statement (1851: `return a+c+r` loads a and c from their slots
/// after the `r = helper(b)` statement, even though both were known
/// literals). Within the call's own statement knowledge survives — 1981's
/// `return sqr(a) + b*c` folds b*c to `add ax,20` AFTER the call returns —
/// so the kill fires at the statement boundary, not at the call site.
/// Killed locals are also marked mutated so the emit-time init-based fold
/// view doesn't resurrect them.
fn kill_if_called(cp: &mut ConstProp) {
    if !cp.saw_call {
        return;
    }
    cp.saw_call = false;
    cp.mutated_locals.extend(cp.l_known.keys().copied());
    cp.mutated_locals.extend(cp.la_known.keys().map(|&(l, _)| l));
    cp.mutated_globals.extend(cp.g_known.keys().copied());
    cp.mutated_globals.extend(cp.ga_known.keys().map(|&(g, _)| g));
    cp.l_known.clear();
    cp.la_known.clear();
            cp.la_var_written.clear();
    cp.g_known.clear();
    cp.ga_known.clear();
}

/// Flatten a left-/right-nested `+` chain into its leaf operands (in order).
/// Non-Add subexpressions are leaves (their own internal structure is kept).
fn flatten_add(e: &Expr, out: &mut Vec<Expr>) {
    if let Expr::BinOp { op: BinOp::Add, left, right } = e {
        flatten_add(left, out);
        flatten_add(right, out);
    } else {
        out.push(e.clone());
    }
}
/// Strip the statically-known value any assignment inside a runtime ternary
/// arm would have recorded. A runtime ternary executes only ONE of its arms,
/// so neither arm's stores yield a value the compiler may substitute into a
/// later read — that read must reload from the slot. The mutation-marking walk
/// above (`prop_expr` on the discarded arm clones) ALSO records the assigned
/// value into `l_known`/`g_known`, which is wrong here, so clear those back
/// out. The `mutated_locals`/`mutated_globals` marks stay, so the emit-time
/// fold view still reloads. Fixture 2476 (`r = c ? (a=5,10) : (b=7,20)` must
/// not fold the untaken arm's `b=7` into a later `r+a+b`).
fn clear_arm_assigned_values(e: &Expr, cp: &mut ConstProp) {
    fn clear_target(t: &AssignTarget, cp: &mut ConstProp) {
        match t {
            AssignTarget::Local(l) => { cp.l_known.remove(l); }
            AssignTarget::Global(g) => { cp.g_known.remove(g); }
            _ => {}
        }
    }
    match e {
        Expr::Seq { sides, value } => {
            for s in sides {
                if let Stmt::Assign { target, value: v } = s {
                    clear_target(target, cp);
                    clear_arm_assigned_values(v, cp);
                }
            }
            clear_arm_assigned_values(value, cp);
        }
        Expr::AssignExpr { target, value } => {
            clear_target(target, cp);
            clear_arm_assigned_values(value, cp);
        }
        Expr::BinOp { left, right, .. } => {
            clear_arm_assigned_values(left, cp);
            clear_arm_assigned_values(right, cp);
        }
        Expr::Ternary { cond, then_arm, else_arm } => {
            clear_arm_assigned_values(cond, cp);
            clear_arm_assigned_values(then_arm, cp);
            clear_arm_assigned_values(else_arm, cp);
        }
        _ => {}
    }
}
/// Nesting depth of the member at `byte_off` within struct `sidx`: 1 for a
/// direct top-level field, 2+ when it falls inside a (non-pointer) struct-typed
/// field, recursing for deeper nesting. Used to order a struct-field sum.
fn field_depth(sidx: usize, byte_off: u16, structs: &[StructDef]) -> usize {
    let Some(s) = structs.get(sidx) else { return 1 };
    for f in &s.fields {
        if byte_off >= f.byte_off && byte_off < f.byte_off + f.size as u16 {
            if let Some(inner) = f.struct_idx
                && !f.is_pointer
            {
                return 1 + field_depth(inner, byte_off - f.byte_off, structs);
            }
            return 1;
        }
    }
    1
}
/// MSC orders a sum of struct-member reads over ONE base by nesting depth —
/// deeper-nested members first, source order preserved within a depth (a stable
/// partition). `o.id + o.inner.a + o.inner.b + o.tail` (offsets 0,2,4,6) loads
/// 2,4,0,6 (oracle-confirmed N1/N2/N3). Flatten the `+` chain, require every
/// operand to be a word GlobalField/LocalField of the same struct base, sort
/// stably by descending depth, and rebuild the left-assoc chain in place.
/// Returns true iff the order actually changed (so the caller recurses once).
/// Fixtures 2102, and the 3342/2313 cluster. Addition is associative for ints,
/// so reordering is value-preserving.
fn reorder_struct_field_sum(e: &mut Expr, cp: &ConstProp) -> bool {
    // Flatten a left-assoc `+` chain into its leaf operands.
    fn flatten<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) -> bool {
        if let Expr::BinOp { op: BinOp::Add, left, right } = e {
            flatten(left, out) && { out.push(right); true }
        } else {
            out.push(e);
            true
        }
    }
    let mut ops: Vec<&Expr> = Vec::new();
    flatten(e, &mut ops);
    if ops.len() < 2 {
        return false;
    }
    // Resolve each operand to (base_kind, base_idx, byte_off) and require all to
    // be word fields of the SAME struct base.
    let mut base: Option<(bool, usize, usize)> = None; // (is_global, base_idx, struct_idx)
    let mut depths: Vec<usize> = Vec::with_capacity(ops.len());
    for op in &ops {
        let (is_global, base_idx, byte_off) = match op {
            Expr::GlobalField { global, byte_off, size: 2 } => (true, *global, *byte_off),
            Expr::LocalField { local, byte_off, size: 2 } => (false, *local, *byte_off),
            _ => return false,
        };
        let sidx = if is_global {
            cp.global_struct_idxs.get(base_idx).copied().flatten()
        } else {
            cp.local_specs.get(base_idx).and_then(|s| s.struct_idx)
        };
        let Some(sidx) = sidx else { return false };
        match base {
            None => base = Some((is_global, base_idx, sidx)),
            Some((g, b, _)) if g == is_global && b == base_idx => {}
            _ => return false,
        }
        depths.push(field_depth(sidx, byte_off, &cp.structs));
    }
    // Stable sort by descending depth. If already in that order, nothing to do.
    let mut order: Vec<usize> = (0..ops.len()).collect();
    order.sort_by(|&i, &j| depths[j].cmp(&depths[i]).then(i.cmp(&j)));
    if order.iter().enumerate().all(|(pos, &i)| pos == i) {
        return false;
    }
    // Rebuild the left-assoc chain in the new order.
    let reordered: Vec<Expr> = order.iter().map(|&i| ops[i].clone()).collect();
    let mut iter = reordered.into_iter();
    let mut acc = iter.next().unwrap();
    for nxt in iter {
        acc = Expr::BinOp { op: BinOp::Add, left: Box::new(acc), right: Box::new(nxt) };
    }
    *e = acc;
    true
}
/// In a left-assoc `+` chain, MSC evaluates all MULTIPLICATIVE terms (`x * y`,
/// whether lowered to `imul` or a shift chain) before the simple/additive terms,
/// stable within each group: `a*100 + b + i*10 + j` emits as `a*100 + i*10 + b
/// + j` (fixture 1818). Only fires when the chain has both a product and a
/// non-product term and every operand is side-effect-free (the reorder must
/// preserve evaluation semantics).
fn reorder_mul_first(e: &mut Expr) -> bool {
    fn flatten<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
        if let Expr::BinOp { op: BinOp::Add, left, right } = e {
            flatten(left, out);
            out.push(right);
        } else {
            out.push(e);
        }
    }
    let mut ops: Vec<&Expr> = Vec::new();
    flatten(e, &mut ops);
    if ops.len() < 2 {
        return false;
    }
    let is_mul = |x: &Expr| matches!(x, Expr::BinOp { op: BinOp::Mul, .. });
    let n_mul = ops.iter().filter(|o| is_mul(o)).count();
    if n_mul == 0 || n_mul == ops.len() {
        return false; // need a genuine product/simple mix
    }
    if !ops.iter().all(|o| crate::codegen::statements::expr_is_pure(o)) {
        return false;
    }
    // Stable partition: product terms first, source order preserved within each.
    let mut order: Vec<usize> = (0..ops.len()).collect();
    order.sort_by_key(|&i| !is_mul(ops[i]));
    if order.iter().enumerate().all(|(pos, &i)| pos == i) {
        return false; // already products-first
    }
    let reordered: Vec<Expr> = order.iter().map(|&i| ops[i].clone()).collect();
    let mut it = reordered.into_iter();
    let mut acc = it.next().unwrap();
    for nxt in it {
        acc = Expr::BinOp { op: BinOp::Add, left: Box::new(acc), right: Box::new(nxt) };
    }
    *e = acc;
    true
}
pub(crate) fn prop_expr(e: &mut Expr, cp: &mut ConstProp) {
    // Reorder a struct-field `+` chain (nested members first) before the rest of
    // const-prop processes it; idempotent, so sub-chain recursion is a no-op.
    if matches!(e, Expr::BinOp { op: BinOp::Add, .. }) {
        reorder_struct_field_sum(e, cp);
    }
    match e {
        Expr::FloatLit(..) => {} // no int const-prop into float literals
        Expr::CastChar { value, .. } | Expr::CastLong { value, .. } => prop_expr(value, cp),
        Expr::AssignExpr { target, value } => {
            // Substitute into the RHS so the cond can fold, then invalidate the
            // target's known value: MSC reloads/reuses-AX for later reads rather
            // than re-materializing the immediate (the store leaves AX set, and
            // the return/use peepholes reuse it). Mirrors the ternary-assign rule.
            prop_expr(value, cp);
            // Fold a runtime index in an `(a[i] = e)` target: substitute `i`,
            // and if it becomes constant, rewrite to the const-index store form
            // so codegen emits a direct `mov [bp+d+off],ax`. Fixture 1986.
            match target {
                AssignTarget::IndexedLocalVar { local, index } => {
                    prop_expr(index, cp);
                    if let Expr::IntLit(k) = index.as_ref() {
                        *target = AssignTarget::IndexedLocal { local: *local, byte_off: (*k * 2) as u16 };
                    }
                }
                AssignTarget::IndexedLocalByteVar { local, index } => {
                    prop_expr(index, cp);
                    if let Expr::IntLit(k) = index.as_ref() {
                        *target = AssignTarget::IndexedLocalByte { local: *local, byte_off: *k as u16 };
                    }
                }
                AssignTarget::IndexedGlobalVar { array, index } => {
                    prop_expr(index, cp);
                    if let Expr::IntLit(k) = index.as_ref() {
                        *target = AssignTarget::IndexedGlobal { array: *array, byte_off: (*k * 2) as u16 };
                    }
                }
                AssignTarget::IndexedGlobalByteVar { array, index } => {
                    prop_expr(index, cp);
                    if let Expr::IntLit(k) = index.as_ref() {
                        *target = AssignTarget::IndexedGlobalByte { array: *array, byte_off: *k as u16 };
                    }
                }
                _ => {}
            }
            // A chained assignment `a = b = c = 7` carries the constant to every
            // target, so a later read folds to the immediate (fixture 1817). A
            // non-constant assigned value still invalidates — MSC reloads/reuses
            // AX for those (the ternary-assign rule).
            let lit = if cp.in_cond { None }
                else if let Expr::IntLit(k) = value.as_ref() { Some(*k) } else { None };
            match target {
                AssignTarget::Local(l) => {
                    cp.mutated_locals.insert(*l);
                    match lit {
                        Some(k) if !cp.local_specs.get(*l).is_some_and(|s| s.is_register) => { cp.l_known.insert(*l, k); }
                        _ => { cp.l_known.remove(l); }
                    }
                }
                AssignTarget::Global(g) => {
                    cp.mutated_globals.insert(*g);
                    match lit { Some(k) => { cp.g_known.insert(*g, k); } None => { cp.g_known.remove(g); } }
                }
                _ => {}
            }
        }
        Expr::Global(idx) => {
            // Long globals are never substituted — their compound
            // updates need `Global(g)` on the lhs for the long-specific
            // assign-codegen path to fire (fixture 207).
            if !cp.suppress_subst
                && !cp.long_globals.contains(idx)
                && let Some(&k) = cp.g_known.get(idx)
            {
                *e = Expr::IntLit(k);
                cp.substituted = true;
            }
        }
        Expr::Local(idx) => {
            // A register local substitutes its known constant only inside an
            // arithmetic expression (`cp.in_arith`); a bare top-level read keeps
            // the register (MSC emits `mov ax,si`). Non-register locals always
            // substitute. Fixture 1763.
            let reg = cp.local_specs.get(*idx).is_some_and(|s| s.is_register);
            if !cp.suppress_subst
                && (!reg || cp.in_arith)
                && let Some(&k) = cp.l_known.get(idx)
            {
                *e = Expr::IntLit(k);
                cp.substituted = true;
            }
        }
        Expr::BinOp { op: op @ (BinOp::LogOr | BinOp::LogAnd), left, .. } => {
            // MSC does NOT substitute constant locals inside || / && operands:
            // `return x || y` with x=1 always emits `cmp [bp-x], 0`, not
            // `cmp 1, 0`. The fold() path (for if-condition dead-branch
            // elimination) still works because fold() reads l_known directly.
            //
            // But a SOURCE-literal left short-circuits at compile time:
            // `0 && <e>` is 0 and `K || <e>` (K != 0) is 1 without ever
            // evaluating the right side — its side effects (calls, ++) are
            // dropped. Fixture 2311.
            if matches!(op, BinOp::LogAnd) && matches!(left.as_ref(), Expr::IntLit(0)) {
                *e = Expr::IntLit(0);
            } else if matches!(op, BinOp::LogOr)
                && matches!(left.as_ref(), Expr::IntLit(k) if *k != 0)
            {
                *e = Expr::IntLit(1);
            }
        }
        Expr::BinOp { op, left, right } => {
            // Pointer subtraction / equality over two same-GLOBAL-base address
            // values folds to a compile-time constant (`&g[7] - &g[2]` → 5,
            // `p == q` (same addr) → 1). Local-base addresses stay runtime.
            if let (Expr::Local(lp), Expr::Local(rp)) = (left.as_ref(), right.as_ref())
                && let (Some(&(lb, lo)), Some(&(rb, ro))) = (cp.ptr_addr.get(lp), cp.ptr_addr.get(rp))
                && let (AliasTarget::Global(lg), AliasTarget::Global(rg)) = (lb, rb)
                && lg == rg
            {
                let elem = cp.local_specs.get(*lp).map(|s| s.pointee_size.max(1)).unwrap_or(1) as i32;
                match op {
                    BinOp::Sub => { *e = Expr::IntLit((lo - ro) / elem); return; }
                    BinOp::Eq => { *e = Expr::IntLit((lo == ro) as i32); return; }
                    BinOp::Ne => { *e = Expr::IntLit((lo != ro) as i32); return; }
                    _ => {}
                }
            }
            // `p == q` / `p != q` over two pointer locals holding `&x`/`&g`
            // (offset 0) whose bases are NOT a foldable same-global pair:
            // substitute each with its address expression so codegen compares
            // the materialized addresses (`lea ax,[q]; lea cx,[p]; cmp cx,ax`).
            // MSC keeps the runtime compare even when the bases are provably
            // distinct. Fixture 1235.
            if matches!(op, BinOp::Eq | BinOp::Ne)
                && let (Some(la), Some(ra)) =
                    (ptr_local_addr(left.as_ref(), cp), ptr_local_addr(right.as_ref(), cp))
            {
                **left = la;
                **right = ra;
                return;
            }
            // Decayed-array pointer arith `a + i` where the index `i` was a
            // VARIABLE at parse (so the parser's `a + <literal>` element-scaling
            // did NOT fire). If const-prop now folds `i` to a constant, scale it
            // by the array element size — otherwise the byte offset is computed in
            // ELEMENTS (fixture 1278: `a + i` (i=1) → bp-6 not bp-7). Literal
            // indices are already byte-scaled at parse and reach here as IntLit,
            // so `right_was_var` keeps them from being scaled twice.
            let right_was_var = !matches!(right.as_ref(), Expr::IntLit(_));
            // MSC evaluates a pointer-arrow field (`pp->y`, a DerefLocalField)
            // BEFORE a simple direct operand — even in a commutative add where
            // both ultimately resolve to direct memory. Once the alias rewrite
            // below flattens `pp->y` to a `GlobalField`, that ordering is lost,
            // so swap the operands now (commutative op, right was the arrow, left
            // is a simple direct operand) → the arrow-origin operand loads first.
            // Fixture 2313 (`p.x + pp->y` → `mov ax,[p+2]; add ax,[p]`).
            let swap_deref_first = matches!(op, BinOp::Add | BinOp::Mul
                    | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
                && matches!(right.as_ref(), Expr::DerefLocalField { ptr_local, .. }
                    if cp.ptr_alias.contains_key(ptr_local))
                && matches!(left.as_ref(), Expr::GlobalField { .. } | Expr::LocalField { .. }
                    | Expr::Global(_) | Expr::Local(_) | Expr::Param(_));
            if swap_deref_first {
                std::mem::swap(left, right);
            }
            // Multi-call add/sub chain: the rightmost call (this binop's `right`,
            // emitted into AX before the left calls park in SI/DI) loads its leaf
            // arguments from MEMORY, not const-propagated immediates. Only the
            // OUTERMOST chain arms the suppression — the left sub-chain's parked
            // calls keep const args (`in_parked_call` blocks re-arming).
            let chain_top = matches!(op, BinOp::Add | BinOp::Sub)
                && !cp.in_parked_call
                && cp_is_call(right)
                && cp_left_call_chain(left);
            let saved_parked = cp.in_parked_call;
            if chain_top { cp.in_parked_call = true; }
            // A register local read is substituted with its constant only when
            // nested inside an arithmetic expression (`a + n`, `n + 1`). A bare
            // top-level read (a `return n` or a copy `a = n` RHS) keeps the
            // register so codegen emits `mov ax,si` / `mov [a],si`. Fixture 1763.
            let saved_arith = cp.in_arith;
            cp.in_arith = true;
            prop_expr(left, cp);
            cp.in_parked_call = saved_parked;
            let saved_suppress = cp.suppress_subst;
            if chain_top { cp.suppress_subst = true; }
            prop_expr(right, cp);
            cp.suppress_subst = saved_suppress;
            cp.in_arith = saved_arith;
            // Algebraic identities on bit ops with a literal 0 operand:
            // `x & 0 → 0` (x dropped, so it must be side-effect-free) and
            // `x | 0 → x` / `0 | x → x`. MSC folds these away — e.g. a
            // `f.a = (f.a & 0) | (v & 0xF)` bitfield clear-then-set collapses
            // to `f.a = v & 0xF`. Fixture 3562.
            match op {
                BinOp::BitAnd => {
                    let lz = matches!(left.as_ref(), Expr::IntLit(0));
                    let rz = matches!(right.as_ref(), Expr::IntLit(0));
                    if (lz && crate::codegen::statements::expr_is_pure(right))
                        || (rz && crate::codegen::statements::expr_is_pure(left))
                    {
                        *e = Expr::IntLit(0);
                        return;
                    }
                }
                BinOp::BitOr => {
                    if matches!(left.as_ref(), Expr::IntLit(0)) { *e = (**right).clone(); return; }
                    if matches!(right.as_ref(), Expr::IntLit(0)) { *e = (**left).clone(); return; }
                }
                _ => {}
            }
            if matches!(op, BinOp::Add | BinOp::Sub)
                && right_was_var
                && let Expr::IntLit(k) = right.as_ref()
                && let Expr::AddrOfLocal(li) = left.as_ref()
                && let Some(s) = cp.local_specs.get(*li)
                && s.array_len > 1 && s.pointee_size == 0
            {
                let off = if matches!(op, BinOp::Sub) { -(*k * s.size as i32) } else { *k * s.size as i32 };
                *e = Expr::BinOp { op: BinOp::Add, left: left.clone(), right: Box::new(Expr::IntLit(off)) };
                return;
            }
            // `e - e` → 0 for identical, side-effect-free operands (MSC folds the
            // algebraic identity; fixture 1779 `v - v`).
            if matches!(op, BinOp::Sub)
                && matches!((left.as_ref(), right.as_ref()),
                    (Expr::Local(a), Expr::Local(b)) if a == b)
                    | matches!((left.as_ref(), right.as_ref()),
                        (Expr::Global(a), Expr::Global(b)) if a == b)
                    | matches!((left.as_ref(), right.as_ref()),
                        (Expr::Param(a), Expr::Param(b)) if a == b)
            {
                *e = Expr::IntLit(0);
                return;
            }
            // Identity simplifications MSC drops (the no-op operation vanishes,
            // leaving just the surviving operand):
            //   x+0, x-0, x<<0, x>>0, x*1, x/1  and  0+x, 1*x.
            // Fixtures 3370 (x+0), 3463 (x*1), 3574 (x/1), 3556 (x<<0).
            if let Expr::IntLit(k) = right.as_ref() {
                let drop = match op {
                    BinOp::Add | BinOp::Sub | BinOp::Shl | BinOp::Shr => *k == 0,
                    BinOp::Mul | BinOp::Div => *k == 1,
                    _ => false,
                };
                if drop {
                    *e = (**left).clone();
                    return;
                }
            }
            if let Expr::IntLit(k) = left.as_ref() {
                // Commutative identities only (`0 + x`, `1 * x`).
                let drop = match op {
                    BinOp::Add => *k == 0,
                    BinOp::Mul => *k == 1,
                    _ => false,
                };
                if drop {
                    *e = (**right).clone();
                    return;
                }
            }
            // Normalize an ADD chain: keep non-constant operands in SOURCE order,
            // move every constant addend to the END (summed into one immediate).
            // MSC emits the memory operands first then a single trailing `add
            // ax,K`. Constants carry no side effects so moving them past the
            // non-constants preserves evaluation order. Fixtures 1850 (combine
            // trailing 2+4→6), 1811/1979 (a leading/interleaved const moves last).
            if matches!(op, BinOp::Add) {
                let mut terms = Vec::new();
                flatten_add(e, &mut terms);
                let mut const_sum = 0i32;
                let mut non_consts: Vec<Expr> = Vec::new();
                let mut const_count = 0u32;
                let mut needs_reorder = false;
                for t in &terms {
                    if let Expr::IntLit(k) = t {
                        const_sum += *k;
                        const_count += 1;
                    } else {
                        if const_count > 0 { needs_reorder = true; } // a non-const after a const
                        non_consts.push(t.clone());
                    }
                }
                // Rebuild only when something actually changes (a const precedes a
                // non-const, or two constants merge) and there is at least one
                // non-constant to anchor the chain.
                if (needs_reorder || const_count >= 2) && !non_consts.is_empty() {
                    let mut iter = non_consts.into_iter();
                    let mut acc = iter.next().expect("non_consts non-empty");
                    for t in iter {
                        acc = Expr::BinOp { op: BinOp::Add, left: Box::new(acc), right: Box::new(t) };
                    }
                    if const_sum != 0 {
                        acc = Expr::BinOp { op: BinOp::Add, left: Box::new(acc), right: Box::new(Expr::IntLit(const_sum)) };
                    }
                    *e = acc;
                    return;
                }
            }
        }
        Expr::Call { args, .. } | Expr::CallStructField { args, .. } => {
            for a in args {
                prop_expr(a, cp);
                // A string-pointer arg `s` (`char *s = "hi"`) is pushed as its
                // constant CONST address (`mov ax,OFFSET str; push ax`), not
                // loaded from the slot. Rewrite to StrLit so the arg emits the
                // immediate. Fixture 3976.
                if let Expr::Local(s) = a
                    && let Some(&AliasTarget::String(idx)) = cp.ptr_alias.get(s)
                {
                    *a = Expr::StrLit(idx);
                }
                // A local/global assigned INSIDE a call argument (`f(n = 7)`) is a
                // sequence point: a later read in the enclosing expression reloads
                // from the slot rather than re-materializing the immediate. Drop
                // the known value (the AssignExpr already marked it mutated).
                // Fixture 1816.
                if let Expr::AssignExpr { target, .. } = a {
                    match target {
                        AssignTarget::Local(l) => { cp.l_known.remove(l); }
                        AssignTarget::Global(g) => { cp.g_known.remove(g); }
                        _ => {}
                    }
                }
            }
            cp.saw_call = true;
        }
        Expr::CallPtr { args, .. } => {
            // Target is a fnptr lvalue (Global/Param/Local) — leave it; just
            // propagate into the arguments.
            for a in args {
                prop_expr(a, cp);
            }
            cp.saw_call = true;
        }
        Expr::FuncAddr(_) => {
            // A relocatable address; nothing to const-fold.
        }
        Expr::Index { .. } | Expr::IndexByte { .. } => {
            // Substitute index first, then try to fold to a known global array element.
            let (array, elem_size, index_ref) = match e {
                Expr::Index { array, index } => (*array, 2u16, index.as_mut()),
                Expr::IndexByte { array, index } => (*array, 1u16, index.as_mut()),
                _ => unreachable!(),
            };
            prop_expr(index_ref, cp);
            if !cp.suppress_subst
                && let Expr::IntLit(k) = index_ref
                && let Ok(byte_off) = u16::try_from(*k as i64 * elem_size as i64)
                && let Some(&v) = cp.ga_known.get(&(array, byte_off))
            {
                *e = Expr::IntLit(v);
            }
        }
        Expr::PtrIndexByte { ptr, index } => {
            prop_expr(index, cp);
            // `p[K]` through a GLOBAL pointer aliased to array `a` → direct
            // `a[K]` element read (runtime, not folded to an immediate — MSC
            // keeps pointer-routed reads as loads). Fixtures 888, 890.
            if let Some(&(AliasTarget::Global(a), base_off)) = cp.ptr_alias_g.get(ptr)
                && let Expr::IntLit(k) = index.as_ref()
            {
                let elem = cp.global_elem_sizes.get(a).copied().unwrap_or(2);
                // `p = &a[J]` shifts the element index by J = base_off/elem.
                let idx = *k + base_off / elem as i32;
                *e = if elem == 1 {
                    Expr::IndexByte { array: a, index: Box::new(Expr::IntLit(idx)) }
                } else {
                    Expr::Index { array: a, index: Box::new(Expr::IntLit(idx)) }
                };
            }
        }
        Expr::ParamIndex { index, .. } => {
            prop_expr(index, cp);
        }
        Expr::PtrArrayElem { index, .. } => {
            // The element is a pointer VALUE read from _DATA at runtime; MSC
            // never folds it to an immediate. Just propagate into the index.
            prop_expr(index, cp);
        }
        Expr::PtrArrayDeref { index, inner, .. } => {
            prop_expr(index, cp);
            prop_expr(inner, cp);
        }
        Expr::LocalPtrArrayDeref { local, index, inner, elem_size } => {
            prop_expr(index, cp);
            prop_expr(inner, cp);
            // `strs[i][j]` where the element `strs[i]` aliases a known string /
            // global / local array → fold to a direct CONST byte / indexed read.
            // Element stride is 2 (pointer slots), so key by `2*i`.
            if let Expr::IntLit(k) = index.as_ref()
                && let Ok(byte_off) = u16::try_from(*k as i64 * 2)
                && let Some(&a) = cp.elem_ptr_alias.get(&(*local, byte_off))
            {
                match a {
                    AliasTarget::String(sidx) => {
                        if *elem_size == 1
                            && let Expr::IntLit(j) = inner.as_ref()
                            && let Ok(off) = u16::try_from(*j as i64 * *elem_size as i64)
                        {
                            *e = Expr::StrLitByte { string_idx: sidx, byte_off: off };
                        }
                    }
                    AliasTarget::Global(g) => {
                        let inner = inner.clone();
                        *e = if *elem_size == 1 {
                            Expr::IndexByte { array: g, index: inner }
                        } else {
                            Expr::Index { array: g, index: inner }
                        };
                        prop_expr(e, cp);
                    }
                    AliasTarget::Local(x) => {
                        let inner = inner.clone();
                        *e = if *elem_size == 1 {
                            Expr::LocalIndexByte { local: x, index: inner }
                        } else {
                            Expr::LocalIndex { local: x, index: inner }
                        };
                        prop_expr(e, cp);
                    }
                }
            }
        }
        Expr::Index2D { is_global, base, row, col, cols, elem } => {
            prop_expr(row, cp);
            prop_expr(col, cp);
            // Both indices constant → fold to a flat 1-D element access, then
            // re-run prop so a known element value folds via la_known/ga_known.
            if let (Expr::IntLit(r), Expr::IntLit(c)) = (row.as_ref(), col.as_ref()) {
                let flat = Box::new(Expr::IntLit(r * *cols as i32 + c));
                *e = match (*is_global, *elem == 1) {
                    (true, false) => Expr::Index { array: *base, index: flat },
                    (true, true) => Expr::IndexByte { array: *base, index: flat },
                    (false, false) => Expr::LocalIndex { local: *base, index: flat },
                    (false, true) => Expr::LocalIndexByte { local: *base, index: flat },
                };
                prop_expr(e, cp);
            }
        }
        Expr::LocalField { .. } => {
            // Substitute the field's known value via la_known
            // keyed by (local, byte_off). For a union, only fold when the read
            // size matches the recorded write size (word↔word puns fold; a
            // byte read of a word store goes to memory).
            if let Expr::LocalField { local, byte_off, size } = e
                && let Some(&v) = cp.la_known.get(&(*local, *byte_off))
                && (!cp.union_locals.contains(local)
                    || cp.la_field_size.get(&(*local, *byte_off)) == Some(size))
            {
                *e = Expr::IntLit(v);
            }
        }
        // A struct-value param field is read from the stack at runtime; params
        // carry no compile-time known value, so nothing to substitute.
        Expr::ParamField { .. } => {}
        Expr::GlobalField { .. } => {
            // Substitute the field's known value via ga_known keyed by
            // (global, byte_off) — works for nested fields too (summed offset).
            if let Expr::GlobalField { global, byte_off, size } = e
                && let Some(&v) = cp.ga_known.get(&(*global, *byte_off))
                && (!cp.union_globals.contains(global)
                    || cp.ga_field_size.get(&(*global, *byte_off)) == Some(size))
            {
                *e = Expr::IntLit(v);
            }
        }
        Expr::DerefLocalField { ptr_local, byte_off, size } => {
            // `pp->f` where pp aliases `&g` / `&x` (incl. the address of a static
            // local, which is a global) → the direct field of g / x, a plain
            // memory load (NOT folded to the field's init value — MSC keeps the
            // load). Mirrors `alias_rewrite_derefs`. Fixture 2313.
            if let Some(&a) = cp.ptr_alias.get(ptr_local) {
                match a {
                    AliasTarget::Global(g) => *e = Expr::GlobalField { global: g, byte_off: *byte_off, size: *size },
                    AliasTarget::Local(x) => *e = Expr::LocalField { local: x, byte_off: *byte_off, size: *size },
                    AliasTarget::String(_) => {}
                }
            }
        }
        Expr::DerefParamField { .. } | Expr::DerefGlobalField { .. } => {
            // Pointer-aliasing const-prop not yet implemented.
        }
        Expr::LocalIndex { .. } | Expr::LocalIndexByte { .. } => {
            // Borrow index and substitute *e with the known element
            // value when the index folds and we've tracked it.
            // Capture whether the read index was a VARIABLE before prop_expr
            // folds it to a literal (the write/read index-form must match for
            // forwarding).
            let read_was_var = match e {
                Expr::LocalIndex { index, .. } | Expr::LocalIndexByte { index, .. } =>
                    !matches!(index.as_ref(), Expr::IntLit(_)),
                _ => false,
            };
            // A CHAR-array `a[i]` (i a bare local/param) whose value does NOT fold
            // is left with its variable index so codegen emits a runtime SI-indexed
            // byte load — captured before prop_expr rewrites `i` to a literal. An
            // int-array read folds the index to a direct `[bp+base+2K]` instead, so
            // it is NOT restored. Fixture 1428 (char, SI) vs 1090 (int, direct).
            let simple_var_index = match e {
                Expr::LocalIndexByte { index, .. } =>
                    match index.as_ref() {
                        Expr::Local(_) | Expr::Param(_) => Some((**index).clone()),
                        _ => None,
                    },
                _ => None,
            };
            let (local, elem_size, known_k) = match e {
                Expr::LocalIndex { local, index } => {
                    prop_expr(index, cp);
                    let k = if let Expr::IntLit(k) = index.as_ref() { Some(*k) } else { None };
                    // Element size from the local (2 for int, 4/8 for float arrays).
                    let sz = cp.local_specs.get(*local).map(|l| l.size as u16).unwrap_or(2);
                    (*local, sz, k)
                }
                Expr::LocalIndexByte { local, index } => {
                    prop_expr(index, cp);
                    let k = if let Expr::IntLit(k) = index.as_ref() { Some(*k) } else { None };
                    (*local, 1u16, k)
                }
                _ => unreachable!(),
            };
            if !cp.suppress_subst
                && let Some(k) = known_k
                && let Ok(byte_off) = u16::try_from(k as i64 * elem_size as i64)
                && let Some(&v) = cp.la_known.get(&(local, byte_off))
                // A variable-indexed read (`a[i]`) forwards only from a
                // variable-indexed write; a literal-write + variable-read does
                // not fold (MSC loads the slot). A literal-indexed read folds
                // from any write. Fixtures 144/1620 (fold) vs 1090/1428 (don't).
                && (!read_was_var || cp.la_var_written.contains(&(local, byte_off)))
            {
                *e = Expr::IntLit(v);
            } else if read_was_var && let Some(orig) = simple_var_index {
                // Value didn't fold — restore the bare variable index so codegen
                // emits a runtime SI-indexed load rather than a folded direct one
                // (MSC keeps `a[i]` runtime even when `i` is known). Fixture 1428.
                match e {
                    Expr::LocalIndex { index, .. } | Expr::LocalIndexByte { index, .. } =>
                        **index = orig,
                    _ => {}
                }
            }
        }
        Expr::DerefByte { .. } | Expr::DerefWord { .. } => {
            // Recurse into the pointer subexpression first (folds the index of
            // `*(p + i)` when i is a known local), then resolve through any alias.
            match e {
                Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => prop_expr(ptr, cp),
                _ => unreachable!(),
            }
            fold_aliased_deref(e, cp);
        }
        Expr::AddrOfGlobal(g) => {
            // Taking a global's address (escaping it — call arg, bare `&g`) lets
            // writes happen through the pointer, so reads after this point must not
            // fold the stale value. The `int *p = &g` alias-recording case
            // early-returns before here, so this only fires for genuine escapes.
            // Drops the scalar value and any known struct/array element values.
            // Fixture 1290 (`inc(&s); return s.x` must reload, not fold to 5).
            cp.mutated_globals.insert(*g);
            cp.g_known.remove(g);
            cp.ga_known.retain(|(gi, _), _| gi != g);
        }
        Expr::AddrOfIndexedGlobal { index, .. } => prop_expr(index, cp),
        Expr::AddrOfLocal(j) => {
            // Taking a local's address (escaping it — call arg, `&x`) allows
            // writes through the pointer; conservatively mark it mutated so reads
            // after this point do not fold the stale init value (fixture 1650).
            // The `int *p = &x` alias-recording case early-returns before here, so
            // this only fires for genuinely escaping addresses; also drop any
            // known array-element values for an escaped array.
            cp.mutated_locals.insert(*j);
            cp.l_known.remove(j);
            cp.la_known.retain(|(l, _), _| l != j);
        }
        Expr::Ternary { cond, then_arm, else_arm } => {
            // Substitute into the condition so fold() can determine the branch.
            cp.substituted = false;
            prop_expr(cond, cp);
            // A COMPARISON-cond ternary whose arms both propagate to literals
            // folds to a constant (assignment emits an immediate store, return
            // a mov-imm): fixture 2670 `x=a>b?a:b` (a,b const) → `mov [x],7`,
            // 588 (globals). Truthy-cond ternaries keep their arms as runtime
            // loads (fixture 1038), and compound arms (e.g. `-a`) stay non-
            // literal so the load / two-epilogue paths still fire (fixture 430).
            // A SOURCE-literal cond (`1 ? &a : &b`) is compile-time: collapse
            // to the chosen arm outright — downstream init/alias machinery
            // sees the plain arm expression. A truthy cond that merely
            // SUBSTITUTED to a literal stays a runtime ternary (1038, 2501).
            // Fixture 2318.
            if !cp.substituted && let Expr::IntLit(k) = cond.as_ref() {
                let mut arm = if *k != 0 { (**then_arm).clone() } else { (**else_arm).clone() };
                prop_expr(&mut arm, cp);
                *e = arm;
                return;
            }
            if matches!(cond.as_ref(), Expr::BinOp { op, .. }
                if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge))
                && let Some(c) = cond.fold(&[])
            {
                let surviving = if c != 0 { &*then_arm } else { &*else_arm };
                if matches!(surviving.as_ref(), Expr::Ternary { .. }) {
                    // Ternary CHAIN: the folded cond collapses to its
                    // surviving ternary arm — and, mirroring the if-statement
                    // rule, a FALSE fold CONSUMES the knowledge it used, so
                    // the surviving arm's conds re-test at runtime (1824
                    // `x<0 ? -1 : (x==0?0:1)` keeps the inner runtime; 1435
                    // chained `a==K` arms after the dropped one emit cmp/jne;
                    // 2272). A TRUE fold keeps values — the surviving chain
                    // continues folding (2344 collapses fully to a literal).
                    if c == 0 && cp.substituted {
                        cp.l_known.clear();
                        cp.g_known.clear();
                        cp.la_known.clear();
            cp.la_var_written.clear();
                        cp.ga_known.clear();
                        // Init-seeded knowledge also lives in the EMIT-time
                        // fold view (locals.inits / globals); mark the
                        // surviving chain's reads as mutated so the emitter
                        // re-tests them at runtime too (1824/1435/2272).
                        fn mark_reads(e: &Expr, cp: &mut ConstProp) {
                            match e {
                                Expr::Local(i) => { cp.mutated_locals.insert(*i); }
                                Expr::Global(g) => { cp.mutated_globals.insert(*g); }
                                Expr::BinOp { left, right, .. } => {
                                    mark_reads(left, cp);
                                    mark_reads(right, cp);
                                }
                                Expr::Ternary { cond, then_arm, else_arm } => {
                                    mark_reads(cond, cp);
                                    mark_reads(then_arm, cp);
                                    mark_reads(else_arm, cp);
                                }
                                Expr::CastChar { value, .. } | Expr::CastLong { value, .. } => mark_reads(value, cp),
                                _ => {}
                            }
                        }
                        mark_reads(surviving, cp);
                    }
                    let mut arm = (**surviving).clone();
                    prop_expr(&mut arm, cp);
                    // `v==0 ? 0 : 1` (and `v!=0 ? 1 : 0`) is the boolean
                    // value of `v != 0` — MSC emits the branchless carry
                    // trick (`cmp v,1; sbb ax,ax; inc ax`), same as `!!v`.
                    // Fixture 1824.
                    if let Expr::Ternary { cond, then_arm, else_arm } = &arm
                        && let Expr::BinOp { op, left, right } = cond.as_ref()
                        && matches!(right.as_ref(), Expr::IntLit(0))
                        && ((matches!(op, BinOp::Eq)
                            && matches!(then_arm.as_ref(), Expr::IntLit(0))
                            && matches!(else_arm.as_ref(), Expr::IntLit(1)))
                            || (matches!(op, BinOp::Ne)
                                && matches!(then_arm.as_ref(), Expr::IntLit(1))
                                && matches!(else_arm.as_ref(), Expr::IntLit(0))))
                    {
                        arm = Expr::BinOp {
                            op: BinOp::Ne,
                            left: left.clone(),
                            right: Box::new(Expr::IntLit(0)),
                        };
                    }
                    *e = arm;
                } else {
                    // Simple arms: collapse only when BOTH propagate to
                    // literals (2670 `x=a>b?a:b` → immediate store; 588).
                    // Non-literal arms keep the runtime ternary so the
                    // emit-time load / two-epilogue paths fire (430, 1350).
                    let mut t = (**then_arm).clone();
                    prop_expr(&mut t, cp);
                    let mut e2 = (**else_arm).clone();
                    prop_expr(&mut e2, cp);
                    if let (Expr::IntLit(tv), Expr::IntLit(ev)) = (&t, &e2) {
                        *e = Expr::IntLit(if c != 0 { *tv } else { *ev });
                    }
                }
            } else {
                // Runtime ternary: walk DISCARDED arm clones so side effects
                // (`y ? y++ : z--`) register their mutations — later reads
                // must not fold stale values. The arms themselves emit
                // unsubstituted. Fixture 2501.
                let mut t = (**then_arm).clone();
                prop_expr(&mut t, cp);
                let mut e2 = (**else_arm).clone();
                prop_expr(&mut e2, cp);
                // Only one arm actually runs at execution time, so neither arm's
                // assignments yield a statically-known value — strip any that the
                // mutation walk above recorded into l_known/g_known so later reads
                // reload from the slot (the mutation marks stay). Fixture 2476.
                clear_arm_assigned_values(then_arm, cp);
                clear_arm_assigned_values(else_arm, cp);
            }
        }
        Expr::Seq { sides, value } => {
            for s in sides { prop_stmt(s, cp); }
            prop_expr(value, cp);
        }
        Expr::PostMutateLocal { local_idx, .. } => {
            cp.mutated_locals.insert(*local_idx);
            cp.l_known.remove(local_idx);
            // `p++` / `*p++` advances the pointer, so any alias (`p = &x`) is now
            // stale — a later `*p` must reload, not re-fold to the old base.
            // Fixture 2000 (`p=a; *p++; *p++; *p`).
            cp.ptr_alias.remove(local_idx);
            cp.ptr_addr.remove(local_idx);
        }
        Expr::PostMutateGlobal { global_idx, .. } => {
            cp.mutated_globals.insert(*global_idx);
            cp.g_known.remove(global_idx);
        }
        Expr::PreMutateLocal { local_idx, .. } => {
            cp.mutated_locals.insert(*local_idx);
            cp.l_known.remove(local_idx);
            cp.ptr_alias.remove(local_idx);
            cp.ptr_addr.remove(local_idx);
        }
        Expr::PreMutateGlobal { global_idx, .. } => {
            cp.mutated_globals.insert(*global_idx);
            cp.g_known.remove(global_idx);
        }
        Expr::PreMutateGlobalField { global, .. } => {
            cp.mutated_globals.insert(*global);
            cp.g_known.remove(global);
        }
        Expr::PostMutateDeref { ptr, step, is_byte } => {
            // `(*p)--` / `(*p)++` where p aliases &x → a DIRECT post-mutate on x
            // (`mov ax,[x]; dec word[x]`), not a pointer-routed read-modify. The
            // PostMutateLocal/Global handler then invalidates x's known value so a
            // later read reloads (the modify changed it — else we'd fold a stale
            // value, a miscompile). Fixture 2449.
            if !*is_byte
                && let Expr::Local(p) = ptr.as_ref()
                && let Some(&a) = cp.ptr_alias.get(p)
            {
                let step = *step;
                match a {
                    AliasTarget::Local(x) => {
                        cp.aliases_used.insert(*p);
                        *e = Expr::PostMutateLocal { local_idx: x, step };
                        prop_expr(e, cp);
                        return;
                    }
                    AliasTarget::Global(g) => {
                        cp.aliases_used.insert(*p);
                        *e = Expr::PostMutateGlobal { global_idx: g, step };
                        prop_expr(e, cp);
                        return;
                    }
                    AliasTarget::String(_) => {}
                }
            }
            prop_expr(ptr, cp);
        }
        Expr::PreMutateDeref { ptr, .. } | Expr::PostIncDeref { ptr, .. } => prop_expr(ptr, cp),
        Expr::PreMutateIndexedGlobal { array, index, .. }
        | Expr::PostMutateIndexedGlobal { array, index, .. } => {
            prop_expr(index, cp);
            cp.mutated_globals.insert(*array);
        }
        Expr::PostMutateLocalIndex { local, index, is_byte, .. } => {
            prop_expr(index, cp);
            cp.mutated_locals.insert(*local);
            // Drop the mutated element's known value so a later `a[K]` read
            // reloads from the slot instead of folding the old value.
            if let Expr::IntLit(k) = index.as_ref() {
                let byte_off = (*k * if *is_byte { 1 } else { 2 }) as u16;
                cp.la_known.remove(&(*local, byte_off));
            }
        }
        // A param's value is unknown at compile time; mutating it changes
        // nothing in the const-prop tables (params aren't tracked).
        Expr::PreMutateParam { .. } | Expr::PostMutateParam { .. } => {}
        // A bit-field read is always a runtime masked load — MSC never folds it,
        // even when the stored value is known. No sub-expressions to propagate.
        Expr::BitField { .. } => {}
        // A string-literal byte read is a CONST load — no folding.
        Expr::StrLitByte { .. } => {}
        // A pointer member chain is a runtime BX-walk — no folding.
        Expr::PtrChainField { base, hops, final_off, final_size } => {
            // `o.p->v` where the field `o.p` aliases `&i` → fold to the direct
            // field `i.v` (`mov ax,[bp+i+v]`) instead of loading the field pointer
            // and dereferencing. The base is the field pointer `o.p` (a
            // LocalField); with no further hops the deref reads the aliased
            // struct's field at final_off. Fixture 1873.
            if hops.is_empty()
                && let Expr::LocalField { local, byte_off, .. } = base.as_ref()
                && let Some(&a) = cp.elem_ptr_alias.get(&(*local, *byte_off))
            {
                match a {
                    AliasTarget::Local(x) => {
                        *e = Expr::LocalField { local: x, byte_off: *final_off, size: *final_size };
                    }
                    AliasTarget::Global(g) => {
                        *e = Expr::GlobalField { global: g, byte_off: *final_off, size: *final_size };
                    }
                    AliasTarget::String(_) => {}
                }
            }
        }
        Expr::StructArrayField { index, .. } => prop_expr(index, cp),
        Expr::LocalStructArrayField { local, index, stride, field_off, size } => {
            prop_expr(index, cp);
            // Index known → fold to a plain LocalField for ADDRESSING only. MSC
            // reads a variable-indexed element from its slot even when the value
            // is statically known (it does NOT substitute the element value), so
            // do NOT re-prop — that would fold via la_known. Fixture 2438 (`i=2`
            // → `mov ax,[bp-4]; add ax,[bp-2]`, not `mov ax,61`).
            if let Expr::IntLit(k) = index.as_ref()
                && let Ok(byte_off) = u16::try_from(*k as i64 * *stride as i64 + *field_off as i64)
            {
                *e = Expr::LocalField { local: *local, byte_off, size: *size };
            }
        }
        Expr::ParamPtrArrayDeref { index, inner, .. } => {
            prop_expr(index, cp);
            prop_expr(inner, cp);
        }
        Expr::ParamStructArrayField { param, index, stride, field_off, size } => {
            prop_expr(index, cp);
            // Index known → fold to a plain DerefParamField (addressing only;
            // params carry no compile-time value so no further substitution).
            if let Expr::IntLit(k) = index.as_ref()
                && let Ok(byte_off) = u16::try_from(*k as i64 * *stride as i64 + *field_off as i64)
            {
                *e = Expr::DerefParamField { ptr_param: *param, byte_off, size: *size };
            }
        }
        Expr::IntLit(_) | Expr::Param(_) | Expr::StrLit(_) => {}
        // Bit-field post-mutate: a self-contained RMW, no sub-expressions to fold.
        Expr::BitFieldPostMutate { .. } => {}
    }
    // After substitution settles, hoist product terms ahead of simple ones in a
    // `+` chain (fixture 1818). Done last so folded-to-constant terms are no
    // longer seen as products.
    if matches!(e, Expr::BinOp { op: BinOp::Add, .. }) {
        reorder_mul_first(e);
    }
}
