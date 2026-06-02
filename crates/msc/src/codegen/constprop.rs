use crate::*;

pub(crate) fn const_prop_globals(
    stmts: &[Stmt],
    local_specs: &[LocalSpec],
    long_globals: &[bool],
) -> (Vec<Stmt>, std::collections::HashSet<usize>, std::collections::HashSet<usize>) {
    let mut cp = ConstProp {
        local_specs: local_specs.to_vec(),
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
        new_stmt
    }).collect();
    (new_stmts, cp.mutated_locals, cp.mutated_globals)
}
pub(crate) fn prop_stmt(stmt: &mut Stmt, cp: &mut ConstProp) {
    match stmt {
        Stmt::Return(e) => prop_expr(e, cp),
        Stmt::ExprStmt(e) => prop_expr(e, cp),
        Stmt::Assign { target, value } => {
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
                AssignTarget::Local(l) => { cp.mutated_locals.insert(*l); }
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
                    // Try to fold the value once more — after
                    // prop_expr's leaf substitution the BinOp may
                    // have two literal operands ready to collapse.
                    if let Some(k) = value.fold(&[]) {
                        *value = Expr::IntLit(k);
                    }
                    if let Expr::IntLit(k) = value {
                        cp.la_known.insert((*local, *byte_off), *k);
                    } else {
                        cp.la_known.remove(&(*local, *byte_off));
                    }
                }
                AssignTarget::IndexedGlobal { array, byte_off }
                | AssignTarget::IndexedGlobalByte { array, byte_off } => {
                    if let Some(k) = value.fold(&[]) {
                        *value = Expr::IntLit(k);
                    }
                    if let Expr::IntLit(k) = value {
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
                    cp.ga_known.clear();
                } else {
                    // No NFC: fold to matched body block. MSC clears the
                    // const-prop tables BEFORE processing the body so that
                    // any inner switches inside the folded arm use runtime
                    // scrutinee loads rather than const-prop'd literals.
                    cp.g_known.clear();
                    cp.l_known.clear();
                    cp.la_known.clear();
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
                    cp.ga_known.clear();
                }
            } else {
                // Runtime scrutinee (not folded): leave as Switch.
                cp.g_known.clear();
                cp.l_known.clear();
                cp.la_known.clear();
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
        local_specs: cp.local_specs.clone(),
    }
}
pub(crate) fn prop_cond(cond: &mut Cond, cp: &mut ConstProp) {
    match cond {
        Cond::Truthy(e) => prop_expr(e, cp),
        Cond::Cmp { left, right, .. } => {
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
        Expr::BinOp { left, right, .. } => {
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
        Expr::PtrIndexByte { index, .. } => {
            prop_expr(index, cp);
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
            // No const-prop tracking for global struct fields yet.
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
                    (*local, 2u16, k)
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
        Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => {
            prop_expr(ptr, cp);
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
