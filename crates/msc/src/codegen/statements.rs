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
        fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIWRQQ" } });
        out.push(0x90);
        out.push(0x9B);
    }
    // Read-and-clear the "function's final top-level statement" flag: only the
    // statement the function loop set it for should see it true. Recursing into
    // a nested block/branch (which calls emit_stmt again) must see false, so a
    // nested terminal `if(c) return e;` isn't mistaken for the function's tail.
    let final_top = locals.final_top_stmt.replace(false);
    match stmt {
        Stmt::Return(expr) => emit_return(expr, locals, frame, return_int, return_long, out, fixups),
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
            emit_runtime_switch(scrutinee, cases, locals, frame, return_int, return_long, out, fixups);
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
            for s in stmts {
                if !reachable { break; }
                emit_stmt(s, locals, frame, return_int, return_long, out, fixups);
                if stmt_always_returns(s, locals) {
                    reachable = false;
                }
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
            let jmp_len = if emit_else_jmp { 2 } else { 0 };
            // The alignment nop aligns the else-label (the je target), which
            // sits past the then-block and the over-else jmp. MSC omits it when
            // this is the function's last top-level statement with no else and a
            // falling-through body — the merge IS the epilogue, which isn't padded.
            // Fixtures 452, 459, 3602.
            let merge_is_epilogue = locals.last_top_stmt.get()
                && else_branch.is_none()
                && !stmt_always_returns(then_branch, locals);
            let needs_nop = !merge_is_epilogue
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
            if emit_else_jmp {
                out.push(0xEB);
                out.push(0x00);
            }
            if needs_nop { out.push(0x90); }
            if let Some(else_branch) = else_branch {
                emit_stmt(else_branch, locals, frame, return_int, return_long, out, fixups);
            }
            if emit_else_jmp {
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
    if !matches!(stmt, Stmt::Assign { .. } | Stmt::Block(_)) {
        locals.last_branch_barrier.set(out.len());
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
/// True when this top-level statement is a straight-line statement carrying
/// exactly one call (the candidate for last-call cleanup elision).
pub(crate) fn stmt_single_call(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::ExprStmt(e) => expr_call_count(e) == 1,
        Stmt::Assign { value, .. } => expr_call_count(value) == 1,
        Stmt::Return(e) => expr_call_count(e) == 1,
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
        | Expr::DerefGlobalField { .. } => true,
        Expr::BinOp { left, right, .. } => expr_is_pure(left) && expr_is_pure(right),
        Expr::Ternary { cond, then_arm, else_arm } => {
            expr_is_pure(cond) && expr_is_pure(then_arm) && expr_is_pure(else_arm)
        }
        Expr::CastChar { value, .. } => expr_is_pure(value),
        Expr::DerefByte { ptr } | Expr::DerefWord { ptr } => expr_is_pure(ptr),
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
        // Loops with a runtime cond can fall through; the
        // const-true infinite-loop case isn't exercised yet so we
        // conservatively answer false.
        Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. } => false,
        Stmt::Break | Stmt::Continue => false,
        Stmt::Switch { cases, .. } => {
            // A switch always returns when there is a default arm (all values
            // covered) and every arm body contains at least one returning stmt.
            let has_default = cases.iter().any(|a| a.value.is_none());
            has_default
                && cases.iter().all(|a| a.body.iter().any(|s| stmt_always_returns(s, locals)))
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
/// True if `e` contains a `(int)(long >> 16)` high-word extract. MSC reads
/// the long's high word from its slot for these rather than folding to a
/// constant, so we must skip the const-fold path and let emit_expr_to_ax
/// emit the slot read(s). Fixtures 1949, 2189.
fn expr_has_long_highword(e: &Expr, locals: &Locals<'_>) -> bool {
    match e {
        Expr::BinOp { op: BinOp::Shr, left, right }
            if long_operand(left, locals) && right.fold(locals.inits) == Some(16) => true,
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
            && cond.fold(locals.inits).is_none()
            && !is_ternary_select_operand(cond, then_arm, else_arm, locals)
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
                || is_long_shr1(expr, locals)
                || is_long_neg(expr, locals)
                || is_long_not(expr, locals)
                || is_long_arith_mem(expr, locals)
                || is_long_plus_int(expr, locals)
                || is_long_const_bitop(expr, locals)
                || is_long_muldiv(expr, locals)
                || is_long_field_elem_or_const_arith(expr, locals)
                || matches!(expr, Expr::Index { array, .. } if locals.is_long_global(*array))
                || matches!(expr, Expr::LocalIndex { local, .. } if locals.is_long_local(*local)))
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
fn stmt_has_loop_break(s: &Stmt) -> bool {
    match s {
        Stmt::Break | Stmt::Continue => true,
        Stmt::If { then_branch, else_branch, .. } =>
            stmt_has_loop_break(then_branch)
                || else_branch.as_ref().is_some_and(|e| stmt_has_loop_break(e)),
        Stmt::Block(ss) => ss.iter().any(stmt_has_loop_break),
        _ => false, // nested While/DoWhile/For/Switch own their break/continue
    }
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
        disps: locals.disps,
        sizes: locals.sizes,
        long_globals: locals.long_globals,
        char_globals: locals.char_globals,
        global_elem_sizes: locals.global_elem_sizes,
        unsigned_globals: locals.unsigned_globals,
        float_globals: locals.float_globals,
        long_locals: locals.long_locals,
        init_literals: locals.init_literals,
        far_ptr_locals: locals.far_ptr_locals,
        array_locals: locals.array_locals,
        unsigned_locals: locals.unsigned_locals,
        local_pointee_sizes: locals.local_pointee_sizes,
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
        pascal_cleanup: locals.pascal_cleanup,
        si_local: locals.si_local,
        di_local: locals.di_local,
        long_param_funcs: locals.long_param_funcs,
        struct_param_funcs: locals.struct_param_funcs,
        struct_return_funcs: locals.struct_return_funcs,
        float_returners: locals.float_returners,
        loop_stack: locals.loop_stack,
        labels: locals.labels,
        label_fixups: locals.label_fixups,
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
        last_branch_barrier: std::cell::Cell::new(0),
        last_top_stmt: std::cell::Cell::new(false),
        final_top_stmt: std::cell::Cell::new(false),
    };
    let n = levels.len();
    let base = out.len();
    let mut b: Vec<u8> = Vec::new();
    let mut bfix: Vec<Fixup> = Vec::new();
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
    if let Some((break_c, rest)) = &inner_break {
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
        if let Some(k) = value.fold(locals.inits) {
            let mut tmp: Vec<Option<i32>> = locals.inits.to_vec();
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
        disps: locals.disps,
        sizes: locals.sizes,
        long_globals: locals.long_globals,
        char_globals: locals.char_globals,
        global_elem_sizes: locals.global_elem_sizes,
        unsigned_globals: locals.unsigned_globals,
        float_globals: locals.float_globals,
        long_locals: locals.long_locals,
        init_literals: locals.init_literals,
        far_ptr_locals: locals.far_ptr_locals,
        array_locals: locals.array_locals,
        unsigned_locals: locals.unsigned_locals,
        local_pointee_sizes: locals.local_pointee_sizes,
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
        pascal_cleanup: locals.pascal_cleanup,
        si_local: locals.si_local,
        di_local: locals.di_local,
        long_param_funcs: locals.long_param_funcs,
        struct_param_funcs: locals.struct_param_funcs,
        struct_return_funcs: locals.struct_return_funcs,
        float_returners: locals.float_returners,
        loop_stack: locals.loop_stack,
        labels: locals.labels,
        label_fixups: locals.label_fixups,
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
        last_branch_barrier: std::cell::Cell::new(0),
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
    // For a `for` loop, `continue` jumps to the step segment, not the cond.
    // Record the body-buffer offset at the start of that segment.
    let mut cont_off: Option<usize> = None;
    for (idx, seg) in body_segments.iter().enumerate() {
        if Some(idx) == cont_seg { cont_off = Some(body_buf.len()); }
        emit_stmt(seg, &body_locals, frame, return_int, return_long, &mut body_buf, &mut body_fixups);
    }
    let mut loop_ctx = locals.loop_stack.borrow_mut().pop().expect("loop stack");
    let mut cmp_buf = Vec::new();
    let mut cmp_fixups: Vec<Fixup> = Vec::new();
    // For &&-conditions MSC puts B (right) before the body with a forward
    // exit-jcc, and A (left) after the body as the loop-back cmp section.
    // Layout: jmp→A_check; [nop]; B_check+exit_jcc; body; A_check; jcc_back→B_check.
    if let Cond::And(cond_a, cond_b) = cond {
        emit_cond_cmp(cond_a, &body_locals, &mut cmp_buf, &mut cmp_fixups);
        let b_exit_disp = i8::try_from(body_buf.len() + cmp_buf.len() + 2)
            .expect("&&-while B exit disp fits in i8");
        let mut b_cond_buf: Vec<u8> = Vec::new();
        let mut b_cond_fixups: Vec<Fixup> = Vec::new();
        emit_cond_skip(cond_b, b_exit_disp, &body_locals, &mut b_cond_buf, &mut b_cond_fixups);
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
    let effective_fold = entry_fold.or_else(|| {
        if cond_has_literal_side(cond) { fold_cond(cond, locals) } else { None }
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
    k: i32,
    cases: &[SwitchArm],
    continuation: &[Stmt],
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) -> bool {
    // Collect NFC arms: V != 0 AND V < k (signed), in source order.
    let nfc_arms: Vec<&SwitchArm> = cases.iter()
        .filter(|a| matches!(a.value, Some(v) if v != 0 && v < k))
        .collect();

    // Fallthrough arm: matched case k, or default if no case k.
    let ft_arm = cases.iter().find(|a| a.value == Some(k))
        .or_else(|| cases.iter().find(|a| a.value.is_none()));

    // Pre-emit a case arm body (stopping at Break), returning (bytes, fixups, terminates).
    let emit_arm = |arm: Option<&SwitchArm>| -> (Vec<u8>, Vec<Fixup>, bool) {
        let mut b: Vec<u8> = Vec::new();
        let mut f: Vec<Fixup> = Vec::new();
        let mut term = false;
        if let Some(arm) = arm {
            for s in &arm.body {
                if matches!(s, Stmt::Break) { break; }
                emit_stmt(s, locals, frame, return_int, return_long, &mut b, &mut f);
                if stmt_always_returns(s, locals) { term = true; break; }
            }
        }
        (b, f, term)
    };

    let (ft_bytes, ft_fxs, ft_term) = emit_arm(ft_arm);
    let nfc_bufs: Vec<(Vec<u8>, Vec<Fixup>, bool)> =
        nfc_arms.iter().map(|arm| emit_arm(Some(arm))).collect();

    // need_cont: true when any body doesn't terminate (→ continuation needed).
    let need_cont = !ft_term || nfc_bufs.iter().any(|(_, _, t)| !t);

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
        if reachable {
            crate::codegen::func::push_epilogue(frame, locals.pascal_cleanup, &mut cont_bytes);
        }
    }

    // ── emit scrutinee load ───────────────────────────────────────────────

    if k == 0 {
        out.extend_from_slice(&[0x2B, 0xC0]); // sub ax, ax
    } else {
        let k16 = (k as u32 & 0xFFFF) as u16;
        let [lo, hi] = k16.to_le_bytes();
        out.extend_from_slice(&[0xB8, lo, hi]); // mov ax, k
    }

    // ── emit comparison chain (je placeholders) ───────────────────────────

    let mut je_patches: Vec<usize> = Vec::new();
    for arm in &nfc_arms {
        let v = arm.value.unwrap();
        let v16 = (v as u32 & 0xFFFF) as u16;
        let [vlo, vhi] = v16.to_le_bytes();
        out.extend_from_slice(&[0x3D, vlo, vhi]); // cmp ax, v
        out.push(0x74); // je short
        je_patches.push(out.len()); // index of rel8 placeholder
        out.push(0x00);
    }

    // ── emit ft body ─────────────────────────────────────────────────────

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

    let has_zero_case = k != 0 && cases.iter().any(|a| a.value == Some(0));
    let direct_path = need_cont && has_zero_case;
    let fwd_path = need_cont && !has_zero_case;

    let mut nfc_starts: Vec<usize> = Vec::new();

    if !need_cont {
        // CASE 1: all bodies terminate.
        if out.len() % 2 != 0 { out.push(0x90); }
        for (nfc_bytes, nfc_fxs, _) in &nfc_bufs {
            nfc_starts.push(out.len());
            let base = out.len();
            for mut f in nfc_fxs.iter().cloned() { f.body_offset += base; fixups.push(f); }
            out.extend_from_slice(nfc_bytes);
            if out.len() % 2 != 0 { out.push(0x90); }
        }
        // Patch je → NFC starts.
        for (idx, &p) in je_patches.iter().enumerate() {
            out[p] = (nfc_starts[idx] as i32 - (p + 1) as i32) as u8;
        }
    } else if fwd_path {
        // CASE 2 FWD: ft body jumps forward past NFC[0] to RL; NFC[0] falls through.
        let ft_jmp_patch = {
            out.push(0xEB);
            let p = out.len();
            out.push(0x00);
            p
        };
        if out.len() % 2 != 0 { out.push(0x90); }

        // NFC[0]: between ft body and return_label. Falls through — no explicit jmp.
        nfc_starts.push(out.len());
        {
            let (b, f, _) = &nfc_bufs[0];
            let base = out.len();
            for mut fx in f.iter().cloned() { fx.body_offset += base; fixups.push(fx); }
            out.extend_from_slice(b);
        }

        // return_label: emit continuation.
        let return_label = out.len();
        {
            let base = out.len();
            for mut f in cont_fxs { f.body_offset += base; fixups.push(f); }
            out.extend_from_slice(&cont_bytes);
        }

        // NFC[1..]: after continuation; each has backward jmp → return_label + nop.
        for (nfc_bytes, nfc_fxs, nfc_term) in nfc_bufs.iter().skip(1) {
            nfc_starts.push(out.len());
            let base = out.len();
            for mut f in nfc_fxs.iter().cloned() { f.body_offset += base; fixups.push(f); }
            out.extend_from_slice(nfc_bytes);
            if !nfc_term {
                out.push(0xEB);
                let p = out.len();
                out.push(0x00);
                let rel8 = return_label as i32 - (p + 1) as i32;
                out[p] = rel8 as u8;
            }
            if out.len() % 2 != 0 { out.push(0x90); }
        }

        // Patch ft_jmp → return_label.
        out[ft_jmp_patch] = (return_label as i32 - (ft_jmp_patch + 1) as i32) as u8;

        // Patch je → NFC starts.
        for (idx, &p) in je_patches.iter().enumerate() {
            out[p] = (nfc_starts[idx] as i32 - (p + 1) as i32) as u8;
        }
    } else {
        // CASE 2 DIRECT: ft body falls through to RL; all NFCs after continuation.
        debug_assert!(direct_path);

        // return_label immediately after ft body.
        let return_label = out.len();
        {
            let base = out.len();
            for mut f in cont_fxs { f.body_offset += base; fixups.push(f); }
            out.extend_from_slice(&cont_bytes);
        }

        // Alignment nop before first NFC body.
        if out.len() % 2 != 0 { out.push(0x90); }

        // All NFC bodies: each has backward jmp → return_label + nop.
        for (nfc_bytes, nfc_fxs, nfc_term) in &nfc_bufs {
            nfc_starts.push(out.len());
            let base = out.len();
            for mut f in nfc_fxs.iter().cloned() { f.body_offset += base; fixups.push(f); }
            out.extend_from_slice(nfc_bytes);
            if !nfc_term {
                out.push(0xEB);
                let p = out.len();
                out.push(0x00);
                let rel8 = return_label as i32 - (p + 1) as i32;
                out[p] = rel8 as u8;
            }
            if out.len() % 2 != 0 { out.push(0x90); }
        }

        // Patch je → NFC starts.
        for (idx, &p) in je_patches.iter().enumerate() {
            out[p] = (nfc_starts[idx] as i32 - (p + 1) as i32) as u8;
        }
    }

    false // caller must not add another epilogue (handled internally)
}
/// Runtime `switch (<scrutinee>) { case K0: body0 case K1: body1 default: ... }`.
/// Matches MSC layout: short cmp+je chain; `jmp short; nop` fallthrough; non-default
/// bodies each get a trailing NOP; default body goes last with no trailing NOP.
pub(crate) fn emit_runtime_switch(
    scrutinee: &Expr,
    cases: &[SwitchArm],
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
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

    // Chain length: per non-default case:
    //   K == 0: `0b c0` (2) + `74 rel8` (2) = 4 bytes
    //   K != 0: `3d K K` (3) + `74 rel8` (2) = 5 bytes
    let chain_len: usize = cases.iter()
        .filter_map(|c| c.value)
        .map(|k| if k == 0 { 4 } else { 5 })
        .sum();

    // Layout selection:
    // - No breaks and default exists → default-as-fallthrough: default body emitted
    //   first (right after chain), non-default bodies after; no jmp after chain.
    // - Otherwise → jmp (2 bytes) + alignment nop (if jmp end is odd) after chain.
    let default_fallthrough = default_idx.is_some() && !any_breaks;
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

    // Compute body offsets (relative to switch start) and per-body nop flags,
    // using absolute position for alignment so the parity is correct even if
    // the switch does not start on an even function offset.
    let mut body_offsets: Vec<usize> = vec![0; cases.len()];
    let mut body_has_nop: Vec<bool> = vec![false; cases.len()];
    let mut running_abs = start + scr_len + chain_len + jmp_nop_len;
    for &i in &emission_order {
        body_offsets[i] = running_abs - start;
        running_abs += case_bufs[i].buf.len();
        if running_abs % 2 != 0 {
            body_has_nop[i] = true;
            running_abs += 1;
        }
    }
    let switch_end = running_abs - start;

    // Emit cmp+je (short) for each non-default case in source order.
    for (i, arm) in cases.iter().enumerate() {
        if let Some(k) = arm.value {
            if k == 0 {
                out.extend_from_slice(&[0x0B, 0xC0]); // or ax, ax
            } else {
                let k16 = (k as u32 & 0xFFFF) as u16;
                out.push(0x3D);
                out.extend_from_slice(&k16.to_le_bytes());
            }
            out.push(0x74); // JE short
            let here_end = out.len() + 1;
            let rel8 = (start + body_offsets[i]) as i32 - here_end as i32;
            out.push(rel8 as u8);
        }
    }

    // Jmp after chain + alignment nop when not using default-fallthrough.
    if !default_fallthrough {
        let jmp_target = if let Some(d) = default_idx {
            start + body_offsets[d]
        } else {
            start + switch_end
        };
        out.push(0xEB);
        let here_end = out.len() + 1;
        out.push((jmp_target as i32 - here_end as i32) as u8);
        if jmp_nop_len == 3 { out.push(0x90); } // alignment nop
    }

    // Emit bodies in emission_order; each gets an alignment nop if needed.
    let break_target = start + switch_end;
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
    // Clear init values for any local mutated in the body, matching
    // the same treatment in emit_loop (used by emit_while/emit_for).
    let body_mutations = collect_loop_body_mutations(&[body_stmt]);
    let body_inits: Vec<Option<i32>> = locals.inits.iter().enumerate()
        .map(|(i, &v)| if body_mutations.contains(&i) { None } else { v })
        .collect();
    let body_locals = Locals {
        inits: &body_inits,
        disps: locals.disps,
        sizes: locals.sizes,
        long_globals: locals.long_globals,
        char_globals: locals.char_globals,
        global_elem_sizes: locals.global_elem_sizes,
        unsigned_globals: locals.unsigned_globals,
        float_globals: locals.float_globals,
        long_locals: locals.long_locals,
        init_literals: locals.init_literals,
        far_ptr_locals: locals.far_ptr_locals,
        array_locals: locals.array_locals,
        unsigned_locals: locals.unsigned_locals,
        local_pointee_sizes: locals.local_pointee_sizes,
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
        pascal_cleanup: locals.pascal_cleanup,
        si_local: locals.si_local,
        di_local: locals.di_local,
        long_param_funcs: locals.long_param_funcs,
        struct_param_funcs: locals.struct_param_funcs,
        struct_return_funcs: locals.struct_return_funcs,
        float_returners: locals.float_returners,
        loop_stack: locals.loop_stack,
        labels: locals.labels,
        label_fixups: locals.label_fixups,
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
        last_branch_barrier: std::cell::Cell::new(0),
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
    let Stmt::Assign { target: AssignTarget::Local(local_idx), value } = body else { return false };
    let Cond::Truthy(Expr::Local(cond_idx)) = cond else { return false };
    if local_idx != cond_idx {
        return false;
    }
    let Expr::BinOp { op, left, right } = value else { return false };
    if !matches!(op, BinOp::Add | BinOp::Sub) {
        return false;
    }
    matches!(left.as_ref(), Expr::Local(li) if li == local_idx)
        && matches!(right.as_ref(), Expr::IntLit(1))
}
