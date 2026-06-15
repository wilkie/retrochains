use crate::*;

/// Synthetic label name for a function's shared tail epilogue — a void early
/// `return;` (`if(c) return;`) jccs here instead of emitting its own epilogue.
/// The `@` can't appear in a C identifier, so it never collides with a user
/// goto label. Recorded at the tail epilogue in `emit_function`. Fixtures 1566/2364.
pub(crate) const EPILOGUE_LABEL: &str = "@epilogue";

/// Emit a single statement (recursive: if-statements contain
/// nested statements). Returns no value — appends directly to `out`.
/// Recognize `(int)<float-global-array>[K]` and `(int)(<…>[K] <op> <floatlit>)`
/// in a return: returns (global_idx, const_index, optional (op, operand_bits)).
fn float_global_index_return(expr: &Expr, locals: &Locals<'_>) -> Option<(usize, i32, Option<(BinOp, u64)>)> {
    let probe = |e: &Expr| -> Option<(usize, i32)> {
        if let Expr::Index { array, index } = e
            && locals.is_float_global(*array)
            && let Some(k) = index.fold(locals.inits)
        {
            Some((*array, k))
        } else {
            None
        }
    };
    match expr {
        Expr::Index { .. } => probe(expr).map(|(a, k)| (a, k, None)),
        Expr::BinOp { op, left, right } if matches!(right.as_ref(), Expr::FloatLit(..)) => {
            let (bits, _) = match right.as_ref() { Expr::FloatLit(b, d) => (*b, *d), _ => unreachable!() };
            probe(left).map(|(a, k)| (a, k, Some((*op, bits))))
        }
        _ => None,
    }
}
/// A statement that continues an x87 float-store run (`a[k] = K.Ff`) — no
/// `fwait` is flushed before it.
fn stmt_is_float_elem_store(stmt: &Stmt, locals: &Locals<'_>) -> bool {
    matches!(stmt,
        Stmt::Assign { target: AssignTarget::IndexedLocal { local, .. }, value: Expr::FloatLit(..) }
            if locals.is_float_local(*local))
}
/// True when a statement's FIRST emitted instruction is an x87 `fld` — a call
/// whose last argument (pushed first, right-to-left) is a float/double value.
/// A pending x87 `fwait` then merges into that `fld`'s leading `9B` rather than
/// being flushed as a separate `90 9B`. Fixtures 2198, 3999.
fn stmt_starts_with_float_push(stmt: &Stmt, locals: &Locals<'_>) -> bool {
    let call_args = match stmt {
        Stmt::ExprStmt(Expr::Call { args, .. } | Expr::CallPtr { args, .. }) => Some(args),
        _ => None,
    };
    let Some(args) = call_args else { return false; };
    match args.last() {
        Some(Expr::FloatLit(..)) => true,
        Some(Expr::Local(i)) => locals.is_float_local(*i),
        _ => false,
    }
}
/// `if (a[i] OP b[i]) { x = a[i] - b[i]; REST }` where a,b are char local arrays
/// at the same runtime index and x is an int local: emit the in-place byte
/// compare, then on the taken path reuse the loaded `b[i]` (in AL) and index (in
/// SI) for the subtraction. Returns false (emitting nothing) if the shape doesn't
/// match. Fixture 2488 (strcmp-style `if(a[i]!=b[i]) diff=a[i]-b[i]`).
fn try_emit_array_cmp_sub_if(
    cond: &Cond,
    then_branch: &Stmt,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) -> bool {
    use crate::codegen::assign::{emit_index_to_si, simple_index_eq};
    // Condition: a[i] OP b[i] (char local arrays, same runtime index).
    let Cond::Cmp { op, left: Expr::LocalIndexByte { local: a, index: ai },
                    right: Expr::LocalIndexByte { local: b, index: bi } } = cond else { return false; };
    if ai.fold(locals.inits).is_some() || !simple_index_eq(ai, bi) { return false; }
    // Then-body: `x = a[i] - b[i]; REST...`.
    let stmts: &[Stmt] = match then_branch {
        Stmt::Block(ss) => ss,
        single => std::slice::from_ref(single),
    };
    let Some((Stmt::Assign { target: AssignTarget::Local(x),
        value: Expr::BinOp { op: BinOp::Sub,
            left: sl, right: sr } }, rest)) = stmts.split_first() else { return false; };
    let (Expr::LocalIndexByte { local: sa, index: si1 }, Expr::LocalIndexByte { local: sb, index: si2 }) =
        (sl.as_ref(), sr.as_ref()) else { return false; };
    if *sa != *a || *sb != *b || !simple_index_eq(si1, ai) || !simple_index_eq(si2, ai) { return false; }
    if locals.size(*x) != 2 || locals.is_long_local(*x) { return false; }
    // Build the taken-path body so the skip jcc can be sized.
    let mut body = Vec::new();
    let mut body_fx: Vec<Fixup> = Vec::new();
    let a_disp = locals.disp(*a);
    body.push(0x98);                                                       // cbw  (b[i] → AX)
    body.extend_from_slice(&[0x8B, 0xC8]);                                 // mov cx,ax
    body.push(0x8A); body.push(bp_modrm(0x42, a_disp)); push_bp_disp(&mut body, a_disp); // mov al,a[i]
    body.push(0x98);                                                       // cbw  (a[i] → AX)
    body.extend_from_slice(&[0x2B, 0xC1]);                                 // sub ax,cx
    let xd = locals.disp(*x);
    body.push(0x89); body.push(bp_modrm(0x46, xd)); push_bp_disp(&mut body, xd); // mov [x],ax
    for s in rest { emit_stmt(s, locals, frame, return_int, return_long, &mut body, &mut body_fx); }
    let Ok(skip) = i8::try_from(body.len()) else { return false; };
    // Condition: `mov si,i; mov al,b[i]; cmp a[i],al; jcc-skip`.
    emit_index_to_si(ai, locals, out, fixups);
    let b_disp = locals.disp(*b);
    out.push(0x8A); out.push(bp_modrm(0x42, b_disp)); push_bp_disp(out, b_disp); // mov al,b[i]
    out.push(0x38); out.push(bp_modrm(0x42, a_disp)); push_bp_disp(out, a_disp); // cmp a[i],al
    let jcc = {
        let j = inverted_jcc(*op);
        if cmp_is_unsigned(cond, locals) { to_unsigned_jcc(j) } else { j }
    };
    out.push(jcc);
    out.push(skip as u8);
    let base = out.len();
    out.extend_from_slice(&body);
    for mut f in body_fx { f.body_offset += base; fixups.push(f); }
    true
}
/// Emit a run of consecutive `arr[idx].field = v` stores (`LocalStructArrayField`
/// targets) that share the same array, runtime index, and stride: compute the row
/// base `si = bp + idx*stride` ONCE, then store each field at `[si + base +
/// field_off]`. Returns the number of statements consumed, or `None` if `stmts`
/// doesn't begin with at least two such same-row stores. Fixture 1914.
fn try_emit_struct_array_field_run(
    stmts: &[Stmt],
    locals: &Locals<'_>,
    _frame: Frame,
    _return_int: bool,
    _return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) -> Option<usize> {
    use crate::codegen::assign::simple_index_eq;
    fn field_store(s: &Stmt) -> Option<(usize, &Expr, u16, u16, u8, &Expr)> {
        if let Stmt::Assign {
            target: AssignTarget::LocalStructArrayField { local, index, stride, field_off, size },
            value,
        } = s {
            Some((*local, index.as_ref(), *stride, *field_off, *size, value))
        } else {
            None
        }
    }
    let (local, index, stride, _, _, _) = field_store(&stmts[0])?;
    let mut run = 1;
    while run < stmts.len() {
        match field_store(&stmts[run]) {
            Some((l, idx, s, _, _, _)) if l == local && s == stride && simple_index_eq(idx, index) => {
                run += 1;
            }
            _ => break,
        }
    }
    if run < 2 {
        return None;
    }
    // Row base once. The stride-6 scale leaves the index value in CX, so a stored
    // value that IS the index can reuse `mov ax,cx` for the FIRST field only.
    crate::codegen::expr::emit_local_struct_row_si(index, stride, locals, out, fixups);
    for (k, s) in stmts[..run].iter().enumerate() {
        let (local, _, _, field_off, size, value) = field_store(s).expect("run entry");
        let disp = locals.disp(local) + field_off as i16;
        if k == 0 && stride == 6 && size == 2 && simple_index_eq(value, index) {
            out.extend_from_slice(&[0x8B, 0xC1]); // mov ax,cx
        } else {
            crate::codegen::expr::emit_expr_to_ax(value, locals, out, fixups);
        }
        if size == 1 {
            if out.last() == Some(&0x98) { out.pop(); } // storing AL — strip cbw
            out.push(0x88);
        } else {
            out.push(0x89);
        }
        if let Ok(d8) = i8::try_from(disp) {
            out.push(0x44); out.push(d8 as u8);
        } else {
            out.push(0x84); out.extend_from_slice(&disp.to_le_bytes());
        }
    }
    Some(run)
}
pub(crate) fn emit_stmt(
    stmt: &Stmt,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // Flush a pending x87 store's `fwait` (90 9B) before any statement that is
    // not another float store — the FxxRQQ emulator's 2-byte patch slot.
    if locals.fpu_pending_fwait.get() && !stmt_is_float_elem_store(stmt, locals) {
        locals.fpu_pending_fwait.set(false);
        // If this statement's first op is an x87 `fld` (a leading float arg push),
        // that instruction's own leading `9B` serves as the pending fwait — no
        // separate `90 9B` flush. Fixtures 2198/3999.
        if !stmt_starts_with_float_push(stmt, locals) {
            fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIWRQQ" } });
            out.push(0x90);
            out.push(0x9B);
        }
    }
    // Read-and-clear the "function's final top-level statement" flag: only the
    // statement the function loop set it for should see it true. Recursing into
    // a nested block/branch (which calls emit_stmt again) must see false, so a
    // nested terminal `if(c) return e;` isn't mistaken for the function's tail.
    let final_top = locals.final_top_stmt.replace(false);
    match stmt {
        Stmt::Return(expr) => {
            // A bare `return E` whose value already has a shared epilogue block
            // (an earlier `if (c) return E`) jumps to it rather than re-emitting
            // the value load + epilogue. MSC shares these. Fixtures 4165, 2772.
            if return_int && !return_long
                && let Some(key) = return_share_key(expr)
                && locals.labels.borrow().contains_key(&key)
            {
                out.push(0xEB); // jmp short @retX
                let pos = out.len();
                out.push(0x00);
                locals.label_fixups.borrow_mut().push((key, pos));
                return;
            }
            emit_return(expr, locals, frame, return_int, return_long, out, fixups)
        }
        Stmt::Empty => {}
        Stmt::ExprStmt(Expr::Call { name, args }) => {
            // The per-call `add sp,N` cleanup is elided only for the function's
            // single last call before a slide-frame epilogue — controlled by the
            // emit_function `elide_call_cleanup` flag read inside emit_call_inner,
            // not here (so earlier sequential calls still clean up). Fixtures
            // 1443, 1872 (single bare call) elide; 4044 keeps all but the last.
            emit_call_inner(name, args, locals, false, out, fixups);
        }
        // A postfix/prefix `param++;` used purely for its side effect (e.g. a
        // comma-operator side, fixture 3308) emits just the in-place mutate — no
        // value load. The Local/Global variants already lower this way via their
        // own statement paths; the Param case is added here.
        Stmt::ExprStmt(Expr::PostMutateParam { param_idx, step })
        | Stmt::ExprStmt(Expr::PreMutateParam { param_idx, step }) => {
            let disp = param_disp(*param_idx);
            let slot_size = if locals.is_char_param(*param_idx) { 1 } else { 2 };
            let pointee = locals.param_pointee_size(*param_idx);
            let eff = if pointee > 0 { *step * pointee as i32 } else { *step };
            crate::codegen::assign::emit_postmutate_local(eff, slot_size, disp, out);
        }
        // A local/global `x++`/`++x`/`x--`/`--x` used purely for its side
        // effect (e.g. a discarded ternary `c ? a++ : a--;` after the cond
        // folds) emits only the in-place mutate — no old/new value load.
        // Fixtures 1202, 1861.
        Stmt::ExprStmt(Expr::PostMutateLocal { local_idx, step })
        | Stmt::ExprStmt(Expr::PreMutateLocal { local_idx, step }) => {
            let disp = locals.disp(*local_idx);
            let slot_size = locals.size(*local_idx);
            let pointee = locals.local_pointee_size(*local_idx);
            let eff = if pointee > 0 { *step * pointee as i32 } else { *step };
            crate::codegen::assign::emit_postmutate_local(eff, slot_size, disp, out);
            // A `huge` pointer normalizes its segment word after the offset
            // advance. A plain `far` pointer does not. Fixtures 1771/1774.
            if locals.is_huge_ptr_local(*local_idx) {
                crate::codegen::assign::emit_huge_normalize(eff, disp, out, fixups);
            }
        }
        Stmt::ExprStmt(Expr::PostMutateGlobal { global_idx, step })
        | Stmt::ExprStmt(Expr::PreMutateGlobal { global_idx, step }) => {
            let is_byte = locals.is_char_global(*global_idx);
            crate::codegen::assign::emit_postmutate_global(*step, *global_idx, is_byte, out, fixups);
        }
        // A discarded ternary `c ? a : b;` whose condition folds to a constant
        // reduces to just the chosen arm's side effects — emit the live arm as
        // its own discarded statement (so a pure arm vanishes, a mutate arm
        // emits only its in-place op). Fixture 1202 (`a>0 ? a++ : a--;`).
        Stmt::ExprStmt(Expr::Ternary { cond, then_arm, else_arm })
            if cond.fold(locals.inits).is_some() =>
        {
            let chosen = if cond.fold(locals.inits) != Some(0) { then_arm } else { else_arm };
            emit_stmt(
                &Stmt::ExprStmt((**chosen).clone()),
                locals, frame, return_int, return_long, out, fixups,
            );
        }
        Stmt::ExprStmt(other @ Expr::Ternary { cond, .. })
            if final_top && !return_int && !return_long
                && cond.fold(locals.inits).is_none() =>
        {
            // A discarded runtime ternary as a void function's FINAL
            // statement: the then-arm's over-else jmp would land on the
            // trailing epilogue — emit the epilogue inline instead (the
            // 3614 if/else rule applied to the ternary shape). The value
            // emitter takes the bytes from `ternary_tail_epilogue`.
            // Fixture 3328.
            *locals.ternary_tail_epilogue.borrow_mut() =
                Some(crate::codegen::func::epilogue_vec(frame, locals.pascal_cleanup));
            emit_expr_to_ax(other, locals, out, fixups);
            *locals.ternary_tail_epilogue.borrow_mut() = None;
        }
        Stmt::ExprStmt(other) => {
            // A discarded expression statement with NO side effects emits
            // nothing — MSC drops dead value computations like `x + 1;`.
            // Expressions with side effects (calls, mutates, assignments)
            // still evaluate; their AX result is simply unused. Fixture 1960.
            if expr_is_pure(other) {
                // dead code — emit nothing
            } else {
                emit_expr_to_ax(other, locals, out, fixups);
            }
        }
        Stmt::Assign { target, value } => emit_assign(target.clone(), value, locals, out, fixups),
        Stmt::Switch { scrutinee, cases } => {
            let known_k = if let Expr::IntLit(k) = scrutinee { Some(*k) } else { None };
            emit_runtime_switch(scrutinee, known_k, cases, locals, frame, return_int, return_long, out, fixups, None, &mut Vec::new());
        }
        Stmt::Break => {
            // Emit a forward `jmp short` (eb) placeholder; the enclosing loop
            // patches the rel8 disp byte at its end. The recorded offset points
            // at the disp byte (one past the opcode).
            out.push(0xEB);
            let pos = out.len();
            out.push(0x00);
            let mut stack = locals.loop_stack.borrow_mut();
            if let Some(top) = stack.last_mut() {
                top.breaks.push(pos);
            } else {
                panic!("break outside a loop or const-folded switch");
            }
        }
        Stmt::Continue => {
            out.push(0xEB);
            let pos = out.len();
            out.push(0x00);
            let mut stack = locals.loop_stack.borrow_mut();
            if let Some(top) = stack.last_mut() {
                top.continues.push(pos);
            } else {
                panic!("continue outside a loop");
            }
        }
        Stmt::Label(name) => {
            // A label with no fall-through aligns to an even offset with a NOP;
            // labels reached by fall-through don't pad (3306 pads, 441 doesn't).
            // "No fall-through" means the preceding instruction is a `ret`
            // (sniffed as a trailing 0xC3) or an unconditional `goto`
            // (precomputed in `backward_labels`, since the jmp's rel8 placeholder
            // can't be byte-sniffed reliably). Fixture 4214 (`goto check;body:`).
            if out.len() % 2 != 0
                && (out.last() == Some(&0xC3) || locals.backward_labels.contains(name))
            {
                out.push(0x90);
            }
            // Record this label's byte offset; jumps to it are backpatched after
            // the whole body is emitted.
            locals.labels.borrow_mut().insert(name.clone(), out.len());
        }
        Stmt::Goto(name) => {
            // Unconditional `jmp short` to the label (rel8 backpatched).
            out.push(0xEB);
            let pos = out.len();
            out.push(0x00);
            locals.label_fixups.borrow_mut().push((name.clone(), pos));
        }
        Stmt::Block(stmts) => {
            // Block has no scoping at the codegen level. Sub-statements
            // emit inline. Const-prop's already been applied at the
            // function level before we got here.
            let mut reachable = true;
            let mut i = 0;
            while i < stmts.len() {
                if !reachable { break; }
                // A run of consecutive `arr[idx].field = v` stores sharing the
                // same array, index, and stride computes the row base (SI) ONCE
                // and stores each field off it, rather than recomputing per store.
                // Fixture 1914.
                if let Some(consumed) = try_emit_struct_array_field_run(
                    &stmts[i..], locals, frame, return_int, return_long, out, fixups)
                {
                    i += consumed;
                    continue;
                }
                emit_stmt(&stmts[i], locals, frame, return_int, return_long, out, fixups);
                if stmt_always_returns(&stmts[i], locals) {
                    reachable = false;
                }
                i += 1;
            }
        }
        Stmt::While { cond, body } => {
            emit_while(cond, body, locals, frame, return_int, return_long, out, fixups);
        }
        Stmt::DoWhile { body, cond } => {
            emit_do_while(body, cond, locals, frame, return_int, return_long, out, fixups);
        }
        Stmt::For { init, cond, step, body } => {
            emit_for(init, cond, step, body, locals, frame, return_int, return_long, out, fixups);
        }
        Stmt::If { cond, then_branch, else_branch } => {
            // `if (a[i] != b[i]) { x = a[i] - b[i]; REST }` (char local arrays,
            // same runtime index): MSC fuses the compare and the subtraction so
            // the body reuses the `b[i]` value (in AL) and the index (in SI) the
            // condition's in-place byte compare already established. Fixture 2488.
            if else_branch.is_none()
                && try_emit_array_cmp_sub_if(cond, then_branch, locals, frame, return_int, return_long, out, fixups)
            {
                return;
            }
            // `if (pure-cond) ;` — empty then-body, no else, side-effect-free
            // condition: MSC emits nothing at all (no compare, no branch).
            // Fixture 3048 (`if (x > 0) ;`).
            if else_branch.is_none()
                && stmt_is_empty(then_branch)
                && cond_is_pure(cond)
            {
                return;
            }
            // `if (cond) goto L;` — and `if (cond) return;` in a VOID function,
            // which MSC routes to a SHARED end epilogue label (EPILOGUE_LABEL,
            // recorded at the function tail) — lower to a single conditional jump
            // to the label (jcc taken when the cond is TRUE). Fixtures 1566/2364.
            let goto_label: Option<String> = if else_branch.is_none() {
                match then_branch.as_ref() {
                    Stmt::Goto(label) => Some(label.clone()),
                    Stmt::Return(Expr::IntLit(0)) if !return_int && !return_long => Some(EPILOGUE_LABEL.to_owned()),
                    _ => None,
                }
            } else { None };
            // `if ((x = e) != 0) return x;` — the cond's embedded assign leaves
            // AX holding x, so the then-return is just a jump into the SHARED
            // tail epilogue with AX live (the following `return K` loads its
            // value and falls into the same labeled epilogue). Fixture 3395.
            let goto_label: Option<String> = goto_label.or_else(|| {
                if else_branch.is_some() || !return_int || return_long
                    // A folding cond (`if ((x = 5))`) keeps the straight-line
                    // splice — no branch at all (513/1434).
                    || fold_cond(cond, locals).is_some()
                {
                    return None;
                }
                let then_inner: &Stmt = match then_branch.as_ref() {
                    Stmt::Block(v) if v.len() == 1 => &v[0],
                    other => other,
                };
                let Stmt::Return(Expr::Local(rx)) = then_inner else { return None };
                // The cond's tested expression when it embeds an assignment
                // that leaves AX = Local(i): `AssignExpr` (`while (*d++=..)`
                // form) or the parenthesized-assign `Seq{[x = e], x}` desugar.
                let assigns_local = |e: &Expr| -> Option<usize> {
                    match e {
                        Expr::AssignExpr { target: AssignTarget::Local(i), .. } => Some(*i),
                        Expr::Seq { sides, value } => {
                            if let [Stmt::Assign { target: AssignTarget::Local(i), .. }] = sides.as_slice()
                                && matches!(value.as_ref(), Expr::Local(v) if v == i)
                            {
                                Some(*i)
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                };
                let assigned = match cond {
                    Cond::Cmp { op: RelOp::Ne, left, right } if matches!(right, Expr::IntLit(0)) =>
                        assigns_local(left),
                    Cond::Truthy(e) => assigns_local(e),
                    _ => None,
                };
                if assigned == Some(*rx)
                    && locals.size(*rx) == 2
                    && !locals.is_long_local(*rx)
                    && locals.reg_for_local(*rx).is_none()
                {
                    Some(EPILOGUE_LABEL.to_owned())
                } else {
                    None
                }
            });
            if let Some(label) = goto_label {
                if let Some(k) = fold_cond(cond, locals) {
                    if k != 0 {
                        out.push(0xEB);
                        let pos = out.len();
                        out.push(0x00);
                        locals.label_fixups.borrow_mut().push((label.clone(), pos));
                    }
                    return;
                }
                let jcc = match cond {
                    Cond::Cmp { op, .. } => loop_back_jcc(*op),
                    _ => 0x75, // jnz for a truthy condition
                };
                emit_cond_cmp_inner(cond, locals, out, fixups);
                out.push(jcc);
                let pos = out.len();
                out.push(0x00);
                locals.label_fixups.borrow_mut().push((label.clone(), pos));
                return;
            }
            // A SECOND (or later) `if (cond) return K;` with the same constant K
            // as an earlier one: MSC emits the `mov ax,K; <epilogue>` block ONCE
            // and routes later occurrences to it via a jump-on-TRUE (the earlier
            // occurrence recorded the `@retK` label at its block). Fixtures 4036
            // (`if(c>='a'&&c<='z') return 1; if(c>='A'&&c<='Z') return 1;`).
            if else_branch.is_none()
                && return_int && !return_long
                && let Stmt::Return(Expr::IntLit(k)) = unwrap_single_stmt(then_branch)
                && fold_cond(cond, locals).is_none()
            {
                let lbl = format!("@ret{k}");
                if locals.labels.borrow().contains_key(&lbl)
                    && emit_if_and_goto_true(cond, &lbl, locals, out, fixups)
                {
                    return;
                }
            }
            // `if (c) return <expr>;` as the function's FINAL statement in an
            // int-returning fall-off function: MSC inverts the cond and jccs to
            // the SHARED tail epilogue, then lets the return-value computation
            // fall through into it (one epilogue, not a duplicate). Fixture 1815.
            if else_branch.is_none()
                && return_int && !return_long
                && final_top
                && let Stmt::Return(rexpr) = then_branch.as_ref()
                && !crate::codegen::calls::long_operand(rexpr, locals)
                && let Cond::Cmp { op, .. } = cond
                && fold_cond(cond, locals).is_none()
            {
                emit_cond_cmp_inner(cond, locals, out, fixups);
                let mut jcc = inverted_jcc(*op);
                if cmp_is_unsigned(cond, locals) {
                    jcc = to_unsigned_jcc(jcc);
                }
                out.push(jcc);
                let pos = out.len();
                out.push(0x00);
                locals.label_fixups.borrow_mut().push((EPILOGUE_LABEL.to_owned(), pos));
                emit_expr_to_ax(rexpr, locals, out, fixups);
                return;
            }
            // `if (cond) break;` / `if (cond) continue;` lower to a single
            // conditional jump (jcc taken when the cond is TRUE) straight to the
            // loop's break/continue target — exactly like `if (cond) goto L`,
            // but the offset is recorded in the enclosing loop ctx (rel8).
            if else_branch.is_none()
                && matches!(then_branch.as_ref(), Stmt::Break | Stmt::Continue)
            {
                let is_break = matches!(then_branch.as_ref(), Stmt::Break);
                if let Some(k) = fold_cond(cond, locals) {
                    if k != 0 {
                        out.push(0xEB);
                        let pos = out.len();
                        out.push(0x00);
                        let mut stack = locals.loop_stack.borrow_mut();
                        let top = stack.last_mut().expect("break/continue outside a loop");
                        if is_break { top.breaks.push(pos); } else { top.continues.push(pos); }
                    }
                    return;
                }
                let jcc = match cond {
                    Cond::Cmp { op, .. } => loop_back_jcc(*op),
                    _ => 0x75, // jnz for a truthy condition
                };
                emit_cond_cmp_inner(cond, locals, out, fixups);
                out.push(jcc);
                let pos = out.len();
                out.push(0x00);
                let mut stack = locals.loop_stack.borrow_mut();
                let top = stack.last_mut().expect("break/continue outside a loop");
                if is_break { top.breaks.push(pos); } else { top.continues.push(pos); }
                return;
            }
            // Constant-condition elision: when the cond folds to a
            // compile-time integer, MSC keeps only the live branch
            // and drops the comparison + jump entirely. Fixtures
            // 4094 (if (0)) and 4095 (if (1)) confirm.
            if let Some(k) = fold_cond(cond, locals) {
                // Even when the cond folds, an assignment embedded in it (e.g.
                // `if ((x = 5))`) still has to execute its store.
                emit_cond_side_effects(cond, locals, out, fixups);
                if k != 0 {
                    emit_stmt(then_branch, locals, frame, return_int, return_long, out, fixups);
                } else if let Some(else_branch) = else_branch {
                    emit_stmt(else_branch, locals, frame, return_int, return_long, out, fixups);
                }
                return;
            }
            // `if (c) r = e1; else r = e2;` — both branches one assignment to
            // the SAME plain int local, with non-literal values: the arms load
            // AX and share a single `mov [r],ax` store at the merge, and a
            // following `return r` reuses the live AX (the barrier is left at
            // the store). Literal arm values keep the per-arm immediate-store
            // shape (2434/2461). Fixture 2865.
            if let Some(eb) = else_branch
                && let Some((t1, v1)) = single_word_local_assign(then_branch, locals)
                && let Some((t2, v2)) = single_word_local_assign(eb, locals)
                && t1 == t2
                && v1.fold(locals.inits).is_none()
                && v2.fold(locals.inits).is_none()
            {
                let mut then_buf = Vec::new();
                let mut then_fixups = Vec::new();
                emit_expr_to_ax(v1, locals, &mut then_buf, &mut then_fixups);
                let cond_size = {
                    let mut sz = out.clone();
                    let base = sz.len();
                    emit_cond_skip(cond, 0i8, locals, &mut sz, &mut Vec::new());
                    sz.len() - base
                };
                // Alignment NOP for the else label (after then + over-else jmp),
                // same rule as the generic if/else path.
                let needs_nop = (out.len() + cond_size + then_buf.len() + 2) % 2 != 0;
                let skip = i8::try_from(then_buf.len() + 2 + usize::from(needs_nop))
                    .expect("then-arm short enough for jcc rel8");
                emit_cond_skip(cond, skip, locals, out, fixups);
                let base = out.len();
                for mut f in then_fixups { f.body_offset += base; fixups.push(f); }
                out.extend_from_slice(&then_buf);
                let mut else_buf = Vec::new();
                let mut else_fixups = Vec::new();
                emit_expr_to_ax(v2, locals, &mut else_buf, &mut else_fixups);
                let over = i8::try_from(else_buf.len() + usize::from(needs_nop))
                    .expect("else-arm short enough for jmp rel8");
                out.push(0xEB);
                out.push(over as u8);
                if needs_nop { out.push(0x90); }
                let base = out.len();
                for mut f in else_fixups { f.body_offset += base; fixups.push(f); }
                out.extend_from_slice(&else_buf);
                // Shared store at the merge; leave the AX-reuse barrier here so
                // a following read of the target reuses the live AX.
                let store_pos = out.len();
                let d = locals.disp(t1);
                out.push(0x89); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
                locals.merge_barrier.set(Some(store_pos));
                return;
            }
            // Build the then-branch into a scratch buffer so we know
            // its byte count for the conditional-jump displacement.
            let mut then_buf = Vec::new();
            let mut then_fixups = Vec::new();
            // Record where the current loop ctx's break/continue
            // slots were before the then-branch emit, so we can shift
            // any newly-recorded offsets to absolute out positions.
            let (pre_breaks, pre_conts) = {
                let stack = locals.loop_stack.borrow();
                stack.last().map(|t| (t.breaks.len(), t.continues.len())).unwrap_or((0, 0))
            };
            // `goto`/label entries recorded while the then-branch goes into the
            // scratch buffer are buffer-relative; snapshot so they can be
            // rebased once the buffer's position in `out` is known. Without
            // this a nested `if (c) goto L;` backpatches a bogus offset,
            // corrupting earlier bytes (fixture 441).
            let lf_start = locals.label_fixups.borrow().len();
            let labels_before: std::collections::HashSet<String> =
                locals.labels.borrow().keys().cloned().collect();
            emit_stmt(then_branch, locals, frame, return_int, return_long, &mut then_buf, &mut then_fixups);
            let then_len = then_buf.len();
            // Alignment NOP: MSC aligns branch-target labels to even byte
            // offsets. If the else-label (= position right after the
            // then-block) would land at an odd offset, add 1 NOP. We
            // pre-emit the cond_skip into a scratch buffer to learn its
            // size (displacement is irrelevant for sizing).
            // Size the cond by emitting onto a COPY of the real `out` so any
            // tail-context peepholes (e.g. AX-reuse of a just-stored local in the
            // compare) fire identically to the actual emission below — otherwise
            // the alignment-nop parity would be computed from a stale size.
            let cond_size = {
                let mut sz = out.clone();
                let base = sz.len();
                emit_cond_skip(cond, 0i8, locals, &mut sz, &mut Vec::new());
                sz.len() - base
            };
            // When the then-branch falls through (does not unconditionally
            // return) and an else-branch exists, MSC emits `jmp SHORT $end`
            // after the then-block so control skips the else-block. If the
            // then-branch always returns, no jmp is needed — control never
            // reaches it (this is the common case, hence it was the only one
            // handled before). Fixture 2434.
            let emit_else_jmp =
                else_branch.is_some() && !stmt_always_returns(then_branch, locals);
            // A void function whose FINAL statement is this if/else: the
            // then-branch's over-else jmp would land on the trailing
            // epilogue — MSC emits the epilogue inline instead (3614).
            let then_tail_epilogue = emit_else_jmp
                && final_top
                && !return_int
                && !return_long;
            let epi_len = crate::codegen::func::epilogue_vec(frame, locals.pascal_cleanup).len();
            let jmp_len = if then_tail_epilogue { epi_len } else if emit_else_jmp { 2 } else { 0 };
            // The alignment nop aligns the else-label (the je target), which
            // sits past the then-block and the over-else jmp. MSC omits it when
            // this is the function's last top-level statement with no else and a
            // falling-through body — the merge IS the epilogue, which isn't padded.
            // Fixtures 452, 459, 3602.
            let merge_is_epilogue = locals.last_top_stmt.get()
                && else_branch.is_none()
                && !stmt_always_returns(then_branch, locals);
            // A then-branch containing a `goto` suppresses the merge-alignment
            // pad (441 — gold leaves the merge odd).
            let then_has_goto = locals.label_fixups.borrow().len() > lf_start;
            // MSC aligns an if-merge only at the function level — an if-merge
            // INSIDE a loop body is left unaligned (only the loop-top is aligned).
            // Fixture 1415 (`while(x){ if(x&1)c++; x>>=1; }`).
            let inside_loop = !locals.loop_stack.borrow().is_empty();
            let needs_nop = !merge_is_epilogue
                && !then_has_goto
                && !inside_loop
                && (out.len() + cond_size + then_len + jmp_len) % 2 != 0;
            let nop_pad = usize::from(needs_nop);
            let take_then_disp = i8::try_from(then_len + jmp_len + nop_pad)
                .expect("then-body short enough for jcc rel8");
            emit_cond_skip(cond, take_then_disp, locals, out, fixups);
            // Bring any then-branch call sites into the parent buffer,
            // offsetting their body_offset by where the then bytes
            // land in `out`.
            let then_base = out.len();
            out.extend_from_slice(&then_buf);
            for mut c in then_fixups {
                c.body_offset += then_base;
                fixups.push(c);
            }
            // Rebase then-branch goto fixups / label definitions into
            // out-relative space (fixture 441).
            if then_base != 0 {
                for (_, pos) in locals.label_fixups.borrow_mut().iter_mut().skip(lf_start) {
                    *pos += then_base;
                }
                for (name, pos) in locals.labels.borrow_mut().iter_mut() {
                    if !labels_before.contains(name) { *pos += then_base; }
                }
            }
            // Record a `@retK` label at this `if (cond) return K;` then-block
            // (the `mov ax,K; <epilogue>` just appended) so a later identical
            // `if (...) return K;` can route to it via a jump (the shared-block
            // path above). Absolute offset, inserted after the rebase so it is
            // not shifted again. First occurrence wins. Fixture 4036.
            if return_int && !return_long
                && let Stmt::Return(re) = unwrap_single_stmt(then_branch)
                && let Some(key) = return_share_key(re)
            {
                locals.labels.borrow_mut().entry(key).or_insert(then_base);
            }
            // Shift break/continue offsets recorded during the then
            // emit by `then_base` so they point at the correct bytes.
            {
                let mut stack = locals.loop_stack.borrow_mut();
                if let Some(top) = stack.last_mut() {
                    for off in &mut top.breaks[pre_breaks..] {
                        *off += then_base;
                    }
                    for off in &mut top.continues[pre_conts..] {
                        *off += then_base;
                    }
                }
            }
            // Over-else jmp placeholder (backpatched once the else-block
            // size is known). It precedes the alignment nop so the nop lands
            // immediately before the else-label.
            let jmp_pos = out.len();
            if then_tail_epilogue {
                crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            } else if emit_else_jmp {
                out.push(0xEB);
                out.push(0x00);
            }
            if needs_nop { out.push(0x90); }
            if let Some(else_branch) = else_branch {
                emit_stmt(else_branch, locals, frame, return_int, return_long, out, fixups);
            }
            if emit_else_jmp && !then_tail_epilogue {
                let disp = i8::try_from(out.len() - (jmp_pos + 2))
                    .expect("else-body short enough for jmp rel8");
                out[jmp_pos + 1] = disp as u8;
            }
        }
    }
    // After any statement that ends at a branch merge (if/loop/switch/labels) or
    // otherwise leaves AX in an undefined state, advance the AX-reuse barrier so a
    // following compare won't reuse a register stored before the merge (fixture
    // 1445). Straight-line stores past this point remain reusable.
    // A `Block` is transparent: its inner statements advance the barrier
    // themselves (a nested if/loop sets it at its own merge), so a block of
    // straight-line assigns leaves AX reusable — matching MSC's flat view of an
    // elided `if(true){v=...}` (fixture 1986).
    // Exception: `if (simple-cond) <exit>;` (no else, then-branch unconditionally
    // returns/breaks/continues/gotos) does NOT merge control flow at the
    // fall-through — the code after it runs only when the condition is FALSE (a
    // single path), so registers there hold exactly what the condition's eval
    // left. Keep the barrier so the next statement can reuse that (e.g.
    // `if(*r==0)return; *r=1;` reuses BX=r). And/Or conditions have multiple
    // false-paths that DO merge, so they still advance the barrier.
    let keep_barrier = matches!(stmt,
        Stmt::If { cond, then_branch, else_branch: None }
            if matches!(cond, Cond::Cmp { .. } | Cond::Truthy(_))
                && if_then_exits(then_branch));
    if !matches!(stmt, Stmt::Assign { .. } | Stmt::Block(_)) && !keep_barrier {
        // A do-while-form loop with no break/continue always runs its body, so
        // AX at the merge is the body's last write — emit_loop leaves a barrier
        // inside the loop instead of at its end (fixture 1411).
        let barrier = locals.merge_barrier.take().unwrap_or(out.len());
        locals.last_branch_barrier.set(barrier);
    }
}
/// True when `stmt` unconditionally transfers control away (return/break/
/// continue/goto, or a block ending in one) — so an `if (c) stmt;` never falls
/// through its then-branch into the following code.
fn if_then_exits(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return(_) | Stmt::Goto(_) | Stmt::Break | Stmt::Continue => true,
        Stmt::Block(v) => v.last().is_some_and(if_then_exits),
        _ => false,
    }
}
/// True when `stmt` unconditionally returns — so a following
/// statement at the same nesting level is unreachable. Used to
/// drop trailing dead code (fixture 4095: `if (1) return 1; return
/// 0;` keeps only the `return 1;` path).
/// True when control never falls through past `stmt` — it returns or jumps away
/// unconditionally. Unlike [`stmt_always_returns`], a bare `goto` counts as an
/// exit, and an `if/else` exits when both arms do. Used to suppress a dead
/// trailing function epilogue after a loop lowered to if/else+goto.
/// Count the function calls in an expression (including CallPtr / make().field
/// and any calls nested in operands or arguments). Used to detect a top-level
/// statement whose SINGLE call is the function's last — that call's cdecl
/// cleanup is elided before a slide-frame epilogue.
pub(crate) fn expr_call_count(e: &Expr) -> usize {
    match e {
        Expr::Call { args, .. } | Expr::CallPtr { args, .. } | Expr::CallStructField { args, .. } => {
            1 + args.iter().map(expr_call_count).sum::<usize>()
        }
        Expr::BinOp { left, right, .. } => expr_call_count(left) + expr_call_count(right),
        Expr::Ternary { cond, then_arm, else_arm } =>
            expr_call_count(cond) + expr_call_count(then_arm) + expr_call_count(else_arm),
        Expr::AssignExpr { value, .. } => expr_call_count(value),
        Expr::Seq { value, .. } => expr_call_count(value),
        Expr::DerefWord { ptr } | Expr::DerefByte { ptr } => expr_call_count(ptr),
        Expr::Index { index, .. } | Expr::IndexByte { index, .. } => expr_call_count(index),
        Expr::CastChar { value, .. } => expr_call_count(value),
        _ => 0,
    }
}
/// Count the OUTERMOST calls in an expression — calls reachable without
/// descending through another call's arguments. A nested call (`f(g())`) does
/// not increase the count because it is fully evaluated, with its own cdecl
/// cleanup, before the enclosing call lands; only the enclosing call's cleanup
/// is a candidate for last-call elision. By contrast `f() + g()` has two
/// outermost calls and must keep both cleanups. Fixture 4192.
pub(crate) fn expr_outer_call_count(e: &Expr) -> usize {
    match e {
        Expr::Call { .. } | Expr::CallPtr { .. } | Expr::CallStructField { .. } => 1,
        Expr::BinOp { left, right, .. } => expr_outer_call_count(left) + expr_outer_call_count(right),
        Expr::Ternary { cond, then_arm, else_arm } =>
            expr_outer_call_count(cond) + expr_outer_call_count(then_arm) + expr_outer_call_count(else_arm),
        Expr::AssignExpr { value, .. } => expr_outer_call_count(value),
        Expr::Seq { value, .. } => expr_outer_call_count(value),
        Expr::DerefWord { ptr } | Expr::DerefByte { ptr } => expr_outer_call_count(ptr),
        Expr::Index { index, .. } | Expr::IndexByte { index, .. } => expr_outer_call_count(index),
        Expr::CastChar { value, .. } => expr_outer_call_count(value),
        _ => 0,
    }
}
/// True when this top-level statement is a straight-line statement carrying
/// exactly one OUTERMOST call (the candidate for last-call cleanup elision).
/// Calls nested inside that call's arguments are fine — they clean up their own
/// args first — so only the enclosing call count matters here.
pub(crate) fn stmt_single_call(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::ExprStmt(e) => expr_outer_call_count(e) == 1,
        Stmt::Assign { value, .. } => expr_outer_call_count(value) == 1,
        Stmt::Return(e) => expr_outer_call_count(e) == 1,
        _ => false,
    }
}
/// Total calls anywhere in a statement (recursing into nested control flow), to
/// locate the last call-bearing top-level statement.
pub(crate) fn stmt_call_count(stmt: &Stmt) -> usize {
    match stmt {
        Stmt::ExprStmt(e) => expr_call_count(e),
        Stmt::Assign { value, .. } => expr_call_count(value),
        Stmt::Return(e) => expr_call_count(e),
        Stmt::Block(v) => v.iter().map(stmt_call_count).sum(),
        Stmt::If { cond: _, then_branch, else_branch } =>
            stmt_call_count(then_branch) + else_branch.as_ref().map_or(0, |e| stmt_call_count(e)),
        _ => 0,
    }
}
/// True when evaluating `e` has no observable side effects, so a discarded
/// expression statement (`e;`) can be dropped entirely. Conservative: only
/// recognizes pure load / arithmetic shapes; calls, assignments, and every
/// pre/post-increment variant make it false, as does any unrecognized node
/// (so we never elide something with a hidden side effect). Fixture 1960.
pub(crate) fn expr_is_pure(e: &Expr) -> bool {
    match e {
        Expr::IntLit(_)
        | Expr::FloatLit(..)
        | Expr::Local(_)
        | Expr::Param(_)
        | Expr::Global(_)
        | Expr::StrLit(_)
        | Expr::StrLitByte { .. }
        | Expr::FuncAddr(_)
        | Expr::AddrOfGlobal(_)
        | Expr::AddrOfLocal(_)
        | Expr::LocalField { .. }
        | Expr::ParamField { .. }
        | Expr::GlobalField { .. }
        | Expr::DerefLocalField { .. }
        | Expr::DerefParamField { .. }
        | Expr::DerefGlobalField { .. }
        | Expr::BitField { .. } => true,
        Expr::BinOp { left, right, .. } => expr_is_pure(left) && expr_is_pure(right),
        Expr::Ternary { cond, then_arm, else_arm } => {
            expr_is_pure(cond) && expr_is_pure(then_arm) && expr_is_pure(else_arm)
        }
        Expr::CastChar { value, .. } => expr_is_pure(value),
        Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => expr_is_pure(ptr),
        // A pointer-chain field read is a side-effect-free memory walk.
        Expr::PtrChainField { base, .. } => expr_is_pure(base),
        Expr::Index { index, .. }
        | Expr::IndexByte { index, .. }
        | Expr::LocalIndex { index, .. }
        | Expr::LocalIndexByte { index, .. }
        | Expr::ParamIndex { index, .. }
        | Expr::PtrIndexByte { index, .. }
        | Expr::PtrArrayElem { index, .. }
        | Expr::AddrOfIndexedGlobal { index, .. }
        | Expr::StructArrayField { index, .. } => expr_is_pure(index),
        Expr::Index2D { row, col, .. } => expr_is_pure(row) && expr_is_pure(col),
        _ => false,
    }
}
/// Whether an expression contains a multiply that MSC lowers to a hardware
/// `imul` rather than a strength-reduced shift/add chain. `x * K` reduces to
/// shifts when K is a power of two, 1..=15, or 256; a larger non-power-of-two
/// constant multiplier emits `imul`. (Runtime×runtime is conservatively NOT
/// flagged — it could be a long multiply, a helper call, not imul.) Used to
/// decide whether the LAST cdecl call's `add sp,N` cleanup folds into the
/// epilogue: it does NOT when an imul follows the call (fixture 1818).
pub(crate) fn expr_has_imul(e: &Expr) -> bool {
    fn mul_is_imul(left: &Expr, right: &Expr) -> bool {
        let shift_ok = |k: i32| k == 256 || (1..=15).contains(&k) || (k > 0 && (k as u32).is_power_of_two());
        match (left, right) {
            (_, Expr::IntLit(k)) | (Expr::IntLit(k), _) => !shift_ok(*k),
            _ => false,
        }
    }
    match e {
        Expr::BinOp { op: BinOp::Mul, left, right } =>
            mul_is_imul(left, right) || expr_has_imul(left) || expr_has_imul(right),
        Expr::BinOp { left, right, .. } => expr_has_imul(left) || expr_has_imul(right),
        Expr::Ternary { cond, then_arm, else_arm } =>
            expr_has_imul(cond) || expr_has_imul(then_arm) || expr_has_imul(else_arm),
        Expr::CastChar { value, .. } | Expr::CastLong { value, .. }
        | Expr::AssignExpr { value, .. } => expr_has_imul(value),
        Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => expr_has_imul(ptr),
        Expr::Index { index, .. } | Expr::IndexByte { index, .. }
        | Expr::LocalIndex { index, .. } | Expr::LocalIndexByte { index, .. }
        | Expr::ParamIndex { index, .. } => expr_has_imul(index),
        Expr::Seq { value, .. } => expr_has_imul(value),
        Expr::Call { args, .. } => args.iter().any(expr_has_imul),
        Expr::CallPtr { target, args } => expr_has_imul(target) || args.iter().any(expr_has_imul),
        _ => false,
    }
}
/// Whether a statement's expressions contain an imul-lowered multiply.
pub(crate) fn stmt_has_imul(s: &Stmt) -> bool {
    match s {
        Stmt::Return(e) | Stmt::ExprStmt(e) => expr_has_imul(e),
        Stmt::Assign { value, .. } => expr_has_imul(value),
        Stmt::Block(v) => v.iter().any(stmt_has_imul),
        _ => false,
    }
}
/// The in-place modification a word global-array element undergoes in the
/// SI-address-CSE pattern (`arr[i] <rmw>; return arr[i]`). Only compound
/// arithmetic (`++`/`--`/`+= K`/`-= K`) qualifies — a const store `arr[i] = K`
/// does NOT CSE the address (MSC stores via BX and folds the read to `mov ax,K`).
/// Fixture 1963.
#[derive(Clone, Copy)]
pub(crate) enum ElemRmw { Add(i32), Sub(i32) }

/// Detect `arr[i] <modify>; return arr[i];` — a write then an immediate read of
/// the SAME word global-array element at a runtime index. MSC computes the
/// element ADDRESS once into SI and reuses it for the write and the read
/// (`mov si,[i]; shl si,1; add si,OFFSET arr; <op> word [si]; mov ax,[si]`),
/// saving SI in the frame. Fixture 1963. Returns (array, index, modify).
pub(crate) fn global_index_cse_pair<'a>(
    write: &'a Stmt,
    read: &Stmt,
    long_globals: &[bool],
    global_elem_sizes: &[usize],
    inits: &[Option<i32>],
) -> Option<(usize, &'a Expr, ElemRmw)> {
    let Stmt::Assign { target: AssignTarget::IndexedGlobalVar { array, index }, value } = write
    else { return None };
    // Word (int) array only: exclude long; char arrays use IndexedGlobalByteVar.
    if long_globals.get(*array).copied().unwrap_or(false) { return None; }
    if global_elem_sizes.get(*array).copied().unwrap_or(2) != 2 { return None; }
    // The index must be a single runtime variable (so the SAME address serves
    // both accesses without recomputation/side effects).
    if index.fold(inits).is_some()
        || !matches!(index.as_ref(), Expr::Param(_) | Expr::Local(_) | Expr::Global(_))
    {
        return None;
    }
    let Stmt::Return(Expr::Index { array: a2, index: i2 }) = read else { return None };
    if a2 != array || !crate::codegen::assign::simple_index_eq(index, i2) { return None; }
    let same_elem = |l: &Expr| matches!(l, Expr::Index { array: la, index: li }
        if la == array && crate::codegen::assign::simple_index_eq(index, li));
    let rmw = match value {
        Expr::BinOp { op, left, right } if same_elem(left.as_ref()) => {
            let Expr::IntLit(k) = right.as_ref() else { return None };
            match op {
                BinOp::Add => ElemRmw::Add(*k),
                BinOp::Sub => ElemRmw::Sub(*k),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some((*array, index.as_ref(), rmw))
}
/// Emit the SI-address-CSE sequence for [`global_index_cse_pair`], ending with
/// the function epilogue (it consumes both the write and the `return` read).
pub(crate) fn emit_global_index_cse(
    array: usize,
    index: &Expr,
    rmw: ElemRmw,
    locals: &Locals<'_>,
    frame: Frame,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // mov si, <index>
    match index {
        Expr::Param(p) => { let d = param_disp(*p); out.push(0x8B); out.push(bp_modrm(0x76, d)); push_bp_disp(out, d); }
        Expr::Local(l) => { let d = locals.disp(*l); out.push(0x8B); out.push(bp_modrm(0x76, d)); push_bp_disp(out, d); }
        Expr::Global(g) => {
            out.push(0x8B);
            let bo = out.len();
            out.extend_from_slice(&[0x36, 0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *g } });
        }
        _ => unreachable!("validated by global_index_cse_pair"),
    }
    out.extend_from_slice(&[0xD1, 0xE6]); // shl si,1  (word element ×2)
    // add si, OFFSET arr  (81 /0; imm16 carries the array's data offset via fixup)
    out.push(0x81);
    let bo = out.len();
    out.extend_from_slice(&[0xC6, 0x00, 0x00]);
    fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: array } });
    // <op> word [si]
    match rmw {
        ElemRmw::Add(1) => out.extend_from_slice(&[0xFF, 0x04]),  // inc word [si]
        ElemRmw::Sub(1) => out.extend_from_slice(&[0xFF, 0x0C]),  // dec word [si]
        ElemRmw::Add(k) | ElemRmw::Sub(k) => {
            let modrm = if matches!(rmw, ElemRmw::Sub(_)) { 0x2C } else { 0x04 };
            if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x83, modrm, k8 as u8]);
            } else {
                out.extend_from_slice(&[0x81, modrm]);
                out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
            }
        }
    }
    out.extend_from_slice(&[0x8B, 0x04]); // mov ax,[si]
    crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
}
/// True when a condition evaluates with no side effects (so an `if` with an
/// empty body can be dropped). Mirrors [`expr_is_pure`] across the Cond tree.
pub(crate) fn cond_is_pure(cond: &Cond) -> bool {
    match cond {
        Cond::Truthy(e) => expr_is_pure(e),
        Cond::Cmp { left, right, .. } => expr_is_pure(left) && expr_is_pure(right),
        Cond::And(a, b) | Cond::Or(a, b) => cond_is_pure(a) && cond_is_pure(b),
    }
}
/// True when a statement produces no code — `;` or a block of only such.
pub(crate) fn stmt_is_empty(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Empty => true,
        Stmt::Block(v) => v.iter().all(stmt_is_empty),
        _ => false,
    }
}
pub(crate) fn stmt_exits(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return(_) | Stmt::Goto(_) => true,
        Stmt::Block(v) => v.last().is_some_and(stmt_exits),
        Stmt::If { then_branch, else_branch, .. } =>
            stmt_exits(then_branch) && else_branch.as_ref().is_some_and(|e| stmt_exits(e)),
        _ => false,
    }
}
pub(crate) fn stmt_always_returns(stmt: &Stmt, locals: &Locals<'_>) -> bool {
    match stmt {
        Stmt::Return(_) => true,
        Stmt::Empty => false,
        Stmt::ExprStmt(_) | Stmt::Assign { .. } => false,
        // A label is a reachable join point; a bare goto isn't treated as a
        // function exit here (so a following label still emits).
        Stmt::Label(_) | Stmt::Goto(_) => false,
        // A const-true loop with no loop-level break never falls through —
        // control can't reach past it, so the function epilogue after it is
        // dead (fixtures 3396/3397). Runtime-cond loops can fall through.
        Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. } =>
            infinite_loop_body(stmt).is_some(),
        Stmt::Break | Stmt::Continue => false,
        Stmt::Switch { cases, .. } => {
            // A switch always returns when there is a default arm (all values
            // covered) and every entry point returns — walking C fall-through
            // across arms, so empty shared-label arms (`case 1: case 2: ret`)
            // count via the body they fall into. Fixture 3350.
            let has_default = cases.iter().any(|a| a.value.is_none());
            has_default
                && (0..cases.len()).all(|i| arm_body_terminates(cases, i, locals))
        }
        Stmt::Block(stmts) => stmts.iter().any(|s| stmt_always_returns(s, locals)),
        Stmt::If { cond, then_branch, else_branch } => {
            if let Some(k) = fold_cond(cond, locals) {
                if k != 0 {
                    // Live branch is the then-branch.
                    stmt_always_returns(then_branch, locals)
                } else if let Some(eb) = else_branch {
                    stmt_always_returns(eb, locals)
                } else {
                    false
                }
            } else {
                // Runtime cond: every branch must always return.
                stmt_always_returns(then_branch, locals)
                    && else_branch
                        .as_ref()
                        .is_some_and(|eb| stmt_always_returns(eb, locals))
            }
        }
    }
}
/// Try to fold the condition to a compile-time boolean (returned as
/// an int: 0 = false, anything else = true). Mirrors MSC's
/// const-condition elision. Fixtures 4094 / 4095.
pub(crate) fn expr_references_local(e: &Expr) -> bool {
    match e {
        Expr::Local(_) | Expr::LocalIndex { .. } | Expr::LocalIndexByte { .. }
            | Expr::LocalField { .. } | Expr::DerefLocalField { .. }
            | Expr::AddrOfLocal(_) => true,
        Expr::BinOp { left, right, .. } => expr_references_local(left) || expr_references_local(right),
        Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => expr_references_local(ptr),
        Expr::Ternary { cond, then_arm, else_arm } => {
            expr_references_local(cond) || expr_references_local(then_arm) || expr_references_local(else_arm)
        }
        Expr::Call { args, .. } => args.iter().any(expr_references_local),
        _ => false,
    }
}
pub(crate) fn cond_references_local(cond: &Cond) -> bool {
    match cond {
        Cond::Truthy(e) => expr_references_local(e),
        Cond::Cmp { left, right, .. } => expr_references_local(left) || expr_references_local(right),
        Cond::And(a, b) | Cond::Or(a, b) => cond_references_local(a) || cond_references_local(b),
    }
}
/// True when at least one leaf of the condition is a pure literal (evaluable
/// without any variable inits). Used to gate while-loop do-while elision:
/// MSC elides the initial jmp only when the condition has a literal side.
pub(crate) fn cond_has_literal_side(cond: &Cond) -> bool {
    match cond {
        Cond::Truthy(e) => e.fold(&[]).is_some(),
        Cond::Cmp { left, right, .. } => {
            left.fold(&[]).is_some() || right.fold(&[]).is_some()
        }
        Cond::And(a, b) | Cond::Or(a, b) => {
            cond_has_literal_side(a) || cond_has_literal_side(b)
        }
    }
}
/// Emit the side effects of a condition's operands when the cond is being
/// elided by const-folding — specifically assignment-expressions like
/// `if ((x=5))`, whose store must still run even though the branch is decided
/// at compile time. Emits each AssignExpr operand (store + value-in-AX).
pub(crate) fn emit_cond_side_effects(cond: &Cond, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    fn walk(e: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
        if matches!(e, Expr::AssignExpr { .. }) {
            emit_expr_to_ax(e, locals, out, fixups);
        }
    }
    match cond {
        Cond::Truthy(e) => walk(e, locals, out, fixups),
        Cond::Cmp { left, right, .. } => {
            walk(left, locals, out, fixups);
            walk(right, locals, out, fixups);
        }
        Cond::And(a, b) | Cond::Or(a, b) => {
            emit_cond_side_effects(a, locals, out, fixups);
            emit_cond_side_effects(b, locals, out, fixups);
        }
    }
}
pub(crate) fn fold_cond(cond: &Cond, locals: &Locals<'_>) -> Option<i32> {
    match cond {
        Cond::Truthy(e) => e.fold(locals.inits),
        Cond::Cmp { op, left, right } => {
            let l = left.fold(locals.inits)?;
            let r = right.fold(locals.inits)?;
            Some(match op {
                RelOp::Eq => i32::from(l == r),
                RelOp::Ne => i32::from(l != r),
                RelOp::Lt => i32::from(l < r),
                RelOp::Gt => i32::from(l > r),
                RelOp::Le => i32::from(l <= r),
                RelOp::Ge => i32::from(l >= r),
            })
        }
        Cond::And(a, b) => {
            let av = fold_cond(a, locals)?;
            if av == 0 { return Some(0); }
            let bv = fold_cond(b, locals)?;
            Some(i32::from(bv != 0))
        }
        Cond::Or(a, b) => {
            let av = fold_cond(a, locals)?;
            if av != 0 { return Some(1); }
            let bv = fold_cond(b, locals)?;
            Some(i32::from(bv != 0))
        }
    }
}
/// True if `e` contains an `(int)(long >> K)` extract (any constant K>0). MSC
/// reads the long's words from the slot and shifts at runtime rather than
/// folding to a constant, so we must skip the const-fold path and let
/// emit_expr_to_ax emit the slot read(s). Fixtures 1949 (>>16), 1951 (>>8), 2189.
fn expr_has_long_highword(e: &Expr, locals: &Locals<'_>) -> bool {
    match e {
        Expr::BinOp { op: BinOp::Shr, left, right }
            if long_operand(left, locals)
                && matches!(right.fold(locals.inits), Some(k) if k > 0) => true,
        Expr::BinOp { left, right, .. } =>
            expr_has_long_highword(left, locals) || expr_has_long_highword(right, locals),
        _ => false,
    }
}
/// A folded-ternary return collapses to an immediate only when both arms are
/// "simple" direct values (a literal, or a variable whose value is known by
/// const-prop). A compound arm (e.g. `-a`) blocks the collapse: MSC then folds
/// only the condition and loads the selected arm at runtime. Non-ternary exprs
/// always qualify, so unrelated folds are unaffected.
fn ternary_folds_to_immediate(expr: &Expr, locals: &Locals<'_>) -> bool {
    fn is_simple_const_arm(e: &Expr, locals: &Locals<'_>) -> bool {
        match e {
            Expr::IntLit(_) => true,
            Expr::Local(_) | Expr::Param(_) | Expr::Global(_) => e.fold(locals.inits).is_some(),
            _ => false,
        }
    }
    match expr {
        Expr::Ternary { then_arm, else_arm, .. } => {
            is_simple_const_arm(then_arm, locals) && is_simple_const_arm(else_arm, locals)
        }
        _ => true,
    }
}
/// MSC distributes an operation applied to a ternary into both arms:
/// `(cond ? a : b) OP k` → `cond ? (a OP k) : (b OP k)` (fixture 432
/// `return (g>0?g:-g)+1` → each arm computes its value, adds 1, then rets).
/// Only lift when the non-ternary operand is side-effect-free and cheap to
/// duplicate (literal / direct variable read).
fn try_lift_ternary_return(expr: &Expr) -> Option<Expr> {
    let Expr::BinOp { op, left, right } = expr else { return None; };
    let dup_ok = |e: &Expr| {
        matches!(e, Expr::IntLit(_) | Expr::Local(_) | Expr::Param(_) | Expr::Global(_))
    };
    if let Expr::Ternary { cond, then_arm, else_arm } = left.as_ref()
        && dup_ok(right)
    {
        return Some(Expr::Ternary {
            cond: cond.clone(),
            then_arm: Box::new(Expr::BinOp { op: *op, left: then_arm.clone(), right: right.clone() }),
            else_arm: Box::new(Expr::BinOp { op: *op, left: else_arm.clone(), right: right.clone() }),
        });
    }
    if let Expr::Ternary { cond, then_arm, else_arm } = right.as_ref()
        && dup_ok(left)
    {
        return Some(Expr::Ternary {
            cond: cond.clone(),
            then_arm: Box::new(Expr::BinOp { op: *op, left: left.clone(), right: then_arm.clone() }),
            else_arm: Box::new(Expr::BinOp { op: *op, left: left.clone(), right: else_arm.clone() }),
        });
    }
    None
}
/// A branch that is exactly one assignment to a plain int word LOCAL
/// (optionally wrapped in a one-statement block): `(local_idx, value)`.
/// Long/float/register/array locals bail — they don't share the merged
/// `mov [r],ax` store shape. Fixture 2865.
fn single_word_local_assign<'e>(s: &'e Stmt, locals: &Locals<'_>) -> Option<(usize, &'e Expr)> {
    let s = match s {
        Stmt::Block(v) if v.len() == 1 => &v[0],
        other => other,
    };
    let Stmt::Assign { target: AssignTarget::Local(i), value } = s else { return None };
    if locals.size(*i) != 2
        || locals.is_long_local(*i)
        || locals.is_float_local(*i)
        || locals.reg_for_local(*i).is_some()
        || locals.array_locals.get(*i).copied().unwrap_or(false)
    {
        return None;
    }
    Some((*i, value))
}
pub(crate) fn emit_return(
    expr: &Expr,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // `return *p` where p is a `long *` param in a long-returning function: load
    // both words of the pointee (`mov bx,[p]; mov ax,[bx]; mov dx,[bx+2]`).
    // Fixture 3285.
    if return_long
        && let Expr::DerefWord { ptr } = expr
        && let Expr::Param(i) = ptr.as_ref()
        && locals.param_pointee_size(*i) == 4
    {
        let d = param_disp(*i) as u8;
        out.extend_from_slice(&[0x8B, 0x5E, d]);       // mov bx,[bp+d]
        out.extend_from_slice(&[0x8B, 0x07]);          // mov ax,[bx]
        out.extend_from_slice(&[0x8B, 0x57, 0x02]);    // mov dx,[bx+2]
        crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
        return;
    }
    // `return *p` where p is a `long *` GLOBAL: load both pointee words. The
    // DerefWord emit produces `mov bx,[p]; mov ax,[bx]`; append `mov dx,[bx+2]`
    // instead of the int-style `cwd`. Fixture 3286.
    if return_long
        && let Expr::DerefWord { ptr } = expr
        && matches!(ptr.as_ref(), Expr::Global(_))
    {
        emit_expr_to_ax(expr, locals, out, fixups);    // mov bx,[p]; mov ax,[bx]
        out.extend_from_slice(&[0x8B, 0x57, 0x02]);    // mov dx,[bx+2]
        crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
        return;
    }
    // `return <long-param> >> 16` (long fn): the high word becomes the low word
    // and the new high word is 0 — `mov ax,[v+2]; sub dx,dx`. Fixtures 2801, 2998.
    if return_long
        && let Expr::BinOp { op: BinOp::Shr, left, right } = expr
        && matches!(right.as_ref(), Expr::IntLit(16))
        && let Expr::Param(i) = left.as_ref()
        && locals.is_long_param(*i)
        && locals.is_unsigned_param(*i) // signed >>16 sign-extends; this is the logical case
    {
        let hi = (param_disp(*i) + 2) as u8;
        out.extend_from_slice(&[0x8B, 0x46, hi]);   // mov ax,[bp+v+2]
        out.extend_from_slice(&[0x2B, 0xD2]);        // sub dx,dx
        crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
        return;
    }
    // `return <long-param> << 16` (long fn): the low word becomes the high word
    // and the new low word is 0 — `mov dx,[v]; sub ax,ax`. Fixture 2875.
    if return_long
        && let Expr::BinOp { op: BinOp::Shl, left, right } = expr
        && matches!(right.as_ref(), Expr::IntLit(16))
        && let Expr::Param(i) = left.as_ref()
        && locals.is_long_param(*i)
    {
        let lo = param_disp(*i) as u8;
        out.extend_from_slice(&[0x8B, 0x56, lo]);   // mov dx,[bp+v]
        out.extend_from_slice(&[0x2B, 0xC0]);        // sub ax,ax
        crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
        return;
    }
    // `return (char)<e>` / `return <e> & 0xFF` (the parser lowers `& 0xFF` to a
    // CastChar) in a long-returning function: produce the (zero/sign-)extended
    // byte in AX, then extend into the long high word — `sub dx,dx` for unsigned
    // (zero-extend), `cwd` for signed. Fixture 3199 (`return v & 0xFFL`).
    if return_long
        && let Expr::CastChar { unsigned, .. } = expr
    {
        emit_expr_to_ax(expr, locals, out, fixups); // AX = extended byte
        if *unsigned {
            out.extend_from_slice(&[0x2B, 0xD2]); // sub dx,dx
        } else {
            out.push(0x99); // cwd
        }
        crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
        return;
    }
    // Small struct returned by value: load the struct local's bytes into the
    // return registers — AX for <=2 bytes, DX:AX for 3-4. Reuse a value the
    // preceding store just left in AL/AX (fixture 3408). 1-byte structs
    // zero-extend the byte (`sub ah,ah`, fixture 2537); 2-byte load a word
    // (2531); int-field structs reuse AX (3408).
    if locals.return_struct_bytes > 0
        && let Expr::Local(i) = expr
    {
        let n = locals.return_struct_bytes;
        let disp = locals.disp(*i);
        // Struct > 4 bytes: copy it to the `_BSS` scratch temp via `movsw` and
        // return `AX = OFFSET $T` (hidden-pointer ABI). Fixtures 2755, 3410.
        if n > 4
            && let Some(bss_offset) = locals.struct_temp_bss_offset
        {
            let bo = out.len();
            out.push(0xBF); out.extend_from_slice(&[0x00, 0x00]); // mov di, OFFSET $T
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::StructTemp { bss_offset } });
            out.push(0x8D); out.push(bp_modrm(0x76, disp)); push_bp_disp(out, disp); // lea si,[bp+disp]
            out.extend_from_slice(&[0x1E, 0x07]); // push ds; pop es
            for _ in 0..(n / 2) { out.push(0xA5); } // movsw
            let bo = out.len();
            out.push(0xB8); out.extend_from_slice(&[0x00, 0x00]); // mov ax, OFFSET $T
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::StructTemp { bss_offset } });
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        }
        if n == 1 {
            let byte_store = { let mut v = vec![0x88, bp_modrm(0x46, disp)]; push_bp_disp(&mut v, disp); v };
            let al_set = out.len() >= byte_store.len() && out[out.len() - byte_store.len()..] == *byte_store;
            if !al_set {
                out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov al,[t]
            }
            out.extend_from_slice(&[0x2A, 0xE4]); // sub ah, ah
        } else {
            let word_store = { let mut v = vec![0x89, bp_modrm(0x46, disp)]; push_bp_disp(&mut v, disp); v };
            let ax_set = out.len() >= word_store.len() && out[out.len() - word_store.len()..] == *word_store;
            if !ax_set {
                out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov ax,[t]
            }
            if n > 2 {
                let hi = disp + 2;
                out.push(0x8B); out.push(bp_modrm(0x56, hi)); push_bp_disp(out, hi); // mov dx,[t+2]
            }
        }
        crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
        return;
    }
    // Returning a struct GLOBAL by value (`return g`): same as the local case
    // but the source is a fixed global address. Fixtures 422 (4B), 423 (6B).
    if locals.return_struct_bytes > 0
        && let Expr::Global(g) = expr
    {
        let n = locals.return_struct_bytes;
        if n > 4
            && let Some(bss_offset) = locals.struct_temp_bss_offset
        {
            let bo = out.len();
            out.push(0xBF); out.extend_from_slice(&[0x00, 0x00]); // mov di, OFFSET $T
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::StructTemp { bss_offset } });
            let bo = out.len();
            out.push(0xBE); out.extend_from_slice(&[0x00, 0x00]); // mov si, OFFSET g
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *g } });
            out.extend_from_slice(&[0x1E, 0x07]); // push ds; pop es
            for _ in 0..(n / 2) { out.push(0xA5); } // movsw
            let bo = out.len();
            out.push(0xB8); out.extend_from_slice(&[0x00, 0x00]); // mov ax, OFFSET $T
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::StructTemp { bss_offset } });
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        }
        if n == 1 {
            let bo = out.len();
            out.push(0xA0); out.extend_from_slice(&[0x00, 0x00]); // mov al, _g
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *g } });
            out.extend_from_slice(&[0x2A, 0xE4]); // sub ah, ah
        } else {
            let bo = out.len();
            out.push(0xA1); out.extend_from_slice(&[0x00, 0x00]); // mov ax, _g
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *g } });
            if n > 2 {
                out.push(0x8B); out.push(0x16); // mov dx, [g+2]
                let bo = out.len();
                out.extend_from_slice(&2u16.to_le_bytes());
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
            }
        }
        crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
        return;
    }
    // Float/double return via the `__fac` accumulator. The parser folds the
    // returned expression to a FloatLit (materialized as a CONST temp):
    //   9B <D9|DD> 06 <off16>  fld  <width> [$T]        (FIDRQQ + FloatLoad)
    //   9B DD 1E <off16>       fstp QWORD [__fac]        (FIDRQQ + ExtData)
    //   B8 <imm16>             mov  ax, OFFSET __fac     (ExtData)
    //   90 9B                  nop; fwait                (FIWRQQ)
    if locals.return_float_width != 0 {
        // Produce the return value on st(0):
        //   FloatLit          → fld <width> [$T]              (const-folded)
        //   int param         → fild WORD [bp+disp]           (`(double)x`)
        //   float/double param→ fld <width> [bp+disp]
        let produced = match expr {
            Expr::FloatLit(bits, is_double) => {
                let width = if *is_double { 8 } else { 4 };
                let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
                fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                out.push(0x9B);
                out.push(op);
                out.push(0x06);
                let bo = out.len() - 1;
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::FloatLoad { bits: *bits, width } });
                true
            }
            Expr::Param(i) if !locals.is_float_param(*i) => {
                // `(double)<int param>` → fild WORD [bp+disp]
                let disp = param_disp(*i);
                fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                out.push(0x9B);
                out.push(0xDF); // fild m16int (/0)
                out.push(bp_modrm(0x46, disp));
                push_bp_disp(out, disp);
                true
            }
            Expr::Param(i) => {
                let width = locals.float_param_width(*i);
                let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
                let disp = param_disp(*i);
                fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                out.push(0x9B);
                out.push(op);
                out.push(bp_modrm(0x46, disp));
                push_bp_disp(out, disp);
                true
            }
            // `<float/double param> OP <float literal>` — load the param onto
            // st(0), then apply the op with the constant as a QWORD memory
            // operand: `f<op> qword [$T]` (DC /r m64). Fixtures 2144, 2146
            // (`x / 2.0`, `d * 10.0`).
            Expr::BinOp { op, left, right }
                if matches!(left.as_ref(), Expr::Param(i) if locals.is_float_param(*i))
                    && matches!(right.as_ref(), Expr::FloatLit(..)) =>
            {
                let Expr::Param(i) = left.as_ref() else { unreachable!() };
                let Expr::FloatLit(bits, _) = right.as_ref() else { unreachable!() };
                let width = locals.float_param_width(*i);
                let ld = if width == 4 { 0xD9u8 } else { 0xDDu8 };
                let disp = param_disp(*i);
                // fld <width> [bp+disp]
                fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                out.push(0x9B);
                out.push(ld);
                out.push(bp_modrm(0x46, disp));
                push_bp_disp(out, disp);
                // f<op> qword [$T]  (DC /r m64: /0 fadd /1 fmul /4 fsub /6 fdiv)
                let reg = match op { BinOp::Add => 0u8, BinOp::Mul => 1, BinOp::Sub => 4, BinOp::Div => 6, _ => 0 };
                fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                out.push(0x9B);
                out.push(0xDC);
                out.push(0x06 | (reg << 3));
                let bo = out.len() - 1;
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::FloatLoad { bits: *bits, width: 8 } });
                true
            }
            _ => false,
        };
        if produced {
            // fstp QWORD [__fac]  (always qword, modrm 1E = /3 [disp16])
            fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
            out.push(0x9B);
            out.push(0xDD);
            out.push(0x1E);
            let bo = out.len() - 1;
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::ExtData { target: "__fac" } });
            // mov ax, OFFSET __fac
            out.push(0xB8);
            let bo = out.len() - 1;
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::ExtData { target: "__fac" } });
            // nop; fwait (emulator patch slot, FIWRQQ marker)
            fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIWRQQ" } });
            out.push(0x90);
            out.push(0x9B);
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        }
    }
    if return_int {
        // `(cond ? a : b) OP k` in a return distributes OP into both arms and
        // emits the two-epilogue ternary structure (fixture 432). Only for
        // runtime conds — folded ternaries fold through the normal paths.
        if let Some(lifted) = try_lift_ternary_return(expr)
            && let Expr::Ternary { cond, .. } = &lifted
            && cond.fold(locals.inits).is_none()
        {
            emit_return(&lifted, locals, frame, return_int, return_long, out, fixups);
            return;
        }
        // Return-of-call peephole: `return f(args);` leaves the
        // result in AX from the call's return value — no extra
        // load before ret. Fixture 4102 confirms.
        // For WithSlide frames, `mov sp, bp` in the epilogue restores sp
        // across the pushed args — no `add sp, N` cleanup needed.
        if let Expr::Call { name, args } = expr
            && let Some(&width) = locals.float_returners.get(&symbol_name(name))
        {
            // `return (int)<float-returning call>`: receive the result into the
            // hidden temp via __fac, then convert. The int args (if any) clean
            // up with the normal `add sp,N` — the float-return receive (di/si
            // movsw copy) follows, so MSC keeps the cleanup here (fixture 4001).
            locals.elide_call_cleanup.set(false);
            emit_call_inner(name, args, locals, false, out, fixups);
            let disp = locals.float_call_temp_disp;
            out.push(0x8D); // lea di, [bp+disp]
            out.push(bp_modrm(0x7E, disp));
            push_bp_disp(out, disp);
            out.extend_from_slice(&[0x8B, 0xF0]); // mov si, ax
            out.extend_from_slice(&[0x16, 0x07]); // push ss; pop es
            for _ in 0..(width / 2) {
                out.push(0xA5); // movsw
            }
            // fld QWORD [bp+disp]; call __ftol
            fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
            out.push(0x9B);
            out.push(0xDD);
            out.push(bp_modrm(0x46, disp));
            push_bp_disp(out, disp);
            let call_off = out.len();
            out.extend_from_slice(&[0xE8, 0x00, 0x00]);
            fixups.push(Fixup { body_offset: call_off, kind: FixupKind::ExtCall { target: "__ftol".to_owned() } });
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if let Expr::Call { name, args } = expr {
            let has_float_arg = args.iter().any(|a| matches!(a, Expr::FloatLit(..)));
            let skip_cleanup = frame.is_with_slide();
            emit_call_inner(name, args, locals, skip_cleanup, out, fixups);
            if has_float_arg {
                // The float-arg push slid SP, so the result is spilled to the
                // frame temp at [bp-2]; MSC emits this store even though AX
                // already holds the value the function returns. The WithSlide
                // epilogue (`mov sp,bp`) reclaims the arg space.
                out.extend_from_slice(&[0x89, 0x46, 0xFE]); // mov [bp-2], ax
            }
        } else if let Expr::CallPtr { target, args } = expr {
            // `return <fnptr>(args);` — like the direct return-of-call, a slide
            // frame skips the per-call cleanup (the epilogue's `mov sp,bp`
            // reclaims the pushed args). Fixtures 187, 2211.
            let skip_cleanup = frame.is_with_slide();
            crate::codegen::calls::emit_call_ptr(target, args, locals, skip_cleanup, out, fixups);
        } else if let Expr::Param(i) = expr
            && locals.is_float_param(*i)
        {
            // `return (int)<float/double param>` — the `(int)` cast parses to
            // an identity, so this is just `Expr::Param`. Load the param onto
            // the FPU stack and convert to int via `__ftol` (result in AX).
            let width = locals.float_param_width(*i);
            let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
            let disp = param_disp(*i);
            fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
            out.push(0x9B);
            out.push(op);
            out.push(bp_modrm(0x46, disp));
            push_bp_disp(out, disp);
            let body_offset = out.len();
            out.extend_from_slice(&[0xE8, 0x00, 0x00]); // call __ftol
            fixups.push(Fixup { body_offset, kind: FixupKind::ExtCall { target: "__ftol".to_owned() } });
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if let Expr::Global(idx) = expr
            && locals.is_float_global(*idx)
        {
            // `return (int)<float/double global>` → fld <width> [g]; call __ftol.
            let width = locals.float_global_width(*idx);
            let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
            fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
            out.push(0x9B);
            out.push(op);
            out.push(0x06); // modrm /0 [disp16]
            let body_offset = out.len() - 1; // the 06 modrm; off16 at +1
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: *idx } });
            let call_off = out.len();
            out.extend_from_slice(&[0xE8, 0x00, 0x00]); // call __ftol
            fixups.push(Fixup { body_offset: call_off, kind: FixupKind::ExtCall { target: "__ftol".to_owned() } });
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if let Some((array, k, fop)) = float_global_index_return(expr, locals) {
            // `return (int)<float-global-array>[K]` (optionally `<op> <floatlit>`):
            //   fld QWORD [arr+K*w]; [f<op> QWORD [$T]]; call __ftol
            let width = locals.float_global_width(array);
            let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
            let byte_off = (k as u32).wrapping_mul(width as u32) as u16;
            fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
            out.push(0x9B);
            out.push(op);
            out.push(0x06); // modrm /0 [disp16]
            let bo = out.len() - 1;
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: array } });
            if let Some((binop, bits)) = fop {
                let reg = match binop {
                    BinOp::Add => 0u8, BinOp::Mul => 1, BinOp::Sub => 4, BinOp::Div => 6, _ => 0,
                };
                fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                out.push(0x9B);
                out.push(0xDC);
                out.push(0x06 | (reg << 3));
                let bo = out.len() - 1;
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::FloatLoad { bits, width: 8 } });
            }
            let call_off = out.len();
            out.extend_from_slice(&[0xE8, 0x00, 0x00]); // call __ftol
            fixups.push(Fixup { body_offset: call_off, kind: FixupKind::ExtCall { target: "__ftol".to_owned() } });
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if let Some(idx) = coupled_return_local(expr)
            && locals.is_float_local(idx)
        {
            // `return (int)(<op on float local>)`. The local's value is on st(0)
            // (coupled init stored with `fst`); otherwise reload with `fld`.
            if locals.fpu_live.get() != Some(idx) {
                let width = locals.float_local_width(idx);
                let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
                let disp = locals.disp(idx);
                fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                out.push(0x9B);
                out.push(op);
                out.push(bp_modrm(0x46, disp));
                push_bp_disp(out, disp);
            }
            // Apply the FP operation to st(0):
            //   bare local        → nothing (value already there)
            //   0 - local         → fchs  (9B D9 E0)
            //   local <op> <flit> → f<op> QWORD [$T]  (DC mem form)
            match expr {
                Expr::BinOp { op: BinOp::Sub, left, .. } if matches!(left.as_ref(), Expr::IntLit(0)) => {
                    fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                    out.extend_from_slice(&[0x9B, 0xD9, 0xE0]); // fchs
                }
                Expr::BinOp { op, right, .. } if matches!(right.as_ref(), Expr::FloatLit(..)) => {
                    let (bits, _) = match right.as_ref() { Expr::FloatLit(b, d) => (*b, *d), _ => unreachable!() };
                    // DC /r m64: /0 fadd, /1 fmul, /4 fsub, /6 fdiv.
                    let reg = match op {
                        BinOp::Add => 0u8, BinOp::Mul => 1, BinOp::Sub => 4, BinOp::Div => 6,
                        _ => 0,
                    };
                    fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                    out.push(0x9B);
                    out.push(0xDC);
                    out.push(0x06 | (reg << 3)); // modrm /reg [disp16]
                    let bo = out.len() - 1;
                    out.extend_from_slice(&[0x00, 0x00]);
                    fixups.push(Fixup { body_offset: bo, kind: FixupKind::FloatLoad { bits, width: 8 } });
                }
                _ => {} // bare local
            }
            let call_off = out.len();
            out.extend_from_slice(&[0xE8, 0x00, 0x00]); // call __ftol
            fixups.push(Fixup { body_offset: call_off, kind: FixupKind::ExtCall { target: "__ftol".to_owned() } });
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if matches!(expr, Expr::Local(i) if locals.is_long_local(*i)) {
            let i = if let Expr::Local(i) = expr { *i } else { unreachable!() };
            if return_long {
                // Long-returning function: `return x`. A known long materializes
                // as `mov ax,lo; cwd` (fixture 298); otherwise load both slot
                // words (`mov ax,[lo]; mov dx,[hi]`).
                if let Some(k) = expr.fold(locals.inits) {
                    emit_long_const_to_dx_ax(k, out);
                } else {
                    let disp = locals.disp(i);
                    let hi = disp + 2;
                    // DX:AX reuse: if the slot was just written from DX:AX
                    // (`mov [lo],ax; mov [hi],dx`), both words are already live —
                    // skip the reload (the long analog of the AX-liveness reuse).
                    // Fixture 3230 (`n = x+1; return n`).
                    let store = {
                        let mut v = vec![0x89, bp_modrm(0x46, disp)];
                        push_bp_disp(&mut v, disp);
                        v.push(0x89); v.push(bp_modrm(0x56, hi));
                        push_bp_disp(&mut v, hi);
                        v
                    };
                    let live = out.len() >= store.len() && out[out.len() - store.len()..] == *store;
                    if !live {
                        emit_expr_to_ax(expr, locals, out, fixups); // mov ax,[lo]
                        out.extend_from_slice(&[0x8B, 0x56, hi as u8]); // mov dx,[hi]
                    }
                }
            } else {
                // `(int)<long-local>`: read the low word from the slot — MSC
                // does NOT fold even when the value is known (fixture 1037). But
                // if AX already holds the low word (just stored via `mov [lo],ax`,
                // with only a `mov [hi],dx` since — which preserves AX), reuse it.
                // Fixture 2521 (`v = *p++; return (int)v`).
                let d = locals.disp(i);
                let store_self = { let mut v = vec![0x89, bp_modrm(0x46, d)]; push_bp_disp(&mut v, d); v };
                let load = { let mut v = vec![0x8B, bp_modrm(0x46, d)]; push_bp_disp(&mut v, d); v };
                if !crate::codegen::expr::ax_holds_word_operand(out, &load, &store_self, locals.last_branch_barrier.get()) {
                    emit_expr_to_ax(expr, locals, out, fixups);
                }
            }
        } else if let Expr::Param(i) = expr
            && locals.is_char_param(*i)
        {
            // `return c` where `c` is a char param: byte load then widen —
            // `cbw` for signed, `sub ah,ah` for unsigned char (zero-extend,
            // fixtures 2881/3320). Plus a cwd to widen into DX:AX when the
            // function returns long (`return (long)c`, fixture 3183).
            let disp = param_disp(*i) as u8;
            out.extend_from_slice(&[0x8A, 0x46, disp]); // mov al, [bp+4]
            if locals.is_unsigned_param(*i) {
                out.extend_from_slice(&[0x2A, 0xE4]); // sub ah, ah
            } else {
                out.push(0x98); // cbw
            }
            if return_long {
                out.push(0x99); // cwd
            }
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if return_long
            && let Expr::Param(i) = expr
            && locals.is_long_param(*i)
        {
            // `return x` where x is a long param in a long-returning function:
            // load low word into AX, high word into DX.
            let lo_disp = param_disp(*i) as u8;
            let hi_disp = (param_disp(*i) + 2) as u8;
            out.extend_from_slice(&[0x8B, 0x46, lo_disp]); // mov ax, [bp+lo]
            out.extend_from_slice(&[0x8B, 0x56, hi_disp]); // mov dx, [bp+hi]
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if let Expr::LocalIndex { local: l, index } = expr
            && !locals.is_long_local(*l)
            && index.fold(locals.inits).is_some()
        {
            let k = index.fold(locals.inits).unwrap();
            let word_disp = locals.disp(*l) + k as i16 * 2;
            let prev_store = {
                let mr = bp_modrm(0x46, word_disp);
                let mut v = vec![0x89, mr];
                push_bp_disp(&mut v, word_disp);
                v
            };
            let ax_already_set = out.len() >= prev_store.len()
                && out[out.len()-prev_store.len()..] == *prev_store;
            if !ax_already_set {
                out.push(0x8B);
                out.push(bp_modrm(0x46, word_disp));
                push_bp_disp(out, word_disp);
            }
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if let Expr::LocalIndexByte { local: l, index } = expr
            && index.fold(locals.inits).is_some()
            // A bare VARIABLE index uses runtime SI addressing (handled by the
            // emit_expr_to_ax fallthrough); only a literal index folds to a direct
            // slot load here. Fixture 1428.
            && !matches!(index.as_ref(), Expr::Local(_) | Expr::Param(_))
        {
            let k = index.fold(locals.inits).unwrap();
            let byte_disp = locals.disp(*l) + k as i16;
            let prev_store = {
                let mr = bp_modrm(0x46, byte_disp);
                let mut v = vec![0x88, mr];
                push_bp_disp(&mut v, byte_disp);
                v
            };
            let al_already_set = out.len() >= prev_store.len()
                && out[out.len()-prev_store.len()..] == *prev_store;
            if !al_already_set {
                out.push(0x8A);
                out.push(bp_modrm(0x46, byte_disp));
                push_bp_disp(out, byte_disp);
            }
            out.push(0x98); // cbw
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if let Expr::Global(idx) = expr
            && locals.is_char_global(*idx)
        {
            // Char global return: AL may already hold the value from a preceding
            // `g op= K` (div/mul end `mov [g],al` = a2 00 00 with a GlobalAddr
            // fixup to g; mod ends `mov al,ah` = 8a c4). Else reload `mov al,[g]`.
            // Widen: `cbw` for signed, `sub ah,ah` for unsigned (fixture 466).
            let ends_store = out.len() >= 3
                && out[out.len() - 3..] == [0xA2, 0x00, 0x00]
                && matches!(fixups.last(), Some(Fixup { kind: FixupKind::GlobalAddr { global_idx }, .. }) if *global_idx == *idx);
            let ends_mov_al_ah = out.len() >= 2 && out[out.len() - 2..] == [0x8A, 0xC4];
            if !(ends_store || ends_mov_al_ah) {
                let bo = out.len();
                out.push(0xA0); // mov al, [g]  (moffs fixup at the opcode position)
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *idx } });
            }
            if locals.is_unsigned_global(*idx) {
                out.extend_from_slice(&[0x2A, 0xE4]); // sub ah, ah
            } else {
                out.push(0x98); // cbw
            }
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if let Expr::Local(i) = expr {
            // `return <register local>` (the var lives in SI/DI). Its value is in
            // the register; the return needs it in AX via `mov ax,reg`. But MSC
            // elides that move when AX already holds the register's value — e.g.
            // after a loop body whose last assignment was `p = p * i` (`...; mov
            // si,ax`), the trailing cmp/inc/jcc preserve AX, so AX == SI at loop
            // exit and `return p` falls straight into the epilogue. The defining
            // event is the `mov reg,ax` self-copy (store_self); `mov ax,reg` is
            // the load we would otherwise emit. Fixture 4208.
            if let Some(reg) = locals.reg_for_local(*i) {
                let load = [0x8B, 0xC0 | reg];        // mov ax, reg
                let store_self = [0x8B, 0xC0 | (reg << 3)]; // mov reg, ax
                let ax_already = crate::codegen::expr::ax_holds_word_operand(
                    out, &load, &store_self, locals.last_branch_barrier.get(),
                );
                if !ax_already {
                    out.extend_from_slice(&load); // mov ax, reg
                }
                crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
                return;
            }
            let disp = locals.disp(*i);
            if locals.size(*i) == 1 && !locals.is_unsigned_local(*i) {
                // Signed char local: check if AL is already loaded from a
                // prior byte-store (`88 46 disp`). If so, just cbw; else
                // reload from the slot. Fixture 1039 (non-literal prologue
                // init leaves AL set; emit just cbw for return).
                let ends_with = |pat: &[u8]| out.len() >= pat.len() && out[out.len() - pat.len()..] == *pat;
                // A char compound div/mod already left the WIDENED result in AX
                // (`mov [c],ah; mov al,ah; cbw` for mod, `mov [c],al; cbw` for div)
                // — `return c` reuses it directly, no reload or extra cbw. (1436)
                let mod_tail = {
                    let mut v = vec![0x88, bp_modrm(0x66, disp)];
                    push_bp_disp(&mut v, disp);
                    v.extend_from_slice(&[0x8A, 0xC4, 0x98]);
                    v
                };
                let div_tail = {
                    let mut v = vec![0x88, bp_modrm(0x46, disp)];
                    push_bp_disp(&mut v, disp);
                    v.push(0x98);
                    v
                };
                if ends_with(&mod_tail) || ends_with(&div_tail) {
                    crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
                    return;
                }
                let prev_store = {
                    let mr = bp_modrm(0x46, disp);
                    let mut v = vec![0x88, mr];
                    push_bp_disp(&mut v, disp);
                    v
                };
                let al_already_set = ends_with(&prev_store);
                if !al_already_set {
                    out.push(0x8A);
                    out.push(bp_modrm(0x46, disp));
                    push_bp_disp(out, disp);
                }
                out.push(0x98); // cbw
                crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
                return;
            }
            if locals.size(*i) == 1 && locals.is_unsigned_local(*i) {
                // Unsigned char local: a preceding char compound div/mod/mul left
                // the zero-extended result in AX already (`mov [c],ah; mov al,ah;
                // sub ah,ah` for mod, `mov [c],al; sub ah,ah` for div/mul). Reuse
                // it; else reload `mov al,[c]; sub ah,ah`. Fixtures 677/678/679.
                let ends_with = |pat: &[u8]| out.len() >= pat.len() && out[out.len() - pat.len()..] == *pat;
                let mod_tail = {
                    let mut v = vec![0x88, bp_modrm(0x66, disp)];
                    push_bp_disp(&mut v, disp);
                    v.extend_from_slice(&[0x8A, 0xC4, 0x2A, 0xE4]);
                    v
                };
                let div_tail = {
                    let mut v = vec![0x88, bp_modrm(0x46, disp)];
                    push_bp_disp(&mut v, disp);
                    v.extend_from_slice(&[0x2A, 0xE4]);
                    v
                };
                if ends_with(&mod_tail) || ends_with(&div_tail) {
                    // The tail already zero-extended into AX — just return.
                    crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
                    return;
                }
                // A plain `mov [c],al` self-store (e.g. `c = f(); return (int)c`)
                // leaves AL holding c — reuse it, only widening. Fixture 1988.
                let self_store = {
                    let mut v = vec![0x88, bp_modrm(0x46, disp)];
                    push_bp_disp(&mut v, disp);
                    v
                };
                if ends_with(&self_store) {
                    out.extend_from_slice(&[0x2A, 0xE4]); // sub ah,ah (widen the live AL)
                    crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
                    return;
                }
                out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov al,[c]
                out.extend_from_slice(&[0x2A, 0xE4]); // sub ah,ah
                crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
                return;
            }
            // Non-char or unsigned-char: check if AX already holds the
            // value (`89 46 d` was last emitted). If so, skip reload.
            let prev_store = {
                let mr = bp_modrm(0x46, disp);
                let mut v = vec![0x89, mr];
                push_bp_disp(&mut v, disp);
                v
            };
            // `x %= K` ends `mov [x],dx; mov ax,dx`, which also leaves AX = x.
            let prev_store_mod = {
                let mr = bp_modrm(0x56, disp);
                let mut v = vec![0x89, mr];
                push_bp_disp(&mut v, disp);
                v.extend_from_slice(&[0x8B, 0xC2]);
                v
            };
            // The reuse is only valid for a STRAIGHT-LINE store (at or past the
            // last branch merge) — after an if/loop merge MSC reloads even though
            // AX may happen to hold the value. Fixture 1445.
            let ends_with = |pat: &[u8]| out.len() >= pat.len()
                && out[out.len() - pat.len()..] == *pat
                && out.len() - pat.len() >= locals.last_branch_barrier.get();
            let ax_already_set = ends_with(&prev_store) || ends_with(&prev_store_mod);
            if !ax_already_set {
                emit_expr_to_ax(expr, locals, out, fixups);
            }
        } else if let Expr::Ternary { cond, then_arm, else_arm } = expr
            && let Some(c) = cond.fold(locals.inits)
            && {
                let arm = if c != 0 { then_arm } else { else_arm };
                matches!(arm.as_ref(), Expr::Ternary { .. })
            }
        {
            // A nested ternary whose OUTER condition folds (e.g. const-prop
            // substituted `x=1`): select the arm and recurse so an inner runtime
            // ternary reaches the two-epilogue structure rather than the shared-
            // tail expr fallthrough. Fixture 2413 (`x?(y?10:20):(z?30:40)`).
            let arm = if c != 0 { then_arm } else { else_arm };
            emit_return(arm, locals, frame, return_int, return_long, out, fixups);
            return;
        } else if let Expr::Ternary { cond, then_arm, else_arm } = expr
            && matches!(then_arm.as_ref(), Expr::IntLit(1))
            && matches!(else_arm.as_ref(), Expr::IntLit(0))
            && let Some(disp) = match cond.as_ref() {
                Expr::Param(i) if !locals.is_char_param(*i) && !locals.is_long_param(*i)
                    && !locals.is_float_param(*i) => Some(param_disp(*i)),
                Expr::Local(i) if locals.size(*i) == 2 && !locals.is_long_local(*i)
                    && !locals.is_float_local(*i) => Some(locals.disp(*i)),
                _ => None,
            }
        {
            // `return x ? 1 : 0` (x a word param/local, truthy → boolean): the same
            // carry trick as `return x != 0` — `cmp word [x],1; sbb ax,ax; inc ax`
            // — not the two-epilogue ternary structure. Fixture 1944.
            out.push(0x83); out.push(bp_modrm(0x7E, disp)); push_bp_disp(out, disp); out.push(0x01); // cmp word [bp+d],1
            out.extend_from_slice(&[0x1B, 0xC0, 0x40]); // sbb ax,ax; inc ax
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
            return;
        } else if let Expr::Ternary { cond, then_arm, else_arm } = expr
            && cond.fold(locals.inits).is_none()
            && !is_ternary_select_operand(cond, then_arm, else_arm, locals)
            && !crate::codegen::expr::is_nested_minmax_ternary(then_arm, else_arm, locals)
        {
            // Runtime `return cond ? a : b` — like the comparison/logical
            // returns, MSC inlines a separate epilogue+ret into EACH arm
            // rather than jumping to a shared end (fixture 3141
            // `return x<0?-1:1` → `cmp x,0; jge else; mov ax,-1; pop bp; ret;
            // nop; mov ax,1; pop bp; ret`). An alignment nop precedes the else
            // arm when the running offset would otherwise be odd.
            let cond_c = cond_from_expr((**cond).clone());
            let mut then_buf = Vec::new();
            let mut then_fx = Vec::new();
            emit_return(then_arm, locals, frame, return_int, return_long, &mut then_buf, &mut then_fx);
            let mut else_buf = Vec::new();
            let mut else_fx = Vec::new();
            emit_return(else_arm, locals, frame, return_int, return_long, &mut else_buf, &mut else_fx);
            let cond_size = {
                let mut b = Vec::new();
                emit_cond_skip(&cond_c, 0, locals, &mut b, &mut Vec::new());
                b.len()
            };
            let needs_nop = (out.len() + cond_size + then_buf.len()) % 2 != 0;
            let skip = i8::try_from(then_buf.len() + needs_nop as usize)
                .expect("ternary return then-block fits in rel8");
            emit_cond_skip(&cond_c, skip, locals, out, fixups);
            let tb = out.len();
            for mut f in then_fx { f.body_offset += tb; fixups.push(f); }
            out.extend_from_slice(&then_buf);
            if needs_nop { out.push(0x90); }
            let eb = out.len();
            for mut f in else_fx { f.body_offset += eb; fixups.push(f); }
            out.extend_from_slice(&else_buf);
            return; // both arms already carry their epilogue
        } else if matches!(expr, Expr::BinOp { op: BinOp::LogOr | BinOp::LogAnd, .. }) {
            // `return x || y` / `return x && y` — MSC always emits the full
            // short-circuit branch structure with TWO separate epilogue+ret
            // paths (one for true=1, one for false=0). Does NOT const-fold.
            let epi = crate::codegen::func::epilogue_vec(frame, locals.pascal_cleanup);
            let take_then_disp = 3i8 + epi.len() as i8;  // true block = 3 (mov ax,1) + epi
            let cond = cond_from_expr(expr.clone());
            emit_cond_skip(&cond, take_then_disp, locals, out, fixups);
            out.extend_from_slice(&[0xB8, 0x01, 0x00]);  // mov ax, 1
            out.extend_from_slice(&epi);                   // epilogue + ret
            out.extend_from_slice(&[0x2B, 0xC0]);         // sub ax, ax
            out.extend_from_slice(&epi);                   // epilogue + ret
            return;  // both branches already have the epilogue
        } else if let Expr::BinOp { op, left, right } = expr
            && matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
            && expr.fold(locals.inits).is_none()
        {
            // Value-numbering: `return a == b` / `a != b` where `b` was just
            // CSE-copied from `a` (`b = a` emitted `mov ax,[a]; mov [b],ax`, AX
            // still live). The two slots are provably equal, so the result is a
            // constant 1/0 — no runtime compare. The tail-scan (gated by the
            // branch barrier) only matches when that exact copy is the immediately
            // preceding code, so a later store to either local breaks it.
            // Fixture 2216 (`a=x*4; b=x<<2; return a==b`).
            if matches!(op, BinOp::Eq | BinOp::Ne)
                && let (Expr::Local(li), Expr::Local(ri)) = (left.as_ref(), right.as_ref())
                && locals.size(*li) == 2 && locals.size(*ri) == 2
                && !locals.is_long_local(*li) && !locals.is_float_local(*li)
            {
                let (da, db) = (locals.disp(*li), locals.disp(*ri));
                let barrier = locals.last_branch_barrier.get();
                if crate::codegen::expr::ax_holds_slot_copy(out, da, db, barrier)
                    || crate::codegen::expr::ax_holds_slot_copy(out, db, da, barrier)
                {
                    if matches!(op, BinOp::Eq) { out.extend_from_slice(&[0xB8, 0x01, 0x00]); }
                    else { out.extend_from_slice(&[0x2B, 0xC0]); }
                    crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
                    return;
                }
            }
            // `return x == 0`/`!x` and `return x != 0` (is-zero / is-nonzero):
            // MSC uses the carry trick `cmp word [x],1` (CF set iff x==0), then
            // `sbb ax,ax` (ax = -CF). `== 0` finishes with `neg ax` (→1 iff x==0);
            // `!= 0` with `inc ax` (→1 iff x!=0). Replaces the two-epilogue branch.
            // Word param/local operand only. Fixtures 3487/3378 (==0), 3642 (!=0).
            if matches!(op, BinOp::Eq | BinOp::Ne)
                && right.fold(locals.inits) == Some(0)
                && let Some(disp) = match left.as_ref() {
                    Expr::Param(i) if !locals.is_char_param(*i) && !locals.is_long_param(*i)
                        && !locals.is_float_param(*i) => Some(param_disp(*i)),
                    Expr::Local(i) if locals.size(*i) == 2 && !locals.is_long_local(*i)
                        && !locals.is_float_local(*i) => Some(locals.disp(*i)),
                    _ => None,
                }
            {
                out.push(0x83); out.push(bp_modrm(0x7E, disp)); push_bp_disp(out, disp); out.push(0x01); // cmp word [bp+d],1
                out.extend_from_slice(&[0x1B, 0xC0]); // sbb ax,ax
                if matches!(op, BinOp::Eq) {
                    out.extend_from_slice(&[0xF7, 0xD8]); // neg ax  → 1 iff x==0
                } else {
                    out.push(0x40); // inc ax  → 1 iff x!=0
                }
                crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
                return;
            }
            // `return x == y` etc — two-epilogue structure with optional NOP
            // to keep the false-path aligned. The inverted_jcc fires when
            // the condition is FALSE and jumps past the true block to the
            // false path (sub ax,ax + epilogue).
            let epi = crate::codegen::func::epilogue_vec(frame, locals.pascal_cleanup);
            let rel_op = match op {
                BinOp::Eq => RelOp::Eq, BinOp::Ne => RelOp::Ne,
                BinOp::Lt => RelOp::Lt, BinOp::Le => RelOp::Le,
                BinOp::Gt => RelOp::Gt, BinOp::Ge => RelOp::Ge,
                _ => unreachable!(),
            };
            let cond = Cond::Cmp { op: rel_op, left: *left.clone(), right: *right.clone() };
            let mut cond_buf = Vec::new();
            emit_cond_skip(&cond, 0i8, locals, &mut cond_buf, &mut Vec::new());
            let cond_size = cond_buf.len();
            let true_block_size = 3 + epi.len(); // mov ax,1 + epilogue
            let needs_nop = (out.len() + cond_size + true_block_size) % 2 != 0;
            let take_then_disp = (true_block_size + needs_nop as usize) as i8;
            emit_cond_skip(&cond, take_then_disp, locals, out, fixups);
            out.extend_from_slice(&[0xB8, 0x01, 0x00]);  // mov ax, 1
            out.extend_from_slice(&epi);                   // epilogue + ret
            if needs_nop { out.push(0x90); }              // alignment NOP
            out.extend_from_slice(&[0x2B, 0xC0]);         // sub ax, ax
            out.extend_from_slice(&epi);                   // epilogue + ret
            return;  // both branches already have the epilogue
        } else if let Some(k) = expr.fold(locals.inits)
            && !expr_has_long_highword(expr, locals)
            && ternary_folds_to_immediate(expr, locals)
            && !matches!(expr, Expr::CastChar { .. })
        {
            // A const-folded ternary collapses to an immediate only when BOTH
            // arms are simple direct values (fixture 167: `a=3; b=7; return
            // a<b?a:b` → `mov ax,3`). If an arm is a compound expression, MSC
            // instead folds only the condition to pick the arm and emits that
            // arm as a runtime load (fixture 430: `a=5; return a>0?a:-a` →
            // `mov ax,[bp-2]`, not `mov ax,5`) — handled by the emit_expr_to_ax
            // fallthrough, whose Ternary arm reproduces the load behavior.
            // AX-liveness: when AX provably already holds k (a preceding store
            // of the constant left it live across only AX-preserving memory
            // writes), reuse it instead of re-materializing. Int return only —
            // the long path needs DX:AX setup. Fixtures 901/902.
            if !return_long
                && crate::codegen::expr::ax_holds_const(out, k, locals.last_branch_barrier.get())
            {
                // AX already == k; emit nothing.
            } else if k == 0 {
                out.extend_from_slice(&[0x2B, 0xC0]);
                if return_long { out.push(0x99); } // cwd: DX:AX = sign-extend(AX)
            } else {
                let lo = (k as u32 & 0xFFFF) as u16;
                out.push(0xB8);
                out.extend_from_slice(&lo.to_le_bytes());
                if return_long {
                    if i16::try_from(k).is_ok() {
                        out.push(0x99); // cwd: high word = sign extension of low
                    } else {
                        let hi = ((k as u32) >> 16) as u16;
                        out.push(0xBA);
                        out.extend_from_slice(&hi.to_le_bytes());
                    }
                }
            }
        } else if return_long
            && let Expr::Global(idx) = expr
            && locals.is_long_global(*idx)
        {
            // Long global return: mov ax, [g]; mov dx, [g+2]
            let body_offset_lo = out.len();
            out.extend_from_slice(&[0xA1, 0x00, 0x00]);
            fixups.push(Fixup { body_offset: body_offset_lo, kind: FixupKind::GlobalAddr { global_idx: *idx } });
            // 8b 16 02 00 = mov dx, [imm16]; placeholder 0x0002 gets patched
            // to global_offset+2 by the existing GlobalAddr addend logic.
            let body_offset_hi = out.len() + 1; // points to modrm, so +1,+2 are the address bytes
            out.extend_from_slice(&[0x8B, 0x16, 0x02, 0x00]);
            fixups.push(Fixup { body_offset: body_offset_hi, kind: FixupKind::GlobalAddr { global_idx: *idx } });
        } else if return_long
            && (is_long_shl(expr, locals)
                || is_long_shl_var(expr, locals)
                || matches!(expr, Expr::CastLong { .. })
                || is_long_shr1(expr, locals)
                || is_long_neg(expr, locals)
                || is_long_not(expr, locals)
                || is_long_arith_mem(expr, locals)
                || is_long_plus_int(expr, locals)
                || is_long_const_bitop(expr, locals)
                || is_long_muldiv(expr, locals)
                || is_long_field_elem_or_const_arith(expr, locals)
                || matches!(expr, Expr::Index { array, .. } if locals.is_long_global(*array))
                || matches!(expr, Expr::LocalIndex { local, .. } if locals.is_long_local(*local))
                || matches!(expr, Expr::PostMutateGlobal { global_idx, .. } if locals.is_long_global(*global_idx)))
        {
            // `return <long> <op> ..` lowered across DX:AX (shift-by-1,
            // inline 2-word arithmetic, or a mul/div/mod helper call).
            emit_long_to_dx_ax(expr, locals, out, fixups);
        } else {
            emit_expr_to_ax(expr, locals, out, fixups);
            if return_long {
                // int->long promotion of `return (long)<int>`: sign-extend
                // (cwd) for signed, zero-extend (sub dx,dx) for unsigned int.
                let unsigned = matches!(expr, Expr::Param(i) if locals.is_unsigned_param(*i))
                    || matches!(expr, Expr::Local(i) if locals.is_unsigned_local(*i))
                    || matches!(expr, Expr::Global(j) if locals.is_unsigned_global(*j));
                if unsigned {
                    out.extend_from_slice(&[0x2B, 0xD2]); // sub dx, dx
                } else {
                    out.push(0x99); // cwd
                }
            }
        }
    }
    crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, out);
}
/// `while (<cond>) <body>` lowers to a test-first shape with the
/// initial jmp landing on the cond, the body and cmp run inline,
/// and a backward jcc closing the loop. MSC aligns the loop-top
/// to an even byte offset — if the position right after the
/// 2-byte forward jmp would be odd, MSC inserts a single NOP pad
/// (fixture 4096); when prior bytes already leave us even, the
/// nop is dropped (fixture 4097's for-loop shows the same rule).
///
/// ```text
/// eb <body[+pad]>     jmp short to cond
/// [90]                nop pad iff next byte would be at odd offset
/// <body>              loop body
/// <cmp>               cond comparison
/// <jcc> <-back>       jne/je back to body start
/// ```
/// Match `while (*p) { accum = accum + *p; p = p + 1; }` — a word-deref
/// accumulate loop. `*p` is a `DerefWord` (so the pointee is int/word, no Locals
/// lookup needed), accum and p are locals. Returns `(accum_local, ptr_local)`.
/// Used by both the dedicated SI-caching emitter and `body_needs_si`. Fixture 1849.
pub(crate) fn while_si_accum(cond: &Cond, body: &Stmt) -> Option<(usize, usize)> {
    let Cond::Truthy(Expr::DerefWord { ptr }) = cond else { return None };
    let Expr::Local(p) = ptr.as_ref() else { return None };
    let Stmt::Block(stmts) = body else { return None };
    if stmts.len() != 2 { return None; }
    // stmt 0: accum = accum + *p
    let Stmt::Assign { target: AssignTarget::Local(a), value } = &stmts[0] else { return None };
    let Expr::BinOp { op: BinOp::Add, left, right } = value else { return None };
    if !matches!(left.as_ref(), Expr::Local(la) if la == a) { return None; }
    if !matches!(right.as_ref(),
        Expr::DerefWord { ptr: rp } if matches!(rp.as_ref(), Expr::Local(rpi) if rpi == p)) {
        return None;
    }
    // stmt 1: p = p + 1
    let Stmt::Assign { target: AssignTarget::Local(p2), value: pv } = &stmts[1] else { return None };
    if p2 != p { return None; }
    let Expr::BinOp { op: BinOp::Add, left: pl, right: pr } = pv else { return None };
    if !matches!(pl.as_ref(), Expr::Local(pli) if pli == p) { return None; }
    if !matches!(pr.as_ref(), Expr::IntLit(1)) { return None; }
    Some((*a, *p))
}
/// Emit the SI-caching word-deref accumulate loop (see [`while_si_accum`]):
/// `jmp COND; [nop]; BODY: add [accum],si; add [p],stride; COND: mov bx,[p];
/// mov si,[bx]; or si,si; jne BODY`. The body starts on an even offset.
fn emit_while_si_accum(accum: usize, p: usize, locals: &Locals<'_>, out: &mut Vec<u8>) {
    let ad = locals.disp(accum);
    let pd = locals.disp(p);
    let stride = locals.local_pointee_size(p).max(1) as u8;
    let jmp_pos = out.len();
    let needs_nop = (jmp_pos + 2) % 2 == 1; // BODY must land on an even offset
    out.push(0xEB);
    let disp_pos = out.len();
    out.push(0x00);
    if needs_nop { out.push(0x90); }
    let body_pos = out.len();
    out.push(0x01); out.push(bp_modrm(0x76, ad)); push_bp_disp(out, ad);          // add [bp+accum],si
    out.push(0x83); out.push(bp_modrm(0x46, pd)); push_bp_disp(out, pd); out.push(stride); // add word [bp+p],stride
    let cond_pos = out.len();
    out.push(0x8B); out.push(bp_modrm(0x5E, pd)); push_bp_disp(out, pd);          // mov bx,[bp+p]
    out.extend_from_slice(&[0x8B, 0x37, 0x0B, 0xF6]);                             // mov si,[bx]; or si,si
    out.push(0x75);                                                               // jne BODY
    let back = body_pos as i64 - (out.len() as i64 + 1);
    out.push(i8::try_from(back).expect("si-accum back jmp fits rel8") as u8);
    let fwd = cond_pos as i64 - (disp_pos as i64 + 1);
    out[disp_pos] = i8::try_from(fwd).expect("si-accum fwd jmp fits rel8") as u8;
}
/// Match `while (*s) { n = n + *s; s++; }` where `s` is a `char *` (param or
/// local) and `n` an int local — the byte-deref accumulate loop. `*s` is a
/// `DerefByte`. Returns `(n_local, s_is_param, s_idx)`. The char value is cached
/// in a hidden stack temp (it must survive the condition's `or al,al` and the
/// body's `cbw`), unlike the int variant which uses SI. Fixture 1690.
pub(crate) fn while_char_accum(cond: &Cond, body: &Stmt) -> Option<(usize, bool, usize)> {
    let Cond::Truthy(Expr::DerefByte { ptr }) = cond else { return None };
    let (s_is_param, s) = match ptr.as_ref() {
        Expr::Param(s) => (true, *s),
        Expr::Local(s) => (false, *s),
        _ => return None,
    };
    let same_s = |e: &Expr| match e {
        Expr::Param(x) => s_is_param && *x == s,
        Expr::Local(x) => !s_is_param && *x == s,
        _ => false,
    };
    let Stmt::Block(stmts) = body else { return None };
    if stmts.len() != 2 { return None; }
    // stmt 0: n = n + *s
    let Stmt::Assign { target: AssignTarget::Local(n), value } = &stmts[0] else { return None };
    let Expr::BinOp { op: BinOp::Add, left, right } = value else { return None };
    if !matches!(left.as_ref(), Expr::Local(ln) if ln == n) { return None; }
    if !matches!(right.as_ref(), Expr::DerefByte { ptr: rp } if same_s(rp.as_ref())) { return None; }
    // stmt 1: s = s + 1
    let pv = match &stmts[1] {
        Stmt::Assign { target: AssignTarget::Param(p), value } if s_is_param && *p == s => value,
        Stmt::Assign { target: AssignTarget::Local(l), value } if !s_is_param && *l == s => value,
        _ => return None,
    };
    let Expr::BinOp { op: BinOp::Add, left: pl, right: pr } = pv else { return None };
    if !same_s(pl.as_ref()) || !matches!(pr.as_ref(), Expr::IntLit(1)) { return None; }
    Some((*n, s_is_param, s))
}
/// True when any top-level `while` in `stmts` is a char-accumulate loop — used by
/// the prologue to reserve the 2-byte caching temp below the locals. Fixture 1690.
pub(crate) fn body_has_char_accum(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| matches!(s, Stmt::While { cond, body }
        if while_char_accum(cond, body).is_some()))
}
/// A `while` loop whose `&&` condition shares one iterator-pointer deref `*p`
/// across all conjuncts: `*p` (a nonzero test) plus any number of `*p ==/!= *q_i`
/// comparisons (in EITHER operand position). MSC computes `*p` ONCE per iteration
/// — common-subexpression elimination over the loop condition — and reuses it:
///   - CHAR (`char *`) → the byte is spilled to a stack temp (no byte-half of SI);
///   - INT (`int *`)   → the word stays in SI (a `push si` register cache).
/// Confirmed against the oracle with three discerning variants (`int *`, a 3-way
/// `*a==*b && *a==*c`, and a swapped `*b==*a`). Fixtures 1352/3418/2362 (char) and
/// 4145/4146/4147 (the variants). This struct models the byte case; the word case
/// is detected the same way but emitted via SI.
#[derive(Clone, Debug)]
enum CseConj {
    /// `*p` — the iterator's nonzero test.
    Truthy,
    /// `*p ==/!= *q` — `q` is the OTHER pointer (whichever side isn't `p`).
    Cmp { q_is_param: bool, q: usize, ne: bool },
}
#[derive(Clone, Debug)]
pub(crate) struct CseDerefWhile {
    p_is_param: bool,
    p: usize,
    /// `*p` is a word (int) deref → cache in SI; otherwise a byte (char) → temp.
    pub(crate) is_word: bool,
    /// Conjuncts in SOURCE order: `[0]` is tested at the loop-back COND (and loads
    /// + caches `*p`); `[1..]` are tested at the top of the loop body.
    conjuncts: Vec<CseConj>,
}
fn deref_ptr_id(e: &Expr) -> Option<(bool, usize, bool)> {
    let (ptr, is_word) = match e {
        Expr::DerefByte { ptr } => (ptr, false),
        Expr::DerefWord { ptr } => (ptr, true),
        _ => return None,
    };
    match ptr.as_ref() {
        Expr::Param(i) => Some((true, *i, is_word)),
        Expr::Local(i) => Some((false, *i, is_word)),
        _ => None,
    }
}
fn flatten_and<'a>(c: &'a Cond, out: &mut Vec<&'a Cond>) {
    if let Cond::And(l, r) = c {
        flatten_and(l, out);
        out.push(r);
    } else {
        out.push(c);
    }
}
/// Unwrap a single-statement block to its inner statement.
fn unwrap_single_stmt(s: &Stmt) -> &Stmt {
    match s {
        Stmt::Block(v) if v.len() == 1 => &v[0],
        other => other,
    }
}
/// The shared-block label for `return <e>` when `e` is a value MSC emits the
/// epilogue for ONCE and routes later identical returns to (a constant or a
/// plain local). MSC shares both `if (c) return E` and bare `return E` blocks.
/// Fixtures 4036 (const), 4165 (local, bare), 2772 (loop rewrite).
fn return_share_key(e: &Expr) -> Option<String> {
    match e {
        Expr::IntLit(k) => Some(format!("@ret{k}")),
        Expr::Local(x) => Some(format!("@retL{x}")),
        Expr::Param(x) => Some(format!("@retP{x}")),
        _ => None,
    }
}
/// Emit `if (cond) goto label` taken when `cond` is TRUE, for a (possibly
/// `&&`-chained) comparison/truthy test. All but the last conjunct jcc-on-FALSE
/// to the fall-through (after this `if`); the last conjunct jcc-on-TRUE to
/// `label`. Returns false (emitting NOTHING) when any conjunct isn't a plain
/// int Cmp/Truthy (nested `||`, long operands → caller falls back). Used to
/// route a SECOND identical `if (cond) return K;` to a shared return-K block.
/// Fixtures 4036 (`&&`), and the single-Cmp case.
fn emit_if_and_goto_true(
    cond: &Cond, label: &str, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>,
) -> bool {
    let mut conjs: Vec<&Cond> = Vec::new();
    flatten_and(cond, &mut conjs);
    for c in &conjs {
        match c {
            Cond::Cmp { left, .. } if !crate::codegen::calls::long_operand(left, locals) => {}
            Cond::Truthy(e) if !crate::codegen::calls::long_operand(e, locals) => {}
            _ => return false,
        }
    }
    let n = conjs.len();
    let mut skip_pos: Vec<usize> = Vec::new();
    for (i, c) in conjs.iter().enumerate() {
        emit_cond_cmp_inner(c, locals, out, fixups);
        let unsigned = cmp_is_unsigned(c, locals);
        if i + 1 < n {
            // not last: jcc-on-FALSE → after this `if` (skip the jump)
            let jcc = match c {
                Cond::Truthy(_) => 0x74, // je on zero
                Cond::Cmp { op, .. } => {
                    let j = inverted_jcc(*op);
                    if unsigned { to_unsigned_jcc(j) } else { j }
                }
                _ => unreachable!(),
            };
            out.push(jcc);
            skip_pos.push(out.len());
            out.push(0x00);
        } else {
            // last: jcc-on-TRUE → the shared block label
            let jcc = match c {
                Cond::Truthy(_) => 0x75, // jne (nonzero)
                Cond::Cmp { op, .. } => {
                    let j = loop_back_jcc(*op);
                    if unsigned { to_unsigned_jcc(j) } else { j }
                }
                _ => unreachable!(),
            };
            out.push(jcc);
            let pos = out.len();
            out.push(0x00);
            locals.label_fixups.borrow_mut().push((label.to_owned(), pos));
        }
    }
    let after = out.len();
    for p in skip_pos {
        out[p] = i8::try_from(after as i64 - (p as i64 + 1)).expect("&& skip rel8 fits") as u8;
    }
    true
}
pub(crate) fn cse_deref_while(cond: &Cond) -> Option<CseDerefWhile> {
    if !matches!(cond, Cond::And(..)) { return None; }
    let mut conjs: Vec<&Cond> = Vec::new();
    flatten_and(cond, &mut conjs);
    // The cached iterator `p` is the (single) bare `*p` truthy test's pointer.
    let mut piw: Option<(bool, usize, bool)> = None;
    for c in &conjs {
        if let Cond::Truthy(e) = c {
            let pid = deref_ptr_id(e)?;
            if piw.is_some() { return None; } // a second truthy → not this shape
            piw = Some(pid);
        }
    }
    let (p_is_param, p, is_word) = piw?;
    // Every conjunct must be `*p` (truthy) or a `==/!=` of `*p` against `*q`,
    // all of the same width as `p`.
    let mut conjuncts = Vec::with_capacity(conjs.len());
    for c in &conjs {
        match c {
            Cond::Truthy(e) => {
                if deref_ptr_id(e)? != (p_is_param, p, is_word) { return None; }
                conjuncts.push(CseConj::Truthy);
            }
            Cond::Cmp { op: op @ (RelOp::Eq | RelOp::Ne), left, right } => {
                let l = deref_ptr_id(left)?;
                let r = deref_ptr_id(right)?;
                if l.2 != is_word || r.2 != is_word { return None; }
                let other = if (l.0, l.1) == (p_is_param, p) { (r.0, r.1) }
                    else if (r.0, r.1) == (p_is_param, p) { (l.0, l.1) }
                    else { return None };
                conjuncts.push(CseConj::Cmp { q_is_param: other.0, q: other.1, ne: matches!(op, RelOp::Ne) });
            }
            _ => return None,
        }
    }
    Some(CseDerefWhile { p_is_param, p, is_word, conjuncts })
}
/// True when any top-level `while` is a BYTE [`cse_deref_while`] loop — used by the
/// prologue to reserve the cache temp and force a slide frame even with no locals.
/// (The word case caches in SI and reserves no temp.)
pub(crate) fn body_has_cse_byte_while(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| matches!(s, Stmt::While { cond, .. }
        if cse_deref_while(cond).is_some_and(|w| !w.is_word)))
}
fn ptr_disp(is_param: bool, idx: usize, locals: &Locals<'_>) -> i16 {
    if is_param { param_disp(idx) } else { locals.disp(idx) }
}
/// The back-edge jcc for the COND conjunct (loop when it is TRUE).
fn cse_back_jcc(c: &CseConj) -> u8 {
    match c {
        CseConj::Truthy => 0x75,                                  // jne (nonzero)
        CseConj::Cmp { ne, .. } => if *ne { 0x75 } else { 0x74 }, // == → je, != → jne
    }
}
/// Emit the loop-back COND test (conjunct 0): load+cache `*p`, then test it. The
/// back-jcc is appended by the caller.
fn emit_cse_cond(cw: &CseDerefWhile, temp: i16, locals: &Locals<'_>, out: &mut Vec<u8>) {
    let pd = ptr_disp(cw.p_is_param, cw.p, locals);
    out.push(0x8B); out.push(bp_modrm(0x5E, pd)); push_bp_disp(out, pd); // mov bx,[bp+p]
    if cw.is_word {
        out.extend_from_slice(&[0x8B, 0x37]); // mov si,[bx]   (SI is the cache)
    } else {
        out.extend_from_slice(&[0x8A, 0x07]); // mov al,[bx]
        out.push(0x88); out.push(bp_modrm(0x46, temp)); push_bp_disp(out, temp); // mov [temp],al
    }
    match &cw.conjuncts[0] {
        CseConj::Truthy => out.extend_from_slice(if cw.is_word { &[0x0B, 0xF6] } else { &[0x0A, 0xC0] }), // or si,si | or al,al
        CseConj::Cmp { q_is_param, q, .. } => {
            let qd = ptr_disp(*q_is_param, *q, locals);
            out.push(0x8B); out.push(bp_modrm(0x5E, qd)); push_bp_disp(out, qd); // mov bx,[bp+q]
            out.extend_from_slice(if cw.is_word { &[0x39, 0x37] } else { &[0x38, 0x07] }); // cmp [bx],si | cmp [bx],al
        }
    }
}
/// Emit the top-of-loop tests (conjuncts 1..) with placeholder forward exit-jccs,
/// reusing the cached `*p`. Records each exit-jcc disp-byte position for patching.
fn emit_cse_loop(cw: &CseDerefWhile, temp: i16, locals: &Locals<'_>, out: &mut Vec<u8>, exits: &mut Vec<usize>) {
    let mut al_loaded = false; // (byte only) whether AL currently holds the cache
    for c in &cw.conjuncts[1..] {
        let exit_jcc = match c {
            CseConj::Cmp { q_is_param, q, ne } => {
                let qd = ptr_disp(*q_is_param, *q, locals);
                out.push(0x8B); out.push(bp_modrm(0x5E, qd)); push_bp_disp(out, qd); // mov bx,[bp+q]
                if cw.is_word {
                    out.extend_from_slice(&[0x39, 0x37]); // cmp [bx],si
                } else {
                    if !al_loaded {
                        out.push(0x8A); out.push(bp_modrm(0x46, temp)); push_bp_disp(out, temp); // mov al,[temp]
                        al_loaded = true;
                    }
                    out.extend_from_slice(&[0x38, 0x07]); // cmp [bx],al
                }
                // exit when the comparison is FALSE: `==` exits on != (jne), `!=` on je.
                if *ne { 0x74 } else { 0x75 }
            }
            CseConj::Truthy => {
                if cw.is_word {
                    out.extend_from_slice(&[0x0B, 0xF6]); // or si,si
                } else {
                    out.push(0x80); out.push(bp_modrm(0x7E, temp)); push_bp_disp(out, temp); out.push(0x00); // cmp byte [temp],0
                }
                0x74 // jz EXIT (exit when zero)
            }
        };
        out.push(exit_jcc);
        exits.push(out.len());
        out.push(0x00); // placeholder disp, patched by the caller
    }
}
/// A two-iterator string-compare loop: `while (*a && *b && *a == *b)` — two
/// DISTINCT char-pointer iterators, each `*p` cached in its own byte temp
/// (`*a`→[bp-2], `*b`→[bp-4]); the `==`/`!=` reuses the cached `*a` against the
/// live `*b` in AL. Conjunct 0 (`*a`) is the loop-back COND; conjuncts 1 (`*b`)
/// and 2 (the compare) are the top-of-body tests. Fixture 2203.
pub(crate) struct TwoIterCse {
    a_is_param: bool,
    a: usize,
    b_is_param: bool,
    b: usize,
    ne: bool,
}
pub(crate) fn two_iter_cse_while(cond: &Cond) -> Option<TwoIterCse> {
    if !matches!(cond, Cond::And(..)) { return None; }
    let mut conjs: Vec<&Cond> = Vec::new();
    flatten_and(cond, &mut conjs);
    if conjs.len() != 3 { return None; }
    let (a_is_param, a, aw) = match conjs[0] { Cond::Truthy(e) => deref_ptr_id(e)?, _ => return None };
    let (b_is_param, b, bw) = match conjs[1] { Cond::Truthy(e) => deref_ptr_id(e)?, _ => return None };
    if aw || bw { return None; }                          // byte (char) derefs only
    if (a_is_param, a) == (b_is_param, b) { return None; } // two distinct iterators
    let (op, l, r) = match conjs[2] {
        Cond::Cmp { op: op @ (RelOp::Eq | RelOp::Ne), left, right } =>
            (op, deref_ptr_id(left)?, deref_ptr_id(right)?),
        _ => return None,
    };
    if l.2 || r.2 { return None; }
    let pair = [(l.0, l.1), (r.0, r.1)];
    if !(pair.contains(&(a_is_param, a)) && pair.contains(&(b_is_param, b))) { return None; }
    Some(TwoIterCse { a_is_param, a, b_is_param, b, ne: matches!(op, RelOp::Ne) })
}
/// True when any top-level `while` is a two-iterator strcmp loop — the prologue
/// reserves two byte temps (4 bytes) and forces a slide frame.
pub(crate) fn body_has_two_iter_cse(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| matches!(s, Stmt::While { cond, .. } if two_iter_cse_while(cond).is_some()))
}
/// Emit the loop-back COND (conjunct 0): load+cache `*a`, then `or al,al`. The
/// back-jcc (jne) is appended by the caller.
fn emit_two_iter_cond(t: &TwoIterCse, temp_a: i16, locals: &Locals<'_>, out: &mut Vec<u8>) {
    let ad = ptr_disp(t.a_is_param, t.a, locals);
    out.push(0x8B); out.push(bp_modrm(0x5E, ad)); push_bp_disp(out, ad);          // mov bx,[bp+a]
    out.extend_from_slice(&[0x8A, 0x07]);                                         // mov al,[bx]
    out.push(0x88); out.push(bp_modrm(0x46, temp_a)); push_bp_disp(out, temp_a);  // mov [temp_a],al
    out.extend_from_slice(&[0x0A, 0xC0]);                                         // or al,al
}
/// Emit the top-of-body tests (conjuncts 1,2) with placeholder forward exit-jccs;
/// records each exit-jcc disp-byte position for the caller to patch.
fn emit_two_iter_loop(t: &TwoIterCse, temp_a: i16, temp_b: i16, locals: &Locals<'_>, out: &mut Vec<u8>, exits: &mut Vec<usize>) {
    let bd = ptr_disp(t.b_is_param, t.b, locals);
    out.push(0x8B); out.push(bp_modrm(0x5E, bd)); push_bp_disp(out, bd);          // mov bx,[bp+b]
    out.extend_from_slice(&[0x8A, 0x07]);                                         // mov al,[bx]  (al=*b)
    out.push(0x88); out.push(bp_modrm(0x46, temp_b)); push_bp_disp(out, temp_b);  // mov [temp_b],al
    out.extend_from_slice(&[0x0A, 0xC0]);                                         // or al,al
    out.push(0x74); exits.push(out.len()); out.push(0x00);                        // je EXIT (conjunct 1: *b==0)
    out.push(0x38); out.push(bp_modrm(0x46, temp_a)); push_bp_disp(out, temp_a);  // cmp [temp_a],al
    out.push(if t.ne { 0x74 } else { 0x75 });                                     // == exits on jne, != on je
    exits.push(out.len()); out.push(0x00);
}
/// Emit the char-deref accumulate loop (see [`while_char_accum`]). `*s` is cached
/// in a 2-byte stack temp at `min(local disp) - 2` (one slot below the locals,
/// reserved by the prologue): `jmp COND; [nop]; BODY: mov al,[temp]; cbw;
/// add [n],ax; inc [s]; COND: mov bx,[s]; mov al,[bx]; mov [temp],al; or al,al;
/// jne BODY`. The body starts on an even offset.
fn emit_while_char_accum(n: usize, s_is_param: bool, s: usize, locals: &Locals<'_>, out: &mut Vec<u8>) {
    let nd = locals.disp(n);
    let sd = if s_is_param { param_disp(s) } else { locals.disp(s) };
    let temp = locals.deepest_local_disp() - 2; // one 2-byte slot below the locals
    let jmp_pos = out.len();
    let needs_nop = (jmp_pos + 2) % 2 == 1;
    out.push(0xEB);
    let disp_pos = out.len();
    out.push(0x00);
    if needs_nop { out.push(0x90); }
    let body_pos = out.len();
    out.push(0x8A); out.push(bp_modrm(0x46, temp)); push_bp_disp(out, temp);       // mov al,[temp]
    out.push(0x98);                                                                // cbw
    out.push(0x01); out.push(bp_modrm(0x46, nd)); push_bp_disp(out, nd);           // add [bp+n],ax
    out.push(0xFF); out.push(bp_modrm(0x46, sd)); push_bp_disp(out, sd);           // inc word [bp+s]
    let cond_pos = out.len();
    out.push(0x8B); out.push(bp_modrm(0x5E, sd)); push_bp_disp(out, sd);           // mov bx,[bp+s]
    out.extend_from_slice(&[0x8A, 0x07]);                                          // mov al,[bx]
    out.push(0x88); out.push(bp_modrm(0x46, temp)); push_bp_disp(out, temp);       // mov [temp],al
    out.extend_from_slice(&[0x0A, 0xC0]);                                          // or al,al
    out.push(0x75);                                                                // jne BODY
    let back = body_pos as i64 - (out.len() as i64 + 1);
    out.push(i8::try_from(back).expect("char-accum back jmp fits rel8") as u8);
    let fwd = cond_pos as i64 - (disp_pos as i64 + 1);
    out[disp_pos] = i8::try_from(fwd).expect("char-accum fwd jmp fits rel8") as u8;
}
pub(crate) fn emit_while(
    cond: &Cond,
    body_stmt: &Stmt,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // `while (*p) { accum = accum + *p; p = p + 1; }` (int* p, int accum): MSC
    // caches `*p` in SI across the condition and body (`mov si,[bx]` in the
    // condition, `add [accum],si` in the body). A dedicated emitter — the body is
    // emitted before the condition in the while form, so it can't tail-scan for
    // the value. Fixture 1849.
    if let Some((accum, p)) = while_si_accum(cond, body_stmt) {
        emit_while_si_accum(accum, p, locals, out);
        return;
    }
    // `while (*s) { n = n + *s; s++; }` (char* s, int n): the byte value is cached
    // in a hidden stack temp across the condition and body. Fixture 1690.
    if let Some((n, s_is_param, s)) = while_char_accum(cond, body_stmt) {
        emit_while_char_accum(n, s_is_param, s, locals, out);
        return;
    }
    // Infinite loop with a trailing `if (c) break;`: MSC folds the break into the
    // back edge — `while (1) { prefix; if (c) break; }` becomes
    // `do { prefix } while (!c)`, looping back on the inverted condition.
    if matches!(cond, Cond::Truthy(Expr::IntLit(k)) if *k != 0)
        && let Some((prefix, neg)) = loop_trailing_break_rewrite(body_stmt)
    {
        let dw_body = Stmt::Block(prefix);
        emit_do_while(&dw_body, &neg, locals, frame, return_int, return_long, out, fixups);
        return;
    }
    // `while (1)` with no break at all: infinite loop, no test (3396).
    if matches!(cond, Cond::Truthy(Expr::IntLit(k)) if *k != 0)
        && !stmt_has_loop_break(body_stmt)
    {
        emit_infinite_loop(body_stmt, locals, frame, return_int, return_long, out, fixups);
        return;
    }
    // `while (C) { switch(...){…}; TAIL }` with a ≥2-case switch: MSC keeps the
    // test at the top (no rotation) and weaves the loop tail into the switch's
    // partial-continuation layout. Fixture 3234.
    if try_emit_while_switch(cond, body_stmt, locals, frame, return_int, return_long, out, fixups) {
        return;
    }
    emit_loop(cond, &[body_stmt], None, None, None, false, locals, frame, return_int, return_long, out, fixups);
}
/// The RelOp for the same comparison with operands swapped (`a OP b` ⟺
/// `b SWAP(OP) a`). Used when one operand is a `register` local that MSC places
/// in the cmp's reg field, forcing the memory operand to the rm side.
pub(crate) fn swap_relop(op: RelOp) -> RelOp {
    match op {
        RelOp::Lt => RelOp::Gt, RelOp::Gt => RelOp::Lt,
        RelOp::Le => RelOp::Ge, RelOp::Ge => RelOp::Le,
        RelOp::Eq => RelOp::Eq, RelOp::Ne => RelOp::Ne,
    }
}
/// The RelOp whose truth is the negation of `op`.
pub(crate) fn negate_relop(op: RelOp) -> RelOp {
    match op {
        RelOp::Eq => RelOp::Ne, RelOp::Ne => RelOp::Eq,
        RelOp::Lt => RelOp::Ge, RelOp::Ge => RelOp::Lt,
        RelOp::Gt => RelOp::Le, RelOp::Le => RelOp::Gt,
    }
}
/// Negate a condition for use as a loop-back test. `And`/`Or` bail (None).
pub(crate) fn negate_cond(c: &Cond) -> Option<Cond> {
    match c {
        Cond::Cmp { op, left, right } =>
            Some(Cond::Cmp { op: negate_relop(*op), left: left.clone(), right: right.clone() }),
        // `if (e) break` loops back while e == 0.
        Cond::Truthy(e) => Some(Cond::Cmp { op: RelOp::Eq, left: e.clone(), right: Expr::IntLit(0) }),
        Cond::And(_, _) | Cond::Or(_, _) => None,
    }
}
/// True when `s` contains a `break`/`continue` belonging to the current loop
/// (i.e. not absorbed by a nested loop or switch).
pub(crate) fn stmt_has_loop_break(s: &Stmt) -> bool {
    match s {
        Stmt::Break | Stmt::Continue => true,
        Stmt::If { then_branch, else_branch, .. } =>
            stmt_has_loop_break(then_branch)
                || else_branch.as_ref().is_some_and(|e| stmt_has_loop_break(e)),
        Stmt::Block(ss) => ss.iter().any(stmt_has_loop_break),
        _ => false, // nested While/DoWhile/For/Switch own their break/continue
    }
}
/// `while (1)` / `for (;;)` / `do {} while (1)` with NO loop-level break:
/// a genuinely infinite loop. Returns its body. The cond must be a SOURCE
/// literal (same gate as `match_once_loop`); `for` only matches the bare
/// `for (;;)` shape. Fixtures 3396/3397.
fn infinite_loop_body(s: &Stmt) -> Option<&Stmt> {
    let const_true = |c: &Cond| matches!(c, Cond::Truthy(Expr::IntLit(k)) if *k != 0);
    let body: &Stmt = match s {
        Stmt::While { cond, body } if const_true(cond) => body,
        Stmt::DoWhile { body, cond } if const_true(cond) => body,
        Stmt::For { init, cond, step, body }
            if const_true(cond)
                && matches!(init.as_ref(), Stmt::Empty)
                && matches!(step.as_ref(), Stmt::Empty) => body,
        _ => return None,
    };
    if stmt_has_loop_break(body) { return None; }
    Some(body)
}
/// Emit an infinite loop (see [`infinite_loop_body`]): the body once, then a
/// short jmp back to its first byte. No cond test, no initial jmp, and no
/// loop-top alignment NOP (gold 3396 loops back to an odd offset). The
/// epilogue after it is suppressed via `stmt_always_returns`.
fn emit_infinite_loop(
    body_stmt: &Stmt,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // Body fold view: clear init values for locals mutated in the body,
    // matching emit_loop (later iterations read the slot, not the init).
    let body_mutations = collect_loop_body_mutations(&[body_stmt]);
    let body_inits: Vec<Option<i32>> = locals.inits.iter().enumerate()
        .map(|(i, &v)| if body_mutations.contains(&i) { None } else { v })
        .collect();
    let body_locals = locals.with_inits(&body_inits);
    let top = out.len();
    // The body appends directly to `out` (nothing is inserted before it, so
    // no fixup/label rebase is needed). No LoopCtx: the body has no breaks.
    emit_stmt(body_stmt, &body_locals, frame, return_int, return_long, out, fixups);
    let disp = top as i64 - (out.len() as i64 + 2);
    out.push(0xeb);
    out.push(i8::try_from(disp).expect("infinite-loop back jmp fits rel8") as u8);
}
/// If a loop body is `{ prefix...; if (c) break; }` with no other loop-level
/// break/continue, return the (cloned) prefix and the negated break condition
/// so the loop can be lowered to `do { prefix } while (!c)`.
pub(crate) fn loop_trailing_break_rewrite(body: &Stmt) -> Option<(Vec<Stmt>, Cond)> {
    let stmts: &[Stmt] = match body {
        Stmt::Block(s) => s,
        other => std::slice::from_ref(other),
    };
    let last = stmts.last()?;
    let Stmt::If { cond, then_branch, else_branch } = last else { return None };
    if else_branch.is_some() || !matches!(then_branch.as_ref(), Stmt::Break) {
        return None;
    }
    let negated = negate_cond(cond)?;
    let prefix: Vec<Stmt> = stmts[..stmts.len() - 1].to_vec();
    if prefix.iter().any(stmt_has_loop_break) {
        return None;
    }
    Some((prefix, negated))
}
/// If a for-loop body starts with `if (c) break;` (no else) and has no other
/// loop-level break/continue, return the break condition and the rest of the body.
fn for_leading_break(body: &Stmt) -> Option<(Cond, Vec<Stmt>)> {
    let stmts: &[Stmt] = match body {
        Stmt::Block(v) => v,
        other => std::slice::from_ref(other),
    };
    let Stmt::If { cond, then_branch, else_branch } = stmts.first()? else { return None };
    if else_branch.is_some() || !matches!(then_branch.as_ref(), Stmt::Break) {
        return None;
    }
    let rest: Vec<Stmt> = stmts[1..].to_vec();
    if rest.iter().any(stmt_has_loop_break) {
        return None;
    }
    Some((cond.clone(), rest))
}
/// A no-step loop whose body starts with `if (c) break;`. Returns the optional
/// init (for-loop), the loop cond (None when literally infinite), the break
/// condition, and the rest of the body.
fn match_leading_break_loop(s: &Stmt) -> Option<(Option<Stmt>, Option<Cond>, Cond, Vec<Stmt>)> {
    let (init, loop_cond, body): (Option<Stmt>, Cond, &Stmt) = match s {
        Stmt::While { cond, body } => (None, cond.clone(), body),
        Stmt::DoWhile { body, cond } => (None, cond.clone(), body),
        Stmt::For { init, cond, step, body } if matches!(step.as_ref(), Stmt::Empty) =>
            (Some((**init).clone()), cond.clone(), body),
        _ => return None,
    };
    // Literally-constant-true cond → infinite loop (no exit test).
    let cond_opt = match &loop_cond {
        Cond::Truthy(Expr::IntLit(k)) if *k != 0 => None,
        other => Some(other.clone()),
    };
    // A finite do-while has its test at the bottom (two exit points) — skip.
    if matches!(s, Stmt::DoWhile { .. }) && cond_opt.is_some() {
        return None;
    }
    let stmts: &[Stmt] = match body {
        Stmt::Block(v) => v,
        other => std::slice::from_ref(other),
    };
    let Stmt::If { cond: bc, then_branch, else_branch } = stmts.first()? else { return None };
    if else_branch.is_some() || !matches!(then_branch.as_ref(), Stmt::Break) {
        return None;
    }
    let rest: Vec<Stmt> = stmts[1..].to_vec();
    if rest.iter().any(stmt_has_loop_break) {
        return None;
    }
    Some((init, cond_opt, bc.clone(), rest))
}
/// Lower a return-terminated no-step loop with a leading `if (c) break;` to the
/// if/else+goto form MSC emits:
///   `while (C) { if (c) break; REST } return X;`
///     →  `[init] L: if (!C || c) { return X } else { REST; goto L }`
/// (for a literally-infinite loop the cond is just `c`). Reuses the existing
/// if/else and goto codegen, which matches MSC's block ordering byte-for-byte.
fn try_rewrite_break_loop(s: &Stmt, ret: &Stmt, id: usize) -> Option<Vec<Stmt>> {
    let (init, cond_opt, break_cond, rest) = match_leading_break_loop(s)?;
    let combined = match cond_opt {
        None => break_cond,
        Some(c) => Cond::Or(Box::new(negate_cond(&c)?), Box::new(break_cond)),
    };
    let label = format!("__bl{id}");
    let mut new = Vec::new();
    if let Some(init) = init
        && !matches!(init, Stmt::Empty)
    {
        new.push(init);
    }
    new.push(Stmt::Label(label.clone()));
    let mut else_block = rest;
    else_block.push(Stmt::Goto(label.clone()));
    new.push(Stmt::If {
        cond: combined,
        then_branch: Box::new(ret.clone()),
        else_branch: Some(Box::new(Stmt::Block(else_block))),
    });
    Some(new)
}
/// Conservative structural equality for scalar expressions — used by the
/// do-while continue tail-merge to decide two statements are identical. Returns
/// false for any shape it doesn't explicitly handle (so a mismatch never merges).
fn expr_eq(a: &Expr, b: &Expr) -> bool {
    use Expr::*;
    match (a, b) {
        (IntLit(x), IntLit(y)) => x == y,
        (Local(x), Local(y)) | (Global(x), Global(y)) | (Param(x), Param(y)) => x == y,
        (BinOp { op: o1, left: l1, right: r1 }, BinOp { op: o2, left: l2, right: r2 }) =>
            o1 == o2 && expr_eq(l1, l2) && expr_eq(r1, r2),
        (DerefByte { ptr: p1 }, DerefByte { ptr: p2 })
        | (DerefWord { ptr: p1 }, DerefWord { ptr: p2 }) => expr_eq(p1, p2),
        (CastChar { value: v1, unsigned: u1, from_var: f1 }, CastChar { value: v2, unsigned: u2, from_var: f2 }) =>
            u1 == u2 && f1 == f2 && expr_eq(v1, v2),
        _ => false,
    }
}
/// Conservative equality for scalar assignment targets.
fn assign_target_eq(a: &AssignTarget, b: &AssignTarget) -> bool {
    use AssignTarget::*;
    match (a, b) {
        (Local(x), Local(y)) | (Global(x), Global(y)) | (Param(x), Param(y)) => x == y,
        (DerefParam(x), DerefParam(y)) | (DerefLocal(x), DerefLocal(y)) => x == y,
        _ => false,
    }
}
/// Conservative structural equality for statements (assignments / expr-stmts).
fn stmt_eq(a: &Stmt, b: &Stmt) -> bool {
    match (a, b) {
        (Stmt::Assign { target: t1, value: v1 }, Stmt::Assign { target: t2, value: v2 }) =>
            assign_target_eq(t1, t2) && expr_eq(v1, v2),
        (Stmt::ExprStmt(e1), Stmt::ExprStmt(e2)) => expr_eq(e1, e2),
        _ => false,
    }
}
/// Do-while continue tail-merge: when the body's leading `if (c) { TAIL; continue; }`
/// ends with the SAME statements `TAIL` that close the loop body, MSC factors the
/// common tail — the `continue` jumps PAST the middle to the shared `TAIL`:
///   do { if (c) { TAIL; continue; } MID; TAIL } while (d);
///     →  do { if (c) goto L; MID; L: TAIL } while (d);
/// Reuses the if/goto/label codegen. Fixture 3326.
fn rewrite_dowhile_continue_tailmerge(s: &Stmt, id: usize) -> Option<Stmt> {
    let Stmt::DoWhile { body, cond } = s else { return None };
    let stmts: &[Stmt] = match body.as_ref() {
        Stmt::Block(v) => v,
        other => std::slice::from_ref(other),
    };
    let Stmt::If { cond: c, then_branch, else_branch: None } = stmts.first()? else { return None };
    let inner: &[Stmt] = match then_branch.as_ref() {
        Stmt::Block(v) => v,
        other => std::slice::from_ref(other),
    };
    if !matches!(inner.last()?, Stmt::Continue) { return None; }
    let tail = &inner[..inner.len() - 1]; // the if-body minus the trailing `continue`
    if tail.is_empty() { return None; }
    let after = &stmts[1..]; // body after the leading if
    if after.len() < tail.len() { return None; }
    let (mid, body_tail) = after.split_at(after.len() - tail.len());
    // The if-body's TAIL must structurally match the loop body's trailing TAIL,
    // and there must be no other break/continue anywhere in MID or the tail.
    if !tail.iter().zip(body_tail).all(|(x, y)| stmt_eq(x, y)) { return None; }
    if mid.iter().chain(body_tail).any(stmt_has_loop_break) { return None; }
    let label = format!("__dc{id}");
    let mut new_body = Vec::new();
    new_body.push(Stmt::If {
        cond: c.clone(),
        then_branch: Box::new(Stmt::Goto(label.clone())),
        else_branch: None,
    });
    new_body.extend_from_slice(mid);
    new_body.push(Stmt::Label(label));
    new_body.extend_from_slice(body_tail);
    Some(Stmt::DoWhile { body: Box::new(Stmt::Block(new_body)), cond: cond.clone() })
}
/// A return-terminated infinite loop with BOTH a leading and a trailing
/// `if (c) break;` (no other break in between):
///   `while (1) { if (c1) break; MIDDLE; if (c2) break; } return X;`
///     →  `L: if (c1) return X;  MIDDLE;  if (!c2) goto L;  return X;`
/// The two `return X` share one epilogue block (return-block sharing): the
/// first (if-guarded) is physical, the bare trailing one jumps to it. The
/// `if (!c2) goto L` is the loop-back. Fixture 2772.
fn try_rewrite_two_break_loop(s: &Stmt, ret: &Stmt, id: usize) -> Option<Vec<Stmt>> {
    let const_true = |c: &Cond| matches!(c, Cond::Truthy(Expr::IntLit(k)) if *k != 0);
    let body = match s {
        Stmt::While { cond, body } if const_true(cond) => body,
        Stmt::DoWhile { body, cond } if const_true(cond) => body,
        Stmt::For { init, cond, step, body }
            if const_true(cond)
                && matches!(init.as_ref(), Stmt::Empty)
                && matches!(step.as_ref(), Stmt::Empty) => body,
        _ => return None,
    };
    let stmts: &[Stmt] = match body.as_ref() {
        Stmt::Block(v) => v,
        other => std::slice::from_ref(other),
    };
    if stmts.len() < 2 { return None; }
    let Stmt::If { cond: c1, then_branch: t1, else_branch: None } = &stmts[0] else { return None };
    if !matches!(t1.as_ref(), Stmt::Break) { return None; }
    let Stmt::If { cond: c2, then_branch: t2, else_branch: None } = stmts.last()? else { return None };
    if !matches!(t2.as_ref(), Stmt::Break) { return None; }
    let middle = &stmts[1..stmts.len() - 1];
    if middle.iter().any(stmt_has_loop_break) { return None; }
    let Stmt::Return(rv) = ret else { return None };
    if return_share_key(rv).is_none() { return None; }
    let neg_c2 = negate_cond(c2)?;
    let label = format!("__tb{id}");
    let mut new = Vec::new();
    new.push(Stmt::Label(label.clone()));
    new.push(Stmt::If { cond: c1.clone(), then_branch: Box::new(ret.clone()), else_branch: None });
    new.extend_from_slice(middle);
    new.push(Stmt::If { cond: neg_c2, then_branch: Box::new(Stmt::Goto(label)), else_branch: None });
    new.push(ret.clone());
    Some(new)
}
/// `while (1) { ...; break; }` — and the do-while(1) / bare `for (;;)`
/// equivalents — executes its body exactly once: MSC drops the loop
/// structure entirely and emits the body once, minus the trailing break.
/// Requires the trailing break to be the ONLY loop-level break/continue.
/// Fixtures 2972, 3023, 3024.
fn match_once_loop(s: &Stmt) -> Option<Vec<Stmt>> {
    let const_true = |c: &Cond| matches!(c, Cond::Truthy(Expr::IntLit(k)) if *k != 0);
    let body = match s {
        Stmt::While { cond, body } if const_true(cond) => body,
        Stmt::DoWhile { body, cond } if const_true(cond) => body,
        Stmt::For { init, cond, step, body }
            if const_true(cond)
                && matches!(init.as_ref(), Stmt::Empty)
                && matches!(step.as_ref(), Stmt::Empty) => body,
        _ => return None,
    };
    let stmts: &[Stmt] = match body.as_ref() {
        Stmt::Block(v) => v,
        other => std::slice::from_ref(other),
    };
    let (last, prefix) = stmts.split_last()?;
    if !matches!(last, Stmt::Break) {
        return None;
    }
    if prefix.iter().any(stmt_has_loop_break) {
        return None;
    }
    Some(prefix.to_vec())
}
/// Count `goto L` statements anywhere in a statement tree.
fn count_gotos(stmts: &[Stmt], l: &str) -> usize {
    fn walk(s: &Stmt, l: &str) -> usize {
        match s {
            Stmt::Goto(m) => usize::from(m == l),
            Stmt::Block(v) => v.iter().map(|s| walk(s, l)).sum(),
            Stmt::If { then_branch, else_branch, .. } =>
                walk(then_branch, l) + else_branch.as_ref().map_or(0, |e| walk(e, l)),
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => walk(body, l),
            Stmt::For { init, step, body, .. } => walk(init, l) + walk(step, l) + walk(body, l),
            Stmt::Switch { cases, .. } =>
                cases.iter().flat_map(|a| a.body.iter()).map(|s| walk(s, l)).sum(),
            _ => 0,
        }
    }
    stmts.iter().map(|s| walk(s, l)).sum()
}
/// MSC's goto flow restructuring: `if (c) goto L; MID...; L: TAIL...` — when
/// MID exits (ends in return/goto) and TAIL terminates — INVERTS the branch
/// and lays the label block inline at the branch site, moving MID to the
/// function tail:  `if (!c) goto M; TAIL...; M: MID...`. The L label is
/// consumed (single referencing goto required). Fixtures 3306, 2230, 1701.
pub(crate) fn fold_goto_restructure(stmts: Vec<Stmt>) -> Vec<Stmt> {
    for i in 0..stmts.len() {
        let Stmt::If { cond, then_branch, else_branch: None } = &stmts[i] else { continue };
        let Stmt::Goto(l) = then_branch.as_ref() else { continue };
        let Some(neg) = negate_cond(cond) else { continue };
        if count_gotos(&stmts, l) != 1 { continue; }
        let Some(j) = stmts.iter().position(|s| matches!(s, Stmt::Label(m) if m == l)) else { continue };
        if j <= i + 1 { continue; }
        let mid = &stmts[i + 1..j];
        let tail = &stmts[j + 1..];
        if !mid.last().is_some_and(stmt_exits) { continue; }
        if !tail.last().is_some_and(stmt_exits) { continue; }
        let m_label = format!("__gr{i}");
        let mut out = stmts[..i].to_vec();
        out.push(Stmt::If {
            cond: neg,
            then_branch: Box::new(Stmt::Goto(m_label.clone())),
            else_branch: None,
        });
        out.extend_from_slice(tail);
        out.push(Stmt::Label(m_label));
        out.extend_from_slice(mid);
        return out;
    }
    stmts
}
/// Top-level pass: rewrite each `[no-step leading-break loop, return]` pair into
/// the if/else+goto form. Run after const-prop so the body reads are already
/// resolved to runtime loads.
pub(crate) fn fold_break_loops(stmts: Vec<Stmt>) -> (Vec<Stmt>, std::collections::HashSet<usize>) {
    let mut out: Vec<Stmt> = Vec::new();
    let mut extra_mut: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut i = 0;
    let mut id = 0usize;
    while i < stmts.len() {
        if let Some(body) = match_once_loop(&stmts[i]) {
            out.extend(body);
            i += 1;
            continue;
        }
        // Do-while continue tail-merge (keeps the loop; just restructures the
        // body), so the normal loop-mutation handling still applies. Fixture 3326.
        if let Some(rw) = rewrite_dowhile_continue_tailmerge(&stmts[i], id) {
            id += 1;
            out.push(rw);
            i += 1;
            continue;
        }
        if i + 1 < stmts.len()
            && matches!(stmts[i + 1], Stmt::Return(_))
            && let Some(rw) = try_rewrite_two_break_loop(&stmts[i], &stmts[i + 1], id)
        {
            id += 1;
            for m in collect_loop_body_mutations(&[&stmts[i]]) {
                extra_mut.insert(m);
            }
            out.extend(rw);
            i += 2;
            continue;
        }
        if i + 1 < stmts.len()
            && matches!(stmts[i + 1], Stmt::Return(_))
            && let Some(rw) = try_rewrite_break_loop(&stmts[i], &stmts[i + 1], id)
        {
            id += 1;
            // The rewritten loop reads its mutated locals across the back edge,
            // but the if/else form is emitted with plain inits — mark them so the
            // emit-time fold view treats them as runtime, not their pre-loop init.
            for m in collect_loop_body_mutations(&[&stmts[i]]) {
                extra_mut.insert(m);
            }
            out.extend(rw);
            i += 2;
            continue;
        }
        out.push(stmts[i].clone());
        i += 1;
    }
    (out, extra_mut)
}
/// True when `s` contains a loop construct (used to bail out of the 2-deep
/// nested-loop threading when the inner body itself has a loop).
fn stmt_contains_loop(s: &Stmt) -> bool {
    match s {
        Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. } => true,
        Stmt::Block(v) => v.iter().any(stmt_contains_loop),
        Stmt::If { then_branch, else_branch, .. } =>
            stmt_contains_loop(then_branch)
                || else_branch.as_ref().is_some_and(|e| stmt_contains_loop(e)),
        _ => false,
    }
}
/// MSC threads a 2-deep nested for-loop: the inner loop's init is hoisted into
/// the outer loop's cond-continuation, and the loops share back edges:
/// ```text
///   I; jmp OCOND; [nop]
///   ITOP:  Sj                              (inner step)
///   ICOND: cmp Cj; jcc(!Cj) -> IEXIT ; B ; jmp ITOP
///   IEXIT: Si                              (outer step)
///   OCOND: cmp Ci; jcc(!Ci) -> EXIT  ; J ; jmp ICOND
///   EXIT:
/// ```
/// Handles only the no-break inner form (the inner body has no break/continue and
/// no further nested loop). Returns false if the shape doesn't match, so the
/// caller falls back to normal emission. Builds into a scratch buffer so a rel8
/// overflow can bail without corrupting `out`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_threaded_for(
    outer_init: &Stmt,
    outer_cond: &Cond,
    outer_step: &Stmt,
    outer_body: &Stmt,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) -> bool {
    // Collect the loop chain outer→inner. Each level is a for-loop with an
    // invertible Cmp cond and a non-empty step; an intermediate body is exactly
    // one nested for-loop. The innermost level's body is the work.
    struct Level<'a> { init: &'a Stmt, cond: &'a Cond, op: RelOp, step: &'a Stmt }
    let mut levels: Vec<Level> = Vec::new();
    {
        let Cond::Cmp { op, .. } = outer_cond else { return false };
        if matches!(outer_step, Stmt::Empty) { return false; }
        levels.push(Level { init: outer_init, cond: outer_cond, op: *op, step: outer_step });
    }
    let mut work_body: &Stmt = outer_body;
    loop {
        let inner = match work_body {
            Stmt::For { .. } => work_body,
            Stmt::Block(v) if v.len() == 1 && matches!(v[0], Stmt::For { .. }) => &v[0],
            _ => break,
        };
        let Stmt::For { init, cond, step, body } = inner else { break };
        let Cond::Cmp { op, .. } = cond else { break };
        if matches!(step.as_ref(), Stmt::Empty) { break; }
        levels.push(Level { init: &**init, cond, op: *op, step: &**step });
        work_body = &**body;
    }
    if levels.len() < 2 {
        return false; // not a nested loop
    }
    // Innermost body: break-free, or a leading `if(c)break;` + rest; no more loops.
    if stmt_contains_loop(work_body) {
        return false;
    }
    let inner_break: Option<(Cond, Vec<Stmt>)> = match for_leading_break(work_body) {
        Some((bc, rest)) => match bc {
            Cond::Cmp { .. } => Some((bc, rest)),
            _ => return false, // only Cmp break conds
        },
        None => {
            if stmt_has_loop_break(work_body) {
                return false; // break not in the canonical leading position
            }
            None
        }
    };
    // Clear inits for every local mutated across the nest so the cond/body reads
    // are runtime, not the pre-loop init constants.
    let muts = collect_loop_body_mutations(&[outer_body, outer_step]);
    let body_inits: Vec<Option<i32>> = locals.inits.iter().enumerate()
        .map(|(i, &v)| if muts.contains(&i) { None } else { v })
        .collect();
    let bl = Locals {
        inits: &body_inits,
        entry_inits: locals.entry_inits,
        disps: locals.disps,
        sizes: locals.sizes,
        long_globals: locals.long_globals,
        char_globals: locals.char_globals,
        global_elem_sizes: locals.global_elem_sizes,
        structs: locals.structs,
        global_struct_idxs: locals.global_struct_idxs,
        local_struct_idxs: locals.local_struct_idxs,
        unsigned_globals: locals.unsigned_globals,
        float_globals: locals.float_globals,
        long_locals: locals.long_locals,
        long_int_fold: locals.long_int_fold,
        init_literals: locals.init_literals,
        far_ptr_locals: locals.far_ptr_locals,
        huge_ptr_locals: locals.huge_ptr_locals,
        array_locals: locals.array_locals,
        unsigned_locals: locals.unsigned_locals,
        local_pointee_sizes: locals.local_pointee_sizes,
        local_pointee_unsigned: locals.local_pointee_unsigned,
        local_float_bits: locals.local_float_bits,
        float_locals: locals.float_locals,
        char_params: locals.char_params,
        param_struct_bytes: locals.param_struct_bytes,
        long_params: locals.long_params,
        unsigned_params: locals.unsigned_params,
        param_float_widths: locals.param_float_widths,
        param_pointee_sizes: locals.param_pointee_sizes,
        char_returners: locals.char_returners,
        long_returners: locals.long_returners,
        pascal_fns: locals.pascal_fns,
        static_fns: locals.static_fns,
        far_fns: locals.far_fns,
        variadic_fns: locals.variadic_fns,
        pascal_cleanup: locals.pascal_cleanup,
        si_local: locals.si_local,
        di_local: locals.di_local,
        long_param_funcs: locals.long_param_funcs,
        struct_param_funcs: locals.struct_param_funcs,
        float_param_funcs: locals.float_param_funcs,
        struct_return_funcs: locals.struct_return_funcs,
        float_returners: locals.float_returners,
        loop_stack: locals.loop_stack,
        labels: locals.labels,
        label_fixups: locals.label_fixups,
        backward_labels: locals.backward_labels,
        fpu_live: locals.fpu_live,
        return_float_width: locals.return_float_width,
        return_struct_bytes: locals.return_struct_bytes,
        struct_temp_bss_offset: locals.struct_temp_bss_offset,
        float_call_temp_disp: locals.float_call_temp_disp,
        fpu_pending_fwait: locals.fpu_pending_fwait,
        param_struct_ptr_bytes: locals.param_struct_ptr_bytes,
        int_cast_ptrs: locals.int_cast_ptrs,
        struct_field_temp_base: locals.struct_field_temp_base,
        elide_call_cleanup: std::cell::Cell::new(false),
        ternary_tail_epilogue: std::cell::RefCell::new(None),
        last_branch_barrier: std::cell::Cell::new(0),
        merge_barrier: std::cell::Cell::new(None),
        last_top_stmt: std::cell::Cell::new(false),
        final_top_stmt: std::cell::Cell::new(false),
    };
    let n = levels.len();
    let base = out.len();
    let mut b: Vec<u8> = Vec::new();
    let mut bfix: Vec<Fixup> = Vec::new();
    // A `goto`/early-return inside the nest records a label fixup (and possibly a
    // label definition) into the SHARED tables with offsets relative to the
    // scratch buffer `b`. Snapshot the table tails / existing labels here so they
    // can be rebased by `base` when `b` is committed. Fixture 4215.
    let lf_start = bl.label_fixups.borrow().len();
    let labels_before: std::collections::HashSet<String> =
        bl.labels.borrow().keys().cloned().collect();
    let mut cond_pos = vec![0usize; n]; // offset of each level's COND
    let mut jge_pos = vec![0usize; n];  // disp byte of each COND's exit jcc
    let mut exit_tgt = vec![0usize; n]; // where each COND's exit jcc lands
    // Outermost init, then jmp to the outermost COND (+ nop to align the
    // innermost loop top to an even offset).
    emit_stmt(levels[0].init, &bl, frame, return_int, return_long, &mut b, &mut bfix);
    let needs_pad = (base + b.len() + 2) % 2 != 0;
    b.push(0xEB);
    let jmp0 = b.len();
    b.push(0x00);
    if needs_pad { b.push(0x90); }
    // Innermost loop body region (level n-1).
    let inner = &levels[n - 1];
    let itop = b.len();
    // An innermost body that is a single `if(D) S;` whose then-branch S always
    // exits (returns or `goto`s out of the nest) fuses the if-test into the
    // loop's conditional back-edge — exactly like `emit_for_fused_if` but inside
    // the thread. The inner COND's exit jcc still goes to IEXIT; then `cmp D;
    // jcc(!D) -> ITOP` is the loop-back, and S is laid out straight after it with
    // no trailing unconditional `jmp ITOP` (S terminates). Fixture 4215
    // (`goto out` of a 2-deep nest).
    let inner_fused_if: Option<(&Cond, &Stmt)> = if inner_break.is_none() {
        match for_body_single_if_exit_or_goto(work_body) {
            Some((d, s)) if matches!(d, Cond::Cmp { .. }) => Some((d, s)),
            _ => None,
        }
    } else {
        None
    };
    if let Some((cond_d, then_s)) = inner_fused_if {
        let Cond::Cmp { op: dop, .. } = cond_d else { unreachable!() };
        emit_stmt(inner.step, &bl, frame, return_int, return_long, &mut b, &mut bfix);
        cond_pos[n - 1] = b.len();
        emit_cond_cmp(inner.cond, &bl, &mut b, &mut bfix);
        b.push(inverted_jcc(inner.op));
        jge_pos[n - 1] = b.len();
        b.push(0x00);
        // The if-test, then a backward jcc to ITOP when D is FALSE (loop back).
        emit_cond_cmp(cond_d, &bl, &mut b, &mut bfix);
        b.push(inverted_jcc(*dop));
        let p = b.len();
        b.push(0x00);
        let Some(d) = i8_disp(itop, p) else { return false };
        b[p] = d;
        // S (the then-branch) falls through here when D is true and terminates;
        // no trailing loop-back jmp. The IEXIT pad is handled by the enclosing
        // level (prev_uncond_jmp is true since inner_break is None).
        emit_stmt(then_s, &bl, frame, return_int, return_long, &mut b, &mut bfix);
    } else if let Some((break_c, rest)) = &inner_break {
        let Cond::Cmp { op: bc_op, .. } = break_c else { return false };
        for s in rest {
            emit_stmt(s, &bl, frame, return_int, return_long, &mut b, &mut bfix);
        }
        emit_stmt(inner.step, &bl, frame, return_int, return_long, &mut b, &mut bfix);
        cond_pos[n - 1] = b.len();
        emit_cond_cmp(inner.cond, &bl, &mut b, &mut bfix);
        b.push(inverted_jcc(inner.op));
        jge_pos[n - 1] = b.len();
        b.push(0x00);
        emit_cond_cmp(break_c, &bl, &mut b, &mut bfix);
        b.push(inverted_jcc(*bc_op)); // loop back when the break cond is false
        let p = b.len();
        b.push(0x00);
        let Some(d) = i8_disp(itop, p) else { return false };
        b[p] = d;
    } else {
        emit_stmt(inner.step, &bl, frame, return_int, return_long, &mut b, &mut bfix);
        cond_pos[n - 1] = b.len();
        emit_cond_cmp(inner.cond, &bl, &mut b, &mut bfix);
        b.push(inverted_jcc(inner.op));
        jge_pos[n - 1] = b.len();
        b.push(0x00);
        emit_stmt(work_body, &bl, frame, return_int, return_long, &mut b, &mut bfix);
        b.push(0xEB);
        let p = b.len();
        b.push(0x00);
        let Some(d) = i8_disp(itop, p) else { return false };
        b[p] = d;
    }
    // Enclosing levels, inner→outer: each emits its step, its COND (with an exit
    // jcc), then the next-inner level's init and a jmp into that inner COND.
    for l in (0..n - 1).rev() {
        // MSC aligns each enclosing level's step label (where the inner COND's
        // exit jcc lands) to an even offset with a nop — but ONLY when the label
        // is a pure jump target, i.e. the preceding region ended with an
        // unconditional jmp. The innermost level's body ends that way only in the
        // no-break case; with a leading `if(c)break;` it ends on a conditional
        // loop-back and the step label is a FALL-THROUGH target (no pad).
        // Fixtures 1700 (pad) vs 2229 (no pad).
        let prev_uncond_jmp = !(l == n - 2 && inner_break.is_some());
        if prev_uncond_jmp && (base + b.len()) % 2 != 0 { b.push(0x90); }
        exit_tgt[l + 1] = b.len(); // the inner COND's exit lands at this step
        emit_stmt(levels[l].step, &bl, frame, return_int, return_long, &mut b, &mut bfix);
        cond_pos[l] = b.len();
        emit_cond_cmp(levels[l].cond, &bl, &mut b, &mut bfix);
        b.push(inverted_jcc(levels[l].op));
        jge_pos[l] = b.len();
        b.push(0x00);
        emit_stmt(levels[l + 1].init, &bl, frame, return_int, return_long, &mut b, &mut bfix);
        b.push(0xEB);
        let p = b.len();
        b.push(0x00);
        let Some(d) = i8_disp(cond_pos[l + 1], p) else { return false };
        b[p] = d;
    }
    // Align EXIT to an even offset (MSC pads the loop exit with a nop).
    if (base + b.len()) % 2 != 0 {
        b.push(0x90);
    }
    exit_tgt[0] = b.len();
    // Backpatch the entry jmp and every level's exit jcc.
    let Some(d) = i8_disp(cond_pos[0], jmp0) else { return false };
    b[jmp0] = d;
    for l in 0..n {
        let Some(d) = i8_disp(exit_tgt[l], jge_pos[l]) else { return false };
        b[jge_pos[l]] = d;
    }
    // Commit: append the scratch buffer and re-base its fixups.
    out.extend_from_slice(&b);
    for mut f in bfix {
        f.body_offset += base;
        fixups.push(f);
    }
    // Rebase `goto`/early-return label fixups and any new label definitions from
    // buffer-relative into out-relative space (a goto out of the nest resolves
    // against the outer label tables). Fixture 4215.
    for (_, pos) in bl.label_fixups.borrow_mut().iter_mut().skip(lf_start) {
        *pos += base;
    }
    for (name, pos) in bl.labels.borrow_mut().iter_mut() {
        if !labels_before.contains(name) { *pos += base; }
    }
    true
}
/// rel8 displacement from a jump whose disp byte is at `disp_pos` to `target`
/// (both buffer-relative). Returns None if it doesn't fit in an i8.
fn i8_disp(target: usize, disp_pos: usize) -> Option<u8> {
    i8::try_from(target as i64 - (disp_pos as i64 + 1)).ok().map(|d| d as u8)
}
/// `for (<init>; <cond>; <step>) <body>` — MSC's layout.
///
/// When the entry condition is known true after the for-init (e.g.
/// `for (i=0; i<4; i++)`), MSC uses do-while form:
/// ```text
/// <init>    init expression-statement
/// <body>    loop body
/// <step>    step expression
/// <cmp>     cond comparison
/// <jcc>     jcc back to body start
/// ```
/// When the entry condition is unknown, the while form is used:
/// ```text
/// <init>
/// eb <step+body[+pad]>  jmp short to cond
/// [90]
/// <step>
/// <body>
/// <cmp>
/// <jcc>     jcc back to step start
/// ```
/// True when `s` is a `break`, or a `Block` whose last statement is a `break`.
fn stmt_ends_in_break(s: &Stmt) -> bool {
    match s {
        Stmt::Break => true,
        Stmt::Block(ss) => ss.last().is_some_and(stmt_ends_in_break),
        _ => false,
    }
}
/// Match a for-loop body that is a single `if (D) S;` (no else) where `S` cannot
/// fall through to the next iteration — it either always returns or ends in a
/// `break`. The shape MSC fuses into a conditional loop back-edge. Returns the
/// if-condition, the then-branch, and whether it returns (vs. breaks).
/// Fixtures 1256 (return), 4048 (break).
fn for_body_single_if_exit<'a>(body: &'a Stmt, locals: &Locals<'_>) -> Option<(&'a Cond, &'a Stmt, bool)> {
    let if_stmt = match body {
        Stmt::If { .. } => body,
        Stmt::Block(ss) if ss.len() == 1 => &ss[0],
        _ => return None,
    };
    if let Stmt::If { cond, then_branch, else_branch: None } = if_stmt {
        if stmt_always_returns(then_branch, locals) {
            return Some((cond, then_branch.as_ref(), true));
        }
        if stmt_ends_in_break(then_branch) {
            return Some((cond, then_branch.as_ref(), false));
        }
    }
    None
}
/// Match a single `if (D) S;` (no else) where S terminates control via a return
/// OR a `goto` (a label-exit out of an enclosing loop nest). Returns the
/// if-condition and the then-branch. Unlike [`for_body_single_if_exit`], a bare
/// `goto`-out counts as terminating here — the threaded-loop emitter fuses the
/// if-test into the inner back-edge and lays S out straight (no loop-back jmp).
/// Fixture 4215.
fn for_body_single_if_exit_or_goto(body: &Stmt) -> Option<(&Cond, &Stmt)> {
    let if_stmt = match body {
        Stmt::If { .. } => body,
        Stmt::Block(ss) if ss.len() == 1 => &ss[0],
        _ => return None,
    };
    if let Stmt::If { cond, then_branch, else_branch: None } = if_stmt {
        if stmt_exits(then_branch) {
            return Some((cond, then_branch.as_ref()));
        }
    }
    None
}
/// True when `s` is a bare `continue` (optionally wrapped in a single-stmt block).
fn stmt_is_continue(s: &Stmt) -> bool {
    matches!(s, Stmt::Continue) || matches!(s, Stmt::Block(b) if b.len() == 1 && matches!(b[0], Stmt::Continue))
}
/// True when `s` is a bare `break` (optionally wrapped in a single-stmt block).
fn stmt_is_break(s: &Stmt) -> bool {
    matches!(s, Stmt::Break) || matches!(s, Stmt::Block(b) if b.len() == 1 && matches!(b[0], Stmt::Break))
}
/// Match a for-loop body `{ if(c1) continue; if(c2) break; REST }` where REST has
/// no further loop control. MSC hoists both leading ifs into the loop's condition
/// section. Returns the continue-cond, the break-cond, and the REST statements.
/// Fixture 2988.
fn for_body_continue_break<'a>(body: &'a Stmt) -> Option<(&'a Cond, &'a Cond, Vec<&'a Stmt>)> {
    let Stmt::Block(ss) = body else { return None; };
    if ss.len() < 2 { return None; }
    let Stmt::If { cond: c1, then_branch: t1, else_branch: None } = &ss[0] else { return None; };
    if !stmt_is_continue(t1) { return None; }
    let Stmt::If { cond: c2, then_branch: t2, else_branch: None } = &ss[1] else { return None; };
    if !stmt_is_break(t2) { return None; }
    let rest: Vec<&Stmt> = ss[2..].iter().collect();
    if rest.iter().any(|s| stmt_has_loop_break(s)) { return None; }
    Some((c1, c2, rest))
}
/// Emit `for(init; C; step) { if(c1) continue; if(c2) break; REST }` in MSC's
/// hoisted form (all of C/c1/c2 simple `Cmp`):
/// ```text
///   init; jmp COND
///   BODY: <REST>
///   STEP: <step>
///   COND: C   j!C  EXIT     ; guard
///         c1  j c1 STEP     ; continue when c1 is true
///         c2  j!c2 BODY     ; loop when c2 is false (c2 true falls through = break)
///   EXIT:
/// ```
/// Returns false (emitting nothing) on any displacement overflow. Fixture 2988.
fn emit_for_continue_break(
    init: &Stmt,
    cond_c: &Cond,
    step: &Stmt,
    cond_c1: &Cond,
    cond_c2: &Cond,
    rest: &[&Stmt],
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) -> bool {
    use crate::codegen::cond::emit_cond_cmp;
    let (Cond::Cmp { op: cop, .. }, Cond::Cmp { op: c1op, .. }, Cond::Cmp { op: c2op, .. }) =
        (cond_c, cond_c1, cond_c2) else { return false; };
    let unsign = |j: u8, c: &Cond| if cmp_is_unsigned(c, locals) { to_unsigned_jcc(j) } else { j };
    let jcc_guard = unsign(inverted_jcc(*cop), cond_c);   // exit when C false
    let jcc_cont = unsign(loop_back_jcc(*c1op), cond_c1);  // to STEP when c1 true
    let jcc_break = unsign(inverted_jcc(*c2op), cond_c2);  // to BODY when c2 false
    // Pre-build every segment; nothing reaches `out` until displacements check out.
    let mut rest_buf = Vec::new();
    let mut rest_fx: Vec<Fixup> = Vec::new();
    for s in rest { emit_stmt(s, locals, frame, return_int, return_long, &mut rest_buf, &mut rest_fx); }
    let mut step_buf = Vec::new();
    let mut step_fx: Vec<Fixup> = Vec::new();
    emit_stmt(step, locals, frame, return_int, return_long, &mut step_buf, &mut step_fx);
    let mut c_buf = Vec::new();
    let mut c_fx: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond_c, locals, &mut c_buf, &mut c_fx);
    let mut c1_buf = Vec::new();
    let mut c1_fx: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond_c1, locals, &mut c1_buf, &mut c1_fx);
    let mut c2_buf = Vec::new();
    let mut c2_fx: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond_c2, locals, &mut c2_buf, &mut c2_fx);
    // Back-edge magnitudes are position-independent — validate them up front.
    let cont_back = step_buf.len() + c_buf.len() + 2 + c1_buf.len() + 2; // STEP ← after c1-jcc
    let break_back = rest_buf.len() + step_buf.len() + c_buf.len() + 2 + c1_buf.len() + 2 + c2_buf.len() + 2;
    if i8::try_from(cont_back).is_err() || i8::try_from(break_back).is_err() { return false; }
    if i8::try_from(c1_buf.len() + 2 + c2_buf.len() + 2).is_err() { return false; }
    emit_stmt(init, locals, frame, return_int, return_long, out, fixups);
    let needs_pad = (out.len() + 2) % 2 != 0;
    let pad = usize::from(needs_pad);
    let jmp_disp = i8::try_from(rest_buf.len() + step_buf.len() + pad).expect("cont/break for jmp fits");
    out.push(0xEB);
    out.push(jmp_disp as u8);
    if needs_pad { out.push(0x90); }
    let body_base = out.len();
    out.extend_from_slice(&rest_buf);
    for mut f in rest_fx { f.body_offset += body_base; fixups.push(f); }
    let step_base = out.len();
    out.extend_from_slice(&step_buf);
    for mut f in step_fx { f.body_offset += step_base; fixups.push(f); }
    // COND: guard + forward exit jcc.
    let c_base = out.len();
    out.extend_from_slice(&c_buf);
    for mut f in c_fx { f.body_offset += c_base; fixups.push(f); }
    let g_fwd = i8::try_from(c1_buf.len() + 2 + c2_buf.len() + 2).expect("cont/break guard fits");
    out.push(jcc_guard);
    out.push(g_fwd as u8);
    // continue test: jump to STEP when c1 is true.
    let c1_base = out.len();
    out.extend_from_slice(&c1_buf);
    for mut f in c1_fx { f.body_offset += c1_base; fixups.push(f); }
    out.push(jcc_cont);
    let cont_disp = i8::try_from(step_base as i32 - (out.len() as i32 + 1)).expect("cont/break cont disp fits");
    out.push(cont_disp as u8);
    // break test: loop back to BODY when c2 is false (c2 true falls through = break).
    let c2_base = out.len();
    out.extend_from_slice(&c2_buf);
    for mut f in c2_fx { f.body_offset += c2_base; fixups.push(f); }
    out.push(jcc_break);
    let break_disp = i8::try_from(body_base as i32 - (out.len() as i32 + 1)).expect("cont/break break disp fits");
    out.push(break_disp as u8);
    true
}
/// Match a for-loop body that is a single `switch (scrut) { case K1: B1 break;
/// case K2: B2 break; }` — exactly two value-cases (no default), each ending in
/// `break`, with no other loop control inside. Returns the scrutinee and the two
/// (value, body-without-break) pairs. Fixture 3402.
fn for_body_switch_2case<'a>(body: &'a Stmt) -> Option<(&'a Expr, i32, Vec<&'a Stmt>, i32, Vec<&'a Stmt>)> {
    let sw = match body {
        Stmt::Switch { .. } => body,
        Stmt::Block(ss) if ss.len() == 1 => &ss[0],
        _ => return None,
    };
    let Stmt::Switch { scrutinee, cases } = sw else { return None; };
    if cases.len() != 2 { return None; }
    let mut out: Vec<(i32, Vec<&Stmt>)> = Vec::new();
    for arm in cases {
        let v = arm.value?; // no default allowed
        let (last, rest) = arm.body.split_last()?;
        if !matches!(last, Stmt::Break) { return None; }
        if rest.iter().any(|s| stmt_has_loop_break(s)) { return None; }
        out.push((v, rest.iter().collect()));
    }
    let (k2, b2) = out.pop().unwrap();
    let (k1, b1) = out.pop().unwrap();
    Some((scrutinee, k1, b1, k2, b2))
}
/// Emit `for(init; C; step) { switch(scrut){ case K1: B1 break; case K2: B2 break; } }`
/// in MSC's switch-in-loop form (C simple `Cmp`): the FIRST case body is hoisted
/// above the step (reached by `je`, its break = fall-through to step); the SECOND
/// is inline after the dispatch with an explicit `jmp step`; a non-match exits to
/// the step. Fixture 3402.
/// ```text
///   init; jmp COND
///   CASE1: <B1>
///   STEP: step
///   COND: C j!C EXIT
///         <scrut → AX>
///         cmp AX,K1; je CASE1
///         cmp AX,K2; jne STEP
///         <B2>; jmp STEP
///   EXIT:
/// ```
#[allow(clippy::too_many_arguments)]
fn emit_for_switch_2case(
    init: &Stmt,
    cond_c: &Cond,
    step: &Stmt,
    scrut: &Expr,
    k1: i32,
    b1: &[&Stmt],
    k2: i32,
    b2: &[&Stmt],
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) -> bool {
    use crate::codegen::cond::emit_cond_cmp;
    let Cond::Cmp { op: cop, .. } = cond_c else { return false; };
    let jcc_exit = {
        let j = inverted_jcc(*cop);
        if cmp_is_unsigned(cond_c, locals) { to_unsigned_jcc(j) } else { j }
    };
    let cmp_ax_k = |k: i32| -> Vec<u8> {
        if k == 0 { vec![0x0B, 0xC0] } // or ax,ax
        else { let mut v = vec![0x3D]; v.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); v }
    };
    let cmp1 = cmp_ax_k(k1);
    let cmp2 = cmp_ax_k(k2);
    // Pre-build segments.
    let mut b1_buf = Vec::new();
    let mut b1_fx: Vec<Fixup> = Vec::new();
    for s in b1 { emit_stmt(s, locals, frame, return_int, return_long, &mut b1_buf, &mut b1_fx); }
    let mut step_buf = Vec::new();
    let mut step_fx: Vec<Fixup> = Vec::new();
    emit_stmt(step, locals, frame, return_int, return_long, &mut step_buf, &mut step_fx);
    let mut c_buf = Vec::new();
    let mut c_fx: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond_c, locals, &mut c_buf, &mut c_fx);
    let mut scrut_buf = Vec::new();
    let mut scrut_fx: Vec<Fixup> = Vec::new();
    emit_expr_to_ax(scrut, locals, &mut scrut_buf, &mut scrut_fx);
    let mut b2_buf = Vec::new();
    let mut b2_fx: Vec<Fixup> = Vec::new();
    for s in b2 { emit_stmt(s, locals, frame, return_int, return_long, &mut b2_buf, &mut b2_fx); }
    // CASE1 (the byte after the aligned jmp) is even; EXIT pad is relative to it,
    // so the hoisted case-1 body length counts toward the parity.
    let after_jmp_step_rel = b1_buf.len() + step_buf.len() + c_buf.len() + 2 + scrut_buf.len()
        + cmp1.len() + 2 + cmp2.len() + 2 + b2_buf.len() + 2;
    let exit_pad = after_jmp_step_rel % 2;
    // Back-edges (position-independent) + guard forward — validate up front.
    let je1_back = b1_buf.len() + step_buf.len() + c_buf.len() + 2 + scrut_buf.len() + cmp1.len() + 2;
    let jne_back = step_buf.len() + c_buf.len() + 2 + scrut_buf.len() + cmp1.len() + 2 + cmp2.len() + 2;
    let jmp_back = step_buf.len() + c_buf.len() + 2 + scrut_buf.len() + cmp1.len() + 2 + cmp2.len() + 2 + b2_buf.len() + 2;
    let guard_fwd = scrut_buf.len() + cmp1.len() + 2 + cmp2.len() + 2 + b2_buf.len() + 2 + exit_pad;
    if i8::try_from(je1_back).is_err() || i8::try_from(jne_back).is_err()
        || i8::try_from(jmp_back).is_err() || i8::try_from(guard_fwd).is_err() { return false; }
    emit_stmt(init, locals, frame, return_int, return_long, out, fixups);
    let needs_pad = (out.len() + 2) % 2 != 0;
    let jmp_pad = usize::from(needs_pad);
    let jmp_disp = i8::try_from(b1_buf.len() + step_buf.len() + jmp_pad).expect("switch-loop jmp fits");
    out.push(0xEB);
    out.push(jmp_disp as u8);
    if needs_pad { out.push(0x90); }
    let case1_base = out.len();
    out.extend_from_slice(&b1_buf);
    for mut f in b1_fx { f.body_offset += case1_base; fixups.push(f); }
    let step_base = out.len();
    out.extend_from_slice(&step_buf);
    for mut f in step_fx { f.body_offset += step_base; fixups.push(f); }
    // COND: guard.
    let c_base = out.len();
    out.extend_from_slice(&c_buf);
    for mut f in c_fx { f.body_offset += c_base; fixups.push(f); }
    out.push(jcc_exit);
    out.push(guard_fwd as u8);
    // scrutinee.
    let scrut_base = out.len();
    out.extend_from_slice(&scrut_buf);
    for mut f in scrut_fx { f.body_offset += scrut_base; fixups.push(f); }
    // cmp K1; je CASE1 (backward).
    out.extend_from_slice(&cmp1);
    out.push(0x74); // je
    let d = i8::try_from(case1_base as i32 - (out.len() as i32 + 1)).expect("switch-loop je fits");
    out.push(d as u8);
    // cmp K2; jne STEP (backward) — non-match exits to the step.
    out.extend_from_slice(&cmp2);
    out.push(0x75); // jne
    let d = i8::try_from(step_base as i32 - (out.len() as i32 + 1)).expect("switch-loop jne fits");
    out.push(d as u8);
    // case 2 body inline, then jmp STEP (its break).
    let b2_base = out.len();
    out.extend_from_slice(&b2_buf);
    for mut f in b2_fx { f.body_offset += b2_base; fixups.push(f); }
    out.push(0xEB);
    let d = i8::try_from(step_base as i32 - (out.len() as i32 + 1)).expect("switch-loop jmp-step fits");
    out.push(d as u8);
    if exit_pad == 1 { out.push(0x90); }
    true
}
/// Match a for-loop body `{ PREFIX...; while(W) WBODY }` whose LAST statement is a
/// while-loop (and PREFIX has no return/loop-control, WBODY no break/continue).
/// MSC fuses the inner while's exit into the OUTER step. Returns the prefix
/// statements, the inner condition, and the inner body. Fixture 1382.
fn for_body_prefix_while<'a>(body: &'a Stmt, locals: &Locals<'_>) -> Option<(Vec<&'a Stmt>, &'a Cond, &'a Stmt)> {
    let Stmt::Block(ss) = body else { return None; };
    if ss.len() < 2 { return None; }
    let Stmt::While { cond: w, body: wbody } = ss.last().unwrap() else { return None; };
    let prefix: Vec<&Stmt> = ss[..ss.len() - 1].iter().collect();
    if prefix.iter().any(|s| stmt_has_loop_break(s) || stmt_always_returns(s, locals)) {
        return None;
    }
    if stmt_has_loop_break(wbody) || stmt_always_returns(wbody, locals) {
        return None;
    }
    Some((prefix, w, wbody.as_ref()))
}
/// Emit `for(init; C; step) { PREFIX; while(W) WBODY }` in MSC's fused form, where
/// the inner while's exit jumps straight to the OUTER step (C, W simple `Cmp`):
/// ```text
///   init; jmp COND
///   STEP: <step>
///   COND: C   j!C EXIT          ; outer guard
///         <PREFIX>
///   INNER: W  j!W STEP          ; inner exit IS the outer back-edge
///         <WBODY>; jmp INNER
///   EXIT:
/// ```
/// Returns false (emitting nothing) on any displacement overflow. Fixture 1382.
fn emit_for_trailing_while(
    init: &Stmt,
    cond_c: &Cond,
    step: &Stmt,
    prefix: &[&Stmt],
    cond_w: &Cond,
    wbody: &Stmt,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) -> bool {
    use crate::codegen::cond::emit_cond_cmp;
    let (Cond::Cmp { op: cop, .. }, Cond::Cmp { op: wop, .. }) = (cond_c, cond_w) else { return false; };
    let unsign = |j: u8, c: &Cond| if cmp_is_unsigned(c, locals) { to_unsigned_jcc(j) } else { j };
    let jcc_c = unsign(inverted_jcc(*cop), cond_c);   // exit when C false
    let jcc_w = unsign(inverted_jcc(*wop), cond_w);   // inner exit (→ STEP) when W false
    // Pre-build every segment; nothing reaches `out` until displacements check out.
    let mut step_buf = Vec::new();
    let mut step_fx: Vec<Fixup> = Vec::new();
    emit_stmt(step, locals, frame, return_int, return_long, &mut step_buf, &mut step_fx);
    let mut c_buf = Vec::new();
    let mut c_fx: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond_c, locals, &mut c_buf, &mut c_fx);
    let mut prefix_buf = Vec::new();
    let mut prefix_fx: Vec<Fixup> = Vec::new();
    for s in prefix { emit_stmt(s, locals, frame, return_int, return_long, &mut prefix_buf, &mut prefix_fx); }
    let mut w_buf = Vec::new();
    let mut w_fx: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond_w, locals, &mut w_buf, &mut w_fx);
    let mut wbody_buf = Vec::new();
    let mut wbody_fx: Vec<Fixup> = Vec::new();
    emit_stmt(wbody, locals, frame, return_int, return_long, &mut wbody_buf, &mut wbody_fx);
    // STEP is even-aligned (the jmp pad guarantees it), so all parities below are
    // relative to STEP=0. Pad after the inner `jmp INNER` so EXIT is even.
    let inner_rel = step_buf.len() + c_buf.len() + 2 + prefix_buf.len();
    let after_jmp_inner_rel = inner_rel + w_buf.len() + 2 + wbody_buf.len() + 2;
    let exit_pad = after_jmp_inner_rel % 2;
    // Back-edges (position-independent) and the guard forward jump — validate up front.
    let w_back = step_buf.len() + c_buf.len() + 2 + prefix_buf.len() + w_buf.len() + 2; // STEP ← after j!W
    let jmp_inner_back = w_buf.len() + 2 + wbody_buf.len() + 2;                          // INNER ← after jmp
    let guard_fwd = prefix_buf.len() + w_buf.len() + 2 + wbody_buf.len() + 2 + exit_pad;
    if i8::try_from(w_back).is_err() || i8::try_from(jmp_inner_back).is_err()
        || i8::try_from(guard_fwd).is_err() { return false; }
    emit_stmt(init, locals, frame, return_int, return_long, out, fixups);
    let needs_pad = (out.len() + 2) % 2 != 0;
    let jmp_pad = usize::from(needs_pad);
    let jmp_disp = i8::try_from(step_buf.len() + jmp_pad).expect("trailing-while jmp fits");
    out.push(0xEB);
    out.push(jmp_disp as u8);
    if needs_pad { out.push(0x90); }
    let step_base = out.len();
    out.extend_from_slice(&step_buf);
    for mut f in step_fx { f.body_offset += step_base; fixups.push(f); }
    // COND: guard + forward exit jcc.
    let c_base = out.len();
    out.extend_from_slice(&c_buf);
    for mut f in c_fx { f.body_offset += c_base; fixups.push(f); }
    out.push(jcc_c);
    out.push(guard_fwd as u8);
    // PREFIX.
    let prefix_base = out.len();
    out.extend_from_slice(&prefix_buf);
    for mut f in prefix_fx { f.body_offset += prefix_base; fixups.push(f); }
    // INNER cond + exit jcc to STEP (backward).
    let inner_base = out.len();
    let w_base = out.len();
    out.extend_from_slice(&w_buf);
    for mut f in w_fx { f.body_offset += w_base; fixups.push(f); }
    out.push(jcc_w);
    let w_disp = i8::try_from(step_base as i32 - (out.len() as i32 + 1)).expect("trailing-while inner exit fits");
    out.push(w_disp as u8);
    // WBODY, then jmp back to INNER (backward).
    let wbody_base = out.len();
    out.extend_from_slice(&wbody_buf);
    for mut f in wbody_fx { f.body_offset += wbody_base; fixups.push(f); }
    out.push(0xEB);
    let inner_disp = i8::try_from(inner_base as i32 - (out.len() as i32 + 1)).expect("trailing-while loop-back fits");
    out.push(inner_disp as u8);
    if exit_pad == 1 { out.push(0x90); }
    true
}
/// Emit the fused-if for-loop (see [`for_body_single_if_return`]):
/// ```text
///   init
///   jmp COND
///   STEP: <step>
///   COND: <C>; j!C AFTER        ; guard — exit the loop when C is false
///         <D>; j!D STEP         ; back-edge — loop when D is false
///         <S>                   ; D true: run then-branch (returns)
///   AFTER: <rest of function>
/// ```
/// Returns false (emitting nothing) if C/D aren't simple `Cmp` conds.
fn emit_for_fused_if(
    init: &Stmt,
    cond_c: &Cond,
    step: &Stmt,
    cond_d: &Cond,
    then_s: &Stmt,
    is_return: bool,
    back_when_true: bool,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) -> bool {
    use crate::codegen::cond::emit_cond_cmp;
    let (Cond::Cmp { op: cop, .. }, Cond::Cmp { op: dop, .. }) = (cond_c, cond_d) else {
        return false;
    };
    let jcc_false = |op: RelOp, c: &Cond| {
        let j = inverted_jcc(op);
        if cmp_is_unsigned(c, locals) { to_unsigned_jcc(j) } else { j }
    };
    let jcc_c = jcc_false(*cop, cond_c);
    // The middle cond's back-edge: loop when it is FALSE for a fused if-body
    // (`if(D) S` continues the loop when D is false), but loop when it is TRUE
    // for a `&&` for-cond's second conjunct (`A && B` continues while B is true).
    let jcc_d = if back_when_true {
        let j = loop_back_jcc(*dop);
        if cmp_is_unsigned(cond_d, locals) { to_unsigned_jcc(j) } else { j }
    } else {
        jcc_false(*dop, cond_d)
    };
    // Pre-build step / C / D / S so we can size the displacements. NOTE: nothing
    // is written to `out` until every displacement is validated, so a `false`
    // return leaves `out` untouched and the caller's normal path takes over.
    let mut step_buf = Vec::new();
    let mut step_fx: Vec<Fixup> = Vec::new();
    emit_stmt(step, locals, frame, return_int, return_long, &mut step_buf, &mut step_fx);
    let mut c_buf = Vec::new();
    let mut c_fx: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond_c, locals, &mut c_buf, &mut c_fx);
    let mut d_buf = Vec::new();
    let mut d_fx: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond_d, locals, &mut d_buf, &mut d_fx);
    let mut s_buf = Vec::new();
    let mut s_fx: Vec<Fixup> = Vec::new();
    if is_return {
        emit_stmt(then_s, locals, frame, return_int, return_long, &mut s_buf, &mut s_fx);
    } else {
        // Break / empty case: no inline then-code survives — a trailing `break`
        // (4048) or an empty `&&` for-cond body (3331) is an implicit fall-through
        // to the after-loop code, so S emits nothing but its leading statements.
        match then_s {
            Stmt::Break | Stmt::Empty => {}
            Stmt::Block(ss) => {
                for s in &ss[..ss.len().saturating_sub(1)] {
                    emit_stmt(s, locals, frame, return_int, return_long, &mut s_buf, &mut s_fx);
                }
            }
            _ => return false,
        }
    }
    // Validate the position-independent displacements up front (the back-edge and
    // the guard's forward jump). The back-edge magnitude bounds step+C+D too, so
    // the small initial-jmp disp can't overflow once these pass.
    let back_mag = step_buf.len() + c_buf.len() + d_buf.len() + 4;
    if i8::try_from(back_mag).is_err() { return false; }
    if i8::try_from(d_buf.len() + 2 + s_buf.len()).is_err() { return false; }
    // init runs straight-line.
    emit_stmt(init, locals, frame, return_int, return_long, out, fixups);
    // Initial `jmp COND`: align so the byte after the 2-byte jmp is even (MSC's
    // loop-top alignment), padding with a NOP and bumping the disp if needed.
    let needs_pad = (out.len() + 2) % 2 != 0;
    let pad = usize::from(needs_pad);
    let jmp_disp = i8::try_from(step_buf.len() + pad).expect("fused-for jmp disp fits");
    out.push(0xEB);
    out.push(jmp_disp as u8);
    if needs_pad { out.push(0x90); }
    // STEP.
    let step_base = out.len();
    out.extend_from_slice(&step_buf);
    for mut f in step_fx { f.body_offset += step_base; fixups.push(f); }
    // COND: guard C, then a forward jcc past the D-section and S to AFTER.
    let c_base = out.len();
    out.extend_from_slice(&c_buf);
    for mut f in c_fx { f.body_offset += c_base; fixups.push(f); }
    // A then-branch that RETURNS ends in `ret`, after which MSC pads with a NOP
    // so the after-loop block (the guard's exit target) is even-aligned. A break
    // then-branch falls THROUGH to the after-loop code, so it is never padded.
    // Fixture 521 (return, padded), 1256 (return, already even), 4048 (break).
    let s_end = c_base + c_buf.len() + 2 + d_buf.len() + 2 + s_buf.len();
    let s_pad = usize::from(is_return && s_end % 2 != 0);
    let c_fwd = i8::try_from(d_buf.len() + 2 + s_buf.len() + s_pad).expect("fused-for guard disp fits");
    out.push(jcc_c);
    out.push(c_fwd as u8);
    // Back-edge: D, then a backward jcc to STEP when D is false.
    let d_base = out.len();
    out.extend_from_slice(&d_buf);
    for mut f in d_fx { f.body_offset += d_base; fixups.push(f); }
    out.push(jcc_d);
    let back_disp = i8::try_from(step_base as i32 - (out.len() as i32 + 1)).expect("fused-for back disp fits");
    out.push(back_disp as u8);
    // S (the then-branch) falls through here when D is true; it returns.
    let s_base = out.len();
    out.extend_from_slice(&s_buf);
    for mut f in s_fx { f.body_offset += s_base; fixups.push(f); }
    if s_pad == 1 { out.push(0x90); }
    true
}
pub(crate) fn emit_for(
    init: &Stmt,
    cond: &Cond,
    step: &Stmt,
    body_stmt: &Stmt,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // 2-deep nested for-loop: MSC threads the two loops (inner init hoisted into
    // the outer cond-continuation). Handled before the normal init emission since
    // the threaded emitter lays down the outer init itself.
    if emit_threaded_for(init, cond, step, body_stmt, locals, frame, return_int, return_long, out, fixups) {
        return;
    }
    // `for (;;)` with no break at all: infinite loop, no test (3397).
    if matches!(init, Stmt::Empty) && matches!(step, Stmt::Empty)
        && matches!(cond, Cond::Truthy(Expr::IntLit(k)) if *k != 0)
        && !stmt_has_loop_break(body_stmt)
    {
        emit_infinite_loop(body_stmt, locals, frame, return_int, return_long, out, fixups);
        return;
    }
    // Normalize a `<register local> OP <memory operand>` loop cond by swapping
    // the operands (and the relop): MSC compares `cmp [mem],si` with the
    // register in the reg field, so `for(...; i < n; ...)` lowers to `cmp [n],si;
    // jg`. Const RHS (`i <= 10`) keeps the register on the left (`cmp si,10`).
    // Fixture 3305.
    let normalized_cond;
    let cond: &Cond = if let Cond::Cmp { op, left, right } = cond
        && let Expr::Local(li) = left
        && locals.reg_for_local(*li).is_some()
        && !matches!(right, Expr::IntLit(_))
        && !matches!(right, Expr::Local(rj) if locals.reg_for_local(*rj).is_some())
    {
        normalized_cond = Cond::Cmp { op: swap_relop(*op), left: right.clone(), right: left.clone() };
        &normalized_cond
    } else {
        cond
    };
    // `for(init; C; step) if(D) S;` where C and D are simple comparisons and the
    // then-branch S always returns: MSC fuses the if-condition into the loop's
    // conditional back-edge ("jne-back trick"), emitting the cond between step and
    // body. Fixture 1256. (A `break` then-branch is the leading-break case below.)
    // This emitter writes its own init, so it sits BEFORE the init emission.
    if let Some((cond_d, then_s, is_return)) = for_body_single_if_exit(body_stmt, locals)
        && matches!(cond, Cond::Cmp { .. })
        && matches!(cond_d, Cond::Cmp { .. })
        && emit_for_fused_if(init, cond, step, cond_d, then_s, is_return, false,
            locals, frame, return_int, return_long, out, fixups)
    {
        return;
    }
    // `for(init; A && B; step) ;` (empty body): MSC decomposes the `&&` into the
    // same cond-in-middle layout — A is the exit guard, B the conditional
    // back-edge (loop while B is TRUE). Fixture 3331 (`i < n && p[i] != 0`).
    if matches!(body_stmt, Stmt::Empty)
        && let Cond::And(a, b) = cond
        && matches!(a.as_ref(), Cond::Cmp { .. })
        && matches!(b.as_ref(), Cond::Cmp { .. })
        && emit_for_fused_if(init, a, step, b, &Stmt::Empty, false, true,
            locals, frame, return_int, return_long, out, fixups)
    {
        return;
    }
    // `for(init; C; step) { if(c1) continue; if(c2) break; REST }`: MSC hoists the
    // leading continue/break ifs into the condition section. Fixture 2988.
    if matches!(cond, Cond::Cmp { .. })
        && let Some((c1, c2, rest)) = for_body_continue_break(body_stmt)
        && matches!(c1, Cond::Cmp { .. })
        && matches!(c2, Cond::Cmp { .. })
        && emit_for_continue_break(init, cond, step, c1, c2, &rest,
            locals, frame, return_int, return_long, out, fixups)
    {
        return;
    }
    // `for(init; C; step) { PREFIX; while(W) WBODY }`: MSC emits the outer loop in
    // the cond-in-middle form and fuses the inner while's exit into the outer step.
    // Fixture 1382.
    if matches!(cond, Cond::Cmp { .. })
        && let Some((prefix, w, wbody)) = for_body_prefix_while(body_stmt, locals)
        && matches!(w, Cond::Cmp { .. })
        && emit_for_trailing_while(init, cond, step, &prefix, w, wbody,
            locals, frame, return_int, return_long, out, fixups)
    {
        return;
    }
    // `for(init; C; step) { switch(scrut){ case K1: B1 break; case K2: B2 break; } }`:
    // MSC hoists case 1 above the step and inlines case 2 in the cond section.
    // Fixture 3402.
    if matches!(cond, Cond::Cmp { .. })
        && let Some((scrut, k1, b1, k2, b2)) = for_body_switch_2case(body_stmt)
        && emit_for_switch_2case(init, cond, step, scrut, k1, &b1, k2, &b2,
            locals, frame, return_int, return_long, out, fixups)
    {
        return;
    }
    emit_stmt(init, locals, frame, return_int, return_long, out, fixups);
    // Infinite for-loop with no step and a trailing `if (c) break;` →
    // `do { prefix } while (!c)` after the init (same fold as while(1)).
    if matches!(step, Stmt::Empty)
        && matches!(cond, Cond::Truthy(Expr::IntLit(k)) if *k != 0)
        && let Some((prefix, neg)) = loop_trailing_break_rewrite(body_stmt)
    {
        let dw_body = Stmt::Block(prefix);
        emit_do_while(&dw_body, &neg, locals, frame, return_int, return_long, out, fixups);
        return;
    }
    // For-loop WITH step and a LEADING `if (c) break;`: fold the break into the
    // loop. `for (init; C; step) { if (c) break; REST }` lowers to a while-form
    // loop over REST+step whose bottom tests are the original cond C (exit guard)
    // followed by the inverted break `!c` (loop-back). MSC always uses the while
    // form here — the do-while opt keys off the syntactic for-cond, which a
    // break-derived bound lacks. The guard is None for a literally-infinite cond.
    if !matches!(step, Stmt::Empty)
        && let Some((break_c, rest)) = for_leading_break(body_stmt)
        && let Some(loop_back) = negate_cond(&break_c)
    {
        let guard_opt = match cond {
            Cond::Truthy(Expr::IntLit(k)) if *k != 0 => Some(None),
            Cond::Cmp { .. } => Some(Some(cond)),
            _ => None, // And/Or/runtime-truthy guard: fall through to normal path
        };
        if let Some(guard) = guard_opt {
            let rest_block = Stmt::Block(rest);
            emit_loop(&loop_back, &[&rest_block, step], None, Some(1), guard, true,
                locals, frame, return_int, return_long, out, fixups);
            return;
        }
    }
    // After the init runs, check if the condition is known true.
    // If so, use do-while (body then step); otherwise while (step then body).
    let entry = for_entry_fold(init, cond, locals);
    if matches!(entry, Some(k) if k != 0) && !matches!(body_stmt, Stmt::Empty) {
        // Do-while form: body THEN step, no initial jmp. `continue` → step (seg 1).
        emit_loop(cond, &[body_stmt, step], entry, Some(1), None, false, locals, frame, return_int, return_long, out, fixups);
    } else {
        // While form: body THEN step, with an initial jmp to the cond (added by
        // emit_loop). `continue` → step (seg 1).
        emit_loop(cond, &[body_stmt, step], None, Some(1), None, false, locals, frame, return_int, return_long, out, fixups);
    }
}
/// Compute `fold_cond` as it would evaluate after the for-init runs.
/// Handles only the common case: `init = Local(idx) = const_expr`.
pub(crate) fn for_entry_fold(init: &Stmt, cond: &Cond, locals: &Locals<'_>) -> Option<i32> {
    if let Stmt::Assign { target: AssignTarget::Local(idx), value } = init {
        // Entry view: a local mutated only inside a loop still folds its
        // declared init for the entry test (do-while elision).
        if let Some(k) = value.fold(locals.entry_inits) {
            let mut tmp: Vec<Option<i32>> = locals.entry_inits.to_vec();
            if *idx < tmp.len() {
                tmp[*idx] = Some(k);
            }
            return fold_cond_raw(cond, &tmp);
        }
    }
    None
}
/// Fold a condition using an explicit inits slice (no Locals wrapper).
pub(crate) fn fold_cond_raw(cond: &Cond, inits: &[Option<i32>]) -> Option<i32> {
    match cond {
        Cond::Truthy(e) => e.fold(inits),
        Cond::Cmp { op, left, right } => {
            let l = left.fold(inits)?;
            let r = right.fold(inits)?;
            Some(match op {
                RelOp::Eq => i32::from(l == r),
                RelOp::Ne => i32::from(l != r),
                RelOp::Lt => i32::from(l < r),
                RelOp::Gt => i32::from(l > r),
                RelOp::Le => i32::from(l <= r),
                RelOp::Ge => i32::from(l >= r),
            })
        }
        Cond::And(a, b) => {
            let av = fold_cond_raw(a, inits)?;
            if av == 0 { return Some(0); }
            let bv = fold_cond_raw(b, inits)?;
            Some(i32::from(bv != 0))
        }
        Cond::Or(a, b) => {
            let av = fold_cond_raw(a, inits)?;
            if av != 0 { return Some(1); }
            let bv = fold_cond_raw(b, inits)?;
            Some(i32::from(bv != 0))
        }
    }
}
/// Collect all local variable indices that are directly assigned
/// inside the loop body statements. Used to clear their `spec.init`
/// values from the body's fold view so MSC emits runtime loads
/// instead of substituting the pre-loop init constant.
/// Collect locals mutated inside a loop CONDITION (or exit guard): a
/// pre/post-mutation (`i--`) or an assignment-expression (`(*p = v)`) used as
/// the test. Unioned into the body fold-view strip set so those locals read
/// from their slot in the body. Fixture 2202.
pub(crate) fn collect_cond_mutations(cond: &Cond, set: &mut std::collections::HashSet<usize>) {
    fn expr_mut(e: &Expr, set: &mut std::collections::HashSet<usize>) {
        match e {
            Expr::PostMutateLocal { local_idx, .. }
            | Expr::PreMutateLocal { local_idx, .. } => { set.insert(*local_idx); }
            Expr::AssignExpr { target, value } => {
                match target {
                    AssignTarget::Local(i)
                    | AssignTarget::DerefLocal(i)
                    | AssignTarget::DerefLocalByte(i)
                    | AssignTarget::DerefPostMutateLocal { local_idx: i, .. } => { set.insert(*i); }
                    _ => {}
                }
                expr_mut(value, set);
            }
            Expr::BinOp { left, right, .. } => { expr_mut(left, set); expr_mut(right, set); }
            Expr::Ternary { cond, then_arm, else_arm } => { expr_mut(cond, set); expr_mut(then_arm, set); expr_mut(else_arm, set); }
            _ => {}
        }
    }
    match cond {
        Cond::Truthy(e) => expr_mut(e, set),
        Cond::Cmp { left, right, .. } => { expr_mut(left, set); expr_mut(right, set); }
        Cond::And(a, b) | Cond::Or(a, b) => {
            collect_cond_mutations(a, set);
            collect_cond_mutations(b, set);
        }
    }
}
pub(crate) fn collect_loop_body_mutations(stmts: &[&Stmt]) -> std::collections::HashSet<usize> {
    fn visit_stmt(stmt: &Stmt, set: &mut std::collections::HashSet<usize>) {
        match stmt {
            Stmt::Assign { target, .. } => {
                match target {
                    AssignTarget::Local(i)
                    | AssignTarget::IndexedLocal { local: i, .. }
                    | AssignTarget::IndexedLocalByte { local: i, .. }
                    | AssignTarget::IndexedLocalVar { local: i, .. }
                    | AssignTarget::IndexedLocalByteVar { local: i, .. }
                    | AssignTarget::LocalField { local: i, .. }
                    | AssignTarget::DerefLocal(i)
                    | AssignTarget::DerefLocalField { ptr_local: i, .. }
                    | AssignTarget::DerefPostMutateLocal { local_idx: i, .. } => {
                        set.insert(*i);
                    }
                    _ => {}
                }
            }
            Stmt::Block(ss) => { for s in ss { visit_stmt(s, set); } }
            Stmt::If { then_branch, else_branch, .. } => {
                visit_stmt(then_branch, set);
                if let Some(eb) = else_branch { visit_stmt(eb, set); }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                visit_stmt(body, set);
            }
            Stmt::For { init, step, body, .. } => {
                visit_stmt(init, set);
                visit_stmt(step, set);
                visit_stmt(body, set);
            }
            Stmt::ExprStmt(e) => { visit_expr(e, set); }
            _ => {}
        }
    }
    fn visit_expr(expr: &Expr, set: &mut std::collections::HashSet<usize>) {
        match expr {
            Expr::PostMutateLocal { local_idx, .. }
            | Expr::PreMutateLocal { local_idx, .. } => { set.insert(*local_idx); }
            Expr::Seq { sides, value } => {
                for s in sides { visit_stmt(s, set); }
                visit_expr(value, set);
            }
            _ => {}
        }
    }
    let mut set = std::collections::HashSet::new();
    for s in stmts { visit_stmt(s, &mut set); }
    set
}
/// Build a loop-body view of `locals`: the same frame/type metadata and the
/// same SHARED mutable tables (loop_stack, labels, label_fixups, fpu state),
/// but with `inits` rebound to `body_inits` (init values cleared for locals
/// mutated inside the loop) and FRESH per-statement scratch cells. Mirrors the
/// inline construction in [`emit_loop`]/[`emit_do_while`].
pub(crate) fn loop_body_locals<'a>(
    locals: &'a Locals<'a>,
    body_inits: &'a [Option<i32>],
) -> Locals<'a> {
    Locals {
        inits: body_inits,
        entry_inits: locals.entry_inits,
        disps: locals.disps,
        sizes: locals.sizes,
        long_globals: locals.long_globals,
        char_globals: locals.char_globals,
        global_elem_sizes: locals.global_elem_sizes,
        structs: locals.structs,
        global_struct_idxs: locals.global_struct_idxs,
        local_struct_idxs: locals.local_struct_idxs,
        unsigned_globals: locals.unsigned_globals,
        float_globals: locals.float_globals,
        long_locals: locals.long_locals,
        long_int_fold: locals.long_int_fold,
        init_literals: locals.init_literals,
        far_ptr_locals: locals.far_ptr_locals,
        huge_ptr_locals: locals.huge_ptr_locals,
        array_locals: locals.array_locals,
        unsigned_locals: locals.unsigned_locals,
        local_pointee_sizes: locals.local_pointee_sizes,
        local_pointee_unsigned: locals.local_pointee_unsigned,
        local_float_bits: locals.local_float_bits,
        float_locals: locals.float_locals,
        char_params: locals.char_params,
        param_struct_bytes: locals.param_struct_bytes,
        long_params: locals.long_params,
        unsigned_params: locals.unsigned_params,
        param_float_widths: locals.param_float_widths,
        param_pointee_sizes: locals.param_pointee_sizes,
        char_returners: locals.char_returners,
        long_returners: locals.long_returners,
        pascal_fns: locals.pascal_fns,
        static_fns: locals.static_fns,
        far_fns: locals.far_fns,
        variadic_fns: locals.variadic_fns,
        pascal_cleanup: locals.pascal_cleanup,
        si_local: locals.si_local,
        di_local: locals.di_local,
        long_param_funcs: locals.long_param_funcs,
        struct_param_funcs: locals.struct_param_funcs,
        float_param_funcs: locals.float_param_funcs,
        struct_return_funcs: locals.struct_return_funcs,
        float_returners: locals.float_returners,
        loop_stack: locals.loop_stack,
        labels: locals.labels,
        label_fixups: locals.label_fixups,
        backward_labels: locals.backward_labels,
        fpu_live: locals.fpu_live,
        return_float_width: locals.return_float_width,
        return_struct_bytes: locals.return_struct_bytes,
        struct_temp_bss_offset: locals.struct_temp_bss_offset,
        float_call_temp_disp: locals.float_call_temp_disp,
        fpu_pending_fwait: locals.fpu_pending_fwait,
        param_struct_ptr_bytes: locals.param_struct_ptr_bytes,
        int_cast_ptrs: locals.int_cast_ptrs,
        struct_field_temp_base: locals.struct_field_temp_base,
        elide_call_cleanup: std::cell::Cell::new(false),
        ternary_tail_epilogue: std::cell::RefCell::new(None),
        last_branch_barrier: std::cell::Cell::new(0),
        merge_barrier: std::cell::Cell::new(None),
        last_top_stmt: std::cell::Cell::new(false),
        final_top_stmt: std::cell::Cell::new(false),
    }
}
/// `while (C) { switch (n) { … } TAIL }` where the switch has ≥2 non-default
/// value cases: MSC does NOT rotate this loop (no initial jmp / test-at-bottom).
/// It emits the cond test at the TOP (exit-jcc forward), then lays the switch
/// out via the partial-continuation FWD layout with TAIL as the continuation —
/// the loop-back `jmp` is woven in right after TAIL, before the trailing case
/// bodies. Fixture 3234. Returns false when the shape doesn't match.
#[allow(clippy::too_many_arguments)]
fn try_emit_while_switch(
    cond: &Cond,
    body_stmt: &Stmt,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) -> bool {
    // Cond must be a single Cmp (the only exit-test shape a fixture exercises).
    let Cond::Cmp { op, .. } = cond else { return false };
    let exit_jcc: u8 = inverted_jcc(*op);
    // Body must be `{ switch(...){…}; TAIL… }` with the switch FIRST.
    let stmts: &[Stmt] = match body_stmt {
        Stmt::Block(v) => v,
        single => std::slice::from_ref(single),
    };
    let Some((Stmt::Switch { scrutinee, cases }, tail)) = stmts.split_first() else { return false };
    // Trigger only for ≥2 non-default value cases (a single-case switch keeps
    // the rotated form — fixtures zprobe-sw1nd / zprobe-while-switch).
    if cases.iter().filter(|a| a.value.is_some()).count() < 2 { return false; }
    if switch_is_table(cases) { return false; }
    // No loop-level break/continue (the switch owns its own breaks); TAIL must
    // not itself break the loop, and the scrutinee must be a runtime value.
    if tail.iter().any(stmt_has_loop_break) { return false; }
    if matches!(scrutinee, Expr::IntLit(_)) { return false; }
    // Each case body must be a simple sequence ending in a trailing top-level
    // `break` (or returning): no C fall-through to the next arm, and no nested
    // break (in an `if`) or loop `continue`, which `emit_arm` would route
    // through the loop stack and leave unpatched. Only the plain shape weaves.
    for a in cases.iter() {
        let (main, trailing_break) = match a.body.last() {
            Some(Stmt::Break) => (&a.body[..a.body.len() - 1], true),
            _ => (a.body.as_slice(), false),
        };
        if main.iter().any(stmt_has_loop_break) { return false; }
        let returns = main.iter().any(|s| stmt_always_returns(s, locals));
        if !trailing_break && !returns { return false; }
    }

    // Body fold view: clear init values for locals mutated in the loop body so
    // case bodies (`s = s + 10`) read the slot, not the pre-loop init.
    let body_mutations = collect_loop_body_mutations(&[body_stmt]);
    let body_inits: Vec<Option<i32>> = locals.inits.iter().enumerate()
        .map(|(i, &v)| if body_mutations.contains(&i) { None } else { v })
        .collect();
    let body_locals = loop_body_locals(locals, &body_inits);
    body_locals.loop_stack.borrow_mut().push(LoopCtx::default());

    // Loop top: the cond test. l_top points at the first cmp byte.
    let l_top = out.len();
    let mut cmp_buf = Vec::new();
    let mut cmp_fixups: Vec<Fixup> = Vec::new();
    emit_cond_cmp(cond, &body_locals, &mut cmp_buf, &mut cmp_fixups);
    let cmp_base = out.len();
    for mut c in cmp_fixups { c.body_offset += cmp_base; fixups.push(c); }
    out.extend_from_slice(&cmp_buf);
    out.push(exit_jcc);
    let exit_patch = out.len();
    out.push(0x00); // END placeholder

    // The switch + TAIL, woven via the partial-continuation FWD layout. ft is
    // the default arm (chain fall-through) when present, else the chain's final
    // jmp reaches the continuation.
    let ft = cases.iter().position(|a| a.value.is_none());
    let fold = ChainFold { ops: build_chain_ops(cases), fallin_arm: None };
    emit_partial_switch_with_continuation(
        scrutinee, None, &fold, ft, cases, tail,
        &body_locals, frame, return_int, return_long, out, fixups,
        Some(l_top),
    );

    // Patch the exit jcc to the post-loop fall-out.
    let end = out.len();
    out[exit_patch] = (end as i32 - (exit_patch + 1) as i32) as u8;

    // Drop the loop context; this shape forbids loop-level break/continue, so
    // there are no placeholders to patch.
    body_locals.loop_stack.borrow_mut().pop();
    true
}
/// Shared loop emitter — handles the alignment-pad, body
/// concatenation, cmp+jcc tail, and backward-jcc displacement.
/// Both while-loops (single-body) and for-loops (step+body) route
/// through here.
pub(crate) fn emit_loop(
    cond: &Cond,
    body_segments: &[&Stmt],
    entry_fold: Option<i32>,
    cont_seg: Option<usize>,
    exit_guard: Option<&Cond>,
    force_while: bool,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // Build a body-specific fold view that clears init values for
    // any local mutated inside the loop body. This prevents the
    // spec.init value (e.g. `i = 0`) from being substituted into
    // body expressions like `s = s + i` in later iterations.
    // The outer `locals` (with init values intact) is still used for
    // the entry-condition fold to detect the do-while optimization.
    let mut body_mutations = collect_loop_body_mutations(body_segments);
    // A local mutated in the loop CONDITION (`while(i--)`) or exit guard is also
    // loop-carried, so it must read from its slot in the body, not fold to its
    // declared init. This only strips the body fold view — the entry-condition
    // fold still uses the full `locals` (so the do-while elision is unaffected).
    // Fixture 2202.
    collect_cond_mutations(cond, &mut body_mutations);
    if let Some(g) = exit_guard { collect_cond_mutations(g, &mut body_mutations); }
    let body_inits: Vec<Option<i32>> = locals.inits.iter().enumerate()
        .map(|(i, &v)| if body_mutations.contains(&i) { None } else { v })
        .collect();
    let body_locals = Locals {
        inits: &body_inits,
        entry_inits: locals.entry_inits,
        disps: locals.disps,
        sizes: locals.sizes,
        long_globals: locals.long_globals,
        char_globals: locals.char_globals,
        global_elem_sizes: locals.global_elem_sizes,
        structs: locals.structs,
        global_struct_idxs: locals.global_struct_idxs,
        local_struct_idxs: locals.local_struct_idxs,
        unsigned_globals: locals.unsigned_globals,
        float_globals: locals.float_globals,
        long_locals: locals.long_locals,
        long_int_fold: locals.long_int_fold,
        init_literals: locals.init_literals,
        far_ptr_locals: locals.far_ptr_locals,
        huge_ptr_locals: locals.huge_ptr_locals,
        array_locals: locals.array_locals,
        unsigned_locals: locals.unsigned_locals,
        local_pointee_sizes: locals.local_pointee_sizes,
        local_pointee_unsigned: locals.local_pointee_unsigned,
        local_float_bits: locals.local_float_bits,
        float_locals: locals.float_locals,
        char_params: locals.char_params,
        param_struct_bytes: locals.param_struct_bytes,
        long_params: locals.long_params,
        unsigned_params: locals.unsigned_params,
        param_float_widths: locals.param_float_widths,
        param_pointee_sizes: locals.param_pointee_sizes,
        char_returners: locals.char_returners,
        long_returners: locals.long_returners,
        pascal_fns: locals.pascal_fns,
        static_fns: locals.static_fns,
        far_fns: locals.far_fns,
        variadic_fns: locals.variadic_fns,
        pascal_cleanup: locals.pascal_cleanup,
        si_local: locals.si_local,
        di_local: locals.di_local,
        long_param_funcs: locals.long_param_funcs,
        struct_param_funcs: locals.struct_param_funcs,
        float_param_funcs: locals.float_param_funcs,
        struct_return_funcs: locals.struct_return_funcs,
        float_returners: locals.float_returners,
        loop_stack: locals.loop_stack,
        labels: locals.labels,
        label_fixups: locals.label_fixups,
        backward_labels: locals.backward_labels,
        fpu_live: locals.fpu_live,
        return_float_width: locals.return_float_width,
        return_struct_bytes: locals.return_struct_bytes,
        struct_temp_bss_offset: locals.struct_temp_bss_offset,
        float_call_temp_disp: locals.float_call_temp_disp,
        fpu_pending_fwait: locals.fpu_pending_fwait,
        param_struct_ptr_bytes: locals.param_struct_ptr_bytes,
        int_cast_ptrs: locals.int_cast_ptrs,
        struct_field_temp_base: locals.struct_field_temp_base,
        elide_call_cleanup: std::cell::Cell::new(false),
        ternary_tail_epilogue: std::cell::RefCell::new(None),
        last_branch_barrier: std::cell::Cell::new(0),
        merge_barrier: std::cell::Cell::new(None),
        last_top_stmt: std::cell::Cell::new(false),
        final_top_stmt: std::cell::Cell::new(false),
    };
    let mut body_buf = Vec::new();
    let mut body_fixups: Vec<Fixup> = Vec::new();
    locals.loop_stack.borrow_mut().push(LoopCtx::default());
    // `goto`/early-return label fixups and label definitions emitted INSIDE the
    // body land in the shared label tables with body-buffer-relative offsets.
    // They must be rebased by the same deltas as `body_fixups` so that a goto
    // to a label outside the loop (and vice-versa) resolves against a single
    // out-relative offset space. Snapshot what exists before the body. Fixtures
    // 438/440/442 (goto out of while/for/do-while).
    let lf_start = locals.label_fixups.borrow().len();
    let labels_before: std::collections::HashSet<String> =
        locals.labels.borrow().keys().cloned().collect();
    let rebase_body_labels = |delta: usize| {
        if delta == 0 { return; }
        for (_, pos) in locals.label_fixups.borrow_mut().iter_mut().skip(lf_start) {
            *pos += delta;
        }
        for (name, pos) in locals.labels.borrow_mut().iter_mut() {
            if !labels_before.contains(name) { *pos += delta; }
        }
    };
    // `while (A || B) { ...; if (c) break; }` — when the OR-loop body ENDS with
    // `if (c) break;`, MSC fuses that test into the loop's back edge (loop back
    // when !c instead of an unconditional jmp). The body arrives as a single
    // Block, so unwrap it, check its last statement, and on a match emit the
    // block WITHOUT that trailing `if` and use `c` as the back-edge cond.
    // Fixture 3233.
    let is_break_stmt = |s: &Stmt| matches!(s, Stmt::Break)
        || matches!(s, Stmt::Block(b) if b.len() == 1 && matches!(b[0], Stmt::Break));
    let mut or_break_cond: Option<Cond> = None;
    let mut fused_segments: Option<Vec<&Stmt>> = None;
    if matches!(cond, Cond::Or(_, _)) {
        let inner: Vec<&Stmt> = match body_segments {
            [single] => match single { Stmt::Block(stmts) => stmts.iter().collect(), _ => vec![*single] },
            _ => body_segments.to_vec(),
        };
        if let Some(Stmt::If { cond: c, then_branch, else_branch: None }) = inner.last().copied()
            && is_break_stmt(then_branch)
        {
            or_break_cond = Some(c.clone());
            fused_segments = Some(inner[..inner.len() - 1].to_vec());
        }
    }
    let segs_to_emit: &[&Stmt] = fused_segments.as_deref().unwrap_or(body_segments);
    // For a `for` loop, `continue` jumps to the step segment, not the cond.
    // Record the body-buffer offset at the start of that segment.
    let mut cont_off: Option<usize> = None;
    for (idx, seg) in segs_to_emit.iter().enumerate() {
        if Some(idx) == cont_seg { cont_off = Some(body_buf.len()); }
        emit_stmt(seg, &body_locals, frame, return_int, return_long, &mut body_buf, &mut body_fixups);
    }
    let mut loop_ctx = locals.loop_stack.borrow_mut().pop().expect("loop stack");
    // `while (A || B) { body }` (A,B simple Cmp/Truthy): MSC puts BOTH conds at
    // the top — A jumps forward INTO the body when true, B jumps forward to exit
    // when false (fall into body when true) — then an unconditional `jmp` back to
    // the top. This is a distinct layout from the cond-at-bottom default, so it's
    // handled here with an early return. It only fires where the code previously
    // PANICKED (top-level || in a loop), so it can't regress existing fixtures.
    // Validated against 4163. (Break-fused loop-backs like 3233 are not yet
    // covered — the trailing `if(c)break` keeps the generic jmp here.)
    if let Cond::Or(cond_a, cond_b) = cond {
        use crate::codegen::cond::{emit_cond_cmp, emit_cond_skip, emit_cond_take};
        // Measure A/B (rel8 jcc is fixed-size, so a dummy disp is fine).
        let mut a_buf = Vec::new();
        let mut a_fx: Vec<Fixup> = Vec::new();
        emit_cond_take(cond_a, 0, &body_locals, &mut a_buf, &mut a_fx);
        let mut b_buf = Vec::new();
        let mut b_fx: Vec<Fixup> = Vec::new();
        emit_cond_skip(cond_b, 0, &body_locals, &mut b_buf, &mut b_fx);
        let (la, lb) = (a_buf.len(), b_buf.len());
        let a_start = out.len();
        let body_len = body_buf.len();
        // Back edge: either a fused `if(c) break` (cmp + jcc-back-when-!c) or an
        // unconditional `jmp` back to L_top. The disp byte is `be_disp_pos`.
        let mut be_buf: Vec<u8> = Vec::new();
        let mut be_fx: Vec<Fixup> = Vec::new();
        if let Some(c) = &or_break_cond {
            emit_cond_cmp(c, &body_locals, &mut be_buf, &mut be_fx);
            let back_jcc = match c {
                Cond::Truthy(_) => 0x74, // je: loop back when expr == 0 (break is when != 0)
                Cond::Cmp { op, .. } => {
                    let j = loop_back_jcc(negate_relop(*op));
                    if cmp_is_unsigned(c, locals) { to_unsigned_jcc(j) } else { j }
                }
                _ => unreachable!("break-cond is Cmp/Truthy"),
            };
            be_buf.push(back_jcc);
        } else {
            be_buf.push(0xEB); // jmp short L_top
        }
        let be_disp_pos = be_buf.len();
        be_buf.push(0x00); // disp placeholder
        let be_len = be_buf.len();
        // L_exit (after the back edge) is padded to an even offset.
        let pad = usize::from((a_start + la + lb + body_len + be_len) % 2 != 0);
        // Re-emit A/B with real disps: A jumps to body (disp = lb), B jumps to
        // exit (disp = body_len + back-edge + pad).
        a_buf.clear();
        a_fx.clear();
        emit_cond_take(cond_a, i8::try_from(lb).expect("|| A disp fits"), &body_locals, &mut a_buf, &mut a_fx);
        b_buf.clear();
        b_fx.clear();
        emit_cond_skip(cond_b, i8::try_from(body_len + be_len + pad).expect("|| B disp fits"), &body_locals, &mut b_buf, &mut b_fx);
        // Patch the back-edge disp → L_top (a_start).
        let be_start = a_start + la + lb + body_len;
        let be_disp = a_start as i32 - (be_start as i32 + be_disp_pos as i32 + 1);
        be_buf[be_disp_pos] = i8::try_from(be_disp).expect("|| back-edge disp fits") as u8;
        let base_a = out.len();
        out.extend_from_slice(&a_buf);
        for mut f in a_fx { f.body_offset += base_a; fixups.push(f); }
        let base_b = out.len();
        out.extend_from_slice(&b_buf);
        for mut f in b_fx { f.body_offset += base_b; fixups.push(f); }
        let body_base = out.len();
        out.extend_from_slice(&body_buf);
        for mut f in body_fixups { f.body_offset += body_base; fixups.push(f); }
        rebase_body_labels(body_base);
        let be_base = out.len();
        out.extend_from_slice(&be_buf);
        for mut f in be_fx { f.body_offset += be_base; fixups.push(f); }
        if pad == 1 { out.push(0x90); }
        let loop_end = out.len();
        // break → L_exit; continue → L_top (re-evaluate the condition).
        for &off in &loop_ctx.breaks {
            let abs = body_base + off;
            out[abs] = i8::try_from(loop_end as i32 - (abs as i32 + 1)).expect("break disp fits") as u8;
        }
        for &off in &loop_ctx.continues {
            let abs = body_base + off;
            out[abs] = i8::try_from(a_start as i32 - (abs as i32 + 1)).expect("continue disp fits") as u8;
        }
        return;
    }
    let mut cmp_buf = Vec::new();
    let mut cmp_fixups: Vec<Fixup> = Vec::new();
    // For &&-conditions MSC puts B (right) before the body with a forward
    // exit-jcc, and A (left) after the body as the loop-back cmp section.
    // Layout: jmp→A_check; [nop]; B_check+exit_jcc; body; A_check; jcc_back→B_check.
    if let Cond::And(cond_a, cond_b) = cond {
        // CSE-deref loops cache the shared `*p` (byte → stack temp) and reuse it
        // across ALL conjuncts (see [`cse_deref_while`]). Conjunct 0 → COND
        // (loop-back), conjuncts 1.. → the top-of-loop exit tests. The generic
        // split below emits exactly two conjuncts, each with its own deref/widen.
        let two = two_iter_cse_while(cond);
        let cse = cse_deref_while(cond);
        let mut b_cond_buf: Vec<u8> = Vec::new();
        let mut b_cond_fixups: Vec<Fixup> = Vec::new();
        if let Some(t) = &two {
            // Two-iterator strcmp loop: conjunct 0 (*a) → COND (cmp_buf),
            // conjuncts 1,2 (*b, cmp) → top-of-body tests (b_cond_buf). Caches
            // `*a` at [bp-2], `*b` at [bp-4].
            let temp_a = body_locals.deepest_local_disp() - 2;
            let temp_b = body_locals.deepest_local_disp() - 4;
            emit_two_iter_cond(t, temp_a, &body_locals, &mut cmp_buf);
            let mut exits: Vec<usize> = Vec::new();
            emit_two_iter_loop(t, temp_a, temp_b, &body_locals, &mut b_cond_buf, &mut exits);
            let to_exit = b_cond_buf.len() + body_buf.len() + cmp_buf.len() + 2;
            for pos in exits {
                let disp = i8::try_from(to_exit - (pos + 1)).expect("two-iter cse exit disp fits in i8");
                b_cond_buf[pos] = disp as u8;
            }
        } else if let Some(cw) = &cse {
            let temp = body_locals.deepest_local_disp() - 2;
            emit_cse_cond(cw, temp, &body_locals, &mut cmp_buf);
            let mut exits: Vec<usize> = Vec::new();
            emit_cse_loop(cw, temp, &body_locals, &mut b_cond_buf, &mut exits);
            // Each top-of-loop exit-jcc targets EXIT (loop_end), which sits past
            // b_cond_buf + body + cmp_buf + the 2-byte back-jcc.
            let to_exit = b_cond_buf.len() + body_buf.len() + cmp_buf.len() + 2;
            for pos in exits {
                let disp = i8::try_from(to_exit - (pos + 1)).expect("cse exit disp fits in i8");
                b_cond_buf[pos] = disp as u8;
            }
        } else {
            emit_cond_cmp(cond_a, &body_locals, &mut cmp_buf, &mut cmp_fixups);
            let b_exit_disp = i8::try_from(body_buf.len() + cmp_buf.len() + 2)
                .expect("&&-while B exit disp fits in i8");
            emit_cond_skip(cond_b, b_exit_disp, &body_locals, &mut b_cond_buf, &mut b_cond_fixups);
        }
        let b_cond_len = b_cond_buf.len();
        for c in &mut body_fixups { c.body_offset += b_cond_len; }
        rebase_body_labels(b_cond_len);
        for off in &mut loop_ctx.breaks { *off += b_cond_len; }
        for off in &mut loop_ctx.continues { *off += b_cond_len; }
        if let Some(o) = cont_off.as_mut() { *o += b_cond_len; }
        b_cond_buf.extend_from_slice(&body_buf);
        body_buf = b_cond_buf;
        let mut new_fixups = b_cond_fixups;
        new_fixups.extend(body_fixups.drain(..));
        body_fixups = new_fixups;
    } else {
        // Use body_locals for the cond so that variables mutated inside the
        // loop body are treated as runtime (not folded to their pre-loop init
        // values). Fixture 1309: `while(a[i])` where `i` starts at 0 but is
        // incremented in the body — must NOT fold the index to 0.
        emit_cond_cmp(cond, &body_locals, &mut cmp_buf, &mut cmp_fixups);
    }
    // An `exit_guard` is a second bottom test placed BEFORE the loop-back cmp:
    //   guard-cmp; jcc(!guard) → loop_end; <loop-back cmp>; jcc(cond) → top
    // Used for a break folded into a for-loop's condition, where the original
    // for-cond C becomes the exit guard and `!break` becomes the loop-back.
    if let Some(guard) = exit_guard {
        let Cond::Cmp { op: gop, .. } = guard else {
            panic!("exit_guard must be a Cmp");
        };
        let mut gbuf = Vec::new();
        let mut gfix: Vec<Fixup> = Vec::new();
        emit_cond_cmp(guard, &body_locals, &mut gbuf, &mut gfix);
        // Forward disp from after the guard jcc to loop_end = the loop-back cmp
        // bytes plus the 2-byte back-jcc that follows them.
        let fwd = i8::try_from(cmp_buf.len() + 2).expect("exit-guard forward disp fits");
        gbuf.push(inverted_jcc(*gop)); // jump out when the guard is false
        gbuf.push(fwd as u8);
        let glen = gbuf.len();
        for c in &mut cmp_fixups { c.body_offset += glen; }
        gfix.extend(cmp_fixups.drain(..));
        cmp_fixups = gfix;
        gbuf.extend_from_slice(&cmp_buf);
        cmp_buf = gbuf;
    }

    // Alignment: position right after the 2-byte `eb XX` should be
    // even. If it would be odd, insert a NOP pad and bump the
    // forward jmp displacement by 1.
    let pos_after_jmp = out.len() + 2;
    let needs_pad = pos_after_jmp % 2 != 0;
    let pad = if needs_pad { 1 } else { 0 };

    // The loop-back jcc takes UNSIGNED form (jb/jae/…) when the comparison is
    // unsigned — including a pointer compare like `while (p < end)`. Fixtures
    // 1361 etc. (cmp_is_unsigned now treats pointer operands as unsigned).
    let back_jcc = |c: &Cond, op: RelOp| {
        let j = loop_back_jcc(op);
        if cmp_is_unsigned(c, locals) { to_unsigned_jcc(j) } else { j }
    };
    let jcc_opcode = match cond {
        Cond::Truthy(_) => 0x75,             // jne (back when nonzero)
        Cond::Cmp { op, .. } => back_jcc(cond, *op),
        // A two-iterator strcmp loop's COND is conjunct 0 (`*a` truthy) → jne.
        Cond::And(..) if two_iter_cse_while(cond).is_some() => 0x75,
        // A CSE-deref loop's COND is conjunct 0 of the flattened `&&` chain.
        Cond::And(..) if let Some(cw) = cse_deref_while(cond) =>
            cse_back_jcc(&cw.conjuncts[0]),
        Cond::And(cond_a, _) => match cond_a.as_ref() {
            Cond::Truthy(_) => 0x75,
            Cond::Cmp { op, .. } => back_jcc(cond_a, *op),
            other => panic!("&&-while: A must be Cmp/Truthy, got: {other:?}"),
        },
        Cond::Or(_, _) => panic!("|| in while/do-while not yet supported"),
    };

    let body_len = body_buf.len();
    let cmp_len = cmp_buf.len();
    // Entry-fold peephole: when the cond folds at entry,
    //   `Some(k)`, k != 0 → first iteration runs unconditionally
    //     (no initial jmp; do-while shape). Fixtures 126, 1044.
    //   `Some(0)`        → loop body never runs; elide entirely
    //     (no jmp/body/cmp). Fixture 1587.
    // entry_fold is a pre-computed override from a for-loop init
    // (for-entry_fold is always respected regardless of alignment).
    // For while-loops (entry_fold=None, fold from cond): MSC only
    // applies the do-while optimization when no NOP alignment pad is
    // needed — i.e. the body would already land on an even address.
    // When a pad is needed (out.len() is odd), MSC emits the full
    // while form (jmp + nop) even if the condition is known true.
    // Fixture 1182 (while, two locals, odd body offset → while form),
    // fixture 1078 (for, two locals, odd body offset → do-while, entry_fold).
    let is_for_entry = entry_fold.is_some();
    // For while-loops (no explicit entry_fold), only apply the do-while
    // elision when at least one operand of the condition is a literal.
    // Both-sides-are-locals (`while (a < b)`) never elides; one-literal
    // (`while (i < 10)`) elides when the init value makes it true.
    // The fold reads the ENTRY view (`entry_inits`): a local mutated only
    // inside the loop still folds its declared init here (MSC's do-while
    // elision), even though every other fold reads the slot (3478).
    let effective_fold = entry_fold.or_else(|| {
        if cond_has_literal_side(cond) { fold_cond_raw(cond, locals.entry_inits) } else { None }
    });
    if matches!(effective_fold, Some(0)) && !cond_references_local(cond) {
        return;
    }
    // Empty body: the initial jmp would be a no-op (jmp +0 or +1 with pad);
    // MSC always uses do-while form when the body is empty.
    // Single local: empirically MSC always uses do-while for single-local
    // while-loops when the entry condition folds true (fixtures 124, 1044, 1158).
    // For 2+ locals with an odd-position body, MSC keeps the while form
    // to maintain even loop-top alignment (fixture 1182).
    // A break-folded loop (force_while) always uses the while form (initial jmp)
    // — MSC's do-while opt keys off the syntactic for-cond, which a for(;;)+break
    // loop doesn't have. The empty-body shortcut still applies.
    let skip_initial_jmp = body_len == 0
        || (!force_while
            && matches!(effective_fold, Some(k) if k != 0)
            && (is_for_entry || !needs_pad || locals.disps.len() == 1));
    if !skip_initial_jmp {
        let forward_disp = i8::try_from(body_len + pad)
            .expect("body+pad short enough for jmp rel8");
        out.push(0xEB);
        out.push(forward_disp as u8);
        if needs_pad {
            out.push(0x90);
        }
    }
    let body_base = out.len();
    out.extend_from_slice(&body_buf);
    for mut c in body_fixups {
        c.body_offset += body_base;
        fixups.push(c);
    }
    let cmp_base = out.len();
    out.extend_from_slice(&cmp_buf);
    for mut c in cmp_fixups {
        c.body_offset += cmp_base;
        fixups.push(c);
    }
    let back_disp = -(i8::try_from(cmp_len + 2 + body_len)
        .expect("loop body+cmp short enough for jcc rel8"));
    out.push(jcc_opcode);
    out.push(back_disp as u8);
    let loop_end = out.len();
    // Patch break/continue placeholders (rel8 `eb/jcc XX`). break → loop_end;
    // continue → the for-loop step (cont_off) if present, else cmp_base. Each
    // offset points at the disp byte.
    rebase_body_labels(body_base);
    let cont_target = cont_off.map(|o| body_base + o).unwrap_or(cmp_base);
    for &off in &loop_ctx.breaks {
        let abs = body_base + off;
        let disp = loop_end as i32 - (abs as i32 + 1);
        out[abs] = i8::try_from(disp).expect("break rel8 disp fits") as u8;
    }
    for &off in &loop_ctx.continues {
        let abs = body_base + off;
        let disp = cont_target as i32 - (abs as i32 + 1);
        out[abs] = i8::try_from(disp).expect("continue rel8 disp fits") as u8;
    }
    // Loop with no break/continue: control always falls out through the cmp
    // section, so AX at loop exit is whatever that straight-line trace left
    // there. Leave the AX-reuse barrier inside the loop instead of at its
    // end — a post-loop read can then reuse a value the trace left in AX,
    // with the loop-tail strip in ax_holds_word_operand walking the
    // inc/cmp/or/jcc bytes. Do-while form trusts the whole body tail (the
    // body always runs — fixture 1411); while form only the cmp section,
    // which executes even for zero iterations (fixture 555).
    if loop_ctx.breaks.is_empty()
        && loop_ctx.continues.is_empty()
        && !matches!(cond, Cond::And(..) | Cond::Or(..))
    {
        let barrier = if skip_initial_jmp {
            body_base + body_locals.last_branch_barrier.get()
        } else {
            cmp_base
        };
        locals.merge_barrier.set(Some(barrier));
    }
}
/// `do <body> while (<cond>);` (fixture 4098). When the body's last
/// instruction already sets ZF for the cond, MSC drops the explicit
/// cmp and chains the jcc directly off the body's flags. Today we
/// detect this peephole specifically for the
/// `do <local> = <local> ± 1; while (<same-local>);` shape — the
/// only shape any fixture exercises.
///
/// ```text
/// <body>              body (sets ZF if peephole applies)
/// [<cmp>]             cmp only when peephole doesn't apply
/// <jcc> <-back>       jne/je back to body
/// ```
/// One low-level dispatch test in a switch compare chain.
///
/// Consecutive shared case labels with contiguous ascending values (`case 1:
/// case 2: case 3: body`) lower as a range pair `cmp lo; jl DEFAULT` +
/// `cmp hi; jle BODY` (fixtures 3965, 3084). Non-contiguous shared values get
/// one `je` each, all targeting the shared body (3350). Singleton cases test
/// `cmp v; je BODY` (`or ax,ax` when v == 0).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TestOp {
    JeBody { v: i32, arm: usize },
    JlDefault { v: i32 },
    JleBody { v: i32, arm: usize },
}

