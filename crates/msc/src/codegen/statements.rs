use crate::*;

/// Emit a single statement (recursive: if-statements contain
/// nested statements). Returns no value — appends directly to `out`.
pub(crate) fn emit_stmt(
    stmt: &Stmt,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    match stmt {
        Stmt::Return(expr) => emit_return(expr, locals, frame, return_int, return_long, out, fixups),
        Stmt::Empty => {}
        Stmt::ExprStmt(Expr::Call { name, args }) => {
            emit_call(name, args, locals, out, fixups);
        }
        Stmt::ExprStmt(other) => {
            // Generic: evaluate the expression, discard the AX result.
            // Side-effect-free shapes still compile; they just leave
            // dead code in AX.
            emit_expr_to_ax(other, locals, out, fixups);
        }
        Stmt::Assign { target, value } => emit_assign(target.clone(), value, locals, out, fixups),
        Stmt::Switch { scrutinee, cases } => {
            emit_runtime_switch(scrutinee, cases, locals, frame, return_int, return_long, out, fixups);
        }
        Stmt::Break => {
            // Emit a forward `jmp near` placeholder; the enclosing
            // loop patches the disp16 at its end.
            let mut stack = locals.loop_stack.borrow_mut();
            if let Some(top) = stack.last_mut() {
                let off = out.len();
                out.extend_from_slice(&[0xE9, 0x00, 0x00]);
                top.breaks.push(off);
            } else {
                panic!("break outside a loop or const-folded switch");
            }
        }
        Stmt::Continue => {
            let mut stack = locals.loop_stack.borrow_mut();
            if let Some(top) = stack.last_mut() {
                let off = out.len();
                out.extend_from_slice(&[0xE9, 0x00, 0x00]);
                top.continues.push(off);
            } else {
                panic!("continue outside a loop");
            }
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
            // Constant-condition elision: when the cond folds to a
            // compile-time integer, MSC keeps only the live branch
            // and drops the comparison + jump entirely. Fixtures
            // 4094 (if (0)) and 4095 (if (1)) confirm.
            if let Some(k) = fold_cond(cond, locals) {
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
            let cond_size = {
                let mut sz = Vec::new();
                emit_cond_skip(cond, 0i8, locals, &mut sz, &mut Vec::new());
                sz.len()
            };
            let needs_nop = (out.len() + cond_size + then_len) % 2 != 0;
            let nop_pad = usize::from(needs_nop);
            let take_then_disp = i8::try_from(then_len + nop_pad)
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
            if needs_nop { out.push(0x90); }
            if let Some(else_branch) = else_branch {
                emit_stmt(else_branch, locals, frame, return_int, return_long, out, fixups);
            }
        }
    }
}
/// True when `stmt` unconditionally returns — so a following
/// statement at the same nesting level is unreachable. Used to
/// drop trailing dead code (fixture 4095: `if (1) return 1; return
/// 0;` keeps only the `return 1;` path).
pub(crate) fn stmt_always_returns(stmt: &Stmt, locals: &Locals<'_>) -> bool {
    match stmt {
        Stmt::Return(_) => true,
        Stmt::Empty => false,
        Stmt::ExprStmt(_) | Stmt::Assign { .. } => false,
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
pub(crate) fn emit_return(
    expr: &Expr,
    locals: &Locals<'_>,
    frame: Frame,
    return_int: bool,
    return_long: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // Float/double return via the `__fac` accumulator. The parser folds the
    // returned expression to a FloatLit (materialized as a CONST temp):
    //   9B <D9|DD> 06 <off16>  fld  <width> [$T]        (FIDRQQ + FloatLoad)
    //   9B DD 1E <off16>       fstp QWORD [__fac]        (FIDRQQ + ExtData)
    //   B8 <imm16>             mov  ax, OFFSET __fac     (ExtData)
    //   90 9B                  nop; fwait                (FIWRQQ)
    if locals.return_float_width != 0 {
        if let Expr::FloatLit(bits, is_double) = expr {
            let width = if *is_double { 8 } else { 4 };
            let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
            // fld <width> [$T]
            fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
            out.push(0x9B);
            out.push(op);
            out.push(0x06);
            let bo = out.len() - 1;
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::FloatLoad { bits: *bits, width } });
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
            out.extend_from_slice(frame.epilogue_bytes());
            return;
        }
    }
    if return_int {
        // Return-of-call peephole: `return f(args);` leaves the
        // result in AX from the call's return value — no extra
        // load before ret. Fixture 4102 confirms.
        // For WithSlide frames, `mov sp, bp` in the epilogue restores sp
        // across the pushed args — no `add sp, N` cleanup needed.
        if let Expr::Call { name, args } = expr {
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
            out.extend_from_slice(frame.epilogue_bytes());
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
            out.extend_from_slice(frame.epilogue_bytes());
            return;
        } else if let Expr::Local(idx) = expr
            && locals.is_float_local(*idx)
        {
            // `return (int)<non-literal float local>`. When the value is live on
            // st(0) (coupled init stored with `fst`), the cast is a bare
            // `call __ftol`; otherwise reload the slot with `fld` first.
            if locals.fpu_live.get() != Some(*idx) {
                let width = locals.float_local_width(*idx);
                let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
                let disp = locals.disp(*idx);
                fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                out.push(0x9B);
                out.push(op);
                out.push(bp_modrm(0x46, disp));
                push_bp_disp(out, disp);
            }
            let call_off = out.len();
            out.extend_from_slice(&[0xE8, 0x00, 0x00]); // call __ftol
            fixups.push(Fixup { body_offset: call_off, kind: FixupKind::ExtCall { target: "__ftol".to_owned() } });
            out.extend_from_slice(frame.epilogue_bytes());
            return;
        } else if matches!(expr, Expr::Local(i) if locals.is_long_local(*i)) {
            // Long locals are read through the 4-byte slot — MSC
            // emits `mov ax, [bp-disp_low]` even when the value is
            // known at compile time (fixture 1037).
            emit_expr_to_ax(expr, locals, out, fixups);
            if return_long {
                // Also load the high word into DX.
                let i = if let Expr::Local(i) = expr { *i } else { unreachable!() };
                let disp = locals.disp(i);
                out.extend_from_slice(&[0x8B, 0x56, (disp + 2) as u8]); // mov dx, [bp+d+2]
            }
        } else if let Expr::Param(i) = expr
            && locals.is_char_param(*i)
        {
            // `return c` where `c` is a char param: byte load + cbw, plus a
            // cwd to widen into DX:AX when the function returns long
            // (`return (long)c`, fixture 3183).
            let disp = param_disp(*i) as u8;
            out.extend_from_slice(&[0x8A, 0x46, disp]); // mov al, [bp+4]
            out.push(0x98); // cbw
            if return_long {
                out.push(0x99); // cwd
            }
            out.extend_from_slice(frame.epilogue_bytes());
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
            out.extend_from_slice(frame.epilogue_bytes());
            return;
        } else if let Expr::LocalIndex { local: l, index } = expr {
            let k = index.fold(locals.inits).unwrap_or(0);
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
            out.extend_from_slice(frame.epilogue_bytes());
            return;
        } else if let Expr::LocalIndexByte { local: l, index } = expr {
            let k = index.fold(locals.inits).unwrap_or(0);
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
            out.extend_from_slice(frame.epilogue_bytes());
            return;
        } else if let Expr::Local(i) = expr {
            let disp = locals.disp(*i);
            if locals.size(*i) == 1 && !locals.is_unsigned_local(*i) {
                // Signed char local: check if AL is already loaded from a
                // prior byte-store (`88 46 disp`). If so, just cbw; else
                // reload from the slot. Fixture 1039 (non-literal prologue
                // init leaves AL set; emit just cbw for return).
                let prev_store = {
                    let mr = bp_modrm(0x46, disp);
                    let mut v = vec![0x88, mr];
                    push_bp_disp(&mut v, disp);
                    v
                };
                let al_already_set = out.len() >= prev_store.len()
                    && out[out.len()-prev_store.len()..] == *prev_store;
                if !al_already_set {
                    out.push(0x8A);
                    out.push(bp_modrm(0x46, disp));
                    push_bp_disp(out, disp);
                }
                out.push(0x98); // cbw
                out.extend_from_slice(frame.epilogue_bytes());
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
            let ax_already_set = out.len() >= prev_store.len()
                && out[out.len()-prev_store.len()..] == *prev_store;
            if !ax_already_set {
                emit_expr_to_ax(expr, locals, out, fixups);
            }
        } else if matches!(expr, Expr::BinOp { op: BinOp::LogOr | BinOp::LogAnd, .. }) {
            // `return x || y` / `return x && y` — MSC always emits the full
            // short-circuit branch structure with TWO separate epilogue+ret
            // paths (one for true=1, one for false=0). Does NOT const-fold.
            let epi = frame.epilogue_bytes();
            let take_then_disp = 3i8 + epi.len() as i8;  // true block = 3 (mov ax,1) + epi
            let cond = cond_from_expr(expr.clone());
            emit_cond_skip(&cond, take_then_disp, locals, out, fixups);
            out.extend_from_slice(&[0xB8, 0x01, 0x00]);  // mov ax, 1
            out.extend_from_slice(epi);                   // epilogue + ret
            out.extend_from_slice(&[0x2B, 0xC0]);         // sub ax, ax
            out.extend_from_slice(epi);                   // epilogue + ret
            return;  // both branches already have the epilogue
        } else if let Expr::BinOp { op, left, right } = expr
            && matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
            && expr.fold(locals.inits).is_none()
        {
            // `return x == y` etc — two-epilogue structure with optional NOP
            // to keep the false-path aligned. The inverted_jcc fires when
            // the condition is FALSE and jumps past the true block to the
            // false path (sub ax,ax + epilogue).
            let epi = frame.epilogue_bytes();
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
            out.extend_from_slice(epi);                   // epilogue + ret
            if needs_nop { out.push(0x90); }              // alignment NOP
            out.extend_from_slice(&[0x2B, 0xC0]);         // sub ax, ax
            out.extend_from_slice(epi);                   // epilogue + ret
            return;  // both branches already have the epilogue
        } else if let Some(k) = expr.fold(locals.inits)
            && !expr_has_long_highword(expr, locals)
        {
            if k == 0 {
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
                || is_long_muldiv(expr, locals))
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
    out.extend_from_slice(frame.epilogue_bytes());
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
    emit_loop(cond, &[body_stmt], None, locals, frame, return_int, return_long, out, fixups);
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
    emit_stmt(init, locals, frame, return_int, return_long, out, fixups);
    // After the init runs, check if the condition is known true.
    // If so, use do-while (body then step); otherwise while (step then body).
    let entry = for_entry_fold(init, cond, locals);
    if matches!(entry, Some(k) if k != 0) && !matches!(body_stmt, Stmt::Empty) {
        // Do-while form: body THEN step, no initial jmp.
        emit_loop(cond, &[body_stmt, step], entry, locals, frame, return_int, return_long, out, fixups);
    } else {
        emit_loop(cond, &[step, body_stmt], None, locals, frame, return_int, return_long, out, fixups);
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
    let body_mutations = collect_loop_body_mutations(body_segments);
    let body_inits: Vec<Option<i32>> = locals.inits.iter().enumerate()
        .map(|(i, &v)| if body_mutations.contains(&i) { None } else { v })
        .collect();
    let body_locals = Locals {
        inits: &body_inits,
        disps: locals.disps,
        sizes: locals.sizes,
        long_globals: locals.long_globals,
        char_globals: locals.char_globals,
        unsigned_globals: locals.unsigned_globals,
        float_globals: locals.float_globals,
        long_locals: locals.long_locals,
        init_literals: locals.init_literals,
        far_ptr_locals: locals.far_ptr_locals,
        array_locals: locals.array_locals,
        unsigned_locals: locals.unsigned_locals,
        float_locals: locals.float_locals,
        char_params: locals.char_params,
        long_params: locals.long_params,
        unsigned_params: locals.unsigned_params,
        param_float_widths: locals.param_float_widths,
        char_returners: locals.char_returners,
        long_param_funcs: locals.long_param_funcs,
        loop_stack: locals.loop_stack,
        fpu_live: locals.fpu_live,
        return_float_width: locals.return_float_width,
    };
    let mut body_buf = Vec::new();
    let mut body_fixups: Vec<Fixup> = Vec::new();
    locals.loop_stack.borrow_mut().push(LoopCtx::default());
    for seg in body_segments {
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
        for off in &mut loop_ctx.breaks { *off += b_cond_len; }
        for off in &mut loop_ctx.continues { *off += b_cond_len; }
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

    // Alignment: position right after the 2-byte `eb XX` should be
    // even. If it would be odd, insert a NOP pad and bump the
    // forward jmp displacement by 1.
    let pos_after_jmp = out.len() + 2;
    let needs_pad = pos_after_jmp % 2 != 0;
    let pad = if needs_pad { 1 } else { 0 };

    let jcc_opcode = match cond {
        Cond::Truthy(_) => 0x75,             // jne (back when nonzero)
        Cond::Cmp { op, .. } => loop_back_jcc(*op),
        Cond::And(cond_a, _) => match cond_a.as_ref() {
            Cond::Truthy(_) => 0x75,
            Cond::Cmp { op, .. } => loop_back_jcc(*op),
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
    let skip_initial_jmp = body_len == 0
        || (matches!(effective_fold, Some(k) if k != 0)
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
    // Patch break/continue placeholders (Break/Continue stmt arms
    // left `e9 00 00` markers). break → loop_end; continue → cmp_base.
    for off in &loop_ctx.breaks {
        let abs = body_base + off;
        let after = abs + 3;
        let target = loop_end as i32 - after as i32;
        let rel = (target as i16) as u16;
        out[abs + 1] = rel.to_le_bytes()[0];
        out[abs + 2] = rel.to_le_bytes()[1];
    }
    for off in &loop_ctx.continues {
        let abs = body_base + off;
        let after = abs + 3;
        let target = cmp_base as i32 - after as i32;
        let rel = (target as i16) as u16;
        out[abs + 1] = rel.to_le_bytes()[0];
        out[abs + 2] = rel.to_le_bytes()[1];
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
            cont_bytes.extend_from_slice(frame.epilogue_bytes());
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
        unsigned_globals: locals.unsigned_globals,
        float_globals: locals.float_globals,
        long_locals: locals.long_locals,
        init_literals: locals.init_literals,
        far_ptr_locals: locals.far_ptr_locals,
        array_locals: locals.array_locals,
        unsigned_locals: locals.unsigned_locals,
        float_locals: locals.float_locals,
        char_params: locals.char_params,
        long_params: locals.long_params,
        unsigned_params: locals.unsigned_params,
        param_float_widths: locals.param_float_widths,
        char_returners: locals.char_returners,
        long_param_funcs: locals.long_param_funcs,
        loop_stack: locals.loop_stack,
        fpu_live: locals.fpu_live,
        return_float_width: locals.return_float_width,
    };
    let mut body_buf = Vec::new();
    let mut body_fixups: Vec<Fixup> = Vec::new();
    locals.loop_stack.borrow_mut().push(LoopCtx::default());
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
    for off in &loop_ctx.breaks {
        let abs = body_base + off;
        let after = abs + 3;
        let target = loop_end as i32 - after as i32;
        let rel = (target as i16) as u16;
        out[abs + 1] = rel.to_le_bytes()[0];
        out[abs + 2] = rel.to_le_bytes()[1];
    }
    for off in &loop_ctx.continues {
        let abs = body_base + off;
        let after = abs + 3;
        let target = cmp_base as i32 - after as i32;
        let rel = (target as i16) as u16;
        out[abs + 1] = rel.to_le_bytes()[0];
        out[abs + 2] = rel.to_le_bytes()[1];
    }
}
/// True when the body's last operation sets ZF appropriately for
/// the cond, so MSC can omit the explicit cmp in a `do-while` loop.
/// Current trigger: `<local> = <local> ± 1;` paired with
/// `while (<same-local>);`. Fixture 4098.
pub(crate) fn body_sets_flags_for_cond(body: &Stmt, cond: &Cond) -> bool {
    // Peel a single-statement block so `do { i--; } while (i);` is
    // recognized identically to `do i--; while (i);` (fixture 1218).
    let body = match body {
        Stmt::Block(stmts) if stmts.len() == 1 => &stmts[0],
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
