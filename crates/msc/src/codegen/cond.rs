use crate::*;

/// Just the cmp half of a cond — used by `emit_while` which pairs
/// the comparison with a backward jcc rather than a forward skip.
pub(crate) fn emit_cond_cmp(cond: &Cond, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    emit_cond_cmp_inner(cond, locals, out, fixups);
}
/// Emit `cmp <X>, <Y>; <inverted-jcc> skip` where `skip` is a
/// forward `rel8` displacement equal to `take_then_disp`. The
/// caller has pre-computed the size of the then-body so we can use
/// the 2-byte jcc form without a fixup. Fixtures 4090 / 4091 / 4092.
pub(crate) fn emit_cond_skip(cond: &Cond, take_then_disp: i8, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match cond {
        Cond::And(a, b) => {
            // Pre-emit `b`'s cmp+jcc-to-skip into a scratch buffer
            // so we know its size. `a`'s skip target jumps over `b`
            // AND the body. `b`'s skip target jumps over just the
            // body — the original `take_then_disp`.
            let mut b_buf = Vec::new();
            let mut b_fixups = Vec::new();
            emit_cond_skip(b, take_then_disp, locals, &mut b_buf, &mut b_fixups);
            let a_skip = i8::try_from(b_buf.len() as i32 + take_then_disp as i32)
                .expect("&&-cond skip fits in rel8");
            emit_cond_skip(a, a_skip, locals, out, fixups);
            let b_base = out.len();
            out.extend_from_slice(&b_buf);
            for mut f in b_fixups {
                f.body_offset += b_base;
                fixups.push(f);
            }
        }
        Cond::Or(a, b) => {
            // `a` true → jump into the body (skipping `b`). `a`
            // false → fall through to `b`. `b` evaluates as a
            // normal skip-cond: true → fall into body, false → skip.
            //
            // For `a`'s "take then" direction we need to invert the
            // jcc to a "take when true" form and target the start of
            // the body. We use emit_cond_take helper which emits
            // cmp + take-then-jcc instead of skip-jcc.
            let mut b_buf = Vec::new();
            let mut b_fixups = Vec::new();
            emit_cond_skip(b, take_then_disp, locals, &mut b_buf, &mut b_fixups);
            // a's "take" disp: jump past b's emission to the body.
            let a_take = i8::try_from(b_buf.len() as i32)
                .expect("||-cond take disp fits in rel8");
            emit_cond_take(a, a_take, locals, out, fixups);
            let b_base = out.len();
            out.extend_from_slice(&b_buf);
            for mut f in b_fixups {
                f.body_offset += b_base;
                fixups.push(f);
            }
        }
        _ => {
            let jcc = match cond {
                Cond::Truthy(_) => 0x74, // je on zero
                Cond::Cmp { op, .. } => inverted_jcc(*op),
                _ => unreachable!(),
            };
            emit_cond_cmp_inner(cond, locals, out, fixups);
            out.push(jcc);
            out.push(take_then_disp as u8);
        }
    }
}
/// Counterpart of `emit_cond_skip` — emits cmp + a jcc that fires
/// when the condition is *true*, skipping ahead by `take_disp`
/// bytes. Used by Cond::Or short-circuit so `a` jumps into the body
/// when satisfied.
pub(crate) fn emit_cond_take(cond: &Cond, take_disp: i8, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match cond {
        Cond::And(_, _) | Cond::Or(_, _) => {
            panic!("nested &&/|| in emit_cond_take not yet supported");
        }
        _ => {
            let jcc = match cond {
                Cond::Truthy(_) => 0x75, // jne (take when nonzero)
                Cond::Cmp { op, .. } => loop_back_jcc(*op),
                _ => unreachable!(),
            };
            emit_cond_cmp_inner(cond, locals, out, fixups);
            out.push(jcc);
            out.push(take_disp as u8);
        }
    }
}
/// Emit only the cmp half of a Cond. Used by both emit_cond_skip
/// (forward jcc for `if`) and emit_cond_cmp (backward jcc for loops).
pub(crate) fn emit_cond_cmp_inner(cond: &Cond, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Long-global vs zero idiom: `mov ax, [g]; or ax, [g+2]` — ZF set
    // iff both halves are zero. Covers `if (g == 0)`, `if (!g)`, and
    // (with inverted jcc) `if (g != 0)`.
    if let Some(idx) = match cond {
        Cond::Truthy(Expr::Global(idx)) if locals.is_long_global(*idx) => Some(*idx),
        Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::Global(idx), right: Expr::IntLit(0) }
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::IntLit(0), right: Expr::Global(idx) }
            if locals.is_long_global(*idx) => Some(*idx),
        _ => None,
    } {
        // mov ax, [g]
        let off = out.len();
        out.extend_from_slice(&[0xA1, 0x00, 0x00]);
        fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: idx } });
        // or ax, [g+2]  →  0b 06 <off+2>
        out.push(0x0B);
        out.push(0x06);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: idx } });
        return;
    }
    match cond {
        Cond::Truthy(Expr::Local(idx)) => emit_cmp_local_imm(*idx, locals, 0, out),
        Cond::Truthy(Expr::Param(idx)) => {
            if locals.is_char_param(*idx) {
                out.extend_from_slice(&[0x80, 0x7E, param_disp(*idx) as u8, 0x00]);
            } else {
                emit_cmp_bp_imm(param_disp(*idx), 0, out);
            }
        }
        Cond::Truthy(Expr::Global(idx)) => emit_cmp_global_imm(*idx, 0, out, fixups),
        Cond::Cmp { op: _, left: Expr::Local(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Local(idx) } => {
            emit_cmp_local_imm(*idx, locals, *k, out);
        }
        Cond::Cmp { op: _, left: Expr::Param(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Param(idx) } => {
            if locals.is_char_param(*idx) {
                // `cmp byte ptr [bp+disp], imm8` — MSC uses byte compare
                // for char params (fixtures 3130, 3131, 3144, 3345 etc).
                out.extend_from_slice(&[0x80, 0x7E, param_disp(*idx) as u8, (*k as u32 & 0xFF) as u8]);
            } else {
                emit_cmp_bp_imm(param_disp(*idx), *k, out);
            }
        }
        Cond::Cmp { op: _, left: Expr::Local(li), right: Expr::Param(pi) } => {
            // MSC loads right (param) into AX, then `cmp [local], ax`.
            emit_load_param(*pi, out);
            let l_disp = locals.disp(*li);
            out.push(0x39); out.push(bp_modrm(0x46, l_disp)); push_bp_disp(out, l_disp);
        }
        Cond::Cmp { op: _, left: Expr::Local(li), right: Expr::Local(rj) } => {
            // MSC loads right into AX, then `cmp [left], ax`.
            emit_load_local(*rj, locals, out);
            let l_disp = locals.disp(*li);
            out.push(0x39); out.push(bp_modrm(0x46, l_disp)); push_bp_disp(out, l_disp);
        }
        Cond::Cmp { op: _, left: Expr::Param(li), right: Expr::Param(rj) } => {
            // MSC loads right (rj) into AX, then `cmp [left], ax`.
            emit_load_param(*rj, out);
            let l_disp = param_disp(*li);
            out.push(0x39); out.push(0x46); out.push(l_disp as u8); // params always small
        }
        Cond::Cmp { op: _, left: Expr::Param(pi), right: Expr::Local(li) } => {
            // MSC loads right (local) into AX, then `cmp [param], ax`.
            emit_load_local(*li, locals, out);
            let p_disp = param_disp(*pi);
            out.push(0x39); out.push(0x46); out.push(p_disp as u8); // params always small
        }
        Cond::Cmp { op: _, left: Expr::Global(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Global(idx) } => {
            emit_cmp_global_imm(*idx, *k, out, fixups);
        }
        // Global-global comparison (non-long): load right into AX, then cmp [left], ax.
        // Result = left - right, so inverted_jcc / loop_back_jcc semantics are standard.
        Cond::Cmp { op: _, left: Expr::Global(li), right: Expr::Global(ri) }
            if !locals.is_long_global(*li) && !locals.is_long_global(*ri) =>
        {
            let off = out.len();
            out.extend_from_slice(&[0xA1, 0x00, 0x00]); // mov ax, [right]
            fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: *ri } });
            let off = out.len();
            out.extend_from_slice(&[0x39, 0x06, 0x00, 0x00]); // cmp [left], ax
            fixups.push(Fixup { body_offset: off + 1, kind: FixupKind::GlobalAddr { global_idx: *li } });
        }
        // Generic fallback for unsupported cond shapes: evaluate LHS
        // into AX, then `cmp ax, K` (`3d K K` for word, `83 f8 K` for
        // imm8sx). Used for `p->x == K` and other DerefLocalField cases.
        Cond::Cmp { op: _, left, right: Expr::IntLit(k) } => {
            emit_expr_to_ax(left, locals, out, fixups);
            emit_cmp_ax_imm(*k, out);
        }
        Cond::Cmp { op: _, left: Expr::IntLit(k), right } => {
            emit_expr_to_ax(right, locals, out, fixups);
            emit_cmp_ax_imm(*k, out);
        }
        // Generic fallback for two-sided non-literal comparisons:
        // evaluate right into AX, save to BX, evaluate left into AX, cmp ax, bx.
        // Result = left - right → standard jcc semantics.
        Cond::Cmp { op: _, left, right } => {
            emit_expr_to_ax(right, locals, out, fixups);
            out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
            emit_expr_to_ax(left, locals, out, fixups);
            out.extend_from_slice(&[0x3B, 0xC3]); // cmp ax, bx
        }
        // `while (a[i])` / `if (a[i])` where `a` is a global int array
        // and `i` is a runtime variable — MSC emits `mov bx, [i];
        // shl bx, 1; cmp WORD PTR [bx+_a], 0` (saves one byte vs
        // loading to AX then `or ax,ax`). Fixture 1309.
        // `while (a[i])` / `if (a[i])` where `a` is a global int array
        // and `i` is a runtime variable — MSC emits `mov bx, [i];
        // shl bx, 1; cmp WORD PTR [bx+_a], 0` (saves one byte vs
        // loading to AX then `or ax,ax`). Fixture 1309.
        Cond::Truthy(Expr::Index { array, index })
            if index.fold(locals.inits).is_none() =>
        {
            match index.as_ref() {
                Expr::Local(i) => {
                    let disp = locals.disp(*i);
                    { out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); } // mov bx, [bp+disp]
                }
                Expr::Param(i) => {
                    let pdisp = param_disp(*i) as i8;
                    out.extend_from_slice(&[0x8B, 0x5E, pdisp as u8]);
                }
                _ => {
                    // Fallback: load to AX, test
                    emit_expr_to_ax(&Expr::Index { array: *array, index: index.clone() }, locals, out, fixups);
                    out.extend_from_slice(&[0x0B, 0xC0]);
                    return;
                }
            }
            out.extend_from_slice(&[0xD1, 0xE3]); // shl bx, 1
            let body_offset = out.len();
            out.extend_from_slice(&[0x83, 0xBF, 0x00, 0x00, 0x00]); // cmp word [bx+addr], 0
            fixups.push(Fixup {
                body_offset: body_offset + 1,
                kind: FixupKind::GlobalAddr { global_idx: *array },
            });
        }
        Cond::Truthy(expr) => {
            // `if (<expr>)` — evaluate to AX, then test ax, ax (or
            // cmp ax, 0). MSC picks `or ax, ax` (`0b c0`) as the
            // 2-byte test. Most truthy-on-local/global cases were
            // handled earlier; this is the fallback.
            emit_expr_to_ax(expr, locals, out, fixups);
            out.extend_from_slice(&[0x0B, 0xC0]);
        }
        other => panic!("Slice 5 cond shape not yet supported: {other:?}"),
    }
}
/// `cmp ax, imm` — MSC always uses `3d imm16` (cmp ax, imm16).
pub(crate) fn emit_cmp_ax_imm(k: i32, out: &mut Vec<u8>) {
    let k16 = (k as u32 & 0xFFFF) as u16;
    out.push(0x3D);
    out.extend_from_slice(&k16.to_le_bytes());
}
/// The signed conditional-jump opcode for **falling through to the
/// then-block** when `op` is satisfied. Used by `emit_cond_skip`: the
/// emitted `jcc skip` triggers when the cond is *false*, so this is
/// the **inversion** of the source-level relop. MSC's signed-jcc
/// mnemonic family is `7c..7f` for the disp8 forms.
pub(crate) fn inverted_jcc(op: RelOp) -> u8 {
    match op {
        RelOp::Eq => 0x75, // jne — skip then-block when not-equal
        RelOp::Ne => 0x74, // je
        RelOp::Lt => 0x7D, // jge — signed
        RelOp::Le => 0x7F, // jg
        RelOp::Gt => 0x7E, // jle
        RelOp::Ge => 0x7C, // jl
    }
}
/// Loop-back jcc opcode — fires when cond is *true*, so it's NOT
/// inverted. Used by `emit_while` / `emit_do_while` for the tail
/// branch back to the loop top.
pub(crate) fn loop_back_jcc(op: RelOp) -> u8 {
    match op {
        RelOp::Eq => 0x74, // je
        RelOp::Ne => 0x75, // jne
        RelOp::Lt => 0x7C, // jl
        RelOp::Le => 0x7E, // jle
        RelOp::Gt => 0x7F, // jg
        RelOp::Ge => 0x7D, // jge
    }
}
/// `cmp word ptr [<global-addr>], imm8` — MSC picks the `83 3e disp imm8`
/// form for global compares against a sign-extended byte (fixtures
/// 4129, 4133). The placeholder address gets a GlobalAddr FIXUP.
pub(crate) fn emit_cmp_global_imm(global_idx: usize, k: i32, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if let Ok(k_i8) = i8::try_from(k) {
        out.push(0x83);
        out.push(0x3E);
        let body_offset = out.len();
        out.extend_from_slice(&[0x00, 0x00, k_i8 as u8]);
        fixups.push(Fixup { body_offset: body_offset - 1, kind: FixupKind::GlobalAddr { global_idx } });
    } else {
        // 16-bit immediate form: `81 3e addr_lo addr_hi imm_lo imm_hi`
        out.push(0x81);
        out.push(0x3E);
        let body_offset = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: body_offset - 1, kind: FixupKind::GlobalAddr { global_idx } });
        let imm = (k as u32 & 0xFFFF) as u16;
        out.extend_from_slice(&imm.to_le_bytes());
    }
}
/// `cmp word ptr [bp-disp], imm` — MSC picks the `83 /7 r/m imm8sx`
/// form when the immediate fits in a sign-extended byte (which is
/// every fixture exercised by Slice 5 today). The 5-byte
/// `81 7e disp imm16` form is reserved for larger constants.
pub(crate) fn emit_cmp_local_imm(idx: usize, locals: &Locals<'_>, k: i32, out: &mut Vec<u8>) {
    if locals.size(idx) == 1 {
        // `cmp byte [bp-disp], imm8` — `80 /7 r/m imm8`. Fixture 124.
        out.extend_from_slice(&[0x80, 0x7E, locals.disp(idx) as u8, (k as u32 & 0xFF) as u8]);
        return;
    }
    emit_cmp_bp_imm(locals.disp(idx), k, out);
}
/// `cmp word ptr [bp+disp], imm` — covers both local (negative disp)
/// and param (positive disp) compares. Picks `83 /7 r/m imm8sx` for
/// the small-immediate form, `81 /7 r/m imm16` for the wide form.
pub(crate) fn emit_cmp_bp_imm(disp: i16, k: i32, out: &mut Vec<u8>) {
    if let Ok(k_i8) = i8::try_from(k) {
        out.push(0x83);
        out.push(bp_modrm(0x7E, disp)); // /7=CMP, mod=01/10 bp-rel
        push_bp_disp(out, disp);
        out.push(k_i8 as u8);
    } else {
        let k16 = (k as u32 & 0xFFFF) as u16;
        out.push(0x81);
        out.push(bp_modrm(0x7E, disp));
        push_bp_disp(out, disp);
        out.extend_from_slice(&k16.to_le_bytes());
    }
}