/// Lower switch arms into the runtime compare-chain test ops, ordered by
/// ascending case value (out-of-order sources are sorted — fixture 1609); a
/// range pair stays one unit keyed by its low bound.
pub(crate) fn build_chain_ops(cases: &[SwitchArm]) -> Vec<TestOp> {
    let mut groups: Vec<(i32, Vec<TestOp>)> = Vec::new();
    let mut pending: Vec<i32> = Vec::new(); // empty-body shared-label values
    for (i, arm) in cases.iter().enumerate() {
        let Some(v) = arm.value else {
            // `case 5: default: body` — labels stacked on the default arm
            // dispatch straight to it.
            for &pv in &pending {
                groups.push((pv, vec![TestOp::JeBody { v: pv, arm: i }]));
            }
            pending.clear();
            continue;
        };
        if arm.body.is_empty() && i + 1 < cases.len() {
            pending.push(v);
            continue;
        }
        let contiguous = !pending.is_empty()
            && pending.windows(2).all(|w| w[1] == w[0] + 1)
            && v == pending[pending.len() - 1] + 1;
        if contiguous {
            groups.push((
                pending[0],
                vec![
                    TestOp::JlDefault { v: pending[0] },
                    TestOp::JleBody { v, arm: i },
                ],
            ));
        } else {
            for &pv in &pending {
                groups.push((pv, vec![TestOp::JeBody { v: pv, arm: i }]));
            }
            groups.push((v, vec![TestOp::JeBody { v, arm: i }]));
        }
        pending.clear();
    }
    groups.sort_by_key(|&(key, _)| key);
    groups.into_iter().flat_map(|(_, ops)| ops).collect()
}

