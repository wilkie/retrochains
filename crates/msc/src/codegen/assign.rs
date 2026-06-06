use crate::*;

/// `<local> = <expr>;`. Phase 1 supports the peephole
/// `<local> = <same-local> ± 1;` → `inc/dec word ptr [bp-disp]`
/// (fixture 4096: `x = x - 1;` in a while body). The general path
/// — `mov ax, <expr>; mov [bp-disp], ax` — is reserved for a
/// future fixture that exercises a non-peephole shape.
pub(crate) fn emit_assign(target: AssignTarget, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // `t = (s1, ..., v)` — run the comma side effects, then assign v to t so a
    // constant v keeps the c7 immediate store (`mov [t],K`) instead of being
    // materialized through AX. Fixture 2234.
    if let Expr::Seq { sides, value: inner } = value {
        for s in sides {
            emit_stmt(s, locals, Frame::BpOnly, true, false, out, fixups);
        }
        emit_assign(target, inner, locals, out, fixups);
        return;
    }
    // Chained assignment `a = b = c = V`: the RHS is an AssignExpr with its own
    // store side effects. Emit it (storing into the inner targets, leaving V in
    // AX), then store AX into this target too — `mov ax,V; mov [c],ax; mov [b],ax;
    // mov [a],ax`. Simple scalar targets. Fixtures 500, 2951, 3513.
    if matches!(value, Expr::AssignExpr { .. })
        && matches!(target, AssignTarget::Local(_) | AssignTarget::Param(_) | AssignTarget::Global(_))
    {
        emit_expr_to_ax(value, locals, out, fixups); // inner stores, value → AX
        match target {
            AssignTarget::Local(i) => { let d = locals.disp(i); out.push(0x89); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); }
            AssignTarget::Param(i) => { let d = param_disp(i); out.push(0x89); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); }
            AssignTarget::Global(g) => { let bo = out.len(); out.extend_from_slice(&[0xA3, 0x00, 0x00]); fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } }); }
            _ => unreachable!(),
        }
        return;
    }
    // Receive a float/double return into a local: the callee returns AX = &__fac,
    // and the caller block-copies `width` bytes from there into the local:
    //   call _fn; lea di,[bp+disp]; mov si,ax; push ss; pop es; movsw×(width/2)
    if let AssignTarget::Local(i) = target
        && locals.is_float_local(i)
        && let Expr::Call { name, args } = value
        && let Some(&width) = locals.float_returners.get(&symbol_name(name))
    {
        emit_call_inner(name, args, locals, false, out, fixups);
        let disp = locals.disp(i);
        out.push(0x8D); // lea di, [bp+disp]
        out.push(bp_modrm(0x7E, disp));
        push_bp_disp(out, disp);
        out.extend_from_slice(&[0x8B, 0xF0]); // mov si, ax
        out.extend_from_slice(&[0x16, 0x07]); // push ss; pop es
        for _ in 0..(width / 2) {
            out.push(0xA5); // movsw
        }
        return;
    }
    // Float/double compound assign `d op= K` (`d = d op K`): apply the op to the
    // live st(0), then `fst` back (keeping it live for the next op / the cast):
    //   9B DC /r <off16>   f<op> QWORD [$T]   (FIDRQQ + FloatLoad, qword operand)
    //   9B DD 56 <disp>    fst   QWORD [bp+disp]  (FIDRQQ)
    if let AssignTarget::Local(i) = target
        && locals.is_float_local(i)
        && let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Local(li) if *li == i)
        && let Expr::FloatLit(bits, _) = right.as_ref()
    {
        let reg = match op {
            BinOp::Add => 0u8, BinOp::Mul => 1, BinOp::Sub => 4, BinOp::Div => 6,
            _ => 0,
        };
        fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
        out.push(0x9B);
        out.push(0xDC);
        out.push(0x06 | (reg << 3));
        let bo = out.len() - 1;
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: bo, kind: FixupKind::FloatLoad { bits: *bits, width: 8 } });
        let disp = locals.disp(i);
        fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
        out.push(0x9B);
        out.push(0xDD);
        out.push(bp_modrm(0x56, disp)); // fst qword [bp+disp]
        push_bp_disp(out, disp);
        locals.fpu_live.set(Some(i));
        return;
    }
    // Float/double array element store `a[K] = K.Ff`:
    //   9B <D9|DD> 06 <off16>   fld  <width> [$T]            (FIDRQQ + FloatLoad)
    //   9B <D9|DD> 5E <disp>    fstp <width> [bp+disp]       (FIDRQQ)
    // No fwait here — it is flushed (90 9B) before the next non-FP statement
    // via `fpu_pending_fwait`.
    if let AssignTarget::IndexedLocal { local, byte_off } = target
        && locals.is_float_local(local)
        && let Expr::FloatLit(bits, _) = value
    {
        let width = locals.float_local_width(local);
        let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
        let disp = locals.disp(local) + byte_off as i16;
        fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
        out.push(0x9B);
        out.push(op);
        out.push(0x06);
        let bo = out.len() - 1;
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: bo, kind: FixupKind::FloatLoad { bits: *bits, width } });
        fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
        out.push(0x9B);
        out.push(op);
        out.push(bp_modrm(0x5E, disp));
        push_bp_disp(out, disp);
        locals.fpu_pending_fwait.set(true);
        return;
    }
    // Near pointer = address-of-global (`int *p = &g` or `&g[K]`): MSC stores
    // the link-time OFFSET directly with `c7 46 disp <off16>` (GlobalAddr fixup
    // whose placeholder carries the +K byte offset), not via AX.
    if let AssignTarget::Local(i) = target
        && !locals.is_far_ptr_local(i)
        && let Some((g, off)) = match value {
            Expr::AddrOfGlobal(g) => Some((*g, 0u16)),
            Expr::BinOp { op: BinOp::Add, left, right }
                if matches!(left.as_ref(), Expr::AddrOfGlobal(_))
                    && matches!(right.as_ref(), Expr::IntLit(_)) =>
            {
                let Expr::AddrOfGlobal(g) = **left else { unreachable!() };
                let Expr::IntLit(k) = **right else { unreachable!() };
                Some((g, (k as u32 & 0xFFFF) as u16))
            }
            _ => None,
        }
    {
        let disp = locals.disp(i);
        out.push(0xC7);
        out.push(bp_modrm(0x46, disp));
        push_bp_disp(out, disp);
        let body_offset = out.len() - 1; // last disp byte; off16 placeholder follows
        out.extend_from_slice(&off.to_le_bytes());
        fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: g } });
        return;
    }
    // Near pointer = string-literal address (`char *s = "lit"`): store the
    // link-time CONST offset directly with `c7 46 disp <off16>` + StrLoad fixup,
    // exactly like the address-of-global store above. Fixtures 157, 1464.
    if let AssignTarget::Local(i) = target
        && !locals.is_far_ptr_local(i)
        && let Expr::StrLit(string_idx) = value
    {
        let disp = locals.disp(i);
        out.push(0xC7);
        out.push(bp_modrm(0x46, disp));
        push_bp_disp(out, disp);
        let body_offset = out.len() - 1; // last disp byte; off16 placeholder follows
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset, kind: FixupKind::StrLoad { string_idx: *string_idx } });
        return;
    }
    // Function-pointer local = address-of-function (`int (*p)() = f`): store the
    // link-time `OFFSET _f` directly with `c7 46 disp <off16>` + FuncAddr fixup,
    // exactly like the address-of-global store above. Fixtures 110, 187, 2211.
    if let AssignTarget::Local(i) = target
        && !locals.is_far_ptr_local(i)
        && let Expr::FuncAddr(name) = value
    {
        let disp = locals.disp(i);
        out.push(0xC7);
        out.push(bp_modrm(0x46, disp));
        push_bp_disp(out, disp);
        let body_offset = out.len() - 1; // last disp byte; off16 placeholder follows
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset, kind: FixupKind::FuncAddr { target: symbol_name(name) } });
        return;
    }
    let local_idx = match target {
        AssignTarget::Local(i) => i,
        AssignTarget::Param(i) => {
            return emit_assign_param(i, value, locals, out, fixups);
        }
        AssignTarget::DerefParam(i) => {
            return emit_assign_deref_param(i, value, locals, out, fixups);
        }
        AssignTarget::Global(g) => {
            return emit_assign_global(g, value, locals, out, fixups);
        }
        AssignTarget::DerefGlobal(g) => {
            return emit_assign_deref_global(g, value, locals, out, fixups);
        }
        AssignTarget::DerefLocal(li) => {
            return emit_assign_deref_local(li, value, locals, out, fixups);
        }
        AssignTarget::DerefLocalByte(li) => {
            // `*<char-ptr local> = v` → `mov bx,[bp-p]; mov byte [bx], imm/al`.
            let disp = locals.disp(li);
            out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); // mov bx,[bp-p]
            if let Some(k) = value.fold(locals.inits) {
                out.extend_from_slice(&[0xC6, 0x07, (k as u32 & 0xFF) as u8]); // mov byte [bx], imm
            } else {
                emit_expr_to_ax(value, locals, out, fixups);
                out.extend_from_slice(&[0x88, 0x07]); // mov [bx], al
            }
            return;
        }
        AssignTarget::DerefLocalOffset { local, byte_off, is_byte } => {
            return emit_assign_deref_local_offset(local, byte_off, is_byte, value, locals, out, fixups);
        }
        AssignTarget::ParamIndexStore { param, index, elem } => {
            let is_byte = elem == 1;
            let st = if is_byte { 0x88u8 } else { 0x89 }; // mov [..],al / [..],ax
            let bx_modrm = |off: i32| -> u8 { if off == 0 { 0x07 } else if (-128..=127).contains(&off) { 0x47 } else { 0x87 } };
            let push_off = |out: &mut Vec<u8>, off: i32| {
                if off == 0 {} else if (-128..=127).contains(&off) { out.push(off as u8); }
                else { out.extend_from_slice(&(off as u16).to_le_bytes()); }
            };
            if let Some(k) = index.fold(locals.inits) {
                let off = k * elem as i32;
                out.extend_from_slice(&[0x8B, 0x5E, param_disp(param) as u8]); // mov bx,[bp+pd]
                if let Some(v) = value.fold(locals.inits) {
                    out.push(if is_byte { 0xC6 } else { 0xC7 });
                    out.push(bx_modrm(off)); push_off(out, off);
                    if is_byte { out.push((v as u32 & 0xFF) as u8); }
                    else { out.extend_from_slice(&((v as u32 & 0xFFFF) as u16).to_le_bytes()); }
                } else {
                    emit_expr_to_ax(&value, locals, out, fixups);
                    out.push(st); out.push(bx_modrm(off)); push_off(out, off);
                }
            } else {
                crate::codegen::expr::emit_load_bx(&index, locals, out, fixups);
                if !is_byte { out.extend_from_slice(&[0xD1, 0xE3]); }
                crate::codegen::expr::emit_load_si(&Expr::Param(param), locals, out, fixups);
                if let Some(v) = value.fold(locals.inits) {
                    // Constant RHS → immediate store `mov byte/word [bx+si], imm`.
                    out.push(if is_byte { 0xC6 } else { 0xC7 });
                    out.push(0x00); // [bx+si]
                    if is_byte { out.push((v as u32 & 0xFF) as u8); }
                    else { out.extend_from_slice(&((v as u32 & 0xFFFF) as u16).to_le_bytes()); }
                } else {
                    emit_expr_to_ax(&value, locals, out, fixups);
                    out.push(st); out.push(0x00); // [bx+si]
                }
            }
            return;
        }
        AssignTarget::Index2D { is_global, base, row, col, cols, elem } => {
            assert!(is_global, "local 2-D runtime store should have const-folded");
            crate::codegen::expr::emit_index2d_regs(&row, &col, cols, elem, locals, out, fixups);
            emit_expr_to_ax(&value, locals, out, fixups); // value → AX
            // mov [base + bx + si], ax/al  (modrm [bx+si]+disp16 = 0x80)
            out.push(if elem == 1 { 0x88 } else { 0x89 });
            let mp = out.len();
            out.push(0x80);
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: mp, kind: FixupKind::GlobalAddr { global_idx: base } });
            return;
        }
        AssignTarget::IndexedGlobal { array, byte_off } => {
            return emit_assign_indexed_global(array, byte_off, value, locals, out, fixups);
        }
        AssignTarget::IndexedGlobalByte { array, byte_off } => {
            return emit_assign_indexed_global_byte(array, byte_off, value, locals, out, fixups);
        }
        AssignTarget::IndexedGlobalVar { array, index } => {
            return emit_assign_indexed_global_var(array, index.as_ref(), false, value, locals, out, fixups);
        }
        AssignTarget::IndexedGlobalByteVar { array, index } => {
            return emit_assign_indexed_global_var(array, index.as_ref(), true, value, locals, out, fixups);
        }
        AssignTarget::StructGlobalCopy { dst, src, bytes } => {
            return emit_struct_global_copy(dst, src, bytes, out, fixups);
        }
        AssignTarget::StructLocalCopy { dst, src, bytes } => {
            // `mov ax,[src]; [mov dx,[src+2];] mov [dst],ax; [mov [dst+2],dx]`.
            let sd = locals.disp(src);
            let dd = locals.disp(dst);
            out.push(0x8B); out.push(bp_modrm(0x46, sd)); push_bp_disp(out, sd); // mov ax,[src]
            if bytes > 2 {
                let h = sd + 2;
                out.push(0x8B); out.push(bp_modrm(0x56, h)); push_bp_disp(out, h); // mov dx,[src+2]
            }
            out.push(0x89); out.push(bp_modrm(0x46, dd)); push_bp_disp(out, dd); // mov [dst],ax
            if bytes > 2 {
                let h = dd + 2;
                out.push(0x89); out.push(bp_modrm(0x56, h)); push_bp_disp(out, h); // mov [dst+2],dx
            }
            return;
        }
        AssignTarget::PtrIndexByte { ptr, disp } => {
            return emit_assign_ptr_index_byte(ptr, disp, value, locals, out, fixups);
        }
        AssignTarget::IndexedLocal { local, byte_off } => {
            return emit_assign_indexed_local(local, byte_off, value, locals, out, fixups);
        }
        AssignTarget::IndexedLocalByte { local, byte_off } => {
            return emit_assign_indexed_local_byte(local, byte_off, value, locals, out, fixups);
        }
        AssignTarget::IndexedLocalVar { local, index } => {
            return emit_assign_indexed_local_var(local, index.as_ref(), value, locals, out, fixups);
        }
        AssignTarget::IndexedLocalByteVar { local, index } => {
            return emit_assign_indexed_local_byte_var(local, index.as_ref(), value, locals, out, fixups);
        }
        AssignTarget::LocalField { local, byte_off, size } => {
            return emit_assign_local_field(local, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::ParamField { param, byte_off, size } => {
            return emit_assign_param_field(param, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::DerefLocalField { ptr_local, byte_off, size } => {
            return emit_assign_deref_local_field(ptr_local, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::GlobalField { global, byte_off, size } => {
            return emit_assign_global_field(global, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::BitField { base, byte_off, bit_off, bit_width } => {
            return emit_assign_bitfield(base, byte_off, bit_off, bit_width, value, locals, out, fixups);
        }
        AssignTarget::StructArrayField { array, index, stride, field_off, size } => {
            // `arr[i].field = v`: `mov bx,[i]; <scale bx>; <v→ax>; mov [_arr+bx+off],ax`.
            emit_load_bx(&index, locals, out, fixups);
            let s = stride as usize;
            if s.is_power_of_two() {
                for _ in 0..s.trailing_zeros() { out.extend_from_slice(&[0xD1, 0xE3]); }
            } else {
                out.push(0x69); out.push(0xDB); out.extend_from_slice(&stride.to_le_bytes());
            }
            emit_expr_to_ax(&value, locals, out, fixups); // v → AX
            if size == 1 {
                if out.last() == Some(&0x98) { out.pop(); } // storing AL — strip cbw
                out.push(0x88);
            } else {
                out.push(0x89);
            }
            out.push(0x87);
            let bo = out.len();
            out.extend_from_slice(&field_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: array } });
            return;
        }
        AssignTarget::DerefParamField { ptr_param, byte_off, size } => {
            return emit_assign_deref_param_field(ptr_param, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::DerefGlobalField { ptr_global, byte_off, size } => {
            return emit_assign_deref_global_field(ptr_global, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::DoubleDerefGlobal(g) => {
            return emit_assign_double_deref_global(g, value, locals, out, fixups);
        }
        AssignTarget::DoubleDerefLocal(pp) => {
            // Fallback (alias didn't resolve it): `mov bx,[bp-pp]; mov bx,[bx];
            // <eval value→AX>; mov [bx], ax`.
            let disp = locals.disp(pp);
            out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); // mov bx,[bp-pp]
            out.extend_from_slice(&[0x8B, 0x1F]); // mov bx, [bx]
            emit_expr_to_ax(value, locals, out, fixups);
            out.extend_from_slice(&[0x89, 0x07]); // mov [bx], ax
            return;
        }
        AssignTarget::DoubleDerefParam(pp) => {
            // `**pp = value` through an `int **` param: `mov bx,[bp+pdisp];
            // mov bx,[bx]; <store value through BX>`. A constant value uses
            // the `c7 07 imm16` direct form (fixture 2906); otherwise the
            // value is evaluated to AX and stored `89 07` (fixtures 2680/3479).
            let disp = param_disp(pp);
            out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); // mov bx,[bp+pp]
            out.extend_from_slice(&[0x8B, 0x1F]); // mov bx, [bx]
            if let Some(k) = value.fold(locals.inits) {
                out.extend_from_slice(&[0xC7, 0x07]); // mov word ptr [bx], imm16
                out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
            } else {
                emit_expr_to_ax(value, locals, out, fixups);
                out.extend_from_slice(&[0x89, 0x07]); // mov [bx], ax
            }
            return;
        }
        AssignTarget::DerefPostMutateLocal { local_idx, step } => {
            return emit_assign_deref_postmutate_local(local_idx, step, value, locals, out, fixups);
        }
        AssignTarget::DerefPostMutateParam { param_idx, step } => {
            return emit_assign_deref_postmutate_param(param_idx, step, value, locals, out, fixups);
        }
    };
    let disp = locals.disp(local_idx);
    // `<struct-local> = <struct-returning call>`: a call returning a struct by
    // value leaves it in AX (<=2) / DX:AX (3-4), or AX = &temp (>4). Store the
    // words into the dest, or movsw-copy from the temp. Fixtures 2614, 2352.
    if let Expr::Call { name, args } = value
        && let Some(&bytes) = locals.struct_return_funcs.get(&symbol_name(name))
    {
        emit_call_inner(name, args, locals, false, out, fixups);
        if bytes <= 4 {
            out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov [dest],ax
            if bytes > 2 {
                let hi = disp + 2;
                out.push(0x89); out.push(bp_modrm(0x56, hi)); push_bp_disp(out, hi); // mov [dest+2],dx
            }
        } else {
            // > 4 bytes: callee returned AX = &temp; movsw-copy into the dest
            // local (`lea di,[dest]; mov si,ax; push ss; pop es; movsw`).
            out.push(0x8D); out.push(bp_modrm(0x7E, disp)); push_bp_disp(out, disp); // lea di,[bp+disp]
            out.extend_from_slice(&[0x8B, 0xF0]); // mov si, ax
            out.extend_from_slice(&[0x16, 0x07]); // push ss; pop es
            for _ in 0..(bytes / 2) { out.push(0xA5); } // movsw
        }
        return;
    }
    // Long local compound mul/div/mod (`a *= r`, `a /= r`, `a %= r`): a runtime
    // helper taking the RHS long (DX:AX, pushed high-then-low) and the local's
    // ADDRESS. Mirrors the long-global path (emit_assign_global) but pushes
    // `lea ax,[bp+disp]` instead of `mov ax,OFFSET g`. Helper named by
    // signedness. Fixtures 345/346/347/766/778/779/820.
    if locals.is_long_local(local_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
        && matches!(left.as_ref(), Expr::Local(l) if *l == local_idx)
    {
        let helper = match (op, locals.is_unsigned_local(local_idx)) {
            (BinOp::Mul, false) => "__aNNalmul",
            (BinOp::Mul, true) => "__aNNaulmul",
            (BinOp::Div, false) => "__aNNaldiv",
            (BinOp::Div, true) => "__aNNauldiv",
            (BinOp::Mod, false) => "__aNNalrem",
            (BinOp::Mod, true) => "__aNNaulrem",
            _ => unreachable!(),
        };
        // RHS long → DX:AX. A KNOWN long value (incl. a long-local operand whose
        // init lives in locals.inits) materializes as `mov ax,lo; cwd` (or
        // `mov ax,lo; mov dx,hi` when it exceeds a sign-extended i16), matching
        // MSC's const materialization rather than a two-word slot load. 345/346/347.
        if let Some(k) = right.fold(locals.inits) {
            emit_long_const_to_dx_ax(k, out);
        } else {
            emit_long_to_dx_ax(right, locals, out, fixups);
        }
        out.push(0x52); // push dx
        out.push(0x50); // push ax
        out.push(0x8D); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // lea ax,[bp+disp]
        out.push(0x50); // push ax
        let call = out.len();
        out.extend_from_slice(&[0xE8, 0x00, 0x00]); // call helper
        fixups.push(Fixup { body_offset: call, kind: FixupKind::ExtCall { target: helper.to_owned() } });
        return;
    }
    // Far/huge pointer assignment: store 2-byte offset + 2-byte SS segment.
    // The offset comes from either an address expression (AddrOfLocal,
    // AddrOfLocal+K, or an array-local decay) or a general expression.
    // The segment is always SS since all local/stack addresses use SS.
    if locals.is_far_ptr_local(local_idx) {
        let offset_disp = disp;
        let segment_disp = disp + 2;
        // Peephole: `p = p ± K` on a far ptr → add/sub only the offset word.
        // Emits `add/sub word ptr [bp-disp], K` (fixture 1651: p++).
        if let Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } = value
            && let Expr::Local(li) = left.as_ref()
            && *li == local_idx
            && let Some(k) = right.fold(locals.inits)
        {
            let k_u16 = (k.unsigned_abs() as u32 & 0xFFFF) as u16;
            let is_sub = matches!(op, BinOp::Sub);
            let modrm = if is_sub { 0x6Eu8 } else { 0x46u8 };
            if k_u16 == 1 {
                out.push(0xFF);
                out.push(bp_modrm(modrm, offset_disp));
                push_bp_disp(out, offset_disp);
            } else if k_u16 <= 127 {
                out.push(0x83);
                out.push(bp_modrm(modrm, offset_disp));
                push_bp_disp(out, offset_disp);
                out.push(k_u16 as u8);
            } else {
                out.push(0x81);
                out.push(bp_modrm(modrm, offset_disp));
                push_bp_disp(out, offset_disp);
                out.extend_from_slice(&k_u16.to_le_bytes());
            }
            return;
        }
        match value {
            Expr::AddrOfLocal(j) => {
                let j_disp = locals.disp(*j);
                out.push(0x8D); out.push(bp_modrm(0x46, j_disp)); push_bp_disp(out, j_disp);
                out.push(0x89); out.push(bp_modrm(0x46, offset_disp)); push_bp_disp(out, offset_disp);
                out.push(0x8C); out.push(bp_modrm(0x56, segment_disp)); push_bp_disp(out, segment_disp);
            }
            Expr::Local(j) if locals.is_array_local(*j) => {
                // Array-to-pointer decay: p = (far *)a → lea not load.
                let j_disp = locals.disp(*j);
                out.push(0x8D); out.push(bp_modrm(0x46, j_disp)); push_bp_disp(out, j_disp);
                out.push(0x89); out.push(bp_modrm(0x46, offset_disp)); push_bp_disp(out, offset_disp);
                out.push(0x8C); out.push(bp_modrm(0x56, segment_disp)); push_bp_disp(out, segment_disp);
            }
            _ => {
                emit_expr_to_ax(value, locals, out, fixups);
                out.push(0x89); out.push(bp_modrm(0x46, offset_disp)); push_bp_disp(out, offset_disp);
                out.push(0x8C); out.push(bp_modrm(0x56, segment_disp)); push_bp_disp(out, segment_disp);
            }
        }
        return;
    }
    // Peephole: `x = x ± K` for int locals collapses to an in-place
    // memory op:
    //   K == 1: `inc/dec word ptr [bp-disp]`     (3 bytes)
    //   |K| ≤ 127, ±: `add/sub [bp-disp], imm8sx` (4 bytes)
    //   larger K, ±: `add/sub [bp-disp], imm16`   (5 bytes)
    // Pattern requires LHS = Local(this) on the BinOp.
    // `a *= K` (K not a power of 2) — MSC's idiom:
    //   `mov ax, K`           (3 bytes; or shorter for ±1, but K=0/±1
    //                          should fold elsewhere)
    //   `imul word [bp-disp]` (3 bytes; F7 /5 r/m)
    //   `mov [bp-disp], ax`   (3 bytes)
    // Same total as `load; imul ax, ax, K; store` but different bytes;
    // MSC consistently picks this shape. Fixture 1243.
    if locals.size(local_idx) == 2
        && let Expr::BinOp { op: BinOp::Mul, left, right } = value
        && let Expr::Local(li) = left.as_ref()
        && *li == local_idx
        && let Some(k) = right.fold(locals.inits)
        && k != 0 && k != 1 && (k & (k - 1)) != 0
    {
        let k16 = (k as u32 & 0xFFFF) as u16;
        out.push(0xB8);
        out.extend_from_slice(&k16.to_le_bytes());
        out.push(0xF7); out.push(bp_modrm(0x6E, disp)); push_bp_disp(out, disp);
        out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        return;
    }
    // Shift/mul peephole on locals:
    //   word (`int x`): `d1 modrm disp` for K=1, else `b1 K d3 modrm disp`.
    //   byte (`char c`): `d0 modrm disp` for K=1, else `b1 K d2 modrm disp`.
    //   modrm = 0x66 (shl, /4) or 0x7e (sar, /7).
    //   `<<= K`, `*= 2^k` → shl.   `>>= K` → sar (signed).
    if let Expr::BinOp { op, left, right } = value
        && let Expr::Local(li) = left.as_ref()
        && *li == local_idx
        && let Some(k) = right.fold(locals.inits)
    {
        let (kind, shift_k) = match (op, k) {
            (BinOp::Shl, k) if k > 0 && k < 16 => (Some(0x66u8), k as u8),
            // `>>=`: SAR (/7, 0x7E) for signed, SHR (/5, 0x6E) for unsigned.
            (BinOp::Shr, k) if k > 0 && k < 16 =>
                (Some(if locals.is_unsigned_local(local_idx) { 0x6Eu8 } else { 0x7Eu8 }), k as u8),
            (BinOp::Mul, k) if k >= 2 && (k & (k - 1)) == 0 => {
                let mut bits = 0u8;
                let mut v = k as u32;
                while v > 1 { bits += 1; v >>= 1; }
                (Some(0x66u8), bits)
            }
            _ => (None, 0),
        };
        if let Some(modrm) = kind {
            let is_byte = locals.size(local_idx) == 1;
            let (one_op, cl_op) = if is_byte { (0xD0u8, 0xD2u8) } else { (0xD1u8, 0xD3u8) };
            if shift_k == 1 {
                out.push(one_op); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp);
            } else {
                out.push(0xB1); out.push(shift_k);
                out.push(cl_op); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp);
            }
            return;
        }
    }
    // Bitwise mem-op peephole: `x |= K`, `x &= K`, `x ^= K` collapse
    // to `op byte/word [bp-disp], imm`. Encoding rules differ by op:
    // AND: if high byte of imm16 is 0xFF (negative i16) → byte form `80`
    //      (low byte clears/sets bits, high byte AND 0xFF = identity).
    //      Special: if low byte is also 0 (clearing low byte entirely) →
    //      `c6 46 disp 0` (mov byte, 0), same effect as `80 ... 00`.
    //      Otherwise (high byte = 0x00, positive small) → word form `81`.
    // OR/XOR: if 0 ≤ imm ≤ 255 → byte form `80` (OR/XOR with 0x00 on
    //      high byte is identity, so byte op gives correct 16-bit result).
    //      Otherwise → sign-extended `83` or full-word `81`.
    if let Expr::BinOp { op, left, right } = value
        && let Expr::Local(li) = left.as_ref()
        && *li == local_idx
        && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && let Some(k) = right.fold(locals.inits)
    {
        // ModR/M reg field /1=or, /4=and, /6=xor; r/m=110 (BP-rel disp8)
        let reg = match op {
            BinOp::BitAnd => 4u8,
            BinOp::BitOr => 1u8,
            BinOp::BitXor => 6u8,
            _ => unreachable!(),
        };
        let modrm = 0x46 | (reg << 3);
        // Long local `x &= K`: AND the low word (81 imm16), then the high word —
        // `mov word [hi],0` when K's high word is 0, `and word [hi],K_hi` when
        // partial, omitted when K_hi is 0xFFFF (identity). Fixtures 289, 342.
        if locals.is_long_local(local_idx) && matches!(op, BinOp::BitAnd) {
            let k32 = k as u32;
            let lo = (k32 & 0xFFFF) as u16;
            let hi = (k32 >> 16) as u16;
            let hi_disp = disp + 2;
            out.push(0x81); out.push(bp_modrm(0x66, disp)); push_bp_disp(out, disp);
            out.extend_from_slice(&lo.to_le_bytes());
            if hi == 0x0000 {
                out.push(0xC7); out.push(bp_modrm(0x46, hi_disp)); push_bp_disp(out, hi_disp);
                out.extend_from_slice(&0u16.to_le_bytes());
            } else if hi != 0xFFFF {
                out.push(0x81); out.push(bp_modrm(0x66, hi_disp)); push_bp_disp(out, hi_disp);
                out.extend_from_slice(&hi.to_le_bytes());
            }
            return;
        }
        let is_byte_slot = locals.size(local_idx) == 1;
        if is_byte_slot {
            let imm = (k as u32 & 0xFF) as u8;
            out.push(0x80); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp); out.push(imm);
            return;
        }
        let imm16 = (k as u32 & 0xFFFF) as u16;
        let imm8 = (imm16 & 0xFF) as u8;
        let high = (imm16 >> 8) as u8;
        match op {
            BinOp::BitAnd => {
                if high == 0xFF {
                    if imm8 == 0x00 {
                        // AND with 0xFF00: clear low byte via mov byte.
                        out.push(0xC6); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); out.push(0x00);
                    } else {
                        // AND with 0xFFxx: byte form (high byte ANDs with 0xFF = identity).
                        out.push(0x80); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp); out.push(imm8);
                    }
                } else if imm8 == 0xFF {
                    // AND with 0xxxFF: only the high byte changes (low byte AND
                    // 0xFF = identity) → byte op on the high byte. Fixture 1942
                    // (`&= 0x00FF` → high byte AND 0x00 = `mov byte [hi],0`).
                    let hi_disp = disp + 1;
                    if high == 0x00 {
                        out.push(0xC6); out.push(bp_modrm(0x46, hi_disp)); push_bp_disp(out, hi_disp); out.push(0x00);
                    } else {
                        out.push(0x80); out.push(bp_modrm(modrm, hi_disp)); push_bp_disp(out, hi_disp); out.push(high);
                    }
                } else {
                    // AND affecting both bytes: word form.
                    out.push(0x81); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp);
                    out.extend_from_slice(&imm16.to_le_bytes());
                }
            }
            BinOp::BitOr | BinOp::BitXor => {
                if imm16 <= 0xFF {
                    // Small non-negative: byte form (high byte OR/XOR 0x00 = identity).
                    out.push(0x80); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp); out.push(imm8);
                } else if imm8 == 0x00 {
                    // OR/XOR with 0xXX00: only the high byte changes (low byte
                    // OR/XOR 0x00 = identity) → byte op on the high byte.
                    // Fixture 1715 (`^= 0x0500` → `xor byte [hi],0x05`).
                    let hi_disp = disp + 1;
                    out.push(0x80); out.push(bp_modrm(modrm, hi_disp)); push_bp_disp(out, hi_disp); out.push(high);
                } else if let Ok(k8) = i8::try_from(k) {
                    out.push(0x83); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp); out.push(k8 as u8);
                } else {
                    out.push(0x81); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp);
                    out.extend_from_slice(&imm16.to_le_bytes());
                }
            }
            _ => unreachable!(),
        }
        return;
    }
    if let Expr::BinOp { op, left, right } = value
        && let Expr::Local(li) = left.as_ref()
        && *li == local_idx
        && matches!(op, BinOp::Add | BinOp::Sub)
        && let Some(k) = right.fold(locals.inits)
    {
        let is_byte = locals.size(local_idx) == 1;
        match (op, k, is_byte) {
            (BinOp::Add, 1, false) if !locals.is_long_local(local_idx) => {
                out.push(0xFF); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                return;
            }
            (BinOp::Sub, 1, false) if !locals.is_long_local(local_idx) => {
                out.push(0xFF); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp);
                return;
            }
            (BinOp::Add, 1, true) => {
                out.push(0xFE); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                return;
            }
            (BinOp::Sub, 1, true) => {
                out.push(0xFE); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp);
                return;
            }
            (op, _, false) => {
                let modrm_base = if matches!(op, BinOp::Add) { 0x46u8 } else { 0x6Eu8 };
                if locals.is_long_local(local_idx) {
                    // Long local `x ±= K`: add/sub low half, then adc/sbb high half.
                    let low = (k as u32 & 0xFFFF) as i16;
                    let high = ((k as i32) >> 16) as i16;
                    let hi_disp = disp + 2;
                    let high_modrm = if matches!(op, BinOp::Add) { 0x56u8 } else { 0x5Eu8 };
                    if let Ok(l8) = i8::try_from(low) {
                        out.push(0x83); out.push(bp_modrm(modrm_base, disp)); push_bp_disp(out, disp); out.push(l8 as u8);
                    } else {
                        out.push(0x81); out.push(bp_modrm(modrm_base, disp)); push_bp_disp(out, disp);
                        out.extend_from_slice(&(low as u16).to_le_bytes());
                    }
                    if let Ok(h8) = i8::try_from(high) {
                        out.push(0x83); out.push(bp_modrm(high_modrm, hi_disp)); push_bp_disp(out, hi_disp); out.push(h8 as u8);
                    } else {
                        out.push(0x81); out.push(bp_modrm(high_modrm, hi_disp)); push_bp_disp(out, hi_disp);
                        out.extend_from_slice(&(high as u16).to_le_bytes());
                    }
                    return;
                }
                if let Ok(k8) = i8::try_from(k) {
                    out.push(0x83); out.push(bp_modrm(modrm_base, disp)); push_bp_disp(out, disp); out.push(k8 as u8);
                } else {
                    out.push(0x81); out.push(bp_modrm(modrm_base, disp)); push_bp_disp(out, disp);
                    let imm = (k as u32 & 0xFFFF) as u16;
                    out.extend_from_slice(&imm.to_le_bytes());
                }
                return;
            }
            (op, _, true) => {
                // `80 /0 imm8` add byte mem / `80 /5 imm8` sub byte mem.
                let modrm = if matches!(op, BinOp::Add) { 0x46u8 } else { 0x6Eu8 };
                out.push(0x80); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp);
                out.push((k as u32 & 0xFF) as u8);
                return;
            }
        }
    }
    // Char-local compound mul/div/mod by a foldable RHS → BYTE op (mirrors the
    // char-global path). mul: `mov al,K; imul|mul byte [c]; mov [c],al`. div:
    // `mov cl,K; mov al,[c]; <extend>; idiv|div cl; mov [c],al`. mod: `…; mov
    // [c],ah; mov al,ah`. The result is widened in place (`cbw` signed / `sub
    // ah,ah` unsigned) so a following `return c` reuses it. Fixtures 1436 (signed),
    // 677/678/679 (unsigned: zero-extend, `div`/`mul`, `sub ah,ah` widen).
    if locals.size(local_idx) == 1
        && let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Local(l) if *l == local_idx)
        && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
        && let Some(k) = right.fold(locals.inits)
    {
        let uns = locals.is_unsigned_local(local_idx);
        let k8 = (k as u32 & 0xFF) as u8;
        let dm = |out: &mut Vec<u8>, modrm: u8| { out.push(0x88); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp); };
        let widen = |out: &mut Vec<u8>| if uns { out.extend_from_slice(&[0x2A, 0xE4]); } else { out.push(0x98); };
        if matches!(op, BinOp::Mul) {
            out.extend_from_slice(&[0xB0, k8]);                  // mov al, K
            // f6 /5 = imul (signed), f6 /4 = mul (unsigned)
            out.push(0xF6); out.push(bp_modrm(if uns { 0x66 } else { 0x6E }, disp)); push_bp_disp(out, disp);
            dm(out, 0x46);                                       // mov [c], al
        } else {
            out.extend_from_slice(&[0xB1, k8]);                  // mov cl, K
            out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov al,[c]
            widen(out);                                          // cbw / sub ah,ah (extend dividend)
            out.push(0xF6); out.push(if uns { 0xF1 } else { 0xF9 }); // div cl / idiv cl
            if matches!(op, BinOp::Div) {
                dm(out, 0x46);                                   // mov [c], al
            } else {
                dm(out, 0x66);                                   // mov [c], ah
                out.extend_from_slice(&[0x8A, 0xC4]);            // mov al, ah
            }
        }
        widen(out); // widen the value (cbw / sub ah,ah) so `return c` reuses it
        return;
    }
    // Local int `x /= K` / `x %= K` by a foldable RHS: divisor-first idiv (MSC does
    // NOT strength-reduce, even `/= 2`). `mov cx,K; mov ax,[x]; cwd|xor dx,dx;
    // idiv|div cx; mov [x],ax` (div) or `mov [x],dx; mov ax,dx` (mod, result in AX).
    if locals.size(local_idx) == 2 && !locals.is_long_local(local_idx)
        && let Expr::BinOp { op: op @ (BinOp::Div | BinOp::Mod), left, right } = value
        && matches!(left.as_ref(), Expr::Local(l) if *l == local_idx)
        && let Some(k) = right.fold(locals.inits)
    {
        let unsigned = locals.is_unsigned_local(local_idx);
        out.push(0xB9); out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); // mov cx,K
        out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov ax,[x]
        if unsigned { out.extend_from_slice(&[0x2B, 0xD2, 0xF7, 0xF1]); } // sub dx,dx; div cx
        else { out.extend_from_slice(&[0x99, 0xF7, 0xF9]); } // cwd; idiv cx
        if matches!(op, BinOp::Div) {
            out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov [x],ax
        } else {
            out.push(0x89); out.push(bp_modrm(0x56, disp)); push_bp_disp(out, disp); // mov [x],dx
            out.extend_from_slice(&[0x8B, 0xC2]); // mov ax,dx
        }
        return;
    }
    // Add/sub-to-memory peephole: `x = x + other` / `x = x - other` where
    // `other` is a non-constant expression (local/param/global). Emits
    // `load other; add/sub [bp-disp], ax` (6 bytes vs 9 for load-modify-store).
    // Only for non-byte (int/word) targets — byte form would need AL.
    if locals.size(local_idx) == 2
        && let Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } = value
        && let Expr::Local(li) = left.as_ref()
        && *li == local_idx
        && right.fold(locals.inits).is_none()
    {
        emit_expr_to_ax(right, locals, out, fixups);
        let opc = if matches!(op, BinOp::Add) { 0x01u8 } else { 0x29u8 };
        out.push(opc); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        return;
    }
    // `long x = f()` where f returns long: the callee already leaves the full
    // long in DX:AX, so store both words directly — NO `cwd` (which would
    // clobber the real high word with a sign-extension of AX). Fixtures 315/321.
    if locals.is_long_local(local_idx)
        && let Expr::Call { name, .. } = value
        && locals.long_returners.contains(&symbol_name(name))
    {
        emit_long_to_dx_ax(value, locals, out, fixups); // call → DX:AX
        out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);   // mov [lo],ax
        let hi = disp + 2;
        out.push(0x89); out.push(bp_modrm(0x56, hi)); push_bp_disp(out, hi);       // mov [hi],dx
        return;
    }
    // General path: evaluate the RHS into AX, then store.
    let is_byte = locals.size(local_idx) == 1;
    // MSC never const-folds || / && into an immediate store (fixture
    // 1466). For ternary: skip fold only when the condition is a
    // non-comparison truthy check — e.g. `a ? b : c` where a is a
    // local (fixture 1038). Comparison conditions fold normally.
    let fold_val = match value {
        Expr::BinOp { op: BinOp::LogOr | BinOp::LogAnd, .. } => None,
        Expr::Ternary { cond, .. }
            if !matches!(cond.as_ref(), Expr::BinOp {
                op: BinOp::Eq | BinOp::Ne | BinOp::Lt
                    | BinOp::Le | BinOp::Gt | BinOp::Ge, ..
            }) => None,
        _ => value.fold(locals.inits),
    };
    if let Some(k) = fold_val {
        if is_byte {
            out.push(0xC6); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            out.push((k as u32 & 0xFF) as u8);
        } else if locals.is_long_local(local_idx) {
            let hi_disp = disp + 2;
            if k == 0 {
                // `long x = 0`: zero AX once and store both halves from it
                // (`sub ax,ax; mov [lo],ax; mov [hi],ax`) — shorter than two
                // immediate stores. Fixture 568.
                out.extend_from_slice(&[0x2B, 0xC0]); // sub ax,ax
                out.push(0x89); out.push(bp_modrm(0x46, hi_disp)); push_bp_disp(out, hi_disp);
                out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            } else {
                // Long local: two word stores — low half at disp, high half at disp+2.
                let low = (k as u32 & 0xFFFF) as u16;
                let high = (((k as i32) >> 16) as u32 & 0xFFFF) as u16;
                out.push(0xC7); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                out.extend_from_slice(&low.to_le_bytes());
                out.push(0xC7); out.push(bp_modrm(0x46, hi_disp)); push_bp_disp(out, hi_disp);
                out.extend_from_slice(&high.to_le_bytes());
            }
        } else {
            let imm = (k as u32 & 0xFFFF) as u16;
            out.push(0xC7); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            out.extend_from_slice(&imm.to_le_bytes());
        }
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if is_byte {
            // Strip trailing CBW emitted for char globals/fields — AL still
            // holds the byte value and we're storing to a char slot.
            if out.last() == Some(&0x98) {
                out.pop();
            }
            // `88 46 disp` — store AL to char slot.
            out.push(0x88); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        } else if locals.is_long_local(local_idx) && !long_operand(value, locals) {
            // An int expression assigned to a long local: the value is in AX;
            // sign-extend to a full long (`cwd`) and store both words. Fixture
            // 3230 (`long n; n = x + 1`). A genuinely-long RHS is handled by the
            // long-specific paths above and never reaches here.
            out.push(0x99); // cwd
            out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);   // mov [lo],ax
            let hi = disp + 2;
            out.push(0x89); out.push(bp_modrm(0x56, hi)); push_bp_disp(out, hi);       // mov [hi],dx
        } else {
            out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        }
    }
}
/// `<global> = <expr>;`. Constant RHS → `c7 06 addr imm16`
/// (mov word ptr [imm16], imm16, 6 bytes); general RHS →
/// `<expr-to-ax>; a3 addr` (mov moffs16, ax, 3 bytes).
/// Both shapes plant a 2-byte address placeholder that the linker
/// resolves via a GlobalAddr fixup.
/// `*<ptr-param> = <expr>;` — store through a param pointer.
/// `mov bx, [bp+pdisp]; mov [bx], imm/ax`. The store width follows the param's
/// POINTEE size: a `char *p` (pointee 1) stores a byte (`c6 07`/`88 07`); an
/// `int *p` (pointee 2) stores a word (`c7 07`/`89 07`). Fixtures 1225 (char*),
/// 3055 (int*, `*p = 42` — must be a word store, not a byte one).
/// For `*dst = RHS`, detect when RHS is `*src` or `*src ± K` where `src` is a
/// pointer PARAM distinct from `dst`. Returns `(src_param, Option<(op, K)>)`.
/// The same-pointee-width is required so the store width matches the load.
pub(crate) fn deref_param_source(value: &Expr, dst: usize, locals: &Locals<'_>) -> Option<(usize, Option<(BinOp, i32)>)> {
    fn src_of(e: &Expr) -> Option<usize> {
        match e {
            Expr::DerefWord { ptr } | Expr::DerefByte { ptr } => match ptr.as_ref() {
                Expr::Param(s) => Some(*s),
                _ => None,
            },
            _ => None,
        }
    }
    let (src, post) = match value {
        Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } => {
            let s = src_of(left)?;
            let k = right.fold(locals.inits)?;
            (s, Some((*op, k)))
        }
        _ => (src_of(value)?, None),
    };
    if src == dst { return None; }
    // Both must have the same pointee width (byte vs word).
    if (locals.param_pointee_size(src) == 1) != (locals.param_pointee_size(dst) == 1) {
        return None;
    }
    Some((src, post))
}
pub(crate) fn emit_assign_deref_param(param_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let pdisp = param_disp(param_idx);
    let is_byte = locals.param_pointee_size(param_idx) == 1;
    // `*p = *p ± K` (self-modify through a pointer param) → load p into BX, then
    // an in-place memory op on `[bx]`: `inc/dec word [bx]` for ±1, else
    // `add/sub word [bx], K`. Mirrors the local/global self-assign peephole.
    // Fixture 4038 (`*p = *p + 1`).
    if let Expr::BinOp { op, left, right } = value
        && matches!(op, BinOp::Add | BinOp::Sub)
        && matches!(left.as_ref(),
            Expr::DerefWord { ptr } | Expr::DerefByte { ptr } if matches!(ptr.as_ref(), Expr::Param(i) if *i == param_idx))
        && let Some(k) = right.fold(locals.inits)
    {
        out.extend_from_slice(&[0x8B, 0x5E, pdisp as u8]); // mov bx,[bp+p]
        let is_sub = matches!(op, BinOp::Sub);
        if k == 1 || k == -1 {
            // inc/dec [bx]. A `-1` flips the direction.
            let dec = is_sub ^ (k == -1);
            let modrm = if dec { 0x0Fu8 } else { 0x07 }; // /1 dec, /0 inc
            out.push(if is_byte { 0xFE } else { 0xFF });
            out.push(modrm);
        } else {
            let modrm = if is_sub { 0x2Fu8 } else { 0x07 }; // /5 sub, /0 add, r/m=[bx]
            if is_byte {
                out.extend_from_slice(&[0x80, modrm, (k as u32 & 0xFF) as u8]);
            } else if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x83, modrm, k8 as u8]);
            } else {
                out.push(0x81); out.push(modrm);
                out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
            }
        }
        return;
    }
    // `*dst = *src` / `*dst = *src ± K` (copy through one pointer param from a
    // deref of ANOTHER pointer param): dst stays in BX for the store, so the
    // source deref uses SI. `mov bx,[dst]; mov si,[src]; mov ax,[si]; [op];
    // mov [bx],ax`. Requires a push-SI frame. Fixtures 2993, 3184.
    if let Some((src, post)) = deref_param_source(value, param_idx, locals) {
        let sdisp = param_disp(src);
        out.extend_from_slice(&[0x8B, 0x5E, pdisp as u8]);  // mov bx,[bp+dst]
        out.push(0x8B); out.push(bp_modrm(0x76, sdisp)); push_bp_disp(out, sdisp); // mov si,[bp+src]
        if is_byte {
            out.extend_from_slice(&[0x8A, 0x04]); // mov al,[si]
        } else {
            out.extend_from_slice(&[0x8B, 0x04]); // mov ax,[si]
        }
        if let Some((op, k)) = post {
            // Apply the trailing `± K` to AX (word). ±1 → inc/dec ax.
            let is_sub = matches!(op, BinOp::Sub);
            if k == 1 || k == -1 {
                let dec = is_sub ^ (k == -1);
                out.push(if dec { 0x48 } else { 0x40 }); // dec/inc ax
            } else {
                let opc = if is_sub { 0x2Du8 } else { 0x05 }; // sub/add ax, imm16
                out.push(opc);
                out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
            }
        }
        if is_byte {
            out.extend_from_slice(&[0x88, 0x07]); // mov [bx],al
        } else {
            out.extend_from_slice(&[0x89, 0x07]); // mov [bx],ax
        }
        return;
    }
    out.push(0x8B);
    out.push(0x5E);
    out.push(pdisp as u8);
    if let Some(k) = value.fold(locals.inits) {
        if is_byte {
            out.extend_from_slice(&[0xC6, 0x07, (k as u32 & 0xFF) as u8]); // mov byte [bx], k
        } else {
            out.push(0xC7);
            out.push(0x07);
            let imm = (k as u32 & 0xFFFF) as u16;
            out.extend_from_slice(&imm.to_le_bytes()); // mov word [bx], imm16
        }
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if is_byte {
            if out.last() == Some(&0x98) { out.pop(); } // strip cbw — storing AL
            out.extend_from_slice(&[0x88, 0x07]); // mov [bx], al
        } else {
            out.extend_from_slice(&[0x89, 0x07]); // mov [bx], ax
        }
    }
}
/// `<param> = <expr>;` — modify the function's local copy. Same
/// peepholes as `emit_assign` apply but with disp8 = `param_disp(i)`
/// (a positive value `[bp+disp]`). Fixture 1224.
pub(crate) fn emit_assign_param(param_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let disp = param_disp(param_idx);
    let is_char = locals.is_char_param(param_idx);
    // `x++` / `x--` → `inc/dec [bp+disp]` (byte or word based on param type).
    // Long param `n ±= K`: add/sub the low half then adc/sbb the high half.
    // The carry must propagate, so even ±1 uses add/adc (not inc). Fixture
    // 3094 (`n += 1L`).
    if locals.is_long_param(param_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Param(i) if *i == param_idx)
        && matches!(op, BinOp::Add | BinOp::Sub)
        && let Some(k) = right.fold(locals.inits)
    {
        let low = (k as u32 & 0xFFFF) as i16;
        let high = ((k as i32) >> 16) as i16;
        let lo_modrm = if matches!(op, BinOp::Add) { 0x46u8 } else { 0x6Eu8 }; // /0 add, /5 sub
        let hi_modrm = if matches!(op, BinOp::Add) { 0x56u8 } else { 0x5Eu8 }; // /2 adc, /3 sbb
        let hi_disp = (disp + 2) as u8;
        if let Ok(l8) = i8::try_from(low) {
            out.extend_from_slice(&[0x83, lo_modrm, disp as u8, l8 as u8]);
        } else {
            out.extend_from_slice(&[0x81, lo_modrm, disp as u8]);
            out.extend_from_slice(&(low as u16).to_le_bytes());
        }
        if let Ok(h8) = i8::try_from(high) {
            out.extend_from_slice(&[0x83, hi_modrm, hi_disp, h8 as u8]);
        } else {
            out.extend_from_slice(&[0x81, hi_modrm, hi_disp]);
            out.extend_from_slice(&(high as u16).to_le_bytes());
        }
        return;
    }
    // `a += b` (and -=, &=, |=, ^=) where a is a long PARAM and the RHS is a
    // runtime long operand: load the RHS into DX:AX, then two-word op into the
    // param's slot — `<op> [a.lo],ax; <op-carry> [a.hi],dx`. Fixture 3425.
    if locals.is_long_param(param_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Param(i) if *i == param_idx)
        && let Some((lo_op, hi_op)) = match op {
            BinOp::Add => Some((0x01u8, 0x11u8)), // add r/m,ax ; adc r/m,dx
            BinOp::Sub => Some((0x29, 0x19)),     // sub ; sbb
            BinOp::BitAnd => Some((0x21, 0x21)),  // no carry → same op both words
            BinOp::BitOr => Some((0x09, 0x09)),
            BinOp::BitXor => Some((0x31, 0x31)),
            _ => None,
        }
        && long_operand(right, locals)
        && right.fold(locals.inits).is_none()
    {
        emit_long_to_dx_ax(right, locals, out, fixups); // RHS → DX:AX
        let a_lo = long_param_disp(param_idx, locals);
        let a_hi = a_lo + 2;
        out.push(lo_op); out.push(bp_modrm(0x46, a_lo)); push_bp_disp(out, a_lo);
        out.push(hi_op); out.push(bp_modrm(0x56, a_hi)); push_bp_disp(out, a_hi);
        return;
    }
    // Pointer-param self-arithmetic `p = p ± K` (`p += K`) scales K by the
    // pointee size: `int *p; p += 1` → `add WORD PTR [bp+d], 2`. Fixture 2922.
    if locals.param_pointee_size(param_idx) > 1
        && let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Param(i) if *i == param_idx)
        && matches!(op, BinOp::Add | BinOp::Sub)
        && let Some(k) = right.fold(locals.inits)
    {
        let scaled = k * locals.param_pointee_size(param_idx) as i32;
        let add_modrm = if matches!(op, BinOp::Add) { 0x46u8 } else { 0x6Eu8 }; // /0 add, /5 sub
        if let Ok(k8) = i8::try_from(scaled) {
            out.extend_from_slice(&[0x83, add_modrm, disp as u8, k8 as u8]);
        } else {
            out.extend_from_slice(&[0x81, add_modrm, disp as u8]);
            out.extend_from_slice(&((scaled as u32 & 0xFFFF) as u16).to_le_bytes());
        }
        return;
    }
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Param(i) if *i == param_idx)
        && matches!(right.as_ref(), Expr::IntLit(1))
        && matches!(op, BinOp::Add | BinOp::Sub)
    {
        let (pfx, modrm) = if is_char {
            (0xFEu8, if matches!(op, BinOp::Add) { 0x46u8 } else { 0x4Eu8 })
        } else {
            (0xFFu8, if matches!(op, BinOp::Add) { 0x46u8 } else { 0x4Eu8 })
        };
        out.extend_from_slice(&[pfx, modrm, disp as u8]);
        return;
    }
    // `x ± K` peephole on param.
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Param(i) if *i == param_idx)
        && matches!(op, BinOp::Add | BinOp::Sub)
        && let Some(k) = right.fold(locals.inits)
    {
        let add_modrm = if matches!(op, BinOp::Add) { 0x46u8 } else { 0x6Eu8 };
        if is_char {
            let k8 = (k as u32 & 0xFF) as u8;
            out.extend_from_slice(&[0x80, add_modrm, disp as u8, k8]);
        } else if let Ok(k8) = i8::try_from(k) {
            out.extend_from_slice(&[0x83, add_modrm, disp as u8, k8 as u8]);
        } else {
            out.extend_from_slice(&[0x81, add_modrm, disp as u8]);
            let imm = (k as u32 & 0xFFFF) as u16;
            out.extend_from_slice(&imm.to_le_bytes());
        }
        return;
    }
    // Long param `x <<=/>>= K` by a constant. K==1 shifts in place across both
    // words (`shl [lo],1; rcl [hi],1` for <<; `sar/shr [hi],1; rcr [lo],1` for >>,
    // high word first). K>1 calls the runtime helper (count in AL, address pushed).
    if locals.is_long_param(param_idx)
        && let Expr::BinOp { op: op @ (BinOp::Shl | BinOp::Shr), left, right } = value
        && matches!(left.as_ref(), Expr::Param(i) if *i == param_idx)
        && let Some(k) = right.fold(locals.inits)
        && (0..32).contains(&k)
    {
        let lo = disp;
        let hi = disp + 2;
        if k == 1 {
            if matches!(op, BinOp::Shl) {
                out.push(0xD1); out.push(bp_modrm(0x66, lo)); push_bp_disp(out, lo); // shl [lo],1
                out.push(0xD1); out.push(bp_modrm(0x56, hi)); push_bp_disp(out, hi); // rcl [hi],1
            } else {
                let hi_reg = if locals.is_unsigned_param(param_idx) { 0x6Eu8 } else { 0x7Eu8 }; // shr / sar
                out.push(0xD1); out.push(bp_modrm(hi_reg, hi)); push_bp_disp(out, hi);
                out.push(0xD1); out.push(bp_modrm(0x5E, lo)); push_bp_disp(out, lo); // rcr [lo],1
            }
        } else {
            let helper = match (op, locals.is_unsigned_param(param_idx)) {
                (BinOp::Shl, _) => "__aNNalshl",
                (BinOp::Shr, false) => "__aNNalshr",
                (BinOp::Shr, true) => "__aNNaulshr",
                _ => unreachable!(),
            };
            out.extend_from_slice(&[0xB0, k as u8]); // mov al, k
            out.push(0x50); // push ax
            out.push(0x8D); out.push(bp_modrm(0x46, lo)); push_bp_disp(out, lo); // lea ax,[bp+lo]
            out.push(0x50); // push ax
            let call = out.len();
            out.extend_from_slice(&[0xE8, 0x00, 0x00]);
            fixups.push(Fixup { body_offset: call, kind: FixupKind::ExtCall { target: helper.to_owned() } });
        }
        return;
    }
    // `param >>= local/param` peephole: `mov cl, [bp+r]; sar/shl [bp+param], cl`
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Param(i) if *i == param_idx)
        && matches!(op, BinOp::Shl | BinOp::Shr)
        && let Some(r_disp) = bp_disp(right, locals)
    {
        out.push(0x8A); out.push(bp_modrm(0x4E, r_disp)); push_bp_disp(out, r_disp); // mov cl, [bp+r]
        let modrm_base = match op {
            BinOp::Shl => 0x66u8,  // /4 = SHL [bp+disp], cl
            BinOp::Shr => 0x7Eu8,  // /7 = SAR [bp+disp], cl
            _ => unreachable!(),
        };
        out.push(0xD3); out.push(bp_modrm(modrm_base, disp)); push_bp_disp(out, disp); // sar/shl [bp+param], cl
        return;
    }
    // `param <<=/>>= K` (and `param *= 2^k`) by a constant → in-place shift:
    //   word: `d1 modrm disp` for K=1, else `b1 K d3 modrm disp`.
    //   byte (char param): `d0`/`d2`.
    //   modrm reg: SHL=4 (0x66), SHR-unsigned=5 (0x6E), SAR-signed=7 (0x7E).
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Param(i) if *i == param_idx)
        && let Some(k) = right.fold(locals.inits)
    {
        let shift_k = match (op, k) {
            (BinOp::Shl, k) if k > 0 && k < 16 => Some(k as u8),
            (BinOp::Shr, k) if k > 0 && k < 16 => Some(k as u8),
            (BinOp::Mul, k) if k >= 2 && (k & (k - 1)) == 0 => {
                let mut b = 0u8; let mut v = k as u32; while v > 1 { b += 1; v >>= 1; } Some(b)
            }
            _ => None,
        };
        if let Some(sk) = shift_k {
            let reg: u8 = match op {
                BinOp::Shl | BinOp::Mul => 4,
                BinOp::Shr => if locals.is_unsigned_param(param_idx) { 5 } else { 7 },
                _ => unreachable!(),
            };
            let modrm_base = 0x46 | (reg << 3);
            let (one_op, cl_op) = if is_char { (0xD0u8, 0xD2u8) } else { (0xD1u8, 0xD3u8) };
            if sk == 1 {
                out.push(one_op); out.push(bp_modrm(modrm_base, disp)); push_bp_disp(out, disp);
            } else {
                out.push(0xB1); out.push(sk);
                out.push(cl_op); out.push(bp_modrm(modrm_base, disp)); push_bp_disp(out, disp);
            }
            return;
        }
    }
    // Generic: const RHS → store imm; else eval then store.
    if let Some(k) = value.fold(locals.inits) {
        if is_char {
            out.extend_from_slice(&[0xC6, 0x46, disp as u8, (k as u32 & 0xFF) as u8]);
        } else {
            let imm = (k as u32 & 0xFFFF) as u16;
            out.extend_from_slice(&[0xC7, 0x46, disp as u8]);
            out.extend_from_slice(&imm.to_le_bytes());
        }
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if is_char {
            out.extend_from_slice(&[0x88, 0x46, disp as u8]); // mov [bp+d], al
        } else {
            out.extend_from_slice(&[0x89, 0x46, disp as u8]); // mov [bp+d], ax
        }
    }
}
pub(crate) fn emit_assign_global(global_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // `<struct-global> = <struct-returning call>`: store AX (<=2 bytes) /
    // DX:AX (3-4) into the destination struct global. Fixture 424.
    if let Expr::Call { name, args } = value
        && let Some(&bytes) = locals.struct_return_funcs.get(&symbol_name(name))
        && bytes <= 4
    {
        emit_call_inner(name, args, locals, false, out, fixups);
        let bo = out.len();
        out.extend_from_slice(&[0xA3, 0x00, 0x00]); // mov [g], ax
        fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx } });
        if bytes > 2 {
            out.push(0x89); out.push(0x16); // mov [g+2], dx
            let off = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        }
        return;
    }
    // Global pointer = address-of-global (`p = a` / `p = &g[K]`): MSC stores the
    // link-time OFFSET directly into the global with one instruction —
    //   c7 06 <&p> <OFFSET a + K>   mov word ptr [_p], OFFSET _a
    // carrying two GlobalAddr fixups (the destination address and the immediate).
    // Fixtures 888, 890.
    if let Some((a, off)) = match value {
        Expr::AddrOfGlobal(a) => Some((*a, 0u16)),
        Expr::BinOp { op: BinOp::Add, left, right }
            if matches!(left.as_ref(), Expr::AddrOfGlobal(_))
                && matches!(right.as_ref(), Expr::IntLit(_)) =>
        {
            let Expr::AddrOfGlobal(a) = **left else { unreachable!() };
            let Expr::IntLit(k) = **right else { unreachable!() };
            Some((a, (k as u32 & 0xFFFF) as u16))
        }
        _ => None,
    } {
        let start = out.len();
        out.extend_from_slice(&[0xC7, 0x06, 0x00, 0x00]); // mov word [disp16], imm16
        fixups.push(Fixup { body_offset: start + 1, kind: FixupKind::GlobalAddr { global_idx } });
        let imm_bo = out.len() - 1; // last disp byte; imm16 placeholder follows
        out.extend_from_slice(&off.to_le_bytes());
        fixups.push(Fixup { body_offset: imm_bo, kind: FixupKind::GlobalAddr { global_idx: a } });
        return;
    }
    // Float/double global store from a literal: `g = 3.14;` →
    //   9B <D9|DD> 06 <off16>   fld  <dword|qword> [$T]   (FIDRQQ + FloatLoad)
    //   9B <D9|DD> 1E <off16>   fstp <dword|qword> [g]    (FIDRQQ + GlobalAddr)
    //   90 9B                   nop; fwait                (FIWRQQ — emulator slot)
    // The `90 9B` is the 8087-emulator's 2-byte patch slot for the standalone
    // wait, not a parity nop, so it is emitted unconditionally.
    if locals.is_float_global(global_idx)
        && let Expr::FloatLit(bits, _) = value
    {
        let width = locals.float_global_width(global_idx);
        let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
        // fld <width> [$T]
        fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
        out.push(0x9B);
        out.push(op);
        out.push(0x06);
        let bo = out.len() - 1;
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: bo, kind: FixupKind::FloatLoad { bits: *bits, width } });
        // fstp <width> [g]  (modrm 1E = /3 [disp16])
        fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
        out.push(0x9B);
        out.push(op);
        out.push(0x1E);
        let bo = out.len() - 1;
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx } });
        // nop + fwait, marker on the nop (the emulator patch slot).
        fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIWRQQ" } });
        out.push(0x90); // nop
        out.push(0x9B); // fwait
        return;
    }
    // Long-global compound shift `g <<= k` / `g >>= k`: MSC calls a runtime
    // helper, passing the shift count (in AL) and the global's address.
    //   mov al, k; push ax; mov ax, OFFSET g; push ax; call __aNN*shl/shr
    // Fixtures 263, 264, 1139.
    if locals.is_long_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(op, BinOp::Shl | BinOp::Shr)
        && matches!(left.as_ref(), Expr::Global(g) if *g == global_idx)
        && let Some(k) = right.fold(locals.inits)
        && (0..32).contains(&k)
    {
        // Shift by 1 is done in place (shl/rcl, sar/shr+rcr); larger counts call
        // the runtime helper. Fixtures 265/266 (in-place) vs 263/264 (helper).
        if k == 1 && emit_long_global_4byte(global_idx, 0, 2, value, true, locals, out, fixups) {
            return;
        }
        let helper = match (op, locals.is_unsigned_global(global_idx)) {
            (BinOp::Shl, _) => "__aNNalshl",       // left shift is sign-agnostic
            (BinOp::Shr, false) => "__aNNalshr",   // arithmetic (signed)
            (BinOp::Shr, true) => "__aNNaulshr",   // logical (unsigned)
            _ => unreachable!(),
        };
        out.extend_from_slice(&[0xB0, k as u8]); // mov al, k
        out.push(0x50); // push ax
        let b8 = out.len();
        out.extend_from_slice(&[0xB8, 0x00, 0x00]); // mov ax, OFFSET g
        fixups.push(Fixup { body_offset: b8, kind: FixupKind::GlobalAddr { global_idx } });
        out.push(0x50); // push ax
        let call = out.len();
        out.extend_from_slice(&[0xE8, 0x00, 0x00]); // call helper
        fixups.push(Fixup { body_offset: call, kind: FixupKind::ExtCall { target: helper.to_owned() } });
        return;
    }
    // Long-global compound mul/div/mod `g *= r` / `g /= r` / `g %= r`: a
    // runtime-helper call taking the RHS long (DX:AX, pushed high-then-low)
    // and the global's address. Fixtures 747, 748, 762, 763, 787, 788.
    if locals.is_long_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
        && matches!(left.as_ref(), Expr::Global(g) if *g == global_idx)
    {
        // MSC names the helper by signedness even though long multiply's low
        // 32 bits are sign-agnostic: `__aNNaulmul` for unsigned (fixture 772),
        // `__aNNalmul` for signed.
        let helper = match (op, locals.is_unsigned_global(global_idx)) {
            (BinOp::Mul, false) => "__aNNalmul",
            (BinOp::Mul, true) => "__aNNaulmul",
            (BinOp::Div, false) => "__aNNaldiv",
            (BinOp::Div, true) => "__aNNauldiv",
            (BinOp::Mod, false) => "__aNNalrem",
            (BinOp::Mod, true) => "__aNNaulrem",
            _ => unreachable!(),
        };
        emit_long_to_dx_ax(right, locals, out, fixups); // RHS long → DX:AX
        out.push(0x52); // push dx
        out.push(0x50); // push ax
        let b8 = out.len();
        out.extend_from_slice(&[0xB8, 0x00, 0x00]); // mov ax, OFFSET g
        fixups.push(Fixup { body_offset: b8, kind: FixupKind::GlobalAddr { global_idx } });
        out.push(0x50); // push ax
        let call = out.len();
        out.extend_from_slice(&[0xE8, 0x00, 0x00]); // call helper
        fixups.push(Fixup { body_offset: call, kind: FixupKind::ExtCall { target: helper.to_owned() } });
        return;
    }
    // `g = a <*|/|%> b` (expression-context long mul/div/mod): evaluate via
    // the runtime helper (result in DX:AX), then store into the global.
    // Fixtures 231, 232, 233, 245, 246, 247.
    if locals.is_long_global(global_idx) && is_long_muldiv(value, locals) {
        emit_long_to_dx_ax(value, locals, out, fixups);
        out.push(0xA3); // mov [g], ax
        let off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        out.push(0x89); out.push(0x16); // mov [g+2], dx
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        return;
    }
    // Long-global = long-global. Plain copy through DX:AX.
    //   `b = a` → `mov ax, [a]; mov dx, [a+2];
    //              mov [b], ax; mov [b+2], dx`
    if locals.is_long_global(global_idx)
        && let Expr::Global(src) = value
        && locals.is_long_global(*src)
    {
        let off = out.len();
        out.extend_from_slice(&[0xA1, 0x00, 0x00]);
        fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: *src } });
        out.push(0x8B);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: *src } });
        out.push(0xA3);
        let off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        out.push(0x89);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        return;
    }
    // Long-global = long-global ± long-global. Loads lhs into DX:AX,
    // applies a memory add/sub to both halves, stores to dest.
    //   `g = a + b` → `mov ax, [a]; mov dx, [a+2];
    //                  add ax, [b]; adc dx, [b+2];
    //                  mov [g], ax; mov [g+2], dx`
    if locals.is_long_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(op, BinOp::Add | BinOp::Sub)
        && let Expr::Global(a) = left.as_ref()
        && let Expr::Global(b) = right.as_ref()
        && locals.is_long_global(*a)
        && locals.is_long_global(*b)
    {
        let (low_op, high_op): (u8, u8) = match op {
            BinOp::Add => (0x03, 0x13), // add r16,m16 / adc r16,m16
            BinOp::Sub => (0x2B, 0x1B), // sub r16,m16 / sbb r16,m16
            _ => unreachable!(),
        };
        // mov ax, [a]
        let off = out.len();
        out.extend_from_slice(&[0xA1, 0x00, 0x00]);
        fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: *a } });
        // mov dx, [a+2]
        out.push(0x8B);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: *a } });
        // op ax, [b]
        out.push(low_op);
        out.push(0x06);
        let off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: *b } });
        // op dx, [b+2]
        out.push(high_op);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: *b } });
        // mov [g], ax
        out.push(0xA3);
        let off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        // mov [g+2], dx
        out.push(0x89);
        out.push(0x16);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        return;
    }
    // Long-global compound `g = g OP <int global>` (add/sub/and/or/xor). MSC
    // does NOT fold an int-GLOBAL operand (unlike an int local) — it loads it
    // and sign-extends with `cwd`, then a two-word register op:
    //   `mov ax,_i; cwd; <op> WORD [g],ax; <op-carry> WORD [g+2],dx`
    // The const-prop side preserves the int-global RHS so it reaches here as
    // Expr::Global. Fixtures 257/258/259/269/270.
    if locals.is_long_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && matches!(left.as_ref(), Expr::Global(g) if *g == global_idx)
        && matches!(right.as_ref(), Expr::Global(r) if !locals.is_long_global(*r))
    {
        let (lo_op, hi_op): (u8, u8) = match op {
            BinOp::Add => (0x01, 0x11), // add / adc
            BinOp::Sub => (0x29, 0x19), // sub / sbb
            BinOp::BitAnd => (0x21, 0x21),
            BinOp::BitOr => (0x09, 0x09),
            BinOp::BitXor => (0x31, 0x31),
            _ => unreachable!(),
        };
        emit_expr_to_ax(right, locals, out, fixups); // mov ax,_i (int global)
        out.push(0x99); // cwd → DX:AX
        out.push(lo_op); out.push(0x06);
        let off = out.len(); out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        out.push(hi_op); out.push(0x16);
        let off = out.len(); out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        return;
    }
    // Long-global compound add/sub by a foldable RHS:
    //   `g += K` → `add [g], K_lo; adc [g+2], K_hi`
    //   `g -= K` → `sub [g], K_lo; sbb [g+2], K_hi`
    // Falls through to the const-store path if the RHS isn't a
    // (Global(g) ± K) shape (fixture 1148).
    if locals.is_long_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(op, BinOp::Add | BinOp::Sub)
        && matches!(left.as_ref(), Expr::Global(g) if *g == global_idx)
        && let Some(k) = right.fold(locals.inits)
    {
        let low = (k as u32 & 0xFFFF) as i16;
        let high = ((k as i32) >> 16) as i16;
        let (sub_low_op, sub_high_op): (u8, u8) = match op {
            BinOp::Add => (0x00, 0x02), // /0 = add, /2 = adc
            BinOp::Sub => (0x05, 0x03), // /5 = sub, /3 = sbb
            _ => unreachable!(),
        };
        // Low-half: 83 06|2E low_imm8 (sx) or 81 06|2E low_imm16
        let low_fits_i8 = i8::try_from(low).is_ok();
        if low_fits_i8 {
            out.push(0x83);
            out.push(0x06 | (sub_low_op << 3));
            let addr_off = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            out.push(low as i8 as u8);
            fixups.push(Fixup {
                body_offset: addr_off - 1,
                kind: FixupKind::GlobalAddr { global_idx },
            });
        } else {
            out.push(0x81);
            out.push(0x06 | (sub_low_op << 3));
            let addr_off = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            out.extend_from_slice(&(low as u16).to_le_bytes());
            fixups.push(Fixup {
                body_offset: addr_off - 1,
                kind: FixupKind::GlobalAddr { global_idx },
            });
        }
        // High-half: 83 16 (or 1E) high_imm8 vs 81 16/1E ...
        let high_fits_i8 = i8::try_from(high).is_ok();
        if high_fits_i8 {
            out.push(0x83);
            out.push(0x06 | (sub_high_op << 3));
            let addr_off = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            out.push(high as i8 as u8);
            fixups.push(Fixup {
                body_offset: addr_off - 1,
                kind: FixupKind::GlobalAddr { global_idx },
            });
        } else {
            out.push(0x81);
            out.push(0x06 | (sub_high_op << 3));
            let addr_off = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            out.extend_from_slice(&(high as u16).to_le_bytes());
            fixups.push(Fixup {
                body_offset: addr_off - 1,
                kind: FixupKind::GlobalAddr { global_idx },
            });
        }
        return;
    }
    // Long-global `g &= K`: AND the low word in place (81 /4 = 81 26), then the
    // high word — `mov word [g+2],0` when K_hi is 0, `and word [g+2],K_hi` when
    // partial, omitted when K_hi is 0xFFFF (identity). Fixture 253.
    if locals.is_long_global(global_idx)
        && let Expr::BinOp { op: BinOp::BitAnd, left, right } = value
        && matches!(left.as_ref(), Expr::Global(g) if *g == global_idx)
        && let Some(k) = right.fold(locals.inits)
    {
        let k32 = k as u32;
        let lo = (k32 & 0xFFFF) as u16;
        let hi = (k32 >> 16) as u16;
        // `& 0xFFFF` on the low word is the AND-identity — MSC omits it entirely
        // (only the high word, here zeroed, is touched). Fixture 448.
        if lo != 0xFFFF {
            out.push(0x81); out.push(0x26); // and word [g], lo
            let off = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
            out.extend_from_slice(&lo.to_le_bytes());
        }
        if hi == 0x0000 {
            out.push(0xC7); out.push(0x06); // mov word [g+2], 0
            let off = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
            out.extend_from_slice(&0u16.to_le_bytes());
        } else if hi != 0xFFFF {
            out.push(0x81); out.push(0x26); // and word [g+2], hi
            let off = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx } });
            out.extend_from_slice(&hi.to_le_bytes());
        }
        return;
    }
    // Long-global `g |= K` / `g ^= K`: OR/XOR the low word in place (byte form
    // when K_lo fits 8 bits, else word form), then the high word only when K_hi
    // is non-zero (OR/XOR by 0 is identity). Fixtures 737, 738, 758.
    if locals.is_long_global(global_idx)
        && let Expr::BinOp { op: op @ (BinOp::BitOr | BinOp::BitXor), left, right } = value
        && matches!(left.as_ref(), Expr::Global(g) if *g == global_idx)
        && let Some(k) = right.fold(locals.inits)
    {
        let reg = if matches!(op, BinOp::BitOr) { 1u8 } else { 6u8 };
        let modrm = 0x06 | (reg << 3);
        let emit_word = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>, off16: u16, imm: u16| {
            if imm <= 0xFF {
                out.push(0x80); out.push(modrm);
                let p = out.len(); out.extend_from_slice(&off16.to_le_bytes());
                fixups.push(Fixup { body_offset: p - 1, kind: FixupKind::GlobalAddr { global_idx } });
                out.push(imm as u8);
            } else {
                out.push(0x81); out.push(modrm);
                let p = out.len(); out.extend_from_slice(&off16.to_le_bytes());
                fixups.push(Fixup { body_offset: p - 1, kind: FixupKind::GlobalAddr { global_idx } });
                out.extend_from_slice(&imm.to_le_bytes());
            }
        };
        let k32 = k as u32;
        emit_word(out, fixups, 0, (k32 & 0xFFFF) as u16);
        let hi = (k32 >> 16) as u16;
        if hi != 0 {
            emit_word(out, fixups, 2, hi);
        }
        return;
    }
    // Long globals get a special 4-byte store: low word at [g],
    // sign-extended high word at [g+2]. Only the constant-RHS shape
    // is wired up (most-common `long g = K;` pattern); a runtime RHS
    // would require DX:AX widening from the int RHS, deferred.
    if locals.is_long_global(global_idx)
        && let Some(k) = value.fold(locals.inits)
    {
        let low = (k as u32 & 0xFFFF) as u16;
        let high = (((k as i32) >> 31) as u32 & 0xFFFF) as u16;
        // c7 06 <addr> <low>
        out.push(0xC7);
        out.push(0x06);
        let addr_off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&low.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
        // c7 06 <addr+2> <high>
        out.push(0xC7);
        out.push(0x06);
        let addr_off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&high.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
        return;
    }
    // Char global compound `g op= K` → in-place BYTE op (`add/sub/and/or/xor
    // byte [g], imm8`, or `inc/dec byte [g]` for ±1). Reached directly or via
    // an aliased `*p op= K` where p points to a char global (fixtures 711-715).
    if locals.is_char_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && let Expr::Global(li) = left.as_ref()
        && *li == global_idx
        && matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && let Some(k) = right.fold(locals.inits)
    {
        let body_offset;
        if matches!(op, BinOp::Add | BinOp::Sub) && (k == 1) {
            out.push(0xFE); // inc/dec byte [g]
            out.push(if matches!(op, BinOp::Add) { 0x06 } else { 0x0E });
            body_offset = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
        } else {
            let modrm = match op {
                BinOp::Add => 0x06u8, BinOp::Sub => 0x2E, BinOp::BitAnd => 0x26,
                BinOp::BitOr => 0x0E, BinOp::BitXor => 0x36, _ => unreachable!(),
            };
            out.push(0x80); // <op> byte [g], imm8
            out.push(modrm);
            body_offset = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            out.push((k as u32 & 0xFF) as u8);
        }
        fixups.push(Fixup { body_offset: body_offset - 1, kind: FixupKind::GlobalAddr { global_idx } });
        return;
    }
    // Peephole: `g = g ± K` for int globals:
    //   K == 1: `inc/dec word ptr [g]`               (4 bytes)
    //   |K| ≤ 127: `add/sub word [g], imm8sx`         (5 bytes)
    //   larger: `add/sub word [g], imm16`             (6 bytes)
    // Long globals are handled by an earlier specialized arm.
    if !locals.is_long_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && let Expr::Global(li) = left.as_ref()
        && *li == global_idx
        && matches!(op, BinOp::Add | BinOp::Sub)
        && let Some(k) = right.fold(locals.inits)
    {
        match (op, k) {
            (BinOp::Add, 1) | (BinOp::Sub, 1) => {
                let modrm = if matches!(op, BinOp::Add) { 0x06 } else { 0x0E };
                out.push(0xFF);
                out.push(modrm);
                let body_offset = out.len();
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup {
                    body_offset: body_offset - 1,
                    kind: FixupKind::GlobalAddr { global_idx },
                });
                return;
            }
            _ => {
                let modrm = if matches!(op, BinOp::Add) { 0x06 } else { 0x2E };
                if let Ok(k8) = i8::try_from(k) {
                    out.push(0x83);
                    out.push(modrm);
                    let body_offset = out.len();
                    out.extend_from_slice(&[0x00, 0x00]);
                    out.push(k8 as u8);
                    fixups.push(Fixup {
                        body_offset: body_offset - 1,
                        kind: FixupKind::GlobalAddr { global_idx },
                    });
                } else {
                    out.push(0x81);
                    out.push(modrm);
                    let body_offset = out.len();
                    out.extend_from_slice(&[0x00, 0x00]);
                    let imm = (k as u32 & 0xFFFF) as u16;
                    out.extend_from_slice(&imm.to_le_bytes());
                    fixups.push(Fixup {
                        body_offset: body_offset - 1,
                        kind: FixupKind::GlobalAddr { global_idx },
                    });
                }
                return;
            }
        }
    }
    // Scalar global compound shift `g <<=/>>= K` (and `g *= 2^k`) → in-place
    // `shl/shr/sar word|byte [g], 1` (D1/D0) or `mov cl,K; ... [g], cl` (D3/D2).
    // modrm reg: SHL=4 (0x26), SHR-unsigned=5 (0x2E), SAR-signed=7 (0x3E).
    if !locals.is_long_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && let Expr::Global(li) = left.as_ref()
        && *li == global_idx
        && let Some(k) = right.fold(locals.inits)
    {
        let shift_k = match (op, k) {
            (BinOp::Shl, k) if k > 0 && k < 16 => Some(k as u8),
            (BinOp::Shr, k) if k > 0 && k < 16 => Some(k as u8),
            (BinOp::Mul, k) if k >= 2 && (k & (k - 1)) == 0 => {
                let mut b = 0u8; let mut v = k as u32; while v > 1 { b += 1; v >>= 1; } Some(b)
            }
            _ => None,
        };
        if let Some(sk) = shift_k {
            let reg: u8 = match op {
                BinOp::Shl | BinOp::Mul => 4,
                BinOp::Shr => if locals.is_unsigned_global(global_idx) { 5 } else { 7 },
                _ => unreachable!(),
            };
            let modrm = 0x06 | (reg << 3);
            let is_byte = locals.is_char_global(global_idx);
            let (one_op, cl_op) = if is_byte { (0xD0u8, 0xD2u8) } else { (0xD1u8, 0xD3u8) };
            if sk != 1 { out.extend_from_slice(&[0xB1, sk]); } // mov cl, K
            out.push(if sk == 1 { one_op } else { cl_op });
            out.push(modrm);
            let disp_pos = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: disp_pos - 1, kind: FixupKind::GlobalAddr { global_idx } });
            return;
        }
    }
    // Long global with runtime RHS: compute DX:AX then store both halves.
    if locals.is_long_global(global_idx) {
        // If it doesn't match any of the earlier specialized arms, the
        // RHS is a runtime expression. Load both words and store both.
        emit_long_to_dx_ax(value, locals, out, fixups);
        out.push(0xA3); // mov [g], ax
        let addr_off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: addr_off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        out.extend_from_slice(&[0x89, 0x16]); // mov [g+2], dx
        let addr_off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: addr_off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        return;
    }
    // Char-global compound mul/div/mod by a foldable RHS → byte runtime op,
    // result left in AL. mul: `mov al,K; imul byte [g]; mov [g],al`. div:
    // `mov cl,K; mov al,[g]; cbw; idiv cl; mov [g],al`. mod: `…; mov [g],ah;
    // mov al,ah`. Fixtures 691/692/693/695/696/699.
    if locals.is_char_global(global_idx)
        && let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Global(g) if *g == global_idx)
        && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
        && let Some(k) = right.fold(locals.inits)
    {
        let put0 = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>| {
            let bo = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx } });
        };
        let k8 = (k as u32 & 0xFF) as u8;
        let uns = locals.is_unsigned_global(global_idx);
        // Unsigned char `g /= 2^m` strength-reduces to `shr byte [g],cl` and
        // `g %= 2^m` to `and byte [g],(2^m-1)`. Fixture 694.
        if uns && k > 1 && (k as u32).is_power_of_two() && matches!(op, BinOp::Div | BinOp::Mod) {
            if matches!(op, BinOp::Div) {
                let m = (k as u32).trailing_zeros() as u8;
                out.extend_from_slice(&[0xB1, m]);              // mov cl, m
                out.push(0xD2); out.push(0x2E); put0(out, fixups); // shr byte [g], cl
            } else {
                out.push(0x80); out.push(0x26); put0(out, fixups); out.push(k8 - 1); // and byte [g], 2^m-1
            }
            return;
        }
        if matches!(op, BinOp::Mul) {
            out.extend_from_slice(&[0xB0, k8]); // mov al, K
            out.push(0xF6); out.push(if uns { 0x26 } else { 0x2E }); put0(out, fixups); // mul/imul byte [g]
            out.push(0xA2); put0(out, fixups); // mov [g], al
        } else {
            out.extend_from_slice(&[0xB1, k8]); // mov cl, K
            out.push(0xA0); put0(out, fixups); // mov al, [g]
            if uns { out.extend_from_slice(&[0x2A, 0xE4]); } else { out.push(0x98); } // sub ah,ah / cbw
            out.extend_from_slice(&[0xF6, if uns { 0xF1 } else { 0xF9 }]); // div/idiv cl
            if matches!(op, BinOp::Div) {
                out.push(0xA2); put0(out, fixups); // mov [g], al
            } else {
                out.push(0x88); out.push(0x26); put0(out, fixups); // mov [g], ah
                out.extend_from_slice(&[0x8A, 0xC4]); // mov al, ah
            }
        }
        return;
    }
    // Scalar int-global compound mul/div/mod/shl/shr by a foldable RHS → runtime
    // in-place op (MSC does NOT const-fold these). Mirrors the indexed-global
    // form with the element address at disp16 0 (GlobalAddr supplies the base).
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Global(g) if *g == global_idx)
        && matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Mul | BinOp::Div | BinOp::Mod)
        && let Some(k) = right.fold(locals.inits)
    {
        let unsigned = locals.is_unsigned_global(global_idx);
        let put0 = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>| {
            let bo = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx } });
        };
        let imm16 = (k as u32 & 0xFFFF) as u16;
        match op {
            BinOp::Shl | BinOp::Shr => {
                out.extend_from_slice(&[0xB1, k as u8]); // mov cl, k
                out.push(0xD3);
                out.push(match op {
                    BinOp::Shl => 0x26u8,                                   // shl /4
                    BinOp::Shr => if unsigned { 0x2E } else { 0x3E },       // shr /5 | sar /7
                    _ => unreachable!(),
                });
                put0(out, fixups);
            }
            BinOp::Mul => {
                out.push(0xB8); out.extend_from_slice(&imm16.to_le_bytes()); // mov ax, k
                out.push(0xF7); out.push(if unsigned { 0x26 } else { 0x2E }); put0(out, fixups); // mul/imul word [g]
                out.push(0xA3); put0(out, fixups); // mov [g], ax
            }
            BinOp::Div | BinOp::Mod => {
                out.push(0xB9); out.extend_from_slice(&imm16.to_le_bytes()); // mov cx, k
                out.push(0xA1); put0(out, fixups); // mov ax, [g]
                if unsigned { out.extend_from_slice(&[0x2B, 0xD2, 0xF7, 0xF1]); } // sub dx,dx; div cx
                else { out.extend_from_slice(&[0x99, 0xF7, 0xF9]); } // cwd; idiv cx
                if matches!(op, BinOp::Div) { out.push(0xA3); put0(out, fixups); } // mov [g], ax
                else { out.extend_from_slice(&[0x89, 0x16]); put0(out, fixups); } // mov [g], dx
            }
            _ => unreachable!(),
        }
        return;
    }
    // Char global scalar store `c = K;` → byte form `mov byte [c], imm8` (c6 06).
    if locals.is_char_global(global_idx)
        && let Some(k) = value.fold(locals.inits)
    {
        out.push(0xC6);
        out.push(0x06);
        let addr_off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        out.push((k as u32 & 0xFF) as u8);
        fixups.push(Fixup { body_offset: addr_off - 1, kind: FixupKind::GlobalAddr { global_idx } });
        return;
    }
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7);
        out.push(0x06);
        let addr_off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&imm.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0xA3);                       // MOV moffs16, AX
        let addr_off = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    }
}
/// Emit a long (4-byte) const-store or compound-assign on a global whose low word
/// is at `[global+lo]` and high word at `[global+hi]`. `self_matches` tells whether
/// a BinOp value's left operand is the lvalue's own self-read (compound). Returns
/// true if it emitted; false for an unsupported shape (caller falls back). Shared by
/// long array elements (`a[K]`) and long struct fields (`s.f`).
pub(crate) fn emit_long_global_4byte(
    global_idx: usize, lo: u16, hi: u16, value: &Expr, self_matches: bool,
    locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>,
) -> bool {
    let put_addr = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>, off: u16| {
        let bo = out.len();
        out.extend_from_slice(&off.to_le_bytes());
        fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx } });
    };
    // `<grp1-op> word [g+off], imm` — 83 (imm8sx) when it fits else 81.
    let word_imm = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>, digit: u8, off: u16, imm: i32| {
        let modrm = 0x06 | (digit << 3);
        if let Ok(k8) = i8::try_from(imm) {
            out.push(0x83); out.push(modrm); put_addr(out, fixups, off); out.push(k8 as u8);
        } else {
            out.push(0x81); out.push(modrm); put_addr(out, fixups, off);
            out.extend_from_slice(&((imm as u32 & 0xFFFF) as u16).to_le_bytes());
        }
    };
    let mov_imm = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>, off: u16, imm: u16| {
        out.push(0xC7); out.push(0x06); put_addr(out, fixups, off);
        out.extend_from_slice(&imm.to_le_bytes());
    };
    // AND keeps the 81 (imm16) word form to clear the high bits.
    let word_imm16 = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>, digit: u8, off: u16, imm: i32| {
        out.push(0x81); out.push(0x06 | (digit << 3)); put_addr(out, fixups, off);
        out.extend_from_slice(&((imm as u32 & 0xFFFF) as u16).to_le_bytes());
    };
    // Long const assign. Zeroing both words goes through AX (high then low); a
    // nonzero value uses one `mov word [g+off], imm` per half.
    if let Some(k) = value.fold(locals.inits) {
        if k == 0 {
            out.extend_from_slice(&[0x2B, 0xC0]); // sub ax, ax
            out.push(0xA3); put_addr(out, fixups, hi); // mov [hi], ax
            out.push(0xA3); put_addr(out, fixups, lo); // mov [lo], ax
        } else {
            mov_imm(out, fixups, lo, (k as u32 & 0xFFFF) as u16);
            mov_imm(out, fixups, hi, (((k as i32) >> 16) as u32 & 0xFFFF) as u16);
        }
        return true;
    }
    // Long compound `<lvalue> op= imm` (foldable RHS, self-read on the left).
    if self_matches
        && let Expr::BinOp { op, right, .. } = value
        && let Some(m) = right.fold(locals.inits)
    {
        let low = (m as u32 & 0xFFFF) as i16 as i32;
        let high = ((m as i32) >> 16) as i32;
        match op {
            BinOp::Add => { word_imm(out, fixups, 0, lo, low); word_imm(out, fixups, 2, hi, high); }
            BinOp::Sub => { word_imm(out, fixups, 5, lo, low); word_imm(out, fixups, 3, hi, high); }
            BinOp::BitAnd => {
                // Low word: when its low byte is 0xFF (AND-identity), MSC touches
                // only the high byte of the low word (clear → mov byte [lo+1],0;
                // partial → and byte [lo+1],hb; all-ones → nothing). Otherwise it
                // ANDs the whole word (81 imm16). Fixtures 393/390 (&255) vs 253 (&15).
                if (low & 0xFF) == 0xFF {
                    let hb = (low >> 8) & 0xFF;
                    if hb == 0xFF {
                        // low word unchanged
                    } else if hb == 0 {
                        out.push(0xC6); out.push(0x06); put_addr(out, fixups, lo + 1); out.push(0x00);
                    } else {
                        out.push(0x80); out.push(0x06 | (4 << 3)); put_addr(out, fixups, lo + 1); out.push(hb as u8);
                    }
                } else {
                    word_imm16(out, fixups, 4, lo, low);
                }
                if high == 0 { mov_imm(out, fixups, hi, 0); }
                else { word_imm16(out, fixups, 4, hi, high); }
            }
            BinOp::BitOr | BinOp::BitXor => {
                let digit = if matches!(op, BinOp::BitOr) { 1 } else { 6 };
                if (0..=255).contains(&low) {
                    out.push(0x80); out.push(0x06 | (digit << 3)); put_addr(out, fixups, lo);
                    out.push(low as u8);
                } else {
                    word_imm(out, fixups, digit, lo, low);
                }
                if high != 0 { word_imm(out, fixups, digit, hi, high); }
            }
            BinOp::Shl if m == 1 => {
                out.extend_from_slice(&[0xD1, 0x26]); put_addr(out, fixups, lo); // shl word [lo],1
                out.extend_from_slice(&[0xD1, 0x16]); put_addr(out, fixups, hi); // rcl word [hi],1
            }
            BinOp::Shr if m == 1 => {
                // high word first: sar/shr [hi],1 then rcr [lo],1 (threads carry down).
                let hi_op = if locals.is_unsigned_global(global_idx) { 0x2E } else { 0x3E }; // shr /5 | sar /7
                out.push(0xD1); out.push(hi_op); put_addr(out, fixups, hi);
                out.extend_from_slice(&[0xD1, 0x1E]); put_addr(out, fixups, lo); // rcr word [lo],1
            }
            _ => return false, // unsupported long compound op (mul/div, shift>1)
        }
        return true;
    }
    false
}
/// `<struct-global> = <struct-global>;` — whole-struct copy. ≤2 words copies via
/// AX/DX; ≥3 words uses a `movsw` run (di=dst, si=src, es=ds). Fixtures 410/413.
pub(crate) fn emit_struct_global_copy(dst: usize, src: usize, bytes: u16, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let words = bytes / 2;
    let g_moffs = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>, opcode: u8, g: usize| {
        let bo = out.len();
        out.push(opcode);
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } });
    };
    if words <= 2 {
        g_moffs(out, fixups, 0xA1, src); // mov ax, [src]
        if words == 2 {
            out.extend_from_slice(&[0x8B, 0x16]); // mov dx, [src+2]
            let bo = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: src } });
        }
        g_moffs(out, fixups, 0xA3, dst); // mov [dst], ax
        if words == 2 {
            out.extend_from_slice(&[0x89, 0x16]); // mov [dst+2], dx
            let bo = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: dst } });
        }
    } else {
        out.extend_from_slice(&[0x57, 0x56]); // push di; push si
        g_moffs(out, fixups, 0xBF, dst); // mov di, OFFSET dst
        g_moffs(out, fixups, 0xBE, src); // mov si, OFFSET src
        out.extend_from_slice(&[0x1E, 0x07]); // push ds; pop es
        for _ in 0..words {
            out.push(0xA5); // movsw
        }
        out.extend_from_slice(&[0x5E, 0x5F]); // pop si; pop di
    }
}
/// bp-relative long (4-byte) const-store or compound at `[bp+lo]`/`[bp+hi]`.
/// Mirror of [`emit_long_global_4byte`] for locals (stack struct fields). Covers
/// const store + add/sub + shl-by-1; returns false otherwise.
pub(crate) fn emit_long_local_4byte(
    lo: i16, hi: i16, value: &Expr, self_matches: bool,
    locals: &Locals<'_>, out: &mut Vec<u8>,
) -> bool {
    let word_imm = |out: &mut Vec<u8>, digit: u8, off: i16, imm: i32| {
        let modrm = 0x46 | (digit << 3);
        if let Ok(k8) = i8::try_from(imm) {
            out.push(0x83); out.push(bp_modrm(modrm, off)); push_bp_disp(out, off); out.push(k8 as u8);
        } else {
            out.push(0x81); out.push(bp_modrm(modrm, off)); push_bp_disp(out, off);
            out.extend_from_slice(&((imm as u32 & 0xFFFF) as u16).to_le_bytes());
        }
    };
    let mov_imm = |out: &mut Vec<u8>, off: i16, imm: u16| {
        out.push(0xC7); out.push(bp_modrm(0x46, off)); push_bp_disp(out, off);
        out.extend_from_slice(&imm.to_le_bytes());
    };
    if let Some(k) = value.fold(locals.inits) {
        if k == 0 {
            out.extend_from_slice(&[0x2B, 0xC0]); // sub ax, ax
            out.push(0x89); out.push(bp_modrm(0x46, hi)); push_bp_disp(out, hi); // mov [hi], ax
            out.push(0x89); out.push(bp_modrm(0x46, lo)); push_bp_disp(out, lo); // mov [lo], ax
        } else {
            mov_imm(out, lo, (k as u32 & 0xFFFF) as u16);
            mov_imm(out, hi, (((k as i32) >> 16) as u32 & 0xFFFF) as u16);
        }
        return true;
    }
    if self_matches
        && let Expr::BinOp { op, right, .. } = value
        && let Some(m) = right.fold(locals.inits)
    {
        let low = (m as u32 & 0xFFFF) as i16 as i32;
        let high = ((m as i32) >> 16) as i32;
        match op {
            BinOp::Add => { word_imm(out, 0, lo, low); word_imm(out, 2, hi, high); }
            BinOp::Sub => { word_imm(out, 5, lo, low); word_imm(out, 3, hi, high); }
            BinOp::Shl if m == 1 => {
                out.push(0xD1); out.push(bp_modrm(0x66, lo)); push_bp_disp(out, lo); // shl word [lo],1
                out.push(0xD1); out.push(bp_modrm(0x56, hi)); push_bp_disp(out, hi); // rcl word [hi],1
            }
            _ => return false,
        }
        return true;
    }
    false
}
/// `<global>[K] = <expr>;` — write at a constant array index. The
/// placeholder address is `byte_off`, which the linker adds to the
/// global's base. Constant RHS → `c7 06 byte_off imm16`; general RHS
/// → `<expr-to-ax>; a3 byte_off`. Fixture 4119.
pub(crate) fn emit_assign_indexed_global(global_idx: usize, byte_off: u16, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Long array element `a[K]` (4-byte element): low word at byte_off, high at
    // byte_off+2. Fixtures 897/899-904.
    if locals.is_long_global(global_idx) {
        let self_matches = matches!(value, Expr::BinOp { left, .. }
            if matches!(left.as_ref(), Expr::Index { array, .. } if *array == global_idx));
        if emit_long_global_4byte(global_idx, byte_off, byte_off.wrapping_add(2), value, self_matches, locals, out, fixups) {
            return;
        }
    }
    // Compound `a[K] op= imm` → in-place word mem-op `add/sub/and/or/xor word
    // ptr [_a+off], imm` (and inc/dec for ±1). The element address placeholder
    // carries byte_off as the GlobalAddr addend. Fixtures 864-877.
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Index { array, index }
            if *array == global_idx
                && matches!(index.as_ref(), Expr::IntLit(k) if (*k as u16).wrapping_mul(2) == byte_off))
        && matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && let Some(k) = right.fold(locals.inits)
    {
        let emit_addr = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>| {
            let bo = out.len();
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx } });
        };
        // inc/dec word for ±1.
        if matches!(op, BinOp::Add | BinOp::Sub) && k == 1 {
            out.push(0xFF);
            out.push(if matches!(op, BinOp::Add) { 0x06 } else { 0x0E });
            emit_addr(out, fixups);
            return;
        }
        let modrm = match op {
            BinOp::Add => 0x06, BinOp::Sub => 0x2E, BinOp::BitAnd => 0x26,
            BinOp::BitOr => 0x0E, BinOp::BitXor => 0x36, _ => unreachable!(),
        };
        // or/xor with a byte-sized immediate use the BYTE form (`80`): the high
        // byte is unaffected, so MSC writes only the low byte. and must stay word
        // (`81`) to clear the high byte; add/sub use imm8-sign-extended (`83`)
        // when it fits, else imm16 (`81`). Fixtures 864/867/868/872.
        if matches!(op, BinOp::BitOr | BinOp::BitXor) && (0..=255).contains(&k) {
            out.push(0x80); out.push(modrm); emit_addr(out, fixups); out.push(k as u8);
        } else if matches!(op, BinOp::Add | BinOp::Sub) && let Ok(k8) = i8::try_from(k) {
            out.push(0x83); out.push(modrm); emit_addr(out, fixups); out.push(k8 as u8);
        } else {
            out.push(0x81); out.push(modrm); emit_addr(out, fixups);
            out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
        }
        return;
    }
    // Compound `a[K] op= imm` for shift / mul / div / mod — load-op-store shapes
    // (no single mem-op). Fixtures 878/882 (shl), 883 (mul), 884 (mod), 885 (div).
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::Index { array, index }
            if *array == global_idx
                && matches!(index.as_ref(), Expr::IntLit(k) if (*k as u16).wrapping_mul(2) == byte_off))
        && matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Mul | BinOp::Div | BinOp::Mod)
        && let Some(k) = right.fold(locals.inits)
    {
        let emit_addr = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>| {
            let bo = out.len();
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx } });
        };
        match op {
            BinOp::Shl | BinOp::Shr => {
                let modrm = if matches!(op, BinOp::Shl) { 0x26 } else { 0x3E }; // shl /4 | sar /7
                if k == 1 {
                    out.push(0xD1); out.push(modrm); emit_addr(out, fixups); // shift word [mem],1
                } else {
                    out.extend_from_slice(&[0xB1, k as u8]); // mov cl, k
                    out.push(0xD3); out.push(modrm); emit_addr(out, fixups); // shift word [mem], cl
                }
            }
            BinOp::Mul => {
                out.push(0xB8); out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); // mov ax, k
                out.extend_from_slice(&[0xF7, 0x2E]); emit_addr(out, fixups); // imul word [mem]
                out.push(0xA3); emit_addr(out, fixups); // mov [mem], ax
            }
            BinOp::Div | BinOp::Mod => {
                out.push(0xB9); out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); // mov cx, k
                out.push(0xA1); emit_addr(out, fixups); // mov ax, [mem]
                out.extend_from_slice(&[0x99, 0xF7, 0xF9]); // cwd; idiv cx
                if matches!(op, BinOp::Div) {
                    out.push(0xA3); emit_addr(out, fixups); // mov [mem], ax
                } else {
                    out.extend_from_slice(&[0x89, 0x16]); emit_addr(out, fixups); // mov [mem], dx
                }
            }
            _ => unreachable!(),
        }
        return;
    }
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7);
        out.push(0x06);
        let addr_off = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        out.extend_from_slice(&imm.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0xA3);
        let addr_off = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        fixups.push(Fixup {
            body_offset: addr_off - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    }
}
/// `<char-global>[K] = <byte>;` — store one byte at a constant
/// index. `c6 06 byte_off imm8`. Fixture 4122.
pub(crate) fn emit_assign_indexed_global_byte(global_idx: usize, byte_off: u16, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Compound `a[K] op= imm` on a char array → in-place byte mem-op
    // `add/sub/and/or/xor byte ptr [_a+off], imm8` (inc/dec byte for ±1).
    // Fixtures 865, 869-873, 877.
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::IndexByte { array, index }
            if *array == global_idx
                && matches!(index.as_ref(), Expr::IntLit(k) if *k as u16 == byte_off))
        && matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && let Some(k) = right.fold(locals.inits)
    {
        let emit_addr = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>| {
            let bo = out.len();
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx } });
        };
        if matches!(op, BinOp::Add | BinOp::Sub) && k == 1 {
            out.push(0xFE);
            out.push(if matches!(op, BinOp::Add) { 0x06 } else { 0x0E });
            emit_addr(out, fixups);
            return;
        }
        let modrm = match op {
            BinOp::Add => 0x06, BinOp::Sub => 0x2E, BinOp::BitAnd => 0x26,
            BinOp::BitOr => 0x0E, BinOp::BitXor => 0x36, _ => unreachable!(),
        };
        out.push(0x80); out.push(modrm); emit_addr(out, fixups);
        out.push((k as u32 & 0xFF) as u8);
        return;
    }
    let k = value.fold(locals.inits).unwrap_or_else(|| {
        panic!("non-constant char-array store value not yet supported")
    });
    let imm = (k as u32 & 0xFF) as u8;
    out.push(0xC6);
    out.push(0x06);
    let addr_off = out.len();
    out.extend_from_slice(&byte_off.to_le_bytes());
    out.push(imm);
    fixups.push(Fixup {
        body_offset: addr_off - 1,
        kind: FixupKind::GlobalAddr { global_idx },
    });
}
/// `<char-ptr-global>[K] = <byte>;` — load pointer into BX, then
/// `c6 47 disp imm8` (mov byte ptr [bx+disp], imm8). Fixture 4124.
pub(crate) fn emit_assign_ptr_index_byte(ptr_idx: usize, disp: i8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let k = value.fold(locals.inits).unwrap_or_else(|| {
        panic!("non-constant ptr-byte-store value not yet supported")
    });
    let imm = (k as u32 & 0xFF) as u8;
    let body_offset = out.len();
    out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
    fixups.push(Fixup {
        body_offset: body_offset + 1,
        kind: FixupKind::GlobalAddr { global_idx: ptr_idx },
    });
    out.extend_from_slice(&[0xC6, 0x47, disp as u8, imm]);
}
/// `*<ptr-global> = <expr>;` — store through a pointer global.
/// Pattern: `mov bx, [p]` (load pointer) then store via `[bx]`.
/// Constant RHS uses `c7 07 imm16`; general RHS uses
/// `<expr-to-ax>; mov [bx], ax` (89 07). Fixture 4116.
/// `<local-int-array>[K] = <expr>;` — word store at element K.
/// Constant RHS → `c7 46 disp imm16` at `disp = locals.disp(local) +
/// byte_off`. Non-constant RHS → `<expr-to-ax>; 89 46 disp`.
pub(crate) fn emit_assign_indexed_local(local_idx: usize, byte_off: u16, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let disp = locals.disp(local_idx) + byte_off as i16;
    // Compound-assign peepholes for `a[k] op= K` (int/word array).
    // Must check BEFORE value.fold() to avoid const-folding the whole expr
    // when MSC emits runtime ops (fixtures 1210, and similar add/sub cases).
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::LocalIndex { local: lx, .. } if *lx == local_idx)
        && let Some(k) = right.fold(locals.inits)
    {
        match op {
            BinOp::Mul if k != 0 && k != 1 && (k & (k - 1)) != 0 => {
                let k16 = (k as u32 & 0xFFFF) as u16;
                out.push(0xB8);
                out.extend_from_slice(&k16.to_le_bytes()); // mov ax, K
                out.extend_from_slice(&[0xF7, 0x6E, disp as u8]); // imul [bp+d]
                out.extend_from_slice(&[0x89, 0x46, disp as u8]); // mov [bp+d], ax
                return;
            }
            BinOp::Add if k == 1 => {
                out.push(0xFF); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                return;
            }
            BinOp::Sub if k == 1 => {
                out.push(0xFF); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp);
                return;
            }
            BinOp::Add | BinOp::Sub => {
                let modrm = if matches!(op, BinOp::Add) { 0x46u8 } else { 0x6Eu8 };
                if let Ok(k8) = i8::try_from(k) {
                    out.push(0x83); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp); out.push(k8 as u8);
                } else {
                    out.push(0x81); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp);
                    out.extend_from_slice(&(k as u32 as u16).to_le_bytes());
                }
                return;
            }
            _ => {}
        }
    }
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
    }
}
/// `<local-char-array>[K] = <byte>;` — byte store at element K.
/// Constant RHS → `c6 46 disp imm8`; non-constant RHS evaluated
/// into AL via cbw-suppression. Fixture 1134.
pub(crate) fn emit_assign_indexed_local_byte(local_idx: usize, byte_off: u16, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, _fixups: &mut Vec<Fixup>) {
    let disp = locals.disp(local_idx) + byte_off as i16;
    // Compound-assign peepholes for `a[k] op= K` (byte/char array).
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::LocalIndexByte { local: lx, .. } if *lx == local_idx)
        && let Some(k) = right.fold(locals.inits)
    {
        match op {
            BinOp::Mul if k != 0 && k != 1 && (k & (k - 1)) != 0 => {
                let k8 = (k as u32 & 0xFF) as u8;
                out.push(0xB0); out.push(k8);                       // mov al, K
                out.extend_from_slice(&[0xF6, 0x6E, disp as u8]);   // imul [bp+d]
                out.extend_from_slice(&[0x88, 0x46, disp as u8]);   // mov [bp+d], al
                return;
            }
            BinOp::Add if k == 1 => {
                out.push(0xFE); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                return;
            }
            BinOp::Sub if k == 1 => {
                out.push(0xFE); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp);
                return;
            }
            BinOp::Add | BinOp::Sub => {
                let modrm = if matches!(op, BinOp::Add) { 0x46u8 } else { 0x6Eu8 };
                let k8 = (k as u32 & 0xFF) as u8;
                out.push(0x80); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp); out.push(k8);
                return;
            }
            _ => {}
        }
    }
    let k = value.fold(locals.inits).unwrap_or_else(|| {
        panic!("non-constant char-local-array store value not yet supported")
    });
    let imm = (k as u32 & 0xFF) as u8;
    out.push(0xC6); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); out.push(imm);
}
/// Load the index expression into SI for BP+SI-based local array
/// indexing.  Only `Expr::Local` and `Expr::Param` are supported
/// (runtime indices must be scalar variables).
pub(crate) fn emit_index_to_si(index: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>) {
    match index {
        Expr::Local(i) => {
            let disp = locals.disp(*i);
            out.push(0x8B); out.push(bp_modrm(0x76, disp)); push_bp_disp(out, disp);
        }
        Expr::Param(i) => {
            let pdisp = param_disp(*i);
            out.extend_from_slice(&[0x8B, 0x76, pdisp as u8]);
        }
        other => panic!("unsupported runtime local-array index expr: {other:?}"),
    }
}
/// Emit a byte-sized RHS into AL. For simple locals/params, use
/// an explicit byte load (`8a 46 disp`) so MSC's `mov al, [bp+d]`
/// form is matched. For other expressions evaluate into AX (AL is
/// the low byte). Fixture 1219.
pub(crate) fn emit_byte_rhs_to_al(value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match value {
        Expr::Local(i) => {
            let disp = locals.disp(*i);
            out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        }
        Expr::Param(i) => {
            let pdisp = param_disp(*i);
            out.extend_from_slice(&[0x8A, 0x46, pdisp as u8]);
        }
        Expr::IntLit(k) => {
            out.extend_from_slice(&[0xB0, (*k as u32 & 0xFF) as u8]); // mov al, imm8
        }
        _ => emit_expr_to_ax(value, locals, out, fixups),
    }
}
/// `<local-int-array>[<expr>] = <expr>;` — SI-based word store.
/// Codegen: `mov si, [idx]; shl si, 1; <rhs→AX>; mov [bp+si+base], ax`.
/// Requires Frame::WithSlideSi. Fixture 1468.
pub(crate) fn emit_assign_indexed_local_var(local_idx: usize, index: &Expr, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let base_disp = locals.disp(local_idx);
    // Constant RHS: store the immediate directly (no AX). `mov si,[i]; shl si,1;
    // mov word [bp+si+base], imm` (c7 /0). Fixture 1933 (`a[i] = 0`).
    if let Some(k) = value.fold(locals.inits) {
        emit_index_to_si(index, locals, out);
        out.extend_from_slice(&[0xD1, 0xE6]); // shl si, 1
        out.push(0xC7); out.push(bp_modrm(0x42, base_disp)); push_bp_disp(out, base_disp);
        out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
        return;
    }
    emit_index_to_si(index, locals, out);
    out.extend_from_slice(&[0xD1, 0xE6]); // shl si, 1
    emit_expr_to_ax(value, locals, out, fixups);
    out.push(0x89); out.push(bp_modrm(0x42, base_disp)); push_bp_disp(out, base_disp); // mov [bp+si+base], ax
}
/// `<global-array>[<expr>] = <expr>;` — runtime index into a global array,
/// BX-based: `mov bx,[i]; [shl bx,1]; <value→AX or imm>; mov [bx+&a], ax/al`.
/// The store modrm is `[bx]+disp16` (0x87) with a GlobalAddr fixup. Constant RHS
/// uses the immediate form (c6/c7). Fixtures 510, 1257, 1366, 1444.
/// `arr[j]` with a RUNTIME word index, read into AX using SI for the index so a
/// LHS index already parked in BX survives: `mov si,[j]; shl si,1; mov ax,[si+&arr]`.
/// Returns true iff `rhs` matched this shape (a bare runtime-indexed word-array
/// global read). MSC uses this for `arr[i] = arr[j]` / `arr[i] += arr[j]` — both
/// index registers must be live at once. Kept in sync with `global_index_rhs_uses_si`.
pub(crate) fn emit_indexed_global_read_via_si(rhs: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) -> bool {
    let Expr::Index { array, index } = rhs else { return false };
    if index.fold(locals.inits).is_some() { return false; }
    emit_index_to_si(index, locals, out);            // mov si,[j]
    out.extend_from_slice(&[0xD1, 0xE6]);            // shl si,1
    out.push(0x8B); out.push(0x84);                  // mov ax,[si+disp16]
    let dp = out.len();
    out.extend_from_slice(&[0x00, 0x00]);
    fixups.push(Fixup { body_offset: dp - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
    true
}
/// Predicate matching the runtime-indexed-global store whose RHS read needs SI
/// (so the frame must push/pop SI). True for `arr[i] = arr[j]` (bare runtime word
/// Index value) and `arr[i] ± arr[j]` self-compound (right operand a runtime word
/// Index). Mirrors `emit_indexed_global_read_via_si`'s firing condition.
pub(crate) fn global_index_rhs_uses_si(target: &AssignTarget, value: &Expr, inits: &[Option<i32>]) -> bool {
    if !matches!(target, AssignTarget::IndexedGlobalVar { .. }) {
        return false;
    }
    let is_rt_index = |e: &Expr| matches!(e, Expr::Index { index, .. } if index.fold(inits).is_none());
    if is_rt_index(value) {
        return true;
    }
    if let Expr::BinOp { op: BinOp::Add | BinOp::Sub, left, right } = value
        && matches!(left.as_ref(), Expr::Index { array, .. } if matches!(target, AssignTarget::IndexedGlobalVar { array: ta, .. } if ta == array))
        && is_rt_index(right)
    {
        return true;
    }
    false
}
pub(crate) fn emit_assign_indexed_global_var(global_idx: usize, index: &Expr, is_byte: bool, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Self-compound `arr[i] += rhs` / `arr[i]++` → in-place op at [bx + &arr].
    if let Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } = value
        && matches!(left.as_ref(), Expr::IndexByte { array, .. } | Expr::Index { array, .. } if *array == global_idx)
    {
        crate::codegen::expr::emit_load_bx(index, locals, out, fixups);
        if !is_byte { out.extend_from_slice(&[0xD1, 0xE3]); } // shl bx, 1
        let addr = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>| {
            let dp = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: dp - 1, kind: FixupKind::GlobalAddr { global_idx } });
        };
        let is_add = matches!(op, BinOp::Add);
        if let Some(k) = right.fold(locals.inits) {
            if k == 1 {
                out.push(if is_byte { 0xFE } else { 0xFF });          // inc/dec
                out.push(if is_add { 0x87 } else { 0x8F }); addr(out, fixups);
            } else if is_byte {
                out.push(0x80); out.push(if is_add { 0x87 } else { 0xAF }); addr(out, fixups);
                out.push((k as u32 & 0xFF) as u8);
            } else if let Ok(k8) = i8::try_from(k) {
                out.push(0x83); out.push(if is_add { 0x87 } else { 0xAF }); addr(out, fixups);
                out.push(k8 as u8);
            } else {
                out.push(0x81); out.push(if is_add { 0x87 } else { 0xAF }); addr(out, fixups);
                out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
            }
        } else if is_byte {
            emit_byte_rhs_to_al(right, locals, out, fixups);
            out.push(if is_add { 0x00 } else { 0x28 }); out.push(0x87); addr(out, fixups); // add/sub [bx+&arr], al
        } else {
            // `arr[i] ± arr[j]` (runtime j): read arr[j] through SI so the LHS
            // index in BX survives (fixture 3593).
            if !emit_indexed_global_read_via_si(right, locals, out, fixups) {
                emit_expr_to_ax(right, locals, out, fixups);
            }
            out.push(if is_add { 0x01 } else { 0x29 }); out.push(0x87); addr(out, fixups); // add/sub [bx+&arr], ax
        }
        return;
    }
    crate::codegen::expr::emit_load_bx(index, locals, out, fixups);
    if !is_byte { out.extend_from_slice(&[0xD1, 0xE3]); } // shl bx, 1
    if let Some(k) = value.fold(locals.inits) {
        out.push(if is_byte { 0xC6 } else { 0xC7 });
        out.push(0x87); // [bx]+disp16
        let dp = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: dp - 1, kind: FixupKind::GlobalAddr { global_idx } });
        if is_byte { out.push((k as u32 & 0xFF) as u8); }
        else { out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); }
    } else {
        if is_byte {
            emit_byte_rhs_to_al(value, locals, out, fixups); // mov al, <rhs byte>
            out.push(0x88);
        } else {
            // `arr[i] = arr[j]` (runtime j): read arr[j] through SI so the LHS
            // index in BX survives (fixture 3011).
            if !emit_indexed_global_read_via_si(value, locals, out, fixups) {
                emit_expr_to_ax(value, locals, out, fixups);
            }
            out.push(0x89);
        }
        out.push(0x87); // [bx]+disp16
        let dp = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: dp - 1, kind: FixupKind::GlobalAddr { global_idx } });
    }
}
/// `<local-char-array>[<expr>] = <byte>;` — SI-based byte store.
/// Codegen: `mov si, [idx]; <rhs→AL>; mov [bp+si+base], al`.
/// No shl since char elements are 1 byte each. Fixture 1219.
pub(crate) fn emit_assign_indexed_local_byte_var(local_idx: usize, index: &Expr, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    emit_index_to_si(index, locals, out);
    emit_byte_rhs_to_al(value, locals, out, fixups);
    let base_disp = locals.disp(local_idx);
    out.push(0x88); out.push(bp_modrm(0x42, base_disp)); push_bp_disp(out, base_disp); // mov [bp+si+base], al
}
/// `<struct-global>.<field> = <expr>;` — store at the global's
/// address + byte_off. Word: `c7 06 disp imm16`; byte: `c6 06 disp imm8`.
/// `s.f = K;` bit-field store (constant value): read-modify-write the 16-bit
/// unit — clear the field's bits, OR in the masked+shifted value. Picks the
/// narrowest AL/AH/AX op form for each of the AND/OR, skips the AND when the
/// value fills the field, skips the OR when the value is 0, and uses a direct
/// `mov al/ah,K` for a byte-aligned full-byte field. Reuses a live AX left by a
/// preceding bit-field store to the same unit. Fixtures 3209, 3217, 1691.
pub(crate) fn emit_assign_bitfield(base: BitBase, byte_off: u16, bit_off: u8, bit_width: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    use crate::codegen::expr::{bf_load_word, bf_store_word, bf_ax_live, emit_expr_to_ax};
    let maskw = (1u32 << bit_width) - 1;
    // Runtime value `s.f = v`: build `(v & maskw) << bit_off` in AX, load the
    // unit into CX, clear the field bits, OR together, store. Fixture 3322.
    let Some(k) = value.fold(locals.inits) else {
        emit_expr_to_ax(value, locals, out, fixups);     // v → AX
        out.push(0x25); out.extend_from_slice(&(maskw as u16).to_le_bytes()); // and ax,maskw
        for _ in 0..bit_off { out.extend_from_slice(&[0xD1, 0xE0]); } // shl ax,1 (×bit_off)
        // Load the unit into CX and clear the field bits there.
        match base {
            BitBase::Global(g) => {
                out.push(0x8B); out.push(0x0E);
                let bo = out.len(); // the imm16 follows the two-byte `8b 0e` opcode
                out.extend_from_slice(&byte_off.to_le_bytes()); // mov cx,[g]
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: g } });
            }
            BitBase::Local(l) => {
                let d = locals.disp(l) + byte_off as i16;
                out.push(0x8B); out.push(bp_modrm(0x4E, d)); push_bp_disp(out, d); // mov cx,[bp+d]
            }
            BitBase::DerefParam(_) => panic!("runtime bit-field store via pointer not supported"),
        }
        let clear = !((maskw << bit_off) & 0xFFFF) & 0xFFFF;
        let (hi, lo) = ((clear >> 8) & 0xFF, clear & 0xFF);
        if hi == 0xFF { out.extend_from_slice(&[0x80, 0xE1, lo as u8]); }       // and cl, lo
        else if lo == 0xFF { out.extend_from_slice(&[0x80, 0xE5, hi as u8]); }  // and ch, hi
        else { out.push(0x81); out.push(0xE1); out.extend_from_slice(&(clear as u16).to_le_bytes()); } // and cx,clear
        out.extend_from_slice(&[0x0B, 0xC1]); // or ax,cx
        bf_store_word(base, byte_off, locals, out, fixups);
        return;
    };
    let val = ((k as u32 & maskw) << bit_off) & 0xFFFF;
    let field_mask = (maskw << bit_off) & 0xFFFF;
    let clear = !field_mask & 0xFFFF;
    // All-ones field set (`val == field_mask`) confined to a single byte: filling
    // the whole field makes the clear redundant — the result is just `old | mask`,
    // so MSC emits a direct memory OR `or byte [unit+b],byteval` (no load/store).
    // Guarded to when AX isn't already holding the unit, and excluding a full
    // byte-aligned byte (handled below as a `mov` of the whole byte). Fixtures
    // 2300, 2301.
    if val == field_mask
        && (val & 0xFF00 == 0 || val & 0x00FF == 0)
        && !(bit_width == 8 && bit_off % 8 == 0)
        && matches!(base, BitBase::Local(_) | BitBase::Global(_))
        && !bf_ax_live(base, byte_off, locals, out, fixups)
    {
        let (byte_idx, imm) = if val & 0x00FF != 0 { (0u16, (val & 0xFF) as u8) }
                              else { (1u16, (val >> 8) as u8) };
        match base {
            BitBase::Local(l) => {
                let d = locals.disp(l) + (byte_off + byte_idx) as i16;
                out.push(0x80); out.push(bp_modrm(0x4E, d)); push_bp_disp(out, d); out.push(imm); // or byte [bp+d],imm8
            }
            BitBase::Global(g) => {
                out.push(0x80); out.push(0x0E); // or byte [moffs16],imm8
                let bo = out.len() - 1;
                out.extend_from_slice(&(byte_off + byte_idx).to_le_bytes());
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } });
                out.push(imm);
            }
            BitBase::DerefParam(_) => unreachable!(),
        }
        return;
    }
    if !bf_ax_live(base, byte_off, locals, out, fixups) {
        bf_load_word(base, byte_off, locals, out, fixups);
    }
    if bit_width == 8 && bit_off % 8 == 0 {
        // Full byte: set it directly (the other byte is untouched).
        if bit_off == 0 {
            out.push(0xB0); out.push(val as u8);       // mov al, K
        } else {
            out.push(0xB4); out.push((val >> 8) as u8); // mov ah, K
        }
    } else {
        // Clear the field bits (skip if the value fills the whole field).
        if val != field_mask {
            let (hi, lo) = ((clear >> 8) & 0xFF, clear & 0xFF);
            if hi == 0xFF {
                out.extend_from_slice(&[0x24, lo as u8]);          // and al, lo
            } else if lo == 0xFF {
                out.extend_from_slice(&[0x80, 0xE4, hi as u8]);    // and ah, hi
            } else {
                out.push(0x25); out.extend_from_slice(&(clear as u16).to_le_bytes()); // and ax, clear
            }
        }
        // OR in the value (skip if zero).
        if val != 0 {
            let (hi, lo) = ((val >> 8) & 0xFF, val & 0xFF);
            if hi == 0 {
                out.extend_from_slice(&[0x0C, lo as u8]);          // or al, lo
            } else if lo == 0 {
                out.extend_from_slice(&[0x80, 0xCC, hi as u8]);    // or ah, hi
            } else {
                out.push(0x0D); out.extend_from_slice(&(val as u16).to_le_bytes()); // or ax, val
            }
        }
    }
    bf_store_word(base, byte_off, locals, out, fixups);
}
pub(crate) fn emit_assign_global_field(global_idx: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Long (4-byte) struct field: a long element at byte_off — reuse the shared
    // long const-store / compound codegen (low at byte_off, high at byte_off+2).
    if size == 4 {
        let self_matches = matches!(value, Expr::BinOp { left, .. }
            if matches!(left.as_ref(), Expr::GlobalField { global: g, byte_off: bo, .. } if *g == global_idx && *bo == byte_off));
        if emit_long_global_4byte(global_idx, byte_off, byte_off.wrapping_add(2), value, self_matches, locals, out, fixups) {
            return;
        }
    }
    // Compound `s.f op= rhs` (preserved self-read): a struct field is a global at
    // `byte_off`, structurally identical to a global-array element — rewrite the
    // self-read to Index/IndexByte and reuse the indexed-global compound codegen.
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::GlobalField { global: g, byte_off: bo, .. } if *g == global_idx && *bo == byte_off)
    {
        let self_read = if size == 1 {
            Expr::IndexByte { array: global_idx, index: Box::new(Expr::IntLit(byte_off as i32)) }
        } else {
            Expr::Index { array: global_idx, index: Box::new(Expr::IntLit((byte_off / 2) as i32)) }
        };
        let rewritten = Expr::BinOp { op: *op, left: Box::new(self_read), right: right.clone() };
        if size == 1 {
            emit_assign_indexed_global_byte(global_idx, byte_off, &rewritten, locals, out, fixups);
        } else {
            emit_assign_indexed_global(global_idx, byte_off, &rewritten, locals, out, fixups);
        }
        return;
    }
    if size == 1 {
        let k = value.fold(locals.inits).unwrap_or_else(|| {
            panic!("non-constant byte global-struct-field store not yet supported")
        });
        let imm = (k as u32 & 0xFF) as u8;
        out.push(0xC6);
        out.push(0x06);
        let body_offset = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        out.push(imm);
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7);
        out.push(0x06);
        let body_offset = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        out.extend_from_slice(&imm.to_le_bytes());
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0xA3);
        let body_offset = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx },
        });
    }
}
/// `<struct-local>.<field> = <expr>;` — store at `disp + byte_off`,
/// picking word vs byte form based on the field's size.
/// `<struct-value-param>.<field> = <expr>;` — store to a field of a struct
/// passed by value, at `[bp + param_base + byte_off]`. Word/byte by field size.
pub(crate) fn emit_assign_param_field(param_idx: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let disp = locals.param_base_disp(param_idx) + byte_off as i16;
    if size == 1 {
        if let Some(k) = value.fold(locals.inits) {
            out.push(0xC6); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            out.push((k as u32 & 0xFF) as u8);
        } else {
            emit_expr_to_ax(value, locals, out, fixups);
            if out.last() == Some(&0x98) { out.pop(); } // strip cbw — storing AL
            out.push(0x88); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        }
    } else if let Some(k) = value.fold(locals.inits) {
        out.push(0xC7); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
    }
}
pub(crate) fn emit_assign_local_field(local_idx: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Long (4-byte) local struct field: low word at disp(local)+byte_off, high at
    // +2. Shared bp-relative long const-store / compound helper.
    if size == 4 {
        let lo = locals.disp(local_idx) + byte_off as i16;
        let self_matches = matches!(value, Expr::BinOp { left, .. }
            if matches!(left.as_ref(), Expr::LocalField { local: l, byte_off: bo, .. } if *l == local_idx && *bo == byte_off));
        if emit_long_local_4byte(lo, lo + 2, value, self_matches, locals, out) {
            return;
        }
    }
    // Compound `s.f op= rhs` (preserved self-read): a local struct field is a local
    // at byte_off — reuse the indexed-local compound codegen by rewriting the
    // self-read to LocalIndex/LocalIndexByte (which only matches on the local).
    if let Expr::BinOp { op, left, right } = value
        && matches!(left.as_ref(), Expr::LocalField { local: l, byte_off: bo, .. } if *l == local_idx && *bo == byte_off)
        && !locals.is_long_local(local_idx)
    {
        let self_read = if size == 1 {
            Expr::LocalIndexByte { local: local_idx, index: Box::new(Expr::IntLit(0)) }
        } else {
            Expr::LocalIndex { local: local_idx, index: Box::new(Expr::IntLit(0)) }
        };
        let rewritten = Expr::BinOp { op: *op, left: Box::new(self_read), right: right.clone() };
        if size == 1 {
            emit_assign_indexed_local_byte(local_idx, byte_off, &rewritten, locals, out, fixups);
        } else {
            emit_assign_indexed_local(local_idx, byte_off, &rewritten, locals, out, fixups);
        }
        return;
    }
    let disp = locals.disp(local_idx) + byte_off as i16;
    if size == 1 {
        let k = value.fold(locals.inits).unwrap_or_else(|| {
            panic!("non-constant byte struct-field store not yet supported")
        });
        let imm = (k as u32 & 0xFF) as u8;
        out.push(0xC6); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); out.push(imm);
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
    }
}
/// `<struct-ptr-param>-><field> = <expr>;` — same shape as the
/// local-pointer version but the BX-load disp is positive (param).
/// `<global-ptr>-><field> = <expr>;` — load the global into BX, then
/// store at `[bx + byte_off]`.
pub(crate) fn emit_assign_deref_global_field(ptr_global: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    out.push(0x8B);
    out.push(0x1E);
    let body_offset = out.len();
    out.extend_from_slice(&[0x00, 0x00]);
    fixups.push(Fixup {
        body_offset: body_offset - 1,
        kind: FixupKind::GlobalAddr { global_idx: ptr_global },
    });
    if size == 1 {
        let k = value.fold(locals.inits).unwrap_or_else(|| {
            panic!("non-constant byte struct-field store via global ptr not yet supported")
        });
        let imm = (k as u32 & 0xFF) as u8;
        if byte_off == 0 {
            out.extend_from_slice(&[0xC6, 0x07, imm]);
        } else {
            out.extend_from_slice(&[0xC6, 0x47, byte_off as u8, imm]);
        }
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        if byte_off == 0 {
            out.extend_from_slice(&[0xC7, 0x07]);
        } else {
            out.extend_from_slice(&[0xC7, 0x47, byte_off as u8]);
        }
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if byte_off == 0 {
            out.extend_from_slice(&[0x89, 0x07]);
        } else {
            out.extend_from_slice(&[0x89, 0x47, byte_off as u8]);
        }
    }
}
pub(crate) fn emit_assign_deref_param_field(ptr_param: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let p_disp = param_disp(ptr_param);
    // In-place self-compound `p->f ± K` (e.g. `p->x++`): load p into BX once, then
    // `inc/dec`/`add/sub` directly at `[bx+off]` — instead of the load-modify-store
    // (which re-loads BX). Fixture 1290 (`p->x++` → `mov bx,[p]; inc word [bx]`).
    if let Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } = value
        && matches!(left.as_ref(),
            Expr::DerefParamField { ptr_param: lp, byte_off: lo, .. } if *lp == ptr_param && *lo == byte_off)
        && let Some(k) = right.fold(locals.inits)
    {
        out.extend_from_slice(&[0x8B, 0x5E, p_disp as u8]); // mov bx,[bp+p]
        let is_sub = matches!(op, BinOp::Sub);
        let off = byte_off;
        let modrm = |out: &mut Vec<u8>, re: u8| {
            if off == 0 { out.push(0x07 | (re << 3)); }
            else { out.push(0x47 | (re << 3)); out.push(off as u8); }
        };
        if k == 1 || k == -1 {
            let dec = is_sub ^ (k == -1);
            out.push(if size == 1 { 0xFE } else { 0xFF });
            modrm(out, if dec { 1 } else { 0 });
        } else {
            let re = if is_sub { 5 } else { 0 };
            if size == 1 { out.push(0x80); modrm(out, re); out.push((k as u32 & 0xFF) as u8); }
            else if let Ok(k8) = i8::try_from(k) { out.push(0x83); modrm(out, re); out.push(k8 as u8); }
            else { out.push(0x81); modrm(out, re); out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); }
        }
        return;
    }
    out.push(0x8B);
    out.push(0x5E);
    out.push(p_disp as u8);
    if size == 1 {
        let k = value.fold(locals.inits).unwrap_or_else(|| {
            panic!("non-constant byte struct-field store via param ptr not yet supported")
        });
        let imm = (k as u32 & 0xFF) as u8;
        if byte_off == 0 {
            out.extend_from_slice(&[0xC6, 0x07, imm]);
        } else {
            out.push(0xC6);
            out.push(0x47);
            out.push(byte_off as u8);
            out.push(imm);
        }
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        if byte_off == 0 {
            out.push(0xC7);
            out.push(0x07);
        } else {
            out.push(0xC7);
            out.push(0x47);
            out.push(byte_off as u8);
        }
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if byte_off == 0 {
            out.extend_from_slice(&[0x89, 0x07]);
        } else {
            out.push(0x89);
            out.push(0x47);
            out.push(byte_off as u8);
        }
    }
}
/// `<struct-ptr-local>-><field> = <expr>;` — `mov bx, [bp+disp];
/// c7 47 byte_off imm16` (or byte form).
pub(crate) fn emit_assign_deref_local_field(ptr_local: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let p_disp = locals.disp(ptr_local);
    // In-place self-compound `p->f ± K` (`p->x++`) — load p into BX once, then
    // mutate `[bx+off]` directly (mirrors the param-ptr field path).
    if let Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } = value
        && matches!(left.as_ref(),
            Expr::DerefLocalField { ptr_local: lp, byte_off: lo, .. } if *lp == ptr_local && *lo == byte_off)
        && let Some(k) = right.fold(locals.inits)
    {
        out.push(0x8B); out.push(bp_modrm(0x5E, p_disp)); push_bp_disp(out, p_disp); // mov bx,[bp-p]
        let is_sub = matches!(op, BinOp::Sub);
        let off = byte_off;
        let modrm = |out: &mut Vec<u8>, re: u8| {
            if off == 0 { out.push(0x07 | (re << 3)); }
            else { out.push(0x47 | (re << 3)); out.push(off as u8); }
        };
        if k == 1 || k == -1 {
            let dec = is_sub ^ (k == -1);
            out.push(if size == 1 { 0xFE } else { 0xFF });
            modrm(out, if dec { 1 } else { 0 });
        } else {
            let re = if is_sub { 5 } else { 0 };
            if size == 1 { out.push(0x80); modrm(out, re); out.push((k as u32 & 0xFF) as u8); }
            else if let Ok(k8) = i8::try_from(k) { out.push(0x83); modrm(out, re); out.push(k8 as u8); }
            else { out.push(0x81); modrm(out, re); out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); }
        }
        return;
    }
    out.push(0x8B);
    out.push(0x5E);
    out.push(p_disp as u8);
    if size == 1 {
        let k = value.fold(locals.inits).unwrap_or_else(|| {
            panic!("non-constant byte struct-field store via ptr not yet supported")
        });
        let imm = (k as u32 & 0xFF) as u8;
        if byte_off == 0 {
            out.extend_from_slice(&[0xC6, 0x07, imm]);
        } else {
            out.push(0xC6);
            out.push(0x47);
            out.push(byte_off as u8);
            out.push(imm);
        }
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        if byte_off == 0 {
            out.push(0xC7);
            out.push(0x07);
        } else {
            out.push(0xC7);
            out.push(0x47);
            out.push(byte_off as u8);
        }
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if byte_off == 0 {
            out.extend_from_slice(&[0x89, 0x07]);
        } else {
            out.push(0x89);
            out.push(0x47);
            out.push(byte_off as u8);
        }
    }
}
/// Emit the mutation half of a postfix `local++`/`local--` expression.
/// The load-into-AX (or BX) half is the caller's responsibility.
/// `slot_size` is the storage size of the local (1 for char, 2 for int/ptr).
pub(crate) fn emit_postmutate_local(step: i32, slot_size: usize, disp: i16, out: &mut Vec<u8>) {
    let abs = step.unsigned_abs() as u32;
    if slot_size == 1 {
        match step {
            1  => { out.push(0xFE); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); }
            -1 => { out.push(0xFE); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp); }
            _  => {
                let m = if step > 0 { 0x46u8 } else { 0x6Eu8 };
                out.push(0x80); out.push(bp_modrm(m, disp)); push_bp_disp(out, disp); out.push(abs as u8);
            }
        }
    } else {
        match step {
            1  => { out.push(0xFF); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); }
            -1 => { out.push(0xFF); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp); }
            _  => {
                let m = if step > 0 { 0x46u8 } else { 0x6Eu8 };
                if abs <= 127 {
                    out.push(0x83); out.push(bp_modrm(m, disp)); push_bp_disp(out, disp); out.push(abs as u8);
                } else {
                    out.push(0x81); out.push(bp_modrm(m, disp)); push_bp_disp(out, disp);
                    out.extend_from_slice(&(abs as u16).to_le_bytes());
                }
            }
        }
    }
}
/// Emit the mutation half of a postfix `global++`/`global--` expression.
/// `step` encodes direction and magnitude; requires a GlobalAddr fixup.
pub(crate) fn emit_postmutate_global(step: i32, global_idx: usize, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let abs = step.unsigned_abs() as u32;
    // body_offset points at the ModRM/reg byte so +1,+2 land on the addr16.
    match step {
        1 => {
            let bo = out.len() + 1;
            out.extend_from_slice(&[0xFF, 0x06, 0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx } });
        }
        -1 => {
            let bo = out.len() + 1;
            out.extend_from_slice(&[0xFF, 0x0E, 0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx } });
        }
        _ => {
            let m = if step > 0 { 0x06u8 } else { 0x2Eu8 };
            let bo = out.len() + 1;
            if abs <= 127 {
                out.extend_from_slice(&[0x83, m, 0x00, 0x00, abs as u8]);
            } else {
                out.extend_from_slice(&[0x81, m, 0x00, 0x00]);
                out.extend_from_slice(&(abs as u16).to_le_bytes());
            }
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx } });
        }
    }
}
/// `*<ptr-local>++ = <expr>;` — store through the OLD pointer value
/// then advance the pointer by `step` bytes.
pub(crate) fn emit_assign_deref_postmutate_local(local_idx: usize, step: i32, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let disp = locals.disp(local_idx);
    // If AX still holds p (the immediately-preceding `p = <addr>` store was
    // `mov [bp-p], ax`), reuse it via `mov bx,ax` instead of reloading
    // `mov bx,[bp-p]` (fixture 1299 `p = buf; *p++ = 'A'`).
    let store = {
        let mut v = vec![0x89, bp_modrm(0x46, disp)];
        push_bp_disp(&mut v, disp);
        v
    };
    if out.len() >= store.len() && out[out.len() - store.len()..] == store[..] {
        out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
    } else {
        out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); // mov bx, [bp-p]
    }
    emit_postmutate_local(step, locals.size(local_idx), disp, out);
    // pointee size = |step| (step encodes the pointer stride)
    let psz = step.unsigned_abs() as usize;
    if let Some(k) = value.fold(locals.inits) {
        if psz == 1 {
            out.extend_from_slice(&[0xC6, 0x07, (k as u32 & 0xFF) as u8]);
        } else {
            out.extend_from_slice(&[0xC7, 0x07]);
            out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
        }
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if psz == 1 {
            out.extend_from_slice(&[0x88, 0x07]);
        } else {
            out.extend_from_slice(&[0x89, 0x07]);
        }
    }
}
/// `*<ptr-param>++ = <expr>;` — load the param pointer into BX (the old
/// value), advance the param in place by `step`, then store the RHS
/// through `[bx]`. The pointer slot is always 2 bytes; `|step|` is the
/// pointee size (1 → byte store, ≥2 → word). Fixtures 2803, 3351.
pub(crate) fn emit_assign_deref_postmutate_param(param_idx: usize, step: i32, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let pdisp = param_disp(param_idx) as i16;
    out.push(0x8B); out.push(bp_modrm(0x5E, pdisp)); push_bp_disp(out, pdisp); // mov bx,[bp+p]
    emit_postmutate_local(step, 2, pdisp, out); // advance the 2-byte pointer slot
    let psz = step.unsigned_abs() as usize;
    // `*d++ = *s++` — the source pointer goes in SI so the dst pointer (BX,
    // loaded above) survives the store: `mov si,[s]; <advance s>; mov al/ax,
    // [si]; mov [bx],al/ax`. Fixtures 1346, 3528.
    if let Expr::PostIncDeref { ptr: src, step: sstep, is_byte: sbyte } = value {
        match src.as_ref() {
            Expr::Param(si) => {
                let sd = param_disp(*si);
                out.push(0x8B); out.push(bp_modrm(0x76, sd)); push_bp_disp(out, sd); // mov si,[bp+s]
                emit_postmutate_local(*sstep, 2, sd, out);
            }
            Expr::Local(li) => {
                let sd = locals.disp(*li);
                out.push(0x8B); out.push(bp_modrm(0x76, sd)); push_bp_disp(out, sd);
                emit_postmutate_local(*sstep, 2, sd, out);
            }
            other => panic!("postinc-deref source not yet supported: {other:?}"),
        }
        if *sbyte { out.extend_from_slice(&[0x8A, 0x04]); } else { out.extend_from_slice(&[0x8B, 0x04]); } // mov al/ax,[si]
        if psz == 1 { out.extend_from_slice(&[0x88, 0x07]); } else { out.extend_from_slice(&[0x89, 0x07]); } // mov [bx],al/ax
        return;
    }
    if let Some(k) = value.fold(locals.inits) {
        if psz == 1 {
            out.extend_from_slice(&[0xC6, 0x07, (k as u32 & 0xFF) as u8]);
        } else {
            out.extend_from_slice(&[0xC7, 0x07]);
            out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
        }
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        if psz == 1 {
            if out.last() == Some(&0x98) { out.pop(); } // storing AL, drop cbw
            out.extend_from_slice(&[0x88, 0x07]);
        } else {
            out.extend_from_slice(&[0x89, 0x07]);
        }
    }
}
/// `*<ptr-local> = <expr>;` — load the pointer local into BX, then
/// store the RHS through `[bx]`. Constant RHS → `c7 07 imm16`;
/// general RHS → `<expr-to-ax>; 89 07`.
pub(crate) fn emit_assign_deref_local(local_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let disp = locals.disp(local_idx);
    if locals.is_far_ptr_local(local_idx) {
        // Far/huge pointer: `les bx,[bp-disp]` then store through ES:BX.
        out.push(0xC4); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp);
        if let Some(k) = value.fold(locals.inits) {
            let imm = (k as u32 & 0xFFFF) as u16;
            out.extend_from_slice(&[0x26, 0xC7, 0x07]);   // mov word ptr es:[bx],imm
            out.extend_from_slice(&imm.to_le_bytes());
        } else {
            emit_expr_to_ax(value, locals, out, fixups);
            out.extend_from_slice(&[0x26, 0x89, 0x07]);   // mov es:[bx],ax
        }
        return;
    }
    // Near pointer. If AX still holds p (the immediately-preceding `p = <addr>`
    // store was `mov [bp-p], ax`), reuse it via `mov bx,ax` instead of reloading
    // `mov bx,[bp-p]` (fixture 1066 `p = a + 1; *p = 5`). Mirrors the *p++ store.
    let store = {
        let mut v = vec![0x89, bp_modrm(0x46, disp)];
        push_bp_disp(&mut v, disp);
        v
    };
    if out.len() >= store.len() && out[out.len() - store.len()..] == store[..] {
        out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
    } else {
        out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); // mov bx, [bp-p]
    }
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.extend_from_slice(&[0xC7, 0x07]);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.extend_from_slice(&[0x89, 0x07]);
    }
}
/// `<ptr-local>[K] = <expr>;` (K≠0) fallback when the pointer has no known
/// alias: `mov bx, [bp-disp]` then store the RHS at `[bx + byte_off]`. The
/// aliased case is rewritten to a direct array store in const-prop, so this
/// only runs for genuine runtime pointers.
pub(crate) fn emit_assign_deref_local_offset(local_idx: usize, byte_off: u16, is_byte: bool, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let disp = locals.disp(local_idx);
    // mov bx, [bp-disp]
    out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp);
    // [bx + byte_off] modrm: reg field filled per opcode; base bx with disp8/16.
    let off = byte_off as i16;
    let modrm_base = if (0..=127).contains(&off) { 0x47 } else { 0x87 };
    let push_off = |out: &mut Vec<u8>| {
        if (0..=127).contains(&off) { out.push(off as u8); }
        else { out.extend_from_slice(&byte_off.to_le_bytes()); }
    };
    if is_byte {
        if let Some(k) = value.fold(locals.inits) {
            out.push(0xC6); out.push(modrm_base); push_off(out); // mov byte [bx+off], imm8
            out.push((k as u32 & 0xFF) as u8);
        } else {
            emit_expr_to_ax(value, locals, out, fixups);
            out.push(0x88); out.push(modrm_base); push_off(out); // mov [bx+off], al
        }
    } else if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.push(0xC7); out.push(modrm_base); push_off(out); // mov word [bx+off], imm16
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.push(0x89); out.push(modrm_base); push_off(out); // mov [bx+off], ax
    }
}
pub(crate) fn emit_assign_double_deref_global(global_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // `**pp = value` — load pp into BX, deref once to get target pointer,
    // then store value through it.
    // `mov bx, [pp]`
    let body_offset = out.len();
    out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
    fixups.push(Fixup {
        body_offset: body_offset + 1,
        kind: FixupKind::GlobalAddr { global_idx },
    });
    // `mov bx, [bx]` — one level of indirection.
    out.extend_from_slice(&[0x8B, 0x1F]);
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        // `c7 07 imm16` — mov word ptr [bx], imm16.
        out.extend_from_slice(&[0xC7, 0x07]);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        // `89 07` — mov [bx], ax.
        out.extend_from_slice(&[0x89, 0x07]);
    }
}
pub(crate) fn emit_assign_deref_global(global_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // `mov bx, [p]` — load pointer global into BX.
    let body_offset = out.len();
    out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
    fixups.push(Fixup {
        body_offset: body_offset + 1,
        kind: FixupKind::GlobalAddr { global_idx },
    });
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        // `c7 07 imm16` — mov word ptr [bx], imm16.
        out.extend_from_slice(&[0xC7, 0x07]);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        // `89 07` — mov [bx], ax.
        out.extend_from_slice(&[0x89, 0x07]);
    }
}
