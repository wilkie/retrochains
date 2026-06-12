use crate::*;

thread_local! {
    /// Set to the CURRENTLY-emitting function's `far` flag. A far function's
    /// params sit at `[bp+6..]` (4-byte far return address) and it returns with
    /// `retf` (CB). `param_disp` and `push_epilogue` consult this so neither has
    /// to be threaded through their ~160 call sites. Set at the top of
    /// `emit_function`; each call overwrites the previous function's value.
    static CUR_FN_FAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// True while emitting a `far` function's body.
pub(crate) fn cur_fn_far() -> bool {
    CUR_FN_FAR.with(|c| c.get())
}

/// Returns true if the function body contains any runtime local array
/// access (read or write with non-constant index). These require the SI
/// register to be saved/restored, upgrading the frame to WithSlideSi.
pub(crate) fn body_needs_si(stmts: &[Stmt], local_inits: &[Option<i32>]) -> bool {
    fn stmt_si(s: &Stmt, inits: &[Option<i32>]) -> bool {
        match s {
            Stmt::Assign { target, value } => {
                matches!(target,
                    AssignTarget::IndexedLocalVar { .. }
                    | AssignTarget::IndexedLocalByteVar { .. }
                    | AssignTarget::Index2D { .. }
                    // runtime local struct-array store `a[i].f = v` (a folded
                    // constant index became a plain LocalField in const-prop, so
                    // any surviving target here has a runtime index → SI).
                    | AssignTarget::LocalStructArrayField { .. })
                    || matches!(target, AssignTarget::ParamIndexStore { index, .. } if index.fold(inits).is_none())
                    // `arr[i] = arr[j]` / `arr[i] ± arr[j]` (runtime indices): the
                    // RHS array read uses SI so the LHS index in BX survives.
                    || crate::codegen::assign::global_index_rhs_uses_si(target, value, inits)
                    // `*dst = *src [± K]` copies through a pointer param from a
                    // deref of another param — the source deref uses SI.
                    || matches!(target, AssignTarget::DerefParam(d)
                        if deref_param_src_is_param(value, *d))
                    // `*d++ = *s++` reads the source through SI so dst (BX) survives.
                    || matches!(target, AssignTarget::DerefPostMutateParam { .. }
                        if matches!(value, Expr::PostIncDeref { .. }))
                    // `p->f = q->g` (struct-pointer field copy) reads q->g via SI
                    // so p (BX) survives the store. Fixture 1401.
                    || (matches!(target, AssignTarget::DerefParamField { .. } | AssignTarget::DerefLocalField { .. })
                        && matches!(value, Expr::DerefParamField { .. } | Expr::DerefLocalField { .. }))
                    || expr_si(value, inits)
            }
            Stmt::Return(e) => expr_si(e, inits),
            Stmt::ExprStmt(e) => expr_si(e, inits),
            Stmt::Block(ss) => ss.iter().any(|s| stmt_si(s, inits)),
            Stmt::If { cond, then_branch, else_branch } => {
                cond_si(cond, inits)
                    || stmt_si(then_branch, inits)
                    || else_branch.as_ref().is_some_and(|e| stmt_si(e, inits))
            }
            Stmt::While { cond, body } => cond_si(cond, inits) || stmt_si(body, inits),
            Stmt::DoWhile { body, cond } => stmt_si(body, inits) || cond_si(cond, inits),
            Stmt::For { init, cond, step, body } => {
                stmt_si(init, inits)
                    || cond_si(cond, inits)
                    || stmt_si(step, inits)
                    || stmt_si(body, inits)
            }
            _ => false,
        }
    }
    fn expr_si(e: &Expr, inits: &[Option<i32>]) -> bool {
        match e {
            Expr::LocalIndex { index, .. } | Expr::LocalIndexByte { index, .. } => {
                index.fold(inits).is_none()
            }
            // `a[i]` on a pointer/array PARAM with a runtime index loads the
            // pointer into SI (`mov si,[bp+p]; mov ax,[bx+si]`).
            Expr::ParamIndex { index, .. } => index.fold(inits).is_none(),
            Expr::Index2D { .. } => true,
            // runtime local struct-array read `a[i].f` scales the index into SI.
            Expr::LocalStructArrayField { .. } => true,
            // `*(ptr + i)` (and the nested `*(p + i + j)`) with a runtime index on
            // a pointer param/local uses SI (`mov si,[p]; mov ax,[bx+si]`); a
            // decayed global array uses BX only. add_deref_uses_si peels the
            // add-tree the same way emit_offset_deref does. Fixture 3468.
            Expr::DerefWord { ptr } | Expr::DerefByte { ptr } => {
                if let Expr::BinOp { op: BinOp::Add, left, right } = ptr.as_ref() {
                    add_deref_uses_si(ptr, inits)
                        || expr_si(left, inits) || expr_si(right, inits)
                } else {
                    expr_si(ptr, inits)
                }
            }
            // Two-call binop `f(..) OP g(..)` parks the right result in SI while
            // the left call runs. Fixtures 1277, 1536, 1918.
            Expr::BinOp { op: BinOp::Add | BinOp::Sub, left, right }
                if crate::codegen::expr::is_two_call_binop(left, right, inits) => true,
            Expr::BinOp { left, right, .. } => expr_si(left, inits) || expr_si(right, inits),
            Expr::Call { args, .. } => args.iter().any(|a| expr_si(a, inits)),
            // `ops[i](args)` — a runtime-indexed fn-ptr array call scales the
            // index into SI (`mov si,[i]; shl si,1; call [bp+si+disp]`). Fixture 2435.
            Expr::CallPtr { target, args } => {
                matches!(target.as_ref(), Expr::LocalIndex { index, .. } if index.fold(inits).is_none())
                    || args.iter().any(|a| expr_si(a, inits))
            }
            // `*d++ = *s++` as an expression/condition reads the source via SI.
            Expr::AssignExpr { target, value } => {
                (matches!(target,
                    AssignTarget::DerefPostMutateParam { .. }
                    | AssignTarget::DerefPostMutateLocal { .. })
                    && matches!(value.as_ref(), Expr::PostIncDeref { .. }))
                    || expr_si(value, inits)
            }
            _ => false,
        }
    }
    // Does an offset-deref of `base` use SI? Mirrors emit_offset_deref's peel:
    // a nested `(p + i) + j` flattens to a Param/Local pointer base with a
    // runtime index sum (→ SI); a constant index uses a disp instead, and a
    // decayed global array (AddrOfGlobal base) uses BX only.
    fn add_deref_uses_si(base: &Expr, inits: &[Option<i32>]) -> bool {
        fn peel(base: &Expr, idx_runtime: bool, inits: &[Option<i32>]) -> bool {
            match base {
                Expr::BinOp { op: BinOp::Add, left, right } => {
                    let r_runtime = right.fold(inits).is_none();
                    peel(left, idx_runtime || r_runtime, inits)
                }
                Expr::Param(_) | Expr::Local(_) => idx_runtime,
                _ => false,
            }
        }
        peel(base, false, inits)
    }
    fn cond_si(c: &Cond, inits: &[Option<i32>]) -> bool {
        match c {
            Cond::Truthy(e) => expr_si(e, inits),
            Cond::Cmp { left, right, .. } => expr_si(left, inits) || expr_si(right, inits),
            Cond::And(a, b) | Cond::Or(a, b) => cond_si(a, inits) || cond_si(b, inits),
        }
    }
    stmts.iter().any(|s| stmt_si(s, local_inits))
}
/// True if the body contains a 3-call add/sub chain (`f()+g()+h()`), which the
/// scheduler parks in SI and DI → the frame must save both. Fixtures 2305, 2343.
pub(crate) fn body_needs_di_si(stmts: &[Stmt], inits: &[Option<i32>]) -> bool {
    fn expr_has(e: &Expr, inits: &[Option<i32>]) -> bool {
        match e {
            Expr::BinOp { op: BinOp::Add | BinOp::Sub, left, right }
                if crate::codegen::expr::is_three_call_binop(left, right, inits) => true,
            Expr::BinOp { left, right, .. } => expr_has(left, inits) || expr_has(right, inits),
            _ => false,
        }
    }
    fn stmt_has(s: &Stmt, inits: &[Option<i32>]) -> bool {
        match s {
            Stmt::Return(e) | Stmt::ExprStmt(e) => expr_has(e, inits),
            Stmt::Assign { value, .. } => expr_has(value, inits),
            Stmt::Block(ss) => ss.iter().any(|s| stmt_has(s, inits)),
            Stmt::If { then_branch, else_branch, .. } =>
                stmt_has(then_branch, inits) || else_branch.as_ref().is_some_and(|e| stmt_has(e, inits)),
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => stmt_has(body, inits),
            Stmt::For { init, step, body, .. } => stmt_has(init, inits) || stmt_has(step, inits) || stmt_has(body, inits),
            _ => false,
        }
    }
    stmts.iter().any(|s| stmt_has(s, inits))
}
/// Structural check matching `deref_param_source`: is `value` a `*Param(s)` or
/// `*Param(s) ± K` with `s != dst`? Used by body_needs_si (no pointee-width
/// info available here; the corpus never mixes widths so the structural test
/// agrees with the codegen path).
fn deref_param_src_is_param(value: &Expr, dst: usize) -> bool {
    fn src_of(e: &Expr) -> Option<usize> {
        match e {
            Expr::DerefWord { ptr } | Expr::DerefByte { ptr } => match ptr.as_ref() {
                Expr::Param(s) => Some(*s),
                _ => None,
            },
            _ => None,
        }
    }
    let s = match value {
        Expr::BinOp { op: BinOp::Add | BinOp::Sub, left, .. } => src_of(left),
        _ => src_of(value),
    };
    matches!(s, Some(s) if s != dst)
}
/// Resolve a statement to the one actually emitted, following constant-folding
/// `if`s to their taken branch (and unwrapping single-statement blocks). A
/// `if (const) <single-call>;` therefore looks like that single call to the
/// cleanup-elision arming. Uses the SAME `fold_cond` the emitter uses, so the
/// resolution can't disagree with what is actually emitted. Fixture 1687.
fn effective_stmt<'a>(stmt: &'a Stmt, locals: &Locals<'_>) -> &'a Stmt {
    const EMPTY: Stmt = Stmt::Empty;
    let mut s = stmt;
    loop {
        match s {
            Stmt::If { cond, then_branch, else_branch } => {
                match crate::codegen::statements::fold_cond(cond, locals) {
                    Some(k) if k != 0 => s = then_branch,
                    Some(_) => match else_branch {
                        Some(eb) => s = eb,
                        None => return &EMPTY,
                    },
                    None => return s,
                }
            }
            Stmt::Block(v) if v.len() == 1 => s = &v[0],
            _ => return s,
        }
    }
}
/// True when `Local(idx)` is consumed as a BARE value somewhere in `stmts`:
/// the whole `return` value, the whole RHS of an assignment, or a whole call
/// argument. A `register` local with such a read must materialize its value
/// into SI/DI (so its const init `mov si,K` is live); a register read that
/// appears only inside a foldable arithmetic expression or condition folds to
/// its known constant and leaves the init dead. Fixtures 1550 (bare `return x`
/// materializes) vs 1560/2069 (folded uses → init elided).
fn local_read_bare(stmts: &[Stmt], idx: usize) -> bool {
    fn is_bare(e: &Expr, idx: usize) -> bool { matches!(e, Expr::Local(i) if *i == idx) }
    fn args_have_bare(args: &[Expr], idx: usize) -> bool { args.iter().any(|a| is_bare(a, idx)) }
    fn expr_has_bare_arg(e: &Expr, idx: usize) -> bool {
        match e {
            Expr::Call { args, .. } => args_have_bare(args, idx),
            _ => false,
        }
    }
    fn stmt_scan(s: &Stmt, idx: usize) -> bool {
        match s {
            Stmt::Return(e) => is_bare(e, idx) || expr_has_bare_arg(e, idx),
            Stmt::Assign { target, value } => {
                (!matches!(target, AssignTarget::Local(t) if *t == idx) && is_bare(value, idx))
                    || expr_has_bare_arg(value, idx)
            }
            Stmt::ExprStmt(e) => expr_has_bare_arg(e, idx),
            Stmt::If { then_branch, else_branch, .. } =>
                stmt_scan(then_branch, idx) || else_branch.as_ref().is_some_and(|e| stmt_scan(e, idx)),
            Stmt::Block(ss) => ss.iter().any(|s| stmt_scan(s, idx)),
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => stmt_scan(body, idx),
            Stmt::For { init, step, body, .. } =>
                stmt_scan(init, idx) || stmt_scan(step, idx) || stmt_scan(body, idx),
            _ => false,
        }
    }
    stmts.iter().any(|s| stmt_scan(s, idx))
}
/// Return the mod=01 or mod=10 modrm byte for `[bp + disp]`.
/// When `disp` fits in i8 (-128..=127) use mod=01 (1-byte disp, modrm as-is).
/// Otherwise use mod=10 (2-byte disp, modrm + 0x40 to flip the mod field).
#[inline]
pub(crate) fn bp_modrm(modrm_mod01: u8, disp: i16) -> u8 {
    if (-128..=127).contains(&disp) { modrm_mod01 } else { modrm_mod01 + 0x40 }
}
/// Push 1 or 2 displacement bytes for a `[bp + disp]` operand.
#[inline]
pub(crate) fn push_bp_disp(buf: &mut Vec<u8>, disp: i16) {
    if (-128..=127).contains(&disp) {
        buf.push(disp as u8);
    } else {
        let b = (disp as u16).to_le_bytes();
        buf.push(b[0]);
        buf.push(b[1]);
    }
}
/// The float local whose value a `return` consumes from x87 st(0) for the
/// FpuStack coupling. Recognizes a bare local (`(int)f`), `0 - local` (unary
/// negation → `fchs`), and `local <op> <floatlit>` (→ `fadd`/`fsub`/…).
pub(crate) fn coupled_return_local(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Local(i) => Some(*i),
        Expr::BinOp { op: BinOp::Sub, left, right }
            if matches!(left.as_ref(), Expr::IntLit(0)) =>
        {
            if let Expr::Local(i) = right.as_ref() { Some(*i) } else { None }
        }
        Expr::BinOp { left, right, .. } if matches!(right.as_ref(), Expr::FloatLit(..)) => {
            if let Expr::Local(i) = left.as_ref() { Some(*i) } else { None }
        }
        _ => None,
    }
}
pub(crate) fn emit_function(
    func: &Function,
    struct_temp_offset: Option<u16>,
    long_globals: &[bool],
    char_globals: &[bool],
    unsigned_globals: &[bool],
    float_globals: &[usize],
    global_elem_sizes: &[usize],
    char_returners: &std::collections::HashSet<String>,
    long_returners: &std::collections::HashSet<String>,
    pascal_fns: &std::collections::HashSet<String>,
    static_fns: &std::collections::HashSet<String>,
    far_fns: &std::collections::HashSet<String>,
    variadic_fns: &std::collections::HashSet<String>,
    float_returners_arg: &std::collections::HashMap<String, usize>,
    long_param_funcs: &std::collections::HashMap<String, Vec<bool>>,
    struct_param_funcs: &std::collections::HashMap<String, Vec<usize>>,
    struct_return_funcs: &std::collections::HashMap<String, usize>,
    struct_is_union: &[bool],
    union_globals: &std::collections::HashSet<usize>,
) -> FunctionEmit {
    // Record this function's far-ness so `param_disp` shifts params to [bp+6..]
    // and `push_epilogue` emits `retf` (CB) instead of `ret` (C3).
    CUR_FN_FAR.with(|c| c.set(func.is_far));
    let (body, mutated_locals, loop_mutated_locals, _mutated_globals) = const_prop_globals(&func.body, &func.locals, long_globals, global_elem_sizes, struct_is_union, union_globals);
    // Splice top-level blocks that contain a switch into the top-level
    // statement stream, so a switch inside a const-folded outer switch's
    // body reaches the partial-switch continuation machinery below
    // (fixture 2641: nested switch in a folded arm). Gated narrowly to
    // switch-bearing blocks to leave every other layout untouched.
    let (body, from_block): (Vec<Stmt>, Vec<bool>) = {
        fn splice(stmts: Vec<Stmt>, depth: usize, out: &mut Vec<(Stmt, bool)>) {
            for s in stmts {
                match s {
                    Stmt::Block(inner)
                        if inner.iter().any(|t| matches!(t, Stmt::Switch { .. })) =>
                    {
                        splice(inner, depth + 1, out);
                    }
                    other => out.push((other, depth > 0)),
                }
            }
        }
        let mut flat: Vec<(Stmt, bool)> = Vec::with_capacity(body.len());
        splice(body, 0, &mut flat);
        flat.into_iter().unzip()
    };
    // Extract a `Vec<Option<i32>>` view for the existing fold path —
    // saves rewriting every codegen helper to know about LocalSpec.
    // Strip the init for any local that was mutated during the
    // const-prop walk — the static `spec.init` no longer reflects
    // the runtime value, so fold(locals.inits) must miss for it.
    // Fixture 1154 (`int b = a++; return a + b;`).
    // Only expose init values for locals whose init is a compile-time
    // constant: pure literal, chained literal, or an identity-operation
    // of a literal-init local (e.g. `int a = x << 0` where x=5).
    // MSC does NOT fold type-cast inits (`int i = (int)sc`, fixture 1732)
    // or derived arithmetic (`int sum = x + y`, fixture 2126).
    // These cases leave `init_is_literal = false` via the parser's logic.
    // The loop ENTRY-fold view: keeps inits for locals only mutated inside a
    // loop, so the while → do-while elision still folds the entry test from
    // the declared init (fixtures 126/1044/1592). Everything else uses
    // `local_inits`, which also drops loop-mutated locals so post-loop reads
    // hit the slot (fixture 3478).
    let entry_inits: Vec<Option<i32>> = func.locals.iter().enumerate()
        .map(|(i, l)| if mutated_locals.contains(&i) || !l.init_is_literal { None } else { l.init })
        .collect();
    let local_inits: Vec<Option<i32>> = func.locals.iter().enumerate()
        .map(|(i, l)| {
            if mutated_locals.contains(&i) || loop_mutated_locals.contains(&i) || !l.init_is_literal {
                None
            } else {
                l.init
            }
        })
        .collect();
    let local_long: Vec<bool> = func.locals.iter().map(|l| l.is_long).collect();
    let local_literals: Vec<bool> = func.locals.iter().map(|l| l.init_is_literal).collect();
    let local_far_ptrs: Vec<bool> = func.locals.iter().map(|l| l.is_far_ptr).collect();
    let local_arrays: Vec<bool> = func.locals.iter().map(|l| l.array_len > 1).collect();
    let local_unsigned: Vec<bool> = func.locals.iter().map(|l| l.is_unsigned).collect();
    let local_pointee: Vec<usize> = func.locals.iter().map(|l| if l.array_len > 1 { 0 } else { l.pointee_size as usize }).collect();
    let local_float: Vec<usize> = func.locals.iter().map(|l| if l.is_float { l.size } else { 0 }).collect();
    let fpu_live: std::cell::Cell<Option<usize>> = std::cell::Cell::new(None);
    let fpu_pending_fwait: std::cell::Cell<bool> = std::cell::Cell::new(false);
    // A `pascal` function pops its arguments in the epilogue (`ret N`); N is the
    // total stack footprint of the params (long=4, struct=even-padded, else 2).
    let pascal_cleanup: u16 = if func.is_pascal {
        (0..func.params.len()).map(|i| {
            if func.param_is_long.get(i).copied().unwrap_or(false) { 4u16 }
            else if func.param_struct_bytes.get(i).copied().unwrap_or(0) > 0 {
                ((func.param_struct_bytes[i] + 1) & !1) as u16
            } else { 2 }
        }).sum()
    } else { 0 };
    let mut bytes = Vec::with_capacity(32);
    let mut fixups: Vec<Fixup> = Vec::new();
    // A call that passes a float/double literal pushes it via `sub sp,N;
    // mov bx,sp; fstp [bx]`, sliding SP — so the function needs a WithSlide
    // frame (cleanup via `mov sp,bp`) plus a 2-byte temp for the call result.
    let has_float_arg_call = func_has_float_arg_call(func);
    // A `make().field` call spills its result to a frame temp sized by chkstk, so
    // the function needs a WithSlide frame even with no declared locals (2629).
    let base_frame = if has_float_arg_call || func.struct_field_temp_count > 0 {
        Frame::WithSlide
    } else {
        Frame::for_function(func)
    };
    // Receiving a float/double return (`double d = f();`) copies 8 bytes from
    // __fac via `movsw`, needing DI and SI saved → WithSlideDiSi.
    let receives_float_return = body.iter().any(|s| matches!(s,
        Stmt::Assign { target: AssignTarget::Local(_), value: Expr::Call { name, .. } }
            if float_returners_arg.contains_key(&symbol_name(name))));
    // `return (int)<float-returning call>` receives the result into a hidden
    // 8-byte temp, then `fld; call __ftol`. Same di+si copy frame.
    let returns_float_call = body.iter().any(|s| matches!(s,
        Stmt::Return(Expr::Call { name, .. })
            if float_returners_arg.contains_key(&symbol_name(name))));
    // Upgrade to WithSlideSi when the body uses runtime local array
    // indexing (SI register must be caller-saved).
    // Returning a struct > 4 bytes copies it to the `_BSS` temp via `movsw`,
    // needing DI+SI saved → WithSlideDiSi. Same for a caller RECEIVING a >4-byte
    // struct return (`dest = f()` → movsw from the temp).
    let receives_big_struct = body.iter().any(|s| matches!(s,
        Stmt::Assign { target: AssignTarget::Local(_) | AssignTarget::Global(_), value: Expr::Call { name, .. } }
            if struct_return_funcs.get(&symbol_name(name)).copied().unwrap_or(0) > 4));
    // A whole-struct copy `g1 = g2;` of >4 bytes lowers to `rep movsw`, needing
    // DI+SI saved. Fixture 3612.
    let copies_big_struct = body.iter().any(|s| matches!(s,
        Stmt::Assign { target: AssignTarget::StructGlobalCopy { bytes, .. } | AssignTarget::StructLocalCopy { bytes, .. }, .. }
            if *bytes > 4));
    let returns_big_struct = func.return_struct_bytes > 4 || receives_big_struct || copies_big_struct;
    // `register int` locals are accessed via SI (1st) / DI (2nd), saved across
    // the call. The locals still reserve their stack slots (so base_frame is
    // already WithSlide), but the frame upgrades to push/pop the register(s).
    // Fixtures 1550 (1 reg → SI), 2069 (2 regs → DI+SI).
    let reg_locals: Vec<usize> = func.locals.iter().enumerate()
        .filter(|(_, l)| l.is_register).map(|(i, _)| i).collect();
    let si_local = reg_locals.first().copied();
    let di_local = reg_locals.get(1).copied();
    let frame = if reg_locals.len() >= 2 {
        Frame::WithSlideDiSi
    } else if reg_locals.len() == 1 {
        Frame::WithSlideSi
    } else if returns_big_struct && matches!(base_frame, Frame::None) {
        // No locals (e.g. `return <global>`) — keep the bp-less prologue but add
        // di/si for the movsw copy.
        Frame::NoneDiSi
    } else if receives_float_return || returns_float_call || returns_big_struct {
        Frame::WithSlideDiSi
    } else if matches!(base_frame, Frame::WithSlide)
        && body_needs_di_si(&body, &local_inits)
    {
        Frame::WithSlideDiSi
    } else if matches!(base_frame, Frame::None)
        && body_needs_di_si(&body, &local_inits)
    {
        Frame::NoneDiSi
    } else if matches!(base_frame, Frame::WithSlide)
        && body_needs_si(&body, &local_inits)
    {
        Frame::WithSlideSi
    } else if matches!(base_frame, Frame::BpOnly)
        && body_needs_si(&body, &local_inits)
    {
        Frame::BpOnlySi
    } else if matches!(base_frame, Frame::None)
        && body_needs_si(&body, &local_inits)
    {
        Frame::NoneSi
    } else {
        base_frame
    };
    // Frame layout: every function-scope local (pointers, far pointers,
    // arrays, structs alike) is ordered by MSC's symbol-table hash-bucket
    // traversal:
    //   bucket = (sum_of_name_bytes % 511) % 16, iterated ascending —
    //   lower bucket → closer to BP (smaller absolute displacement).
    // The % 511 (2^9-1, an end-around-carry fold) matters only for names
    // whose byte sum reaches 512 (e.g. `count`, `result`); shorter names
    // reduce to sum % 16. Within a bucket the chain is LIFO (linked-list
    // prepend), so the last-declared variable in a bucket gets the
    // shallowest slot. Derived empirically from the full fixture corpus
    // (gold `; var = -N` layout comments): the single-pass rule fits every
    // function-scope layout, including pointer/array interleavings such as
    // `cp,ca,ip,ia` (1778) and `p1,a,p2` (1773).
    // Block-level locals (declared inside a nested `{ ... }`) are laid out
    // separately, DEEPER than the function frame, by a dedicated pass below.
    let mut local_disps: Vec<i16> = vec![0i16; func.locals.len()];
    let mut cumulative: i32 = 0;
    let name_hash = |i: usize| -> u32 {
        let h: u32 = func.local_names[i].bytes().map(|b| b as u32).sum();
        (h % 511) % 16
    };
    {
        let mut indices: Vec<usize> = func.locals.iter().enumerate()
            .filter(|(_, s)| s.block_offset.is_none())
            .map(|(i, _)| i)
            .collect();
        indices.sort_by(|&a, &b| name_hash(a).cmp(&name_hash(b)).then(b.cmp(&a)));
        for i in indices {
            let spec = &func.locals[i];
            cumulative += spec.storage_bytes() as i32;
            local_disps[i] = -i16::try_from(cumulative).expect("local disp fits");
        }
    }
    // Block-local pass: each block local sits at `-(F + block_offset)`, where
    // `F = cumulative` is the function frame size computed above. `block_offset`
    // is the parser-assigned cumulative depth (with slot reuse across sibling
    // blocks); the high-water mark extends the frame below `F`.
    let f = cumulative;
    let mut block_frame_max: i32 = 0;
    for (i, spec) in func.locals.iter().enumerate() {
        if let Some(off) = spec.block_offset {
            local_disps[i] = -i16::try_from(f + off as i32).expect("block local disp fits");
            block_frame_max = block_frame_max.max(off as i32);
        }
    }
    cumulative = f + block_frame_max;
    let local_sizes: Vec<usize> = func.locals.iter().map(|l| l.size).collect();
    // The float-arg call result is spilled to a 2-byte temp at [bp-2]; reserve
    // it at the deepest frame slot so chkstk sizes the frame to include it.
    // `make().field` temps occupy 4 bytes each, just below the locals, allocated
    // in source order (temp #0 shallowest). temp #0's disp = -(cumulative + 4).
    let struct_field_temp_base: i16 = if func.struct_field_temp_count > 0 {
        -i16::try_from(cumulative + 4).expect("struct-field temp disp fits")
    } else {
        0
    };
    let frame_bytes: usize = cumulative as usize
        + 4 * func.struct_field_temp_count as usize
        + if has_float_arg_call { 2 } else { 0 }
        + if returns_float_call { 8 } else { 0 };
    // The float-call-return temp is the deepest 8-byte slot.
    let float_call_temp_disp: i16 = if returns_float_call {
        -i16::try_from(frame_bytes).expect("frame fits")
    } else {
        0
    };

    match frame {
        Frame::None | Frame::NoneDiSi | Frame::NoneSi => bytes.extend_from_slice(&[0x33, 0xC0]),
        Frame::BpOnly | Frame::BpOnlySi => bytes.extend_from_slice(&[0x55, 0x8B, 0xEC, 0x33, 0xC0]),
        Frame::WithSlide | Frame::WithSlideSi | Frame::WithSlideDiSi => {
            bytes.extend_from_slice(&[0x55, 0x8B, 0xEC]);
            bytes.push(0xB8);
            bytes.extend_from_slice(
                &u16::try_from(frame_bytes)
                    .expect("frame fits in u16")
                    .to_le_bytes(),
            );
        }
    }
    // Call to __chkstk — emit a placeholder `e8 00 00` and record
    // a Fixup so the post-pass knows to write the EXTDEF-relative
    // FIXUPP record (chkstk is always external; resolved via the
    // OMF FIXUPP machinery, not in-band).
    let body_offset = bytes.len();
    bytes.extend_from_slice(&[0xE8, 0x00, 0x00]);
    fixups.push(Fixup {
        body_offset,
        kind: FixupKind::ExtCall { target: "__chkstk".to_owned() },
    });
    // WithSlideDiSi / NoneDiSi: save DI then SI for the `movsw` copy.
    if matches!(frame, Frame::WithSlideDiSi | Frame::NoneDiSi) {
        bytes.push(0x57); // push di
        bytes.push(0x56); // push si
    }
    // For WithSlideSi / BpOnlySi / NoneSi: save SI after the frame is established.
    if matches!(frame, Frame::WithSlideSi | Frame::BpOnlySi | Frame::NoneSi) {
        bytes.push(0x56); // push si
    }

    // FpuStack coupling: the float local returned via `(int)<local>` is stored
    // with `fst` (st(0) kept live) so the cast is a bare `call __ftol` — but
    // only when it is the LAST float local initialized (otherwise a later float
    // init would have clobbered st(0)). Literal float locals fold to `mov ax,K`
    // via const-prop, so they never reach here as `Return(Local)`.
    // A float local is coupled (st(0) kept live via `fst`) when its `(int)` read
    // would NOT const-fold: either its init wasn't a direct literal, or it was
    // mutated (compound FP assign) so the fold view dropped its init.
    let returned_float_local = body.iter().rev().find_map(|s| match s {
        Stmt::Return(e) => coupled_return_local(e),
        _ => None,
    }).filter(|&i| func.locals.get(i).map(|l| {
        l.is_float && (!l.init_is_literal || mutated_locals.contains(&i))
    }).unwrap_or(false));
    let last_float_init = func.locals.iter().enumerate()
        .filter(|(_, l)| l.is_float && l.float_bits.is_some())
        .map(|(i, _)| i)
        .next_back();
    let coupled_float_local = match (returned_float_local, last_float_init) {
        (Some(r), Some(l)) if r == l => Some(r),
        _ => None,
    };
    // Whether the first op after the prologue float inits is itself an FP
    // instruction — then no `fwait` is needed after the last float init. True
    // for a coupled `(int)<local>` (→ `call __ftol`) and for a float `return`
    // (→ `fld …; fstp __fac`).
    let body_starts_fp = coupled_float_local.is_some()
        || (func.return_float_width != 0
            && matches!(body.first(), Some(Stmt::Return(Expr::FloatLit(..)))));

    // Initialized-local writes — `int x = K;` → `c7 46 disp lo hi`;
    // `char x = K;` → `c6 46 disp imm8`. Arrays don't get a constant
    // init here; their element stores live in the prelude body the
    // parser synthesized.
    for (i, spec) in func.locals.iter().enumerate() {
        // Float/double locals: load the const-pool value, store it to the slot:
        //   9B <D9|DD> 06 <off16>   fld  <dword|qword> [$T]   (FIDRQQ + FloatLoad)
        //   9B <D9|DD> 5E/56 <disp> fstp/fst <dword|qword> [bp+disp]   (FIDRQQ)
        // The coupled local (consumed by `(int)<local>`) uses `fst` (5E→56) to
        // keep st(0) live. A standalone `fwait` (`90 9B`, FIWRQQ — the 8087
        // emulator's 2-byte patch slot) follows only when the NEXT emitted op
        // is non-FP: i.e. this is the last float init and it is NOT coupled (a
        // coupled value is consumed by the body's `call __ftol`). Consecutive
        // float inits need no fwait between them.
        if spec.is_float {
            if let Some(bits) = spec.float_bits {
                let disp = local_disps[i];
                let op = if spec.size == 4 { 0xD9u8 } else { 0xDDu8 };
                let coupled = coupled_float_local == Some(i);
                // Shared-load peephole: an adjacent preceding float local with
                // the same (bits, width) already loaded the value onto st(0)
                // (it used `fst`), so this store reuses it without a new `fld`.
                let same = |j: usize| func.locals.get(j)
                    .map(|l| l.is_float && l.float_bits == Some(bits) && l.size == spec.size)
                    .unwrap_or(false);
                let prev_same = i > 0 && same(i - 1);
                let next_same = same(i + 1);
                if !prev_same {
                    // fld <width> [$T]
                    fixups.push(Fixup { body_offset: bytes.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                    bytes.push(0x9B);
                    bytes.push(op);
                    bytes.push(0x06);
                    let body_offset = bytes.len() - 1; // the 06 modrm; off16 at +1
                    bytes.extend_from_slice(&[0x00, 0x00]);
                    fixups.push(Fixup { body_offset, kind: FixupKind::FloatLoad { bits, width: spec.size } });
                }
                // `fst` keeps st(0) live — used when coupled (the cast consumes
                // it) or when the next adjacent local shares this value.
                let keep = coupled || next_same;
                fixups.push(Fixup { body_offset: bytes.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                bytes.push(0x9B);
                bytes.push(op);
                bytes.push(bp_modrm(if keep { 0x56 } else { 0x5E }, disp));
                push_bp_disp(&mut bytes, disp);
                if coupled {
                    fpu_live.set(Some(i));
                }
                // fwait only when the next op is non-FP.
                let more_floats_after = func.locals[i + 1..].iter()
                    .any(|l| l.is_float && l.float_bits.is_some());
                if !more_floats_after && !body_starts_fp {
                    fixups.push(Fixup { body_offset: bytes.len(), kind: FixupKind::FloatMarker { target: "FIWRQQ" } });
                    bytes.push(0x90); // nop — emulator patch slot
                    bytes.push(0x9B); // fwait
                }
            }
            continue;
        }
        if let Some(value) = spec.init {
            // A `register` local lives in SI/DI, not its stack slot — its init
            // materializes the value INTO the register: `mov si,K` (or `sub si,si`
            // for 0), never a slot store. The init is ELIDED when the register is
            // never materialized at runtime (all reads fold to its constant) —
            // 1560 (`if(x)`) / 2069 (`return x+y`). A bare read or a runtime
            // mutation keeps it live — 1550 (`return x`).
            if spec.is_register {
                if !(mutated_locals.contains(&i) || local_read_bare(&body, i)) {
                    continue;
                }
                let reg = if di_local == Some(i) { 7u8 } else { 6u8 };
                if value == 0 {
                    bytes.extend_from_slice(&[0x2B, 0xC0 | (reg << 3) | reg]); // sub reg,reg
                } else {
                    bytes.push(0xB8 | reg); // mov reg, imm16
                    bytes.extend_from_slice(&((value as u32 & 0xFFFF) as u16).to_le_bytes());
                }
                continue;
            }
            let disp = local_disps[i];
            if spec.is_long {
                let low = (value as u32 & 0xFFFF) as u16;
                let high = (((value as i32) >> 16) as u32 & 0xFFFF) as u16;
                if low == 0 && high == 0 {
                    // Zero-init peephole: `xor ax, ax; mov [bp-d_hi],
                    // ax; mov [bp-d_lo], ax` — 8 bytes vs 10 for the
                    // two `c7 46` stores. Fixture 1737.
                    bytes.extend_from_slice(&[0x2B, 0xC0]);
                    bytes.push(0x89);
                    bytes.push(bp_modrm(0x46, disp + 2));
                    push_bp_disp(&mut bytes, disp + 2);
                    bytes.push(0x89);
                    bytes.push(bp_modrm(0x46, disp));
                    push_bp_disp(&mut bytes, disp);
                } else {
                    // Long: two word stores. `c7 46 disp_low <lo>`
                    // then `c7 46 disp_high <hi>`.
                    bytes.push(0xC7);
                    bytes.push(bp_modrm(0x46, disp));
                    push_bp_disp(&mut bytes, disp);
                    bytes.extend_from_slice(&low.to_le_bytes());
                    bytes.push(0xC7);
                    bytes.push(bp_modrm(0x46, disp + 2));
                    push_bp_disp(&mut bytes, disp + 2);
                    bytes.extend_from_slice(&high.to_le_bytes());
                }
            } else if spec.size == 1 {
                let imm = (value as u32 & 0xFF) as u8;
                if spec.init_via_cast {
                    // Explicit `(char)` cast init: MSC evaluates through AL.
                    // `b0 imm8; 88 46 disp` — fixture 1039.
                    bytes.push(0xB0);
                    bytes.push(imm);
                    bytes.push(0x88);
                    bytes.push(bp_modrm(0x46, disp));
                    push_bp_disp(&mut bytes, disp);
                } else {
                    // Direct / chained literal init: compact byte-store.
                    // `c6 46 disp imm8` — fixtures 1040, 1045, 1046, etc.
                    bytes.push(0xC6);
                    bytes.push(bp_modrm(0x46, disp));
                    push_bp_disp(&mut bytes, disp);
                    bytes.push(imm);
                }
            } else {
                let imm = (value as u32 & 0xFFFF) as u16;
                bytes.push(0xC7);
                bytes.push(bp_modrm(0x46, disp));
                push_bp_disp(&mut bytes, disp);
                bytes.extend_from_slice(&imm.to_le_bytes());
            }
        }
    }

    let loop_stack: std::cell::RefCell<Vec<LoopCtx>> = std::cell::RefCell::new(Vec::new());
    let labels: std::cell::RefCell<std::collections::HashMap<String, usize>> = std::cell::RefCell::new(std::collections::HashMap::new());
    let label_fixups: std::cell::RefCell<Vec<(String, usize)>> = std::cell::RefCell::new(Vec::new());
    let param_is_char: Vec<bool> = func.param_is_char.clone();
    let param_is_long: Vec<bool> = func.param_is_long.clone();
    let param_is_unsigned: Vec<bool> = func.param_is_unsigned.clone();
    let param_float_widths: Vec<usize> = func.param_float_width.clone();
    let param_pointee_sizes: Vec<usize> = func.param_pointee_size.clone();
    let locals_view = Locals {
        inits: &local_inits,
        entry_inits: &entry_inits,
        disps: &local_disps,
        sizes: &local_sizes,
        long_globals,
        char_globals,
        global_elem_sizes,
        unsigned_globals,
        float_globals,
        long_locals: &local_long,
        init_literals: &local_literals,
        far_ptr_locals: &local_far_ptrs,
        array_locals: &local_arrays,
        unsigned_locals: &local_unsigned,
        local_pointee_sizes: &local_pointee,
        float_locals: &local_float,
        char_params: &param_is_char,
        param_struct_bytes: &func.param_struct_bytes,
        long_params: &param_is_long,
        unsigned_params: &param_is_unsigned,
        param_float_widths: &param_float_widths,
        param_pointee_sizes: &param_pointee_sizes,
        param_struct_ptr_bytes: &func.param_struct_ptr_bytes,
        int_cast_ptrs: &func.int_cast_ptrs,
        char_returners,
        long_returners,
        pascal_fns,
        static_fns,
        far_fns,
        variadic_fns,
        pascal_cleanup,
        si_local,
        di_local,
        long_param_funcs,
        struct_param_funcs,
        struct_return_funcs,
        float_returners: float_returners_arg,
        loop_stack: &loop_stack,
        labels: &labels,
        label_fixups: &label_fixups,
        fpu_live: &fpu_live,
        return_float_width: func.return_float_width,
        return_struct_bytes: func.return_struct_bytes,
        struct_temp_bss_offset: struct_temp_offset,
        float_call_temp_disp,
        fpu_pending_fwait: &fpu_pending_fwait,
        struct_field_temp_base,
        elide_call_cleanup: std::cell::Cell::new(false),
        ternary_tail_epilogue: std::cell::RefCell::new(None),
        last_branch_barrier: std::cell::Cell::new(0),
        merge_barrier: std::cell::Cell::new(None),
        last_top_stmt: std::cell::Cell::new(false),
        final_top_stmt: std::cell::Cell::new(false),
    };

    // The last top-level call-bearing statement: if it carries exactly one call
    // and the function uses a slide frame, that call's cdecl cleanup is elided
    // (the epilogue's `mov sp,bp` restores SP). Earlier calls keep their cleanup.
    let last_call_idx: Option<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, s)| crate::codegen::statements::stmt_call_count(effective_stmt(s, &locals_view)) > 0)
        .last()
        .map(|(i, _)| i);

    let mut reachable = true;
    let mut i = 0;
    while i < body.len() {
        // A label is a join point (a goto may target it), so it revives
        // reachability even after a return / unconditional goto.
        if matches!(&body[i], Stmt::Label(_)) { reachable = true; }
        if !reachable { i += 1; continue; }
        let stmt = &body[i];

        // Arm/disarm the last-call cleanup elision for this statement.
        locals_view.elide_call_cleanup.set(
            frame.is_with_slide()
                && last_call_idx == Some(i)
                && crate::codegen::statements::stmt_single_call(effective_stmt(stmt, &locals_view)),
        );

        // Detect a partial switch (const literal scrutinee with NFC cases).
        // emit_partial_switch_with_continuation handles the switch + all
        // remaining statements in one shot and returns whether the function
        // is still reachable after the continuation.
        if let Stmt::Switch { scrutinee, cases } = stmt {
            use crate::codegen::statements::{
                arm_body_terminates, build_chain_ops, fold_chain_ops, ChainFold, TestOp,
            };
            let known_k = if let Expr::IntLit(k) = scrutinee { Some(*k) } else { None };
            let all_ops = build_chain_ops(cases);
            let fold = match known_k {
                Some(k) => fold_chain_ops(all_ops, k),
                None => ChainFold { ops: all_ops, fallin_arm: None },
            };
            // The fall-through position: the matched arm when a test resolved
            // taken, else the default arm (the chain's final `jmp default`
            // elides — its body falls in; fixtures 133/554/1599/1606), else
            // empty (the chain's final jmp goes to the continuation; 1896/454).
            let ft = fold.fallin_arm
                .or_else(|| cases.iter().position(|a| a.value.is_none()));
            // A live body that breaks (or falls off the switch) reaches the
            // continuation, which the partial layout owns. All-returning
            // bodies with no fall-through arm lay out as a plain chain
            // (fixtures 3965/2620/3967) — handled by emit_runtime_switch.
            let live_nonterm = fold.ops.iter().any(|op| match op {
                TestOp::JeBody { arm, .. } | TestOp::JleBody { arm, .. } =>
                    !arm_body_terminates(cases, *arm, &locals_view),
                TestOp::JlDefault { .. } => false,
            }) || matches!(ft, Some(a) if !arm_body_terminates(cases, a, &locals_view));
            // Runtime scrutinees use this layout only when a live body breaks
            // out (fixture 454); all-terminating and default-fallthrough
            // shapes keep the plain emit_runtime_switch layout (1281, 3350),
            // as do its special forms (empty / default-only / single-case-jne
            // — all break-free).
            let use_partial = !crate::codegen::statements::switch_is_table(cases)
                && !fold.ops.is_empty()
                && match known_k {
                    Some(_) => ft.is_some() || live_nonterm,
    // Runtime: break-out bodies use the FWD continuation
                    // layout (454) — but only for lexically top-level
                    // switches; one spliced out of a folded outer switch
                    // keeps the plain sequential layout (2369). A lone
                    // terminating case body from a folded block lays the
                    // continuation at the chain fall-through with the body
                    // after it (DIRECT — 2641); at top level it keeps the
                    // jne form (3082). All-terminating multi-case switches
                    // keep the plain chain (3084).
                    None => (live_nonterm
                        && !from_block[i]
                        && crate::codegen::statements::switch_has_break(cases))
                        || (from_block[i]
                            && cases.len() == 1
                            && ft.is_none()
                            && arm_body_terminates(cases, 0, &locals_view)),
                };
            if use_partial {
                let cont_reachable = emit_partial_switch_with_continuation(
                    scrutinee,
                    known_k,
                    &fold,
                    ft,
                    cases,
                    &body[i + 1..],
                    &locals_view,
                    frame,
                    func.return_int,
                    func.return_long,
                    &mut bytes,
                    &mut fixups,
                );
                reachable = cont_reachable;
                break; // all remaining statements consumed inside the call
            }
        }

        // Mark when a trailing no-else `if` can omit its merge-alignment NOP: the
        // merge falls through to the function's exit — either this is the last
        // statement, or the only thing after it is a `return` (whose value +
        // epilogue MSC does not pad). Fixtures 452, 459 (`...; return 0;`), 3602.
        locals_view.last_top_stmt.set(
            i + 1 == body.len() || matches!(body.get(i + 1), Some(Stmt::Return(_))),
        );
        locals_view.final_top_stmt.set(i + 1 == body.len());

        // A top-level `return e` in a function whose shared exit ($EX /
        // EPILOGUE_LABEL) is already targeted (jump-table return bodies)
        // emits only the value load and FALLS into the labeled trailing
        // epilogue — one epilogue, shared. Fixture 4027.
        if func.return_int
            && !func.return_long
            && matches!(stmt, Stmt::Return(_))
            && label_fixups.borrow().iter()
                .any(|(n, _)| n == crate::codegen::statements::EPILOGUE_LABEL)
        {
            let mut buf: Vec<u8> = Vec::new();
            let mut fxs: Vec<crate::Fixup> = Vec::new();
            let mut epi: Vec<u8> = Vec::new();
            push_epilogue(frame, pascal_cleanup, &mut epi);
            emit_stmt(stmt, &locals_view, frame, func.return_int, func.return_long,
                      &mut buf, &mut fxs);
            assert!(buf.ends_with(&epi), "return must end with the epilogue");
            buf.truncate(buf.len() - epi.len());
            let base = bytes.len();
            for mut f in fxs { f.body_offset += base; fixups.push(f); }
            bytes.extend_from_slice(&buf);
            reachable = false;
            break;
        }

        emit_stmt(
            stmt,
            &locals_view,
            frame,
            func.return_int,
            func.return_long,
            &mut bytes,
            &mut fixups,
        );
        if stmt_always_returns(stmt, &locals_view) || stmt_exits(stmt) {
            reachable = false;
        }
        i += 1;
    }

    // Implicit return at the end of functions that fall off without an
    // explicit `return`. MSC emits leave+ret for both void and int-returning
    // functions. The epilogue shape follows the function's frame. It is also the
    // SHARED target (EPILOGUE_LABEL) for void early returns (`if(c) return;`),
    // so emit it when control falls through OR an early return jccs to it. The
    // label is recorded at the epilogue's start, before the jump resolution below.
    let epilogue_targeted = label_fixups.borrow().iter()
        .any(|(n, _)| n == crate::codegen::statements::EPILOGUE_LABEL);
    if reachable || epilogue_targeted {
        labels.borrow_mut().insert(crate::codegen::statements::EPILOGUE_LABEL.to_owned(), bytes.len());
        push_epilogue(frame, pascal_cleanup, &mut bytes);
    }
    // Resolve `goto`/label jumps: backpatch each pending rel8 displacement now
    // that every label's byte offset is known (incl. the tail EPILOGUE_LABEL).
    {
        let labels = labels.borrow();
        for (name, pos) in label_fixups.borrow().iter() {
            let target = *labels.get(name)
                .unwrap_or_else(|| panic!("goto to undefined label `{name}`"));
            let disp = target as i64 - (*pos as i64 + 1);
            bytes[*pos] = i8::try_from(disp).expect("goto rel8 displacement fits") as u8;
        }
    }

    if bytes.len() % 2 != 0 {
        bytes.push(0x90);
    }

    FunctionEmit { bytes, fixups }
}
/// Mangle a C function name into the OBJ symbol it produces.
/// MSC's small-model convention prefixes every function with `_`.
pub(crate) fn symbol_name(c_name: &str) -> String {
    format!("_{c_name}")
}
/// OMF symbol for a function. `pascal` functions are exported UPPERCASE with no
/// leading underscore (MS Pascal convention); cdecl functions get the standard
/// `_name`. Fixtures 1653/2063.
pub(crate) fn fn_symbol(f: &Function) -> String {
    if f.is_pascal { f.name.to_uppercase() }
    // A `static` function is TU-private: its OMF symbol is the bare C name with
    // no leading underscore (fixtures 1708/1916/2155).
    else if f.is_static { f.name.clone() }
    else { symbol_name(&f.name) }
}
/// OMF symbol for a callee referenced by bare C name, given whether it is a
/// known `pascal` function in this unit.
pub(crate) fn callee_symbol(c_name: &str, is_pascal: bool) -> String {
    if is_pascal { c_name.to_uppercase() } else { symbol_name(c_name) }
}
/// As `callee_symbol`, but a `static` callee resolves to its bare C name (no
/// leading `_`), matching its LEXTDEF/LPUBDEF symbol. Fixtures 1708/1916/2155.
pub(crate) fn callee_symbol_full(c_name: &str, is_pascal: bool, is_static: bool) -> String {
    if is_pascal { c_name.to_uppercase() }
    else if is_static { c_name.to_owned() }
    else { symbol_name(c_name) }
}
pub(crate) fn param_disp(idx: usize) -> i16 {
    // A `far` function's return address is 4 bytes (IP+CS), so its params start
    // 2 bytes deeper at [bp+6] rather than [bp+4].
    let base = if cur_fn_far() { 6 } else { 4 };
    i16::try_from(base + (idx * 2)).expect("param disp fits in i16")
}
/// Append the function epilogue. For a `pascal` function (`pascal_cleanup > 0`)
/// the trailing `ret` (0xC3) becomes `ret N` (0xC2 + imm16), popping the
/// caller's arguments. cdecl functions emit the frame's plain epilogue.
pub(crate) fn push_epilogue(frame: Frame, pascal_cleanup: u16, out: &mut Vec<u8>) {
    let epi = frame.epilogue_bytes();
    if pascal_cleanup > 0 {
        out.extend_from_slice(&epi[..epi.len() - 1]); // all but the trailing C3
        out.push(0xC2);
        out.extend_from_slice(&pascal_cleanup.to_le_bytes());
    } else if cur_fn_far() {
        // Far function: the trailing `ret` (C3) becomes `retf` (CB).
        out.extend_from_slice(&epi[..epi.len() - 1]);
        out.push(0xCB);
    } else {
        out.extend_from_slice(epi);
    }
}
/// Like [`push_epilogue`] but returns the bytes, for sites that need the
/// epilogue's length (short-circuit `return a||b` double-epilogue paths).
pub(crate) fn epilogue_vec(frame: Frame, pascal_cleanup: u16) -> Vec<u8> {
    let mut v = Vec::new();
    push_epilogue(frame, pascal_cleanup, &mut v);
    v
}