/// Whether any arm body contains a top-level `break` (the only kind the
/// switch emitters treat as a switch exit).
pub(crate) fn switch_has_break(cases: &[SwitchArm]) -> bool {
    cases.iter().any(|a| a.body.iter().any(|s| matches!(s, Stmt::Break)))
}

/// Whether a statement, as emitted, completes by FALLING THROUGH off its
/// physical end (the next instruction is reached straight-line), as opposed to
/// terminating (`return`) or ending in a jump (a loop back-edge, a nested
/// switch's no-match jmp, an `if` whose physically-last branch leaves). Drives
/// the partial-switch continuation layout (DIRECT vs FWD): a first case body
/// that completes normally is placed right before the continuation so it falls
/// through (FWD); one that always returns/jumps to its break goes after the
/// continuation (DIRECT). Oracle-probed on no-default nested switches
/// (fixtures 3277, probes B/C/E/F/G).
fn stmt_ends_with_fallthrough(s: &Stmt) -> bool {
    match s {
        Stmt::ExprStmt(_) | Stmt::Assign { .. } | Stmt::Empty | Stmt::Label(_)
        | Stmt::Continue => true,
        Stmt::Return(_) | Stmt::Goto(_) | Stmt::Break => false,
        // A loop's normal exit fuses to the following break target (its exit
        // jcc jumps straight there); the physical tail is the back-edge jmp.
        Stmt::While { .. } | Stmt::For { .. } | Stmt::DoWhile { .. } => false,
        // A nested switch ends in its no-match jmp / a terminating last body.
        Stmt::Switch { .. } => false,
        Stmt::Block(v) => v.last().is_none_or(stmt_ends_with_fallthrough),
        // An `if` falls through iff its physically-last branch does (no else →
        // the then-branch; with else → either branch reaches the merge point).
        Stmt::If { then_branch, else_branch, .. } => match else_branch {
            None => stmt_ends_with_fallthrough(then_branch),
            Some(e) => stmt_ends_with_fallthrough(then_branch)
                || stmt_ends_with_fallthrough(e),
        },
    }
}

