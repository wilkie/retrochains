use crate::*;

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
pub(crate) fn const_prop_globals(
    stmts: &[Stmt],
    local_specs: &[LocalSpec],
    long_globals: &[bool],
    global_elem_sizes: &[usize],
    struct_is_union: &[bool],
    union_globals: &std::collections::HashSet<usize>,
) -> (Vec<Stmt>, std::collections::HashSet<usize>, std::collections::HashSet<usize>) {
    // Locals whose struct type is a union — their field writes stay out of the
    // const-prop tables so punned sibling reads aren't folded (fixtures 177/919).
    let union_locals: std::collections::HashSet<usize> = local_specs.iter().enumerate()
        .filter(|(_, s)| s.struct_idx.map(|si| struct_is_union.get(si).copied().unwrap_or(false)).unwrap_or(false))
        .map(|(i, _)| i)
        .collect();
    let mut cp = ConstProp {
        local_specs: local_specs.to_vec(),
        global_elem_sizes: global_elem_sizes.to_vec(),
        union_locals,
        union_globals: union_globals.clone(),
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
    let stmts = drop_dead_after_goto(stmts.to_vec());
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
            if let (AssignTarget::DerefLocal(p) | AssignTarget::DerefLocalByte(p)) = target
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
                let elem = cp.global_elem_sizes.get(a).copied().unwrap_or(2);
                let k = *disp as i64;
                *target = if elem == 1 {
                    AssignTarget::IndexedGlobalByte { array: a, byte_off: k as u16 }
                } else {
                    AssignTarget::IndexedGlobal { array: a, byte_off: (k * elem as i64) as u16 }
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
                 Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, ref left, .. }) => {
                    if let Expr::IndexByte { array: lx, index } = left.as_ref() {
                        *lx == t && matches!(index.as_ref(), Expr::IntLit(k) if *k as u16 == byte_off)
                    } else { false }
                }
                (AssignTarget::IndexedGlobal { array: t, byte_off },
                 Expr::BinOp { op: BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor, ref left, .. }) => {
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
                    if let Expr::IntLit(k) = value
                        && !value_was_ternary
                    {
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
                        if let Some(sz) = write_field_size {
                            cp.la_field_size.insert((*local, *byte_off), sz);
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
                    if !from_ptr_store && let Expr::IntLit(k) = value {
                        cp.ga_known.insert((*array, *byte_off), *k);
                        if let Some(sz) = write_field_size {
                            cp.ga_field_size.insert((*array, *byte_off), sz);
                        }
                    } else {
                        cp.ga_known.remove(&(*array, *byte_off));
                        cp.ga_field_size.remove(&(*array, *byte_off));
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
            // Exiting a nested block scope flushes MSC's known-value cache:
            // a read after the block reloads from its slot rather than folding
            // a value learned inside (or before) the block. Mirrors the loop /
            // if boundary clears. Fixtures 2258, 2316, 2467 (shadowed locals).
            cp.g_known.clear();
            cp.l_known.clear();
            cp.la_known.clear();
            cp.ptr_alias.clear();
            cp.ptr_alias_g.clear();
            cp.ptr_addr.clear();
            cp.ga_known.clear();
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
        global_elem_sizes: cp.global_elem_sizes.clone(),
        union_locals: cp.union_locals.clone(),
        union_globals: cp.union_globals.clone(),
        la_field_size: cp.la_field_size.clone(),
        ga_field_size: cp.ga_field_size.clone(),
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
        Expr::CastChar { value, .. } => prop_expr(value, cp),
        Expr::AssignExpr { target, value } => {
            // Substitute into the RHS so the cond can fold, then invalidate the
            // target's known value: MSC reloads/reuses-AX for later reads rather
            // than re-materializing the immediate (the store leaves AX set, and
            // the return/use peepholes reuse it). Mirrors the ternary-assign rule.
            prop_expr(value, cp);
            match target {
                AssignTarget::Local(l) => { cp.mutated_locals.insert(*l); cp.l_known.remove(l); }
                AssignTarget::Global(g) => { cp.mutated_globals.insert(*g); cp.g_known.remove(g); }
                _ => {}
            }
        }
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
        }
        Expr::Call { args, .. } => {
            for a in args {
                prop_expr(a, cp);
            }
        }
        Expr::CallPtr { args, .. } => {
            // Target is a fnptr lvalue (Global/Param/Local) — leave it; just
            // propagate into the arguments.
            for a in args {
                prop_expr(a, cp);
            }
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
                let elem = cp.global_elem_sizes.get(a).copied().unwrap_or(2);
                *e = if elem == 1 {
                    Expr::IndexByte { array: a, index: Box::new(Expr::IntLit(k)) }
                } else {
                    Expr::Index { array: a, index: Box::new(Expr::IntLit(k)) }
                };
            }
        }
        Expr::ParamIndex { index, .. } => {
            prop_expr(index, cp);
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
            prop_expr(cond, cp);
            // A COMPARISON-cond ternary whose arms both propagate to literals
            // folds to a constant (assignment emits an immediate store, return
            // a mov-imm): fixture 2670 `x=a>b?a:b` (a,b const) → `mov [x],7`,
            // 588 (globals). Truthy-cond ternaries keep their arms as runtime
            // loads (fixture 1038), and compound arms (e.g. `-a`) stay non-
            // literal so the load / two-epilogue paths still fire (fixture 430).
            let mut replacement = None;
            if matches!(cond.as_ref(), Expr::BinOp { op, .. }
                if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge))
                && let Some(c) = cond.fold(&[])
            {
                let mut t = (**then_arm).clone();
                prop_expr(&mut t, cp);
                let mut e2 = (**else_arm).clone();
                prop_expr(&mut e2, cp);
                if let (Expr::IntLit(tv), Expr::IntLit(ev)) = (&t, &e2) {
                    replacement = Some(if c != 0 { *tv } else { *ev });
                }
            }
            if let Some(v) = replacement {
                *e = Expr::IntLit(v);
            }
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
