use crate::*;

pub(crate) fn const_prop_globals(
    stmts: &[Stmt],
    local_specs: &[LocalSpec],
    long_globals: &[bool],
    char_globals: &[bool],
) -> (Vec<Stmt>, std::collections::HashSet<usize>, std::collections::HashSet<usize>) {
    let mut cp = ConstProp {
        local_specs: local_specs.to_vec(),
        global_is_char: char_globals.to_vec(),
        ..ConstProp::default()
    };
    for (i, &is_long) in long_globals.iter().enumerate() {
        if is_long { cp.long_globals.insert(i); }
    }
    // Pre-seed l_known with the locals' constant inits so the
    // const-fold pass sees `int x = 1; switch(x)` as having x=1
    // without re-deriving it from prologue stores.
    for (i, spec) in local_specs.iter().enumerate() {
        // Skip long locals: substituting `Local(c)` → `IntLit(K)` would
        // make `return (int)c;` emit a const load instead of a slot
        // read (fixture 1037). The emit-time fold_cond path still sees
        // the init via `locals.inits` for cond elision (1632).
        if spec.is_long { continue; }
        // Only literal-init locals (pure compile-time constant) get
        // substituted. Locals whose init came from another local —
        // `int n = c;` (1043), `char c = a + b;` (1046) — stay as
        // `Local` so reads go through the slot. Literal-init keeps
        // (`int x = 5;` 4081, `char c = 'A'+1;` 1023).
        if !spec.init_is_literal { continue; }
        if let Some(k) = spec.init {
            cp.l_known.insert(i, k);
        }
    }
    let new_stmts: Vec<Stmt> = stmts.iter().map(|s| {
        let mut new_stmt = s.clone();
        prop_stmt(&mut new_stmt, &mut cp);
        let used: Vec<usize> = cp.aliases_used.drain().collect();
        for p in used { cp.ptr_alias.remove(&p); }
        new_stmt
    }).collect();
    (new_stmts, cp.mutated_locals, cp.mutated_globals)
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
                *e = match a {
                    AliasTarget::Local(x) => Expr::Local(x),
                    AliasTarget::Global(g) => Expr::Global(g),
                };
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
/// pointer alias `p -> base`:
///   - offset 0  → the aliased lvalue `Local(x)`/`Global(g)`, then const-folded
///     (a scalar pointee like `p = &g; *p` folds to g's value — fixture 596).
///   - offset K≠0 → the base array's element `base[K/elem]`, left as a runtime
///     element read (MSC does NOT fold these — fixtures 1019, 888).
/// Marks the alias used (single-use semantics, drained per top-level stmt).
fn fold_aliased_deref(e: &mut Expr, cp: &mut ConstProp) {
    let is_byte = matches!(e, Expr::DerefByte { .. });
    let (p, byte_off) = {
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
    let Some(&base) = cp.ptr_alias.get(&p) else { return };
    cp.aliases_used.insert(p);
    if byte_off == 0 {
        *e = match base {
            AliasTarget::Local(x) => Expr::Local(x),
            AliasTarget::Global(g) => Expr::Global(g),
        };
        prop_expr(e, cp);
        return;
    }
    let elem = if is_byte { 1 } else { 2 };
    if byte_off < 0 || byte_off % elem != 0 { return; }
    let idx = Box::new(Expr::IntLit(byte_off / elem));
    *e = match (base, is_byte) {
        (AliasTarget::Local(a), false) => Expr::LocalIndex { local: a, index: idx },
        (AliasTarget::Local(a), true) => Expr::LocalIndexByte { local: a, index: idx },
        (AliasTarget::Global(g), false) => Expr::Index { array: g, index: idx },
        (AliasTarget::Global(g), true) => Expr::IndexByte { array: g, index: idx },
    };
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
pub(crate) fn prop_stmt(stmt: &mut Stmt, cp: &mut ConstProp) {
    match stmt {
        Stmt::Return(e) => prop_expr(e, cp),
        Stmt::ExprStmt(e) => prop_expr(e, cp),
        Stmt::Assign { target, value } => {
            // Set when a `p[K]=v` pointer store is rewritten to a direct array
            // element store: MSC's element table does NOT track writes that went
            // through a pointer, so the element stays unknown (later direct reads
            // must NOT fold). Fixture 1017.
            let mut from_ptr_store = false;
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
                if let Expr::AddrOfGlobal(a) = value {
                    cp.ptr_alias_g.insert(*p, AliasTarget::Global(*a));
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
            if let AssignTarget::DerefLocal(p) = target
                && let Some(&a) = cp.ptr_alias.get(p)
            {
                cp.aliases_used.insert(*p);
                *target = match a {
                    AliasTarget::Local(x) => AssignTarget::Local(x),
                    AliasTarget::Global(g) => AssignTarget::Global(g),
                };
                alias_rewrite_derefs(value, cp);
            }
            // `p[K] = ...` (constant K≠0) where p aliases a base array → a direct
            // element store `base[K] = ...`. byte_off is already in pointee bytes.
            if let AssignTarget::DerefLocalOffset { local: p, byte_off, is_byte } = target
                && let Some(&a) = cp.ptr_alias.get(p)
            {
                cp.aliases_used.insert(*p);
                from_ptr_store = true;
                let (byte_off, is_byte) = (*byte_off, *is_byte);
                *target = match (a, is_byte) {
                    (AliasTarget::Local(x), false) => AssignTarget::IndexedLocal { local: x, byte_off },
                    (AliasTarget::Local(x), true) => AssignTarget::IndexedLocalByte { local: x, byte_off },
                    (AliasTarget::Global(g), false) => AssignTarget::IndexedGlobal { array: g, byte_off },
                    (AliasTarget::Global(g), true) => AssignTarget::IndexedGlobalByte { array: g, byte_off },
                };
            }
            // `p[K] = ...` through a GLOBAL pointer aliased to array `a` → direct
            // `a[K]` store. The parser lowers this to PtrIndexByte{ptr:p, disp:K}.
            if let AssignTarget::PtrIndexByte { ptr: p, disp } = target
                && let Some(&AliasTarget::Global(a)) = cp.ptr_alias_g.get(p)
            {
                from_ptr_store = true;
                let is_byte = cp.global_is_char.get(a).copied().unwrap_or(false);
                let k = *disp as i64;
                *target = if is_byte {
                    AssignTarget::IndexedGlobalByte { array: a, byte_off: k as u16 }
                } else {
                    AssignTarget::IndexedGlobal { array: a, byte_off: (k * 2) as u16 }
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
            let self_assign_addsub = match (target.clone(), value.clone()) {
                (AssignTarget::Local(t), Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, left, .. }) => {
                    matches!(left.as_ref(), Expr::Local(l) if *l == t)
                }
                (AssignTarget::Global(t), Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, left, .. }) => {
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
                _ => false,
            };
            if self_assign_addsub
                && let Expr::BinOp { right, .. } = value
            {
                prop_expr(right, cp);
            } else {
                prop_expr(value, cp);
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
                _ => {}
            }
            match target {
                AssignTarget::Global(g) => {
                    if let Expr::IntLit(k) = value {
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
                    if let Expr::IntLit(k) = value {
                        cp.l_known.insert(*l, *k);
                    } else {
                        cp.l_known.remove(l);
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
                    if !from_ptr_store && let Some(k) = value.fold(&[]) {
                        cp.la_known.insert((*local, *byte_off), k);
                    } else {
                        cp.la_known.remove(&(*local, *byte_off));
                    }
                }
                AssignTarget::IndexedGlobal { array, byte_off }
                | AssignTarget::IndexedGlobalByte { array, byte_off }
                | AssignTarget::GlobalField { global: array, byte_off, .. } => {
                    if let Some(k) = value.fold(&[]) {
                        *value = Expr::IntLit(k);
                    }
                    if !from_ptr_store && let Expr::IntLit(k) = value {
                        cp.ga_known.insert((*array, *byte_off), *k);
                    } else {
                        cp.ga_known.remove(&(*array, *byte_off));
                    }
                }
                _ => {}
            }
        }
        Stmt::Empty => {}
        Stmt::If { cond, then_branch, else_branch } => {
            // Fold the cond using current knowledge, then propagate
            // into each branch with an isolated copy so writes don't
            // leak across paths. After the if, conservatively clear.
            prop_cond(cond, cp);
            let mut sub = cp_clone(cp);
            prop_stmt(then_branch, &mut sub);
            if let Some(eb) = else_branch {
                let mut sub2 = cp_clone(cp);
                prop_stmt(eb, &mut sub2);
            }
            // After a branch we don't know which path was taken.
            cp.g_known.clear();
            cp.l_known.clear();
            cp.la_known.clear();
            cp.ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
            cp.ga_known.clear();
        }
        Stmt::Block(stmts) => {
            for s in stmts {
                prop_stmt(s, cp);
            }
        }
        Stmt::Switch { scrutinee, cases } => {
            prop_expr(scrutinee, cp);
            if let Expr::IntLit(k) = scrutinee {
                let k = *k;
                // Check whether any NFC cases exist: V != 0 AND V < k (signed).
                let has_nfc = cases.iter()
                    .any(|a| matches!(a.value, Some(v) if v != 0 && v < k));

                if has_nfc {
                    // Partial switch: emit_function will call
                    // emit_partial_switch_with_continuation.
                    cp.g_known.clear();
                    cp.l_known.clear();
                    cp.la_known.clear();
            cp.ptr_alias.clear();
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
            cp.ptr_alias.clear();
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
            cp.ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
                    cp.ga_known.clear();
                }
            } else {
                // Runtime scrutinee (not folded): leave as Switch.
                cp.g_known.clear();
                cp.l_known.clear();
                cp.la_known.clear();
            cp.ptr_alias.clear();
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
            cp.g_known.clear();
            cp.l_known.clear();
            cp.la_known.clear();
            cp.ptr_alias.clear();
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
        ptr_alias: cp.ptr_alias.clone(),
        ptr_alias_g: cp.ptr_alias_g.clone(),
        aliases_used: cp.aliases_used.clone(),
        ptr_addr: cp.ptr_addr.clone(),
        local_specs: cp.local_specs.clone(),
        global_is_char: cp.global_is_char.clone(),
    }
}
/// If `e` is a pointer local holding `&x`/`&g` (offset 0), the address
/// expression `AddrOfLocal(x)`/`AddrOfGlobal(g)` — used to lower `if (p)` /
/// `if (p == 0)` to `cmp bp,K` / `mov ax,&g; or ax,ax`.
fn ptr_local_addr(e: &Expr, cp: &ConstProp) -> Option<Expr> {
    if let Expr::Local(p) = e
        && let Some(&(base, 0)) = cp.ptr_addr.get(p)
    {
        Some(match base {
            AliasTarget::Local(x) => Expr::AddrOfLocal(x),
            AliasTarget::Global(g) => Expr::AddrOfGlobal(g),
        })
    } else {
        None
    }
}
pub(crate) fn prop_cond(cond: &mut Cond, cp: &mut ConstProp) {
    match cond {
        Cond::Truthy(e) => {
            if let Some(addr) = ptr_local_addr(e, cp) {
                *e = addr;
                return;
            }
            prop_expr(e, cp)
        }
        Cond::Cmp { op, left, right } => {
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
            // `if (p == q)` over two pointer locals holding `&x`/`&g` (offset 0)
            // whose bases aren't a foldable same-global pair: substitute each
            // with its address so emit materializes both. Fixtures 1235 (locals),
            // 3944-3947 (distinct globals).
            if matches!(op, RelOp::Eq | RelOp::Ne)
                && let (Some(la), Some(ra)) = (ptr_local_addr(left, cp), ptr_local_addr(right, cp))
            {
                *left = la;
                *right = ra;
                return;
            }
            prop_expr(left, cp);
            prop_expr(right, cp);
        }
        Cond::And(a, b) | Cond::Or(a, b) => {
            prop_cond(a, cp);
            prop_cond(b, cp);
        }
    }
}
pub(crate) fn prop_expr(e: &mut Expr, cp: &mut ConstProp) {
    match e {
        Expr::FloatLit(..) => {} // no int const-prop into float literals
        Expr::Global(idx) => {
            // Long globals are never substituted — their compound
            // updates need `Global(g)` on the lhs for the long-specific
            // assign-codegen path to fire (fixture 207).
            if !cp.long_globals.contains(idx)
                && let Some(&k) = cp.g_known.get(idx)
            {
                *e = Expr::IntLit(k);
            }
        }
        Expr::Local(idx) => {
            if let Some(&k) = cp.l_known.get(idx) {
                *e = Expr::IntLit(k);
            }
        }
        Expr::BinOp { op: BinOp::LogOr | BinOp::LogAnd, .. } => {
            // MSC does NOT substitute constant locals inside || / && operands:
            // `return x || y` with x=1 always emits `cmp [bp-x], 0`, not
            // `cmp 1, 0`. The fold() path (for if-condition dead-branch
            // elimination) still works because fold() reads l_known directly.
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
            prop_expr(left, cp);
            prop_expr(right, cp);
        }
        Expr::Call { args, .. } => {
            for a in args {
                prop_expr(a, cp);
            }
        }
        Expr::Index { .. } | Expr::IndexByte { .. } => {
            // Substitute index first, then try to fold to a known global array element.
            let (array, elem_size, index_ref) = match e {
                Expr::Index { array, index } => (*array, 2u16, index.as_mut()),
                Expr::IndexByte { array, index } => (*array, 1u16, index.as_mut()),
                _ => unreachable!(),
            };
            prop_expr(index_ref, cp);
            if let Expr::IntLit(k) = index_ref
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
            if let Some(&AliasTarget::Global(a)) = cp.ptr_alias_g.get(ptr)
                && let Expr::IntLit(k) = index.as_ref()
            {
                let k = *k;
                let is_byte = cp.global_is_char.get(a).copied().unwrap_or(false);
                *e = if is_byte {
                    Expr::IndexByte { array: a, index: Box::new(Expr::IntLit(k)) }
                } else {
                    Expr::Index { array: a, index: Box::new(Expr::IntLit(k)) }
                };
            }
        }
        Expr::ParamIndex { index, .. } => {
            prop_expr(index, cp);
        }
        Expr::LocalField { .. } => {
            // Substitute the field's known value via la_known
            // keyed by (local, byte_off).
            if let Expr::LocalField { local, byte_off, .. } = e
                && let Some(&v) = cp.la_known.get(&(*local, *byte_off))
            {
                *e = Expr::IntLit(v);
            }
        }
        Expr::GlobalField { .. } => {
            // Substitute the field's known value via ga_known keyed by
            // (global, byte_off) — works for nested fields too (summed offset).
            if let Expr::GlobalField { global, byte_off, .. } = e
                && let Some(&v) = cp.ga_known.get(&(*global, *byte_off))
            {
                *e = Expr::IntLit(v);
            }
        }
        Expr::DerefLocalField { .. } | Expr::DerefParamField { .. } | Expr::DerefGlobalField { .. } => {
            // Pointer-aliasing const-prop not yet implemented.
        }
        Expr::LocalIndex { .. } | Expr::LocalIndexByte { .. } => {
            // Borrow index and substitute *e with the known element
            // value when the index folds and we've tracked it.
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
            if let Some(k) = known_k
                && let Ok(byte_off) = u16::try_from(k as i64 * elem_size as i64)
                && let Some(&v) = cp.la_known.get(&(local, byte_off))
            {
                *e = Expr::IntLit(v);
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
        Expr::AddrOfGlobal(_) => {}
        Expr::AddrOfLocal(j) => {
            // Taking a local's address allows writes through the pointer;
            // conservatively mark it as mutated so reads after this point
            // do not fold the stale init value (fixture 1650).
            cp.mutated_locals.insert(*j);
            cp.l_known.remove(j);
        }
        Expr::Ternary { cond, then_arm: _, else_arm: _ } => {
            // Only substitute into the condition so fold() can determine
            // the branch; arms are emitted as runtime loads (fixture 1038).
            prop_expr(cond, cp);
        }
        Expr::Seq { sides, value } => {
            for s in sides { prop_stmt(s, cp); }
            prop_expr(value, cp);
        }
        Expr::PostMutateLocal { local_idx, .. } => {
            cp.mutated_locals.insert(*local_idx);
            cp.l_known.remove(local_idx);
        }
        Expr::PostMutateGlobal { global_idx, .. } => {
            cp.mutated_globals.insert(*global_idx);
            cp.g_known.remove(global_idx);
        }
        Expr::PreMutateLocal { local_idx, .. } => {
            cp.mutated_locals.insert(*local_idx);
            cp.l_known.remove(local_idx);
        }
        Expr::PreMutateGlobal { global_idx, .. } => {
            cp.mutated_globals.insert(*global_idx);
            cp.g_known.remove(global_idx);
        }
        Expr::IntLit(_) | Expr::Param(_) | Expr::StrLit(_) => {}
    }
}