/// Whether a case body reaches its trailing `break` (or fall-out) by straight-
/// line fall-through, i.e. its physically-last statement completes normally.
fn case_body_falls_through(body: &[Stmt]) -> bool {
    let main = match body.last() {
        Some(Stmt::Break) => &body[..body.len() - 1],
        _ => body,
    };
    main.last().is_none_or(stmt_ends_with_fallthrough)
}

/// True when at least one arm's body falls through into the NEXT arm (no
/// trailing `break`/`return`), i.e. the cases form a C cascading fall-through
/// chain rather than self-contained arms. MSC lays such a switch out as one
/// contiguous source-order block (the plain `emit_runtime_switch`
/// default-fallthrough layout), not the FWD partial-continuation layout that
/// each-arm-breaks shapes use. Fixture 4206 (cascading `case HIGH/MID/LOW`)
/// vs 4167 (each arm has its own break — no cascade).
pub(crate) fn switch_cases_cascade(cases: &[SwitchArm]) -> bool {
    cases.iter().take(cases.len().saturating_sub(1)).any(|arm| {
        // No explicit break and the body completes by falling through →
        // control reaches the following arm's label.
        !arm.body.iter().any(|s| matches!(s, Stmt::Break))
            && case_body_falls_through(&arm.body)
    })
}

