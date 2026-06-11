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
    // Single-use: drain the alias so a later deref reloads at runtime.
    if from_alias { cp.aliases_used.insert(p); } else { cp.ptr_addr.remove(&p); }
    let elem = if is_byte { 1 } else { 2 };
    let total = base_off + deref_off;
    if total < 0 || total % elem != 0 { return; }
    if deref_off == 0 && total == 0 {
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
            if let (AssignTarget::DerefLocal(p) | AssignTarget::DerefLocalByte(p)) = target
                && let Some(&a) = cp.ptr_alias.get(p)
                && !matches!(a, AliasTarget::String(_))
            {
                cp.aliases_used.insert(*p);
                *target = match a {
                    AliasTarget::Local(x) => AssignTarget::Local(x),
                    AliasTarget::Global(g) => AssignTarget::Global(g),
                    AliasTarget::String(_) => unreachable!(),
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
            // The pre-prop RHS, when it is a bare scalar LOCAL read. MSC reloads
            // (rather than propagating the folded value) when assigning across a
            // type CONVERSION — a char/long/signedness change — even though the
            // source local is known (`int n; char c=5; n=c; return n` reloads n).
            // Captured here to suppress the known-value recording below for such
            // converting copies. Fixtures 604/1312/2386/2491/2387.
            let rhs_src_local: Option<usize> = if let Expr::Local(i) = value { Some(*i) } else { None };
            if self_assign_addsub
                && let Expr::BinOp { right, .. } = value
            {
                // Long globals are normally never substituted (the LHS self-read
                // must stay `Global(g)` for the long-assign codegen). But here the
                // self-read LHS is preserved separately — only the RHS operand is
                // prop'd — so a KNOWN long-global RHS may fold to a literal, letting
                // a long compound `g op= h` lower to immediates / `mov ax,K; cwd`.
                // Fixtures 734/735/741/742.
                let known_long_global = match right.as_ref() {
                    Expr::Global(gi) if cp.long_globals.contains(gi) => cp.g_known.get(gi).copied(),
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
                    if let Expr::IntLit(k) = value
                        && !value_was_ternary
                        && !converting
                        && !cp.local_specs.get(*l).is_some_and(|s| s.is_register)
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
            prop_cond(cond, cp);
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
                cp.ga_known.clear();
                // Init-seeded knowledge also lives in the EMIT-time fold view
                // (locals.inits) — mark the surviving else-if chain's cond
                // reads as mutated so the emitter re-tests them at runtime,
                // mirroring the ternary-chain rule. `if (a>0)...else if (a<0)`
                // with `int a = 0;` elides the first arm but emits a real
                // `cmp [a],0` for the second (fixture 1201).
                let mut chain: Option<&Stmt> = else_branch.as_deref();
                while let Some(Stmt::If { cond, else_branch, .. }) = chain {
                    mark_cond_reads(cond, cp);
                    chain = else_branch.as_deref();
                }
            }
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
            cp.ptr_alias.clear();
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
            cp.ptr_alias.clear();
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
            cp.ptr_alias.clear();
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
            cp.ptr_alias.clear();
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
            cp.ptr_alias.clear();
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
            cp.ptr_alias.clear();
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
        union_locals: cp.union_locals.clone(),
        union_globals: cp.union_globals.clone(),
        la_field_size: cp.la_field_size.clone(),
        ga_field_size: cp.ga_field_size.clone(),
        in_cond: cp.in_cond,
        saw_call: cp.saw_call,
        substituted: cp.substituted,
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
            Expr::CastChar { value, .. } => mark_expr(value, cp),
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
            prop_expr(left, cp);
            prop_expr(right, cp);
        }
        Cond::And(a, b) => {
            prop_cond(a, cp);
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
        Expr::CastChar { value, .. } => eval_const_int(value),
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
    cp.g_known.clear();
    cp.ga_known.clear();
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
            if !cp.long_globals.contains(idx)
                && let Some(&k) = cp.g_known.get(idx)
            {
                *e = Expr::IntLit(k);
                cp.substituted = true;
            }
        }
        Expr::Local(idx) => {
            if let Some(&k) = cp.l_known.get(idx)
                && !cp.local_specs.get(*idx).is_some_and(|s| s.is_register)
            {
                *e = Expr::IntLit(k);
                cp.substituted = true;
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
            // Decayed-array pointer arith `a + i` where the index `i` was a
            // VARIABLE at parse (so the parser's `a + <literal>` element-scaling
            // did NOT fire). If const-prop now folds `i` to a constant, scale it
            // by the array element size — otherwise the byte offset is computed in
            // ELEMENTS (fixture 1278: `a + i` (i=1) → bp-6 not bp-7). Literal
            // indices are already byte-scaled at parse and reach here as IntLit,
            // so `right_was_var` keeps them from being scaled twice.
            let right_was_var = !matches!(right.as_ref(), Expr::IntLit(_));
            prop_expr(left, cp);
            prop_expr(right, cp);
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
        Expr::Call { args, .. } | Expr::CallStructField { args, .. } => {
            for a in args {
                prop_expr(a, cp);
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
                                Expr::CastChar { value, .. } => mark_reads(value, cp),
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
        Expr::PreMutateDeref { ptr, .. } | Expr::PostIncDeref { ptr, .. }
        | Expr::PostMutateDeref { ptr, .. } => prop_expr(ptr, cp),
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
        Expr::PtrChainField { .. } => {}
        Expr::StructArrayField { index, .. } => prop_expr(index, cp),
        Expr::IntLit(_) | Expr::Param(_) | Expr::StrLit(_) => {}
    }
}