/// A case body of the shape `[stmt*, Switch, Break]` — a nested switch as the
/// last statement, ended by a `break`. Returns (leading stmts, the nested
/// switch's scrutinee+cases). In the partial-switch DIRECT layout MSC fuses
/// such a nested switch's no-match jmp straight to the outer continuation (RL)
/// instead of double-jumping through a separate break jmp. Fixture 3277.
fn switch_tail_body(body: &[Stmt]) -> Option<(&[Stmt], &Expr, &[SwitchArm])> {
    let (tail, main) = body.split_last()?;
    if !matches!(tail, Stmt::Break) { return None; }
    let (last, lead) = main.split_last()?;
    let Stmt::Switch { scrutinee, cases } = last else { return None };
    // A jump-table switch lowers differently — leave it on the buffer path.
    if switch_is_table(cases) { return None; }
    Some((lead, scrutinee, cases))
}

/// Whether control entering `cases[start]`'s body always returns before
/// leaving the switch (walking C fall-through across arms; a `break` — or
/// running off the end — reaches the post-switch continuation instead).
pub(crate) fn arm_body_terminates(cases: &[SwitchArm], start: usize, locals: &Locals<'_>) -> bool {
    for arm in &cases[start..] {
        for s in &arm.body {
            if matches!(s, Stmt::Break) { return false; }
            if stmt_always_returns(s, locals) { return true; }
        }
    }
    false
}

/// Result of resolving a chain against a compile-time scrutinee K.
pub(crate) struct ChainFold {
    /// Surviving tests (the prefix walked before any truncation, minus deletions).
    pub ops: Vec<TestOp>,
    /// `Some(arm)` when a test resolved TAKEN: control falls into that arm's
    /// body right after the surviving tests; everything later is dead.
    pub fallin_arm: Option<usize>,
}

/// MSC's late value-tracking fold over the chain, with AX known == K. Only a
/// compare against the EXACT known constant resolves (the flags are then
/// "equal"): a not-taken jcc (`jl` with v == K) is deleted outright, a taken
/// jcc (`je`/`jle` with v == K) truncates the chain — the matched body falls
/// in and the rest is dead. Compares against any OTHER constant are kept
/// (`cmp ax,3; jle` with K == 2 stays a runtime test — fixture 3965). The
/// case-0 `or ax,ax; je` resolves for any known K (1601/1603/1609).
pub(crate) fn fold_chain_ops(ops: Vec<TestOp>, k: i32) -> ChainFold {
    let mut out: Vec<TestOp> = Vec::new();
    for op in ops {
        match op {
            TestOp::JlDefault { v } if v == k => {} // equal → jl never taken
            TestOp::JeBody { v, arm } | TestOp::JleBody { v, arm } if v == k => {
                return ChainFold { ops: out, fallin_arm: Some(arm) };
            }
            TestOp::JeBody { v, .. } if v == 0 && k != 0 => {} // or ax,ax; je
            other => out.push(other),
        }
    }
    ChainFold { ops: out, fallin_arm: None }
}

/// Partial switch for a const-scrutinee k with NFC cases (V != 0, V < k signed).
///
/// MSC does NOT fold the whole switch to just the matched case body. Instead it
/// emits a comparison chain for NFC cases, the fallthrough (matched / default)
/// body, then a forward-jmp layout:
///
/// FWD path (no `case 0` in switch, or k == 0):
///   [mov ax,k] [cmp chain] [fallthrough] [eb→RL] [nop?] [NFC[0] body]
///   ← return_label (RL) here →
///   [continuation] [nop?] [NFC[1] body][eb→RL][nop?] …
///
/// DIRECT path (switch has `case 0` and k != 0):
///   [mov ax,k] [cmp chain] [fallthrough]
///   ← return_label (RL) here →
///   [continuation] [nop?] [NFC[0] body][eb→RL][nop?] [NFC[1] body][eb→RL][nop?] …
///
/// Nops align each NFC body start to an even code-segment offset.
/// Returns true if the code following the continuation is still reachable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_partial_switch_with_continuation(
    scrutinee: &Expr,
    known_k: Option<i32>,
    fold: &ChainFold,
    ft_idx: Option<usize>,
    cases: &[SwitchArm],
    continuation: &[Stmt],
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
    // When `Some(l_top)`, this switch is the body of a test-at-top loop: the
    // continuation is the loop tail, and instead of a function epilogue MSC
    // emits a backward `jmp l_top` woven in right after it (before the trailing
    // case bodies). Fixture 3234.
    loop_back: Option<usize>,
) -> bool {

    // Bodies referenced by the surviving tests, in op order, deduped. A
    // surviving `jl` half targets the default body when one exists (appended
    // here), or the continuation (RL) when there is none (fixture 520).
    // Tests targeting the fall-through arm itself resolve to its inline copy
    // right after the chain — it is never re-emitted as a trailing body
    // (fixture 2224: `jl default` with ft == default).
    let mut nfc_idxs: Vec<usize> = Vec::new();
    for op in &fold.ops {
        if let TestOp::JeBody { arm, .. } | TestOp::JleBody { arm, .. } = op
            && Some(*arm) != ft_idx
            && !nfc_idxs.contains(arm)
        {
            nfc_idxs.push(*arm);
        }
    }
    let has_jl = fold.ops.iter().any(|op| matches!(op, TestOp::JlDefault { .. }));
    let jl_default_arm: Option<usize> = if has_jl {
        cases.iter().position(|a| a.value.is_none())
    } else {
        None
    };
    if let Some(d) = jl_default_arm
        && Some(d) != ft_idx
        && !nfc_idxs.contains(&d)
    {
        nfc_idxs.push(d);
    }
    let jl_to_cont = has_jl && jl_default_arm.is_none();

    // Pre-emit a case body starting at arm `start` (walking C fall-through
    // across arm boundaries, stopping at Break), returning (bytes, fixups,
    // terminates). `known_ax` is the value AX is known to hold at the body's
    // entry (the scrutinee constant, for the fall-through arm): a leading
    // `return K` of that same value skips its `mov ax,K` — AX already holds
    // it. Fixture 3971.
    let emit_arm = |start: Option<usize>, known_ax: Option<i32>| -> (Vec<u8>, Vec<Fixup>, bool) {
        let mut b: Vec<u8> = Vec::new();
        let mut f: Vec<Fixup> = Vec::new();
        let mut term = false;
        let mut first = true;
        let Some(start) = start else { return (b, f, term) };
        'outer: for arm in &cases[start..] {
            for s in &arm.body {
                if matches!(s, Stmt::Break) { break 'outer; }
                if first
                    && return_int && !return_long
                    && let Stmt::Return(Expr::IntLit(v)) = s
                    && known_ax == Some(*v)
                {
                    crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, &mut b);
                    term = true;
                    break 'outer;
                }
                first = false;
                emit_stmt(s, locals, frame, return_int, return_long, &mut b, &mut f);
                if stmt_always_returns(s, locals) { term = true; break 'outer; }
            }
        }
        (b, f, term)
    };

    let (ft_bytes, ft_fxs, ft_term) = emit_arm(ft_idx, known_k);
    let nfc_bufs: Vec<(Vec<u8>, Vec<Fixup>, bool)> =
        nfc_idxs.iter().map(|&ai| emit_arm(Some(ai), None)).collect();

    // need_cont: true when any body doesn't terminate, or a `jl` test reaches
    // the continuation directly (→ continuation needed). A loop body always
    // needs its tail + loop-back emitted.
    let need_cont = loop_back.is_some()
        || !ft_term || nfc_bufs.iter().any(|(_, _, t)| !t) || jl_to_cont;

    // Pre-emit continuation (only when needed).
    let mut cont_bytes: Vec<u8> = Vec::new();
    let mut cont_fxs: Vec<Fixup> = Vec::new();
    if need_cont {
        let mut reachable = true;
        for s in continuation {
            if !reachable { break; }
            emit_stmt(s, locals, frame, return_int, return_long,
                      &mut cont_bytes, &mut cont_fxs);
            if stmt_always_returns(s, locals) { reachable = false; }
        }
        // A loop tail loops back rather than returning — the back-jmp is woven
        // in by the FWD layout below, so no function epilogue here.
        if reachable && loop_back.is_none() {
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, &mut cont_bytes);
        }
    }

    // ── emit scrutinee load ───────────────────────────────────────────────

    match known_k {
        Some(0) => out.extend_from_slice(&[0x2B, 0xC0]), // sub ax, ax
        Some(k) => {
            let k16 = (k as u32 & 0xFFFF) as u16;
            let [lo, hi] = k16.to_le_bytes();
            out.extend_from_slice(&[0xB8, lo, hi]); // mov ax, k
        }
        None => emit_expr_to_ax(scrutinee, locals, out, fixups),
    }

    // ── emit comparison chain (jcc placeholders) ──────────────────────────

    // Each pending patch: (rel8 placeholder position, target arm index or
    // None = continuation / RL).
    let mut jcc_patches: Vec<(usize, Option<usize>)> = Vec::new();
    for op in &fold.ops {
        let (v, jcc, target) = match *op {
            TestOp::JeBody { v, arm } => (v, 0x74u8, Some(arm)),
            TestOp::JleBody { v, arm } => (v, 0x7E, Some(arm)),
            TestOp::JlDefault { v } => (v, 0x7C, jl_default_arm),
        };
        if v == 0 && matches!(op, TestOp::JeBody { .. }) {
            out.extend_from_slice(&[0x0B, 0xC0]); // or ax, ax
        } else {
            let v16 = (v as u32 & 0xFFFF) as u16;
            let [vlo, vhi] = v16.to_le_bytes();
            out.extend_from_slice(&[0x3D, vlo, vhi]); // cmp ax, v
        }
        out.push(jcc);
        jcc_patches.push((out.len(), target)); // index of rel8 placeholder
        out.push(0x00);
    }

    // ── emit ft body ─────────────────────────────────────────────────────

    let ft_start = out.len();
    {
        let base = out.len();
        for mut f in ft_fxs { f.body_offset += base; fixups.push(f); }
        out.extend_from_slice(&ft_bytes);
    }

    // MSC uses three distinct layouts:
    //
    // CASE 1 – no continuation (all bodies terminate):
    //   [ft+nop_if_odd] [NFC[0]+nop_if_odd] ... [NFC[n]+nop_if_odd]
    //
    // CASE 2 FWD – continuation needed, no `case 0` in switch (or K==0):
    //   [ft] [ft_jmp→RL] [nop_if_odd]
    //   [NFC[0]]                       ← falls through to RL
    //   RL: [continuation]
    //   [NFC[1]+jmp→RL+nop_if_odd] ...
    //
    // CASE 2 DIRECT – continuation needed, `case 0` exists AND K != 0:
    //   [ft]                           ← falls through directly to RL
    //   RL: [continuation]
    //   [nop_if_odd]
    //   [NFC[0]+jmp→RL+nop_if_odd] ... [NFC[n]+jmp→RL+nop_if_odd]

    let has_zero_case = known_k.is_some_and(|k| k != 0)
        && cases.iter().any(|a| a.value == Some(0));
    // Continuation-layout choice for a runtime scrutinee:
    //   FWD    — the first surviving case body is placed before the continuation
    //            so it falls through, eliding its break.
    //   DIRECT — the chain fall-through reaches the continuation inline, all
    //            case bodies after it (each jumping back).
    // With a default arm the default body is the chain fall-through, so the
    // historical break-free rule applies (2641). With NO default, MSC lays the
    // continuation inline right after the dispatch chain (DIRECT) in exactly two
    // oracle-confirmed shapes (else it places the continuation after the first
    // falling-through case body — FWD — 454/4166-a):
    //   • a break-free switch (its bodies fall to the continuation C-style), or
    //   • a TWO-body switch whose FIRST body is a "jump-out" — it neither
    //     returns nor falls through, but reaches the continuation via a jump (a
    //     nested switch / loop / `if` whose tail leaves). Probed: [J,T] 3277,
    //     [J,F] / [J,J] DIRECT; vs [T,F] (first terminates) and any 3-body
    //     shape ([J,T,F]/[J,T,T]/…) → FWD.
    // The loop-back form keeps FWD: its backward jmp is woven only into FWD (3234).
    let nfc0_is_jumpout = nfc_idxs.first().is_some_and(|&i|
        !case_body_falls_through(&cases[i].body)
            && !arm_body_terminates(cases, i, locals));
    let direct_path = need_cont
        && (has_zero_case
            || (known_k.is_none() && match ft_idx {
                None => !switch_has_break(cases)
                    || (nfc_idxs.len() == 2 && nfc0_is_jumpout && loop_back.is_none()),
                Some(_) => !switch_has_break(cases),
            }));
    let fwd_path = need_cont && !direct_path;

    // arm index → body start offset, filled as bodies are laid out.
    let mut nfc_starts: Vec<(usize, usize)> = Vec::new();
    let mut return_label: Option<usize> = None;

    if !need_cont {
        // CASE 1: all bodies terminate.
        if out.len() % 2 != 0 { out.push(0x90); }
        for (bi, (nfc_bytes, nfc_fxs, _)) in nfc_bufs.iter().enumerate() {
            nfc_starts.push((nfc_idxs[bi], out.len()));
            let base = out.len();
            for mut f in nfc_fxs.iter().cloned() { f.body_offset += base; fixups.push(f); }
            out.extend_from_slice(nfc_bytes);
            if out.len() % 2 != 0 { out.push(0x90); }
        }
    } else if fwd_path {
        // CASE 2 FWD: the continuation (RL) sits right after the FIRST case body
        // that falls through to it (or after the last body when none does).
        // Bodies BEFORE RL reach it by falling through (the fall-through body
        // itself) or by a forward jump (a terminating body just returns; a
        // jump-out body — nested switch / loop / `if` whose tail leaves —
        // jumps forward, with nested switches fused straight to RL). Bodies
        // AFTER RL jump back to it. With NFC[0] the fall-through body this is
        // the classic 454/4166-a / loop (3234) shape; a later fall-through body
        // pushes RL toward the end (fixture 4173). A terminating ft body needs
        // no chain jmp (fixture 520).
        let ft_jmp_patch = if !ft_term {
            out.push(0xEB);
            let p = out.len();
            out.push(0x00);
            Some(p)
        } else {
            None
        };
        if out.len() % 2 != 0 { out.push(0x90); }

        let n = nfc_idxs.len();
        let ff = nfc_idxs.iter().position(|&i| case_body_falls_through(&cases[i].body));
        let rl_after = ff.unwrap_or(n.saturating_sub(1));

        // Forward fall-out jumps (the chain jmp, and the fall-out jumps of
        // bodies emitted BEFORE the continuation) are recorded and patched to RL
        // once it is known — RL is simply the offset reached after the last
        // before-RL body, so no size pre-computation is needed.
        let mut fwd_patches: Vec<usize> = ft_jmp_patch.into_iter().collect();

        // Bodies before RL (0..=rl_after). The fall-through body (rl_after, when
        // some body falls through) is placed last with no jmp and no trailing
        // nop — RL follows it directly.
        for bi in 0..if n > 0 { rl_after + 1 } else { 0 } {
            nfc_starts.push((nfc_idxs[bi], out.len()));
            let term = nfc_bufs[bi].2;
            let falls_through = bi == rl_after && ff.is_some();
            if !term
                && let Some((lead, ss, sc)) = switch_tail_body(&cases[nfc_idxs[bi]].body)
            {
                for s in lead { emit_stmt(s, locals, frame, return_int, return_long, out, fixups); }
                let sk = if let Expr::IntLit(k) = ss { Some(*k) } else { None };
                // Placeholder fall-out target; positions land in fwd_patches.
                emit_runtime_switch(ss, sk, sc, locals, frame, return_int, return_long, out, fixups, Some(0), &mut fwd_patches);
            } else {
                let base = out.len();
                for mut f in nfc_bufs[bi].1.iter().cloned() { f.body_offset += base; fixups.push(f); }
                out.extend_from_slice(&nfc_bufs[bi].0);
                if !term && !falls_through {
                    out.push(0xEB);
                    fwd_patches.push(out.len());
                    out.push(0x00);
                }
            }
            if bi != rl_after && out.len() % 2 != 0 { out.push(0x90); }
        }

        // return_label: emit continuation.
        let rl = out.len();
        return_label = Some(rl);
        {
            let base = out.len();
            for mut f in cont_fxs { f.body_offset += base; fixups.push(f); }
            out.extend_from_slice(&cont_bytes);
        }

        // Loop tail: weave the backward jmp to the loop top right after the
        // continuation, before the trailing case bodies (fixture 3234).
        if let Some(l_top) = loop_back {
            out.push(0xEB);
            let p = out.len();
            out.push(0x00);
            out[p] = (l_top as i32 - (p + 1) as i32) as u8;
        }

        // Bodies after RL (rl_after+1..): each starts even and jumps back to RL
        // (RL now known, so switch-tail fusion bakes its jumps directly).
        for bi in (rl_after + 1)..n {
            if out.len() % 2 != 0 { out.push(0x90); }
            nfc_starts.push((nfc_idxs[bi], out.len()));
            let term = nfc_bufs[bi].2;
            if !term
                && let Some((lead, ss, sc)) = switch_tail_body(&cases[nfc_idxs[bi]].body)
            {
                for s in lead { emit_stmt(s, locals, frame, return_int, return_long, out, fixups); }
                let sk = if let Expr::IntLit(k) = ss { Some(*k) } else { None };
                emit_runtime_switch(ss, sk, sc, locals, frame, return_int, return_long, out, fixups, Some(rl), &mut Vec::new());
            } else {
                let base = out.len();
                for mut f in nfc_bufs[bi].1.iter().cloned() { f.body_offset += base; fixups.push(f); }
                out.extend_from_slice(&nfc_bufs[bi].0);
                if !term {
                    out.push(0xEB);
                    let p = out.len();
                    out.push(0x00);
                    out[p] = (rl as i32 - (p + 1) as i32) as u8;
                }
            }
            if out.len() % 2 != 0 { out.push(0x90); }
        }

        // Patch all forward fall-out jumps → return_label.
        for p in fwd_patches {
            out[p] = (rl as i32 - (p + 1) as i32) as u8;
        }
    } else {
        // CASE 2 DIRECT: ft body falls through to RL; all NFCs after continuation.
        debug_assert!(direct_path);

        // return_label immediately after ft body.
        let rl = out.len();
        return_label = Some(rl);
        {
            let base = out.len();
            for mut f in cont_fxs { f.body_offset += base; fixups.push(f); }
            out.extend_from_slice(&cont_bytes);
        }

        // Alignment nop before first NFC body.
        if out.len() % 2 != 0 { out.push(0x90); }

        // All NFC bodies: each has backward jmp → return_label + nop. A body
        // ending in a nested switch (`[…, switch, break]`) is re-emitted
        // DIRECTLY so the nested switch's no-match fall-out targets RL itself
        // (fallout_override) — MSC fuses that edge instead of double-jumping
        // through a separate trailing `jmp RL`. Fixture 3277.
        for (bi, (nfc_bytes, nfc_fxs, nfc_term)) in nfc_bufs.iter().enumerate() {
            nfc_starts.push((nfc_idxs[bi], out.len()));
            if !nfc_term
                && let Some((lead, sub_scrut, sub_cases)) = switch_tail_body(&cases[nfc_idxs[bi]].body)
            {
                for s in lead {
                    emit_stmt(s, locals, frame, return_int, return_long, out, fixups);
                }
                let sub_k = if let Expr::IntLit(k) = sub_scrut { Some(*k) } else { None };
                emit_runtime_switch(sub_scrut, sub_k, sub_cases, locals, frame,
                                    return_int, return_long, out, fixups, Some(rl), &mut Vec::new());
            } else {
                let base = out.len();
                for mut f in nfc_fxs.iter().cloned() { f.body_offset += base; fixups.push(f); }
                out.extend_from_slice(nfc_bytes);
                if !nfc_term {
                    out.push(0xEB);
                    let p = out.len();
                    out.push(0x00);
                    let rel8 = rl as i32 - (p + 1) as i32;
                    out[p] = rel8 as u8;
                }
            }
            if out.len() % 2 != 0 { out.push(0x90); }
        }
    }

    // Resolve chain jcc targets: arm bodies by layout position; `jl` with no
    // default arm targets the continuation (RL).
    for &(p, target) in &jcc_patches {
        let dest = match target {
            Some(arm) if Some(arm) == ft_idx => ft_start,
            Some(arm) => nfc_starts.iter().find(|&&(a, _)| a == arm)
                .map(|&(_, s)| s)
                .expect("chain test targets an unemitted body"),
            None => return_label.expect("jl-to-continuation requires a continuation"),
        };
        out[p] = (dest as i32 - (p + 1) as i32) as u8;
    }

    false // caller must not add another epilogue (handled internally)
}
/// Whether a switch lowers to a jump table: at least 7 distinct singleton
/// case values forming an exactly-dense range. 6 dense cases stay a compare
/// chain (3971/1603/4031); 8 and 10 dense go through the table (158/1898/
/// 2337/3480/4027). The table is never const-folded — MSC can't track the
/// scrutinee through the indirect jmp (1898 keeps the full table with x
/// known).
pub(crate) fn switch_is_table(cases: &[SwitchArm]) -> bool {
    let vals: Vec<i32> = cases.iter().filter_map(|a| a.value).collect();
    if vals.len() < 7 {
        return false;
    }
    // Shared labels (an empty-bodied value arm) keep the chain lowering.
    if cases.iter().any(|a| a.value.is_some() && a.body.is_empty()) {
        return false;
    }
    let mut sorted = vals.clone();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.len() == vals.len()
        && (i64::from(sorted[sorted.len() - 1]) - i64::from(sorted[0]) + 1)
            == vals.len() as i64
}

/// Jump-table switch (fixtures 158/1898/2337/3480/4027):
///
///   [scrutinee→ax] [sub ax,lo]? cmp ax,hi-lo; ja DEFAULT-or-END;
///   add ax,ax; xchg ax,bx; jmp WORD PTR cs:$Ltab[bx]; [nop?]
///   bodies (source order, each nop-padded to even):
///     - break bodies jmp to the post-switch fall-out (1898/158)
///     - `return e` bodies NOT among the last 5 (default counts as a body)
///       load the value and jmp to the shared exit epilogue $EX
///       (EPILOGUE_LABEL); the last 5 inline their epilogue (2337/3480/4027)
///   $Ltab: DW per value — _TEXT-relative body offsets (each word carries a
///   `c4 off 8e` FIXUP; the dispatch disp16 a `c4 off 5e` one)
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_table_switch(
    scrutinee: &Expr,
    cases: &[SwitchArm],
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    let default_idx = cases.iter().position(|c| c.value.is_none());
    let lo = cases.iter().filter_map(|a| a.value).min().unwrap();
    let hi = cases.iter().filter_map(|a| a.value).max().unwrap();

    // The epilogue byte pattern, used to convert an inline `return e` into
    // the shared-exit jmp form (strip the tail, append `jmp $EX`).
    let mut epi: Vec<u8> = Vec::new();
    crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, &mut epi);

    emit_expr_to_ax(scrutinee, locals, out, fixups);
    if lo != 0 {
        out.push(0x2D); // sub ax, lo
        out.extend_from_slice(&((lo as u32 & 0xFFFF) as u16).to_le_bytes());
    }
    out.push(0x3D); // cmp ax, hi-lo
    out.extend_from_slice(&(((hi - lo) as u32 & 0xFFFF) as u16).to_le_bytes());
    out.push(0x77); // ja → default body / post-switch
    let ja_patch = out.len();
    out.push(0x00);
    out.extend_from_slice(&[0x03, 0xC0]); // add ax, ax
    out.push(0x93); // xchg ax, bx
    out.extend_from_slice(&[0x2E, 0xFF, 0xA7]); // jmp word cs:[bx+disp16]
    let table_disp_pos = out.len();
    out.extend_from_slice(&[0x00, 0x00]);
    fixups.push(Fixup { body_offset: table_disp_pos, kind: FixupKind::SwitchTableJmp });
    if out.len() % 2 != 0 { out.push(0x90); }

    // Bodies in source order. `breaks` collect (placeholder pos) for the
    // post-switch patch; body starts recorded per arm for the table words.
    let mut body_starts: Vec<usize> = vec![0; cases.len()];
    let mut break_patches: Vec<usize> = Vec::new();
    for (i, arm) in cases.iter().enumerate() {
        body_starts[i] = out.len();
        let from_end = cases.len() - 1 - i;
        // Early `return e` body → value load + jmp $EX (shared exit).
        if from_end >= 5
            && return_int && !return_long
            && arm.body.len() == 1
            && let Stmt::Return(_) = &arm.body[0]
        {
            let mut buf: Vec<u8> = Vec::new();
            let mut fxs: Vec<Fixup> = Vec::new();
            emit_stmt(&arm.body[0], locals, frame, return_int, return_long, &mut buf, &mut fxs);
            assert!(buf.ends_with(&epi), "return body must end with the epilogue");
            buf.truncate(buf.len() - epi.len());
            let base = out.len();
            for mut f in fxs { f.body_offset += base; fixups.push(f); }
            out.extend_from_slice(&buf);
            out.push(0xEB);
            let pos = out.len();
            out.push(0x00);
            locals.label_fixups.borrow_mut().push((EPILOGUE_LABEL.to_owned(), pos));
        } else {
            for s in &arm.body {
                if matches!(s, Stmt::Break) {
                    out.push(0xEB);
                    break_patches.push(out.len());
                    out.push(0x00);
                    break;
                }
                emit_stmt(s, locals, frame, return_int, return_long, out, fixups);
                if stmt_always_returns(s, locals) { break; }
            }
        }
        if out.len() % 2 != 0 { out.push(0x90); }
    }

    // The table itself: one word per value, ascending from lo.
    let table_off = out.len();
    for v in lo..=hi {
        let arm = cases.iter().position(|a| a.value == Some(v))
            .expect("dense table covers every value");
        fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::SwitchTableWord });
        out.extend_from_slice(&(body_starts[arm] as u16).to_le_bytes());
    }

    // Patch the dispatch disp16 with the table's function-relative offset
    // (lib.rs rebases it to the segment and emits the OMF fixup).
    out[table_disp_pos] = (table_off & 0xFF) as u8;
    out[table_disp_pos + 1] = ((table_off >> 8) & 0xFF) as u8;

    // `ja` and break jmps target the default body / post-switch fall-out.
    let switch_end = out.len();
    let ja_target = default_idx.map(|d| body_starts[d]).unwrap_or(switch_end);
    out[ja_patch] = (ja_target as i32 - (ja_patch + 1) as i32) as u8;
    for p in break_patches {
        out[p] = (switch_end as i32 - (p + 1) as i32) as u8;
    }
}

/// Runtime `switch (<scrutinee>) { case K0: body0 case K1: body1 default: ... }`.
/// Matches MSC layout: short cmp+jcc chain (with `jl`/`jle` range pairs for
/// contiguous shared-label runs); `jmp short; nop` fallthrough; non-default
/// bodies each get a trailing NOP; default body goes last with no trailing NOP.
///
/// `known_k` is the compile-time scrutinee value when the chain survives the
/// const fold WITHOUT truncating (no test compares against exactly K): the
/// only effect is deletion of resolved-not-taken tests — a `jl` half whose
/// `cmp` constant equals K (fixtures 1895/2620/3967) and a case-0 `or ax,ax;
/// je` when K != 0.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_runtime_switch(
    scrutinee: &Expr,
    known_k: Option<i32>,
    cases: &[SwitchArm],
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
    // When `Some(rl)`, this switch is the tail of an enclosing partial-switch
    // case body: its no-match fall-out and its case `break`s target the outer
    // continuation `rl` (an absolute _TEXT offset) DIRECTLY rather than the
    // local post-switch position — MSC fuses the two edges, avoiding a
    // double-jump through a separate break jmp. Fixture 3277. The rel8 byte of
    // every such fall-out jump is recorded in `fallout_jmps` so a caller that
    // doesn't yet know `rl` (a FORWARD reference, e.g. a body emitted before the
    // continuation in the FWD layout) can pass a placeholder and patch them
    // afterwards (fixture 4173).
    fallout_override: Option<usize>,
    fallout_jmps: &mut Vec<usize>,
) {
    if switch_is_table(cases) {
        emit_table_switch(scrutinee, cases, locals, frame, return_int, return_long, out, fixups);
        return;
    }

    struct CaseBuf {
        buf: Vec<u8>,
        fixups: Vec<Fixup>,
        breaks: Vec<usize>, // offsets of `eb XX` short-jmp placeholders
        term: bool,
    }
    let mut case_bufs: Vec<CaseBuf> = Vec::new();
    for arm in cases {
        let mut buf = Vec::new();
        let mut fxs: Vec<Fixup> = Vec::new();
        let mut breaks: Vec<usize> = Vec::new();
        let mut term = false;
        for s in &arm.body {
            if matches!(s, Stmt::Break) {
                let off = buf.len();
                buf.extend_from_slice(&[0xEB, 0x00]); // short jmp placeholder (2 bytes)
                breaks.push(off);
            } else {
                emit_stmt(s, locals, frame, return_int, return_long, &mut buf, &mut fxs);
                if stmt_always_returns(s, locals) { term = true; break; }
            }
        }
        case_bufs.push(CaseBuf { buf, fixups: fxs, breaks, term });
    }

    let mut scrutinee_buf = Vec::new();
    let mut scrutinee_fixups: Vec<Fixup> = Vec::new();
    emit_expr_to_ax(scrutinee, locals, &mut scrutinee_buf, &mut scrutinee_fixups);

    // Empty switch: MSC emits only the scrutinee load (side-effect) and
    // falls through — no cmp chain, no jmp, no NOP. Fixture 2893.
    if cases.is_empty() {
        let start = out.len();
        for mut f in scrutinee_fixups { f.body_offset += start; fixups.push(f); }
        out.extend_from_slice(&scrutinee_buf);
        return;
    }

    let default_idx = cases.iter().position(|c| c.value.is_none());
    let any_breaks = case_bufs.iter().any(|cb| !cb.breaks.is_empty());

    let start = out.len();

    // When every case is a default (no value-case comparisons needed), MSC
    // omits the scrutinee load entirely — there is nothing to compare against.
    // Fixture 3673 (single default: return -1).
    let has_nondefault = cases.iter().any(|c| c.value.is_some());
    let scr_len = if has_nondefault { scrutinee_buf.len() } else { 0 };
    if has_nondefault {
        for mut f in scrutinee_fixups { f.body_offset += start; fixups.push(f); }
        out.extend_from_slice(&scrutinee_buf);
    }

    // Single-case no-default with no breaks: JNE layout.
    // MSC uses `jne → continuation` so the matching case body comes first,
    // then the continuation is emitted by the caller. E.g. fixture 3082.
    if default_idx.is_none() && cases.len() == 1 && !any_breaks {
        let ci = 0;
        let arm_val = cases[ci].value.unwrap();
        let k16 = (arm_val as u32 & 0xFFFF) as u16;
        if arm_val == 0 {
            out.extend_from_slice(&[0x0B, 0xC0]); // or ax, ax
        } else {
            out.push(0x3D);
            out.extend_from_slice(&k16.to_le_bytes());
        }
        // jne → cont (skips case body + nop)
        let body_size = case_bufs[ci].buf.len();
        let body_nop = if (start + scr_len + (if arm_val == 0 { 2 } else { 3 }) + 2 + body_size) % 2 != 0 { 1usize } else { 0 };
        out.push(0x75); // jne
        out.push((body_size + body_nop) as u8);
        // case body
        let body_base = out.len();
        for mut f in case_bufs[ci].fixups.iter().cloned() { f.body_offset += body_base; fixups.push(f); }
        out.extend_from_slice(&case_bufs[ci].buf);
        if body_nop != 0 { out.push(0x90); }
        return;
    }

    // Build the chain test ops (range pairs for contiguous shared-label runs,
    // singleton `je` otherwise). A known scrutinee constant deletes resolved
    // never-taken tests; truncation cannot happen here (the partial-switch
    // path owns that), but guard for safety by ignoring the fold if it would.
    let ops: Vec<TestOp> = {
        let all = build_chain_ops(cases);
        match known_k {
            Some(kv) => {
                let f = fold_chain_ops(all.clone(), kv);
                if f.fallin_arm.is_none() { f.ops } else { all }
            }
            None => all,
        }
    };

    // A scrutinee ending in an add/sub on AX leaves ZF reflecting AX==0, so a
    // leading case-0 test needs no `or ax,ax` — the `je` follows the
    // arithmetic directly. Fixture 3650 (`switch (x - 1)`).
    let scrutinee_sets_zf =
        matches!(scrutinee, Expr::BinOp { op: BinOp::Sub | BinOp::Add, .. });

    // Chain length: per test:
    //   v == 0 je: `0b c0` (2) + `74 rel8` (2) = 4 bytes (2 when the
    //   scrutinee already set ZF and this is the first test)
    //   otherwise: `3d v v` (3) + jcc rel8 (2) = 5 bytes
    let chain_len: usize = ops.iter().enumerate()
        .map(|(i, op)| match op {
            TestOp::JeBody { v: 0, .. } if i == 0 && scrutinee_sets_zf => 2,
            TestOp::JeBody { v: 0, .. } => 4,
            _ => 5,
        })
        .sum();

    // Layout selection:
    // - No breaks and default exists → default-as-fallthrough: default body emitted
    //   first (right after chain), non-default bodies after; no jmp after chain.
    // - Otherwise → jmp (2 bytes) + alignment nop (if jmp end is odd) after chain.
    let default_fallthrough = default_idx.is_some();
    let jmp_nop_len: usize = if default_fallthrough {
        0
    } else {
        // MSC adds a nop after the jmp when the jmp's end position is odd, to
        // align the first case body to an even absolute address.
        let jmp_end_abs = start + scr_len + chain_len + 2;
        if jmp_end_abs % 2 != 0 { 3 } else { 2 }
    };

    // Emission order:
    // - default-fallthrough: default first, then non-default in source order
    // - otherwise: non-default in source order, then default
    let emission_order: Vec<usize> = if default_fallthrough {
        let d = default_idx.unwrap();
        let mut v = vec![d];
        v.extend((0..cases.len()).filter(|&i| i != d));
        v
    } else {
        let mut v: Vec<usize> = (0..cases.len()).filter(|&i| cases[i].value.is_some()).collect();
        if let Some(d) = default_idx { v.push(d); }
        v
    };

    // Default-fallthrough layout places the default body FIRST, ahead of the
    // value-case bodies. When the default body falls off the end of the switch
    // (no `break`/`return`) it must NOT run into the value cases that follow —
    // MSC emits an explicit `jmp <end>` to skip them. We synthesize that as a
    // break placeholder on the default buffer so it's patched to switch_end
    // like any other break. Only needed when the default is not the last body
    // emitted and doesn't already terminate or break. Fixture 4206.
    if default_fallthrough {
        let d = default_idx.unwrap();
        let is_last = emission_order.last() == Some(&d);
        if !is_last && !case_bufs[d].term && case_bufs[d].breaks.is_empty() {
            let off = case_bufs[d].buf.len();
            case_bufs[d].buf.extend_from_slice(&[0xEB, 0x00]);
            case_bufs[d].breaks.push(off);
        }
    }

    // The final emitted body's trailing break jumps to the very next
    // instruction (the post-switch fall-out) — MSC drops the jmp entirely
    // (and the alignment nop with it). Fixture 2369 (nested switch).
    // (When this switch's fall-out is redirected to an outer continuation, the
    // last break jumps far to `rl`, not the next instruction, so it can't be
    // elided.)
    let mut elided_last_break: Option<usize> = None;
    if fallout_override.is_none()
        && let Some(&last) = emission_order.last()
        && let Some(&boff) = case_bufs[last].breaks.last()
        && boff + 2 == case_bufs[last].buf.len()
    {
        case_bufs[last].buf.truncate(boff);
        case_bufs[last].breaks.pop();
        elided_last_break = Some(last);
    }

    // Compute body offsets (relative to switch start) and per-body nop flags,
    // using absolute position for alignment so the parity is correct even if
    // the switch does not start on an even function offset.
    let mut body_offsets: Vec<usize> = vec![0; cases.len()];
    let mut body_has_nop: Vec<bool> = vec![false; cases.len()];
    let mut running_abs = start + scr_len + chain_len + jmp_nop_len;
    for &i in &emission_order {
        body_offsets[i] = running_abs - start;
        running_abs += case_bufs[i].buf.len();
        // A body whose trailing break was elided loses its alignment pad too
        // (MSC drops the whole `jmp; nop` tail — fixture 2369).
        if running_abs % 2 != 0 && elided_last_break != Some(i) {
            body_has_nop[i] = true;
            running_abs += 1;
        }
    }
    let switch_end = running_abs - start;

    // Emit the chain tests. `jl` halves target the default body when one
    // exists, else the post-switch fall-out (fixtures 3084, 3965).
    for (i, op) in ops.iter().enumerate() {
        let (v, jcc, target_off) = match *op {
            TestOp::JeBody { v, arm } => (v, 0x74u8, body_offsets[arm]),
            TestOp::JleBody { v, arm } => (v, 0x7E, body_offsets[arm]),
            TestOp::JlDefault { v } => (
                v,
                0x7C,
                default_idx.map(|d| start + body_offsets[d])
                    .or(fallout_override)
                    .unwrap_or(start + switch_end)
                    - start,
            ),
        };
        if v == 0 && matches!(op, TestOp::JeBody { .. }) {
            if !(i == 0 && scrutinee_sets_zf) {
                out.extend_from_slice(&[0x0B, 0xC0]); // or ax, ax
            }
        } else {
            let v16 = (v as u32 & 0xFFFF) as u16;
            out.push(0x3D);
            out.extend_from_slice(&v16.to_le_bytes());
        }
        out.push(jcc);
        let here_end = out.len() + 1;
        let rel8 = (start + target_off) as i32 - here_end as i32;
        out.push(rel8 as u8);
        // A `jl` with no default targets the fall-out (continuation/override).
        if fallout_override.is_some()
            && matches!(op, TestOp::JlDefault { .. })
            && default_idx.is_none()
        {
            fallout_jmps.push(out.len() - 1);
        }
    }

    // Jmp after chain + alignment nop when not using default-fallthrough.
    if !default_fallthrough {
        let jmp_target = if let Some(d) = default_idx {
            start + body_offsets[d]
        } else {
            fallout_override.unwrap_or(start + switch_end)
        };
        out.push(0xEB);
        let rel8_pos = out.len();
        out.push((jmp_target as i32 - (rel8_pos + 1) as i32) as u8);
        if fallout_override.is_some() && default_idx.is_none() {
            fallout_jmps.push(rel8_pos);
        }
        if jmp_nop_len == 3 { out.push(0x90); } // alignment nop
    }

    // Emit bodies in emission_order; each gets an alignment nop if needed.
    let break_target = fallout_override.unwrap_or(start + switch_end);
    let mut case_bufs_indexed: Vec<Option<CaseBuf>> = case_bufs.into_iter().map(Some).collect();
    for &case_idx in &emission_order {
        let cb = case_bufs_indexed[case_idx].take().unwrap();
        let body_base = out.len();
        for mut f in cb.fixups { f.body_offset += body_base; fixups.push(f); }
        let mut body = cb.buf;
        for off in cb.breaks {
            let after_jmp = body_base + off + 2;
            let rel8 = break_target as i32 - after_jmp as i32;
            body[off + 1] = rel8 as u8;
            if fallout_override.is_some() { fallout_jmps.push(body_base + off + 1); }
        }
        out.extend_from_slice(&body);
        if body_has_nop[case_idx] { out.push(0x90); }
    }
}
pub(crate) fn emit_do_while(
    body_stmt: &Stmt,
    cond: &Cond,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // `do { prefix; if (c) break; } while (1);` — an infinite do-while with a
    // trailing break — folds the break into the back edge: `do { prefix } while (!c)`.
    if matches!(cond, Cond::Truthy(Expr::IntLit(k)) if *k != 0)
        && let Some((prefix, neg)) = loop_trailing_break_rewrite(body_stmt)
    {
        let dw_body = Stmt::Block(prefix);
        // `neg` is never a compile-time constant, so this does not re-trigger.
        emit_do_while(&dw_body, &neg, locals, frame, return_int, return_long, out, fixups);
        return;
    }
    // `do {} while (1)` with no break at all: infinite loop, no test.
    if matches!(cond, Cond::Truthy(Expr::IntLit(k)) if *k != 0)
        && !stmt_has_loop_break(body_stmt)
    {
        emit_infinite_loop(body_stmt, locals, frame, return_int, return_long, out, fixups);
        return;
    }
    // Clear init values for any local mutated in the body, matching
    // the same treatment in emit_loop (used by emit_while/emit_for).
    let body_mutations = collect_loop_body_mutations(&[body_stmt]);
    let body_inits: Vec<Option<i32>> = locals.inits.iter().enumerate()
        .map(|(i, &v)| if body_mutations.contains(&i) { None } else { v })
        .collect();
    let body_locals = Locals {
        inits: &body_inits,
        entry_inits: locals.entry_inits,
        disps: locals.disps,
        sizes: locals.sizes,
        long_globals: locals.long_globals,
        char_globals: locals.char_globals,
        global_elem_sizes: locals.global_elem_sizes,
        structs: locals.structs,
        global_struct_idxs: locals.global_struct_idxs,
        local_struct_idxs: locals.local_struct_idxs,
        unsigned_globals: locals.unsigned_globals,
        float_globals: locals.float_globals,
        long_locals: locals.long_locals,
        long_int_fold: locals.long_int_fold,
        init_literals: locals.init_literals,
        far_ptr_locals: locals.far_ptr_locals,
        huge_ptr_locals: locals.huge_ptr_locals,
        array_locals: locals.array_locals,
        unsigned_locals: locals.unsigned_locals,
        local_pointee_sizes: locals.local_pointee_sizes,
        local_pointee_unsigned: locals.local_pointee_unsigned,
        local_float_bits: locals.local_float_bits,
        float_locals: locals.float_locals,
        char_params: locals.char_params,
        param_struct_bytes: locals.param_struct_bytes,
        long_params: locals.long_params,
        unsigned_params: locals.unsigned_params,
        param_float_widths: locals.param_float_widths,
        param_pointee_sizes: locals.param_pointee_sizes,
        char_returners: locals.char_returners,
        long_returners: locals.long_returners,
        pascal_fns: locals.pascal_fns,
        static_fns: locals.static_fns,
        far_fns: locals.far_fns,
        variadic_fns: locals.variadic_fns,
        pascal_cleanup: locals.pascal_cleanup,
        si_local: locals.si_local,
        di_local: locals.di_local,
        long_param_funcs: locals.long_param_funcs,
        struct_param_funcs: locals.struct_param_funcs,
        float_param_funcs: locals.float_param_funcs,
        struct_return_funcs: locals.struct_return_funcs,
        float_returners: locals.float_returners,
        loop_stack: locals.loop_stack,
        labels: locals.labels,
        label_fixups: locals.label_fixups,
        backward_labels: locals.backward_labels,
        fpu_live: locals.fpu_live,
        return_float_width: locals.return_float_width,
        return_struct_bytes: locals.return_struct_bytes,
        struct_temp_bss_offset: locals.struct_temp_bss_offset,
        float_call_temp_disp: locals.float_call_temp_disp,
        fpu_pending_fwait: locals.fpu_pending_fwait,
        param_struct_ptr_bytes: locals.param_struct_ptr_bytes,
        int_cast_ptrs: locals.int_cast_ptrs,
        struct_field_temp_base: locals.struct_field_temp_base,
        elide_call_cleanup: std::cell::Cell::new(false),
        ternary_tail_epilogue: std::cell::RefCell::new(None),
        last_branch_barrier: std::cell::Cell::new(0),
        merge_barrier: std::cell::Cell::new(None),
        last_top_stmt: std::cell::Cell::new(false),
        final_top_stmt: std::cell::Cell::new(false),
    };
    let mut body_buf = Vec::new();
    let mut body_fixups: Vec<Fixup> = Vec::new();
    locals.loop_stack.borrow_mut().push(LoopCtx::default());
    // Rebase body-relative `goto`/label fixups into out-relative space, same as
    // emit_loop (fixture 442: goto out of a do-while).
    let lf_start = locals.label_fixups.borrow().len();
    let labels_before: std::collections::HashSet<String> =
        locals.labels.borrow().keys().cloned().collect();
    let rebase_body_labels = |delta: usize| {
        if delta == 0 { return; }
        for (_, pos) in locals.label_fixups.borrow_mut().iter_mut().skip(lf_start) {
            *pos += delta;
        }
        for (name, pos) in locals.labels.borrow_mut().iter_mut() {
            if !labels_before.contains(name) { *pos += delta; }
        }
    };
    emit_stmt(body_stmt, &body_locals, frame, return_int, return_long, &mut body_buf, &mut body_fixups);
    let loop_ctx = locals.loop_stack.borrow_mut().pop().expect("loop stack");
    let body_len = body_buf.len();
    // `do body while (0);` — body runs once, then the cond fails and
    // we drop through. Emit just the body and return. Only safe when
    // the cond references no Locals (otherwise the body may mutate
    // them, e.g. fixture 1316). Fixture 1588.
    if matches!(fold_cond(cond, locals), Some(0)) && !cond_references_local(cond) {
        let body_base = out.len();
        for mut c in body_fixups { c.body_offset += body_base; fixups.push(c); }
        rebase_body_labels(body_base);
        out.extend_from_slice(&body_buf);
        return;
    }
    let elide_cmp = body_sets_flags_for_cond(body_stmt, cond);
    let mut cmp_buf = Vec::new();
    let mut cmp_fixups: Vec<Fixup> = Vec::new();
    // For do-while (A && B): emit A with a forward exit-jcc (skipping B+jcc_back),
    // then B's cmp (the loop-back check). jcc_back opcode comes from B's condition.
    if let Cond::And(cond_a, cond_b) = cond {
        let mut b_cmp_buf: Vec<u8> = Vec::new();
        let mut b_cmp_fixups: Vec<Fixup> = Vec::new();
        emit_cond_cmp(cond_b, locals, &mut b_cmp_buf, &mut b_cmp_fixups);
        let a_exit_disp = i8::try_from(b_cmp_buf.len() + 2)
            .expect("do-while && A exit disp fits");
        emit_cond_skip(cond_a, a_exit_disp, locals, &mut cmp_buf, &mut cmp_fixups);
        let a_len = cmp_buf.len();
        for mut f in b_cmp_fixups { f.body_offset += a_len; cmp_fixups.push(f); }
        cmp_buf.extend_from_slice(&b_cmp_buf);
    } else if !elide_cmp {
        emit_cond_cmp(cond, locals, &mut cmp_buf, &mut cmp_fixups);
    }
    let cmp_len = cmp_buf.len();
    let jcc_opcode = match cond {
        Cond::Truthy(_) => 0x75,             // jne (back when nonzero)
        Cond::Cmp { op, .. } => loop_back_jcc(*op),
        Cond::And(_, cond_b) => match cond_b.as_ref() {
            Cond::Cmp { op, .. } => loop_back_jcc(*op),
            other => panic!("do-while &&: B must be Cmp, got: {other:?}"),
        },
        Cond::Or(_, _) => panic!("|| in do-while not yet supported"),
    };
    let body_base = out.len();
    out.extend_from_slice(&body_buf);
    for mut c in body_fixups {
        c.body_offset += body_base;
        fixups.push(c);
    }
    rebase_body_labels(body_base);
    let cmp_base = out.len();
    out.extend_from_slice(&cmp_buf);
    for mut c in cmp_fixups {
        c.body_offset += cmp_base;
        fixups.push(c);
    }
    let back_disp = -(i8::try_from(cmp_len + 2 + body_len)
        .expect("loop body+cmp short enough for jcc rel8"));
    out.push(jcc_opcode);
    out.push(back_disp as u8);
    let loop_end = out.len();
    // break → loop_end; continue → the cmp section. Each recorded offset points
    // at the rel8 disp byte (one past the eb/jcc opcode).
    for &off in &loop_ctx.breaks {
        let abs = body_base + off;
        let disp = loop_end as i32 - (abs as i32 + 1);
        out[abs] = i8::try_from(disp).expect("break rel8 disp fits") as u8;
    }
    for &off in &loop_ctx.continues {
        let abs = body_base + off;
        let disp = cmp_base as i32 - (abs as i32 + 1);
        out[abs] = i8::try_from(disp).expect("continue rel8 disp fits") as u8;
    }
}
/// True when the body's last operation sets ZF appropriately for
/// the cond, so MSC can omit the explicit cmp in a `do-while` loop.
/// Current trigger: `<local> = <local> ± 1;` paired with
/// `while (<same-local>);`. Fixture 4098.
pub(crate) fn body_sets_flags_for_cond(body: &Stmt, cond: &Cond) -> bool {
    // The flag-setting `i--`/`i++` only needs to be the LAST statement of the
    // body — its `dec/inc word [i]` is then the last instruction emitted before
    // the loop-back jcc, so the comparison is redundant. `do { i--; } while (i)`
    // (fixture 1218) and `do { sum += i; i--; } while (i)` (1887) both qualify.
    let body = match body {
        Stmt::Block(stmts) if !stmts.is_empty() => stmts.last().unwrap(),
        other => other,
    };
    let Stmt::Assign { target, value } = body else { return false };
    // The flag-setting store and the cond must name the same storage — either a
    // local decrement/increment tested by `while (l)`, or the global analog
    // tested by `while (g)` (fixture 922, reached via the do-while rewrite).
    let lhs_is_target = |e: &Expr| match (target, e) {
        (AssignTarget::Local(i), Expr::Local(j)) => i == j,
        (AssignTarget::Global(i), Expr::Global(j)) => i == j,
        _ => false,
    };
    let target_matches_cond = match (target, cond) {
        (AssignTarget::Local(i), Cond::Truthy(Expr::Local(j))) => i == j,
        (AssignTarget::Global(i), Cond::Truthy(Expr::Global(j))) => i == j,
        _ => return false,
    };
    if !target_matches_cond {
        return false;
    }
    let Expr::BinOp { op, left, right } = value else { return false };
    if !matches!(op, BinOp::Add | BinOp::Sub) {
        return false;
    }
    lhs_is_target(left.as_ref()) && matches!(right.as_ref(), Expr::IntLit(1))
}
