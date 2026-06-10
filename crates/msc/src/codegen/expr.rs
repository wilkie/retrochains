use crate::*;

/// Append the bytes that compute `expr` into AX. Caller has already
/// emitted the prologue + chkstk call; what we emit here lives
/// between the chkstk call and the return-path epilogue. Phase 1
/// supports a tight set of patterns — every other shape panics with
/// a clear message so the missing case is obvious when a future
/// fixture hits it.
/// Load an int operand into SI (modrm reg 110) — `mov si,[bp+disp]` for a
/// param/local, else evaluate into AX and `mov si,ax`.
pub(crate) fn emit_load_si(e: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match e {
        Expr::Param(i) => { out.extend_from_slice(&[0x8B, 0x76, param_disp(*i) as u8]); }
        Expr::Local(i) => { let d = locals.disp(*i); out.push(0x8B); out.push(bp_modrm(0x76, d)); push_bp_disp(out, d); }
        _ => { emit_expr_to_ax(e, locals, out, fixups); out.extend_from_slice(&[0x8B, 0xF0]); } // mov si,ax
    }
}
/// Load an int operand into BX (modrm reg 011).
pub(crate) fn emit_load_bx(e: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match e {
        Expr::Param(i) => { out.extend_from_slice(&[0x8B, 0x5E, param_disp(*i) as u8]); }
        Expr::Local(i) => { let d = locals.disp(*i); out.push(0x8B); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d); }
        // A word global / global struct-field pointer loads straight into BX
        // (`mov bx,[g+off]` = 8b 1e) — `*(g.p)`. Fixture 2981.
        Expr::Global(g) if !locals.is_char_global(*g) && !locals.is_long_global(*g) => {
            out.extend_from_slice(&[0x8B, 0x1E]);
            let bo = out.len(); out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
        }
        Expr::GlobalField { global, byte_off, size: 2 } => {
            out.extend_from_slice(&[0x8B, 0x1E]);
            let bo = out.len(); out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *global } });
        }
        // `*p` (word deref of a simple pointer param/local) as an index: load the
        // pointer into BX then deref in place (`mov bx,[p]; mov bx,[bx]`), rather
        // than going through AX. Fixture 3584 (`arr[*p]`).
        Expr::DerefWord { ptr } if matches!(ptr.as_ref(), Expr::Param(_) | Expr::Local(_)) => {
            match ptr.as_ref() {
                Expr::Param(i) => { out.extend_from_slice(&[0x8B, 0x5E, param_disp(*i) as u8]); }
                Expr::Local(i) => { let d = locals.disp(*i); out.push(0x8B); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d); }
                _ => unreachable!(),
            }
            out.extend_from_slice(&[0x8B, 0x1F]); // mov bx,[bx]
        }
        // `a + b` (both simple word operands) loaded straight into BX:
        // `mov bx,[a]; add bx,[b]`. Used for a multi-term pointer index
        // `*(p + i + j)`. Falls back to AX when an operand isn't a memory word.
        Expr::BinOp { op: BinOp::Add, left, right }
            if matches!(right.as_ref(), Expr::Param(_) | Expr::Local(_) | Expr::Global(_))
                && matches!(left.as_ref(), Expr::Param(_) | Expr::Local(_) | Expr::Global(_) | Expr::BinOp { op: BinOp::Add, .. }) =>
        {
            emit_load_bx(left, locals, out, fixups);
            match right.as_ref() {
                Expr::Param(i) => { out.push(0x03); out.push(bp_modrm(0x5E, param_disp(*i))); push_bp_disp(out, param_disp(*i)); }
                Expr::Local(i) => { let d = locals.disp(*i); out.push(0x03); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d); }
                Expr::Global(g) => {
                    out.extend_from_slice(&[0x03, 0x1E]);
                    let bo = out.len(); out.extend_from_slice(&[0x00, 0x00]);
                    fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
                }
                _ => unreachable!(),
            }
        }
        _ => { emit_expr_to_ax(e, locals, out, fixups); out.extend_from_slice(&[0x8B, 0xD8]); } // mov bx,ax
    }
}
/// Emit `*(base + idx)` (pointer-arithmetic deref) into AX/AL. `is_byte` selects
/// the element width (byte+cbw vs word; the index scales by the element size).
/// Handles base = decayed global array (AddrOfGlobal) or a pointer param/local,
/// with a constant or runtime index. Returns false for shapes it can't handle.
pub(crate) fn emit_offset_deref(base: &Expr, idx: &Expr, is_byte: bool, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) -> bool {
    let read = if is_byte { 0x8Au8 } else { 0x8B }; // mov al / mov ax (r, r/m)
    let elem: i32 = if is_byte { 1 } else { 2 };
    // Nested pointer arithmetic `*((p + i) + j)`: the real pointer is the inner
    // base; fold the inner index together with the outer one into a combined
    // runtime index (`mov bx,[i]; add bx,[j]`). Fixture 3468.
    if let Expr::BinOp { op: BinOp::Add, left, right } = base {
        let combined = Expr::BinOp {
            op: BinOp::Add, left: Box::new((**right).clone()), right: Box::new(idx.clone()),
        };
        return emit_offset_deref(left, &combined, is_byte, locals, out, fixups);
    }
    let kfold = idx.fold(locals.inits);
    match base {
        Expr::AddrOfGlobal(g) => {
            if let Some(k) = kfold {
                // mov ax/al, [_g + k*elem]   (a1 / a0 + GlobalAddr addend)
                let off = (k * elem) as u16;
                out.push(if is_byte { 0xA0 } else { 0xA1 });
                let bo = out.len();
                out.extend_from_slice(&off.to_le_bytes());
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
            } else {
                // mov bx,[idx]; shl bx (word); mov ax/al,[_g + bx]   (modrm [bx]+disp16 = 0x87)
                emit_load_bx(idx, locals, out, fixups);
                if !is_byte { out.extend_from_slice(&[0xD1, 0xE3]); }
                out.push(read); out.push(0x87);
                let bo = out.len();
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
            }
        }
        Expr::Param(_) | Expr::Local(_) => {
            if let Some(k) = kfold {
                // mov bx,[p]; mov ax/al,[bx + k*elem]
                emit_load_bx(base, locals, out, fixups);
                let off = (k * elem) as i32;
                if off == 0 {
                    out.push(read); out.push(0x07); // [bx]
                } else if let Ok(o8) = i8::try_from(off) {
                    out.push(read); out.push(0x47); out.push(o8 as u8); // [bx+disp8]
                } else {
                    out.push(read); out.push(0x87); out.extend_from_slice(&(off as u16).to_le_bytes());
                }
            } else {
                // mov bx,[idx]; shl bx (word); mov si,[p]; mov ax/al,[bx+si]
                emit_load_bx(idx, locals, out, fixups);
                if !is_byte { out.extend_from_slice(&[0xD1, 0xE3]); }
                emit_load_si(base, locals, out, fixups);
                out.push(read); out.push(0x00); // [bx+si]
            }
        }
        _ => return false,
    }
    if is_byte { out.push(0x98); } // cbw
    true
}
/// Load the pointer element at `index` of an array-of-pointers global `array`
/// into BX (`arr[index]`). This phase leaves AX untouched.
pub(crate) fn emit_ptr_array_load_bx(array: usize, index: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if let Some(k) = index.fold(locals.inits) {
        // mov bx, _arr+2k   (8b 1e <off16>)
        let off = (k as u32).wrapping_mul(2) as u16;
        out.extend_from_slice(&[0x8B, 0x1E]);
        let bo = out.len();
        out.extend_from_slice(&off.to_le_bytes());
        fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: array } });
    } else {
        // mov bx,[i]; shl bx,1; mov bx,[bx+_arr]   (8b 9f <off16>)
        emit_load_bx(index, locals, out, fixups);
        out.extend_from_slice(&[0xD1, 0xE3]); // shl bx,1
        out.push(0x8B); out.push(0x9F);
        let bo = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: array } });
    }
}
/// Read the pointee at `[bx + inner*elem_size]` into AX/AL (BX already holds
/// the pointer). Char element widens with `cbw`. Only a constant `inner` index
/// is supported (the corpus uses J ∈ {0,1}).
pub(crate) fn emit_ptr_array_read_elem(inner: &Expr, elem_size: u8, locals: &Locals<'_>, out: &mut Vec<u8>) {
    let j = inner.fold(locals.inits).unwrap_or_else(|| {
        panic!("non-constant pointer-array inner index not supported")
    });
    let off = j * elem_size as i32;
    let read = if elem_size == 1 { 0x8Au8 } else { 0x8B };
    if off == 0 {
        out.push(read); out.push(0x07); // [bx]
    } else if let Ok(o8) = i8::try_from(off) {
        out.push(read); out.push(0x47); out.push(o8 as u8); // [bx+disp8]
    } else {
        out.push(read); out.push(0x87); out.extend_from_slice(&(off as u16).to_le_bytes());
    }
    if elem_size == 1 { out.push(0x98); } // cbw
}
/// Scale SI by `factor`: power-of-two → `shl si,1` steps; 3 → `mov ax,si; shl
/// si,1; add si,ax`; otherwise `imul si,si,imm16`.
fn scale_si(out: &mut Vec<u8>, factor: usize) {
    match factor {
        0 | 1 => {}
        f if f.is_power_of_two() => { for _ in 0..f.trailing_zeros() { out.extend_from_slice(&[0xD1, 0xE6]); } }
        3 => out.extend_from_slice(&[0x8B, 0xC6, 0xD1, 0xE6, 0x03, 0xF0]),
        f => { out.push(0x69); out.push(0xF6); out.extend_from_slice(&(f as u16).to_le_bytes()); }
    }
}
/// Emit the SI=row*rowstride, BX=col*elem address setup shared by 2-D read/write.
pub(crate) fn emit_index2d_regs(row: &Expr, col: &Expr, cols: usize, elem: usize, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    emit_load_si(row, locals, out, fixups);
    scale_si(out, cols * elem);
    emit_load_bx(col, locals, out, fixups);
    for _ in 0..(elem.max(1)).trailing_zeros() { out.extend_from_slice(&[0xD1, 0xE3]); } // shl bx,1
}
pub(crate) fn emit_expr_to_ax(expr: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match expr {
        Expr::AssignExpr { target, value } => {
            // Post-mutate deref targets (`*p++ = v`) compute the stored value as
            // part of the store sequence (dst pointer first, then the source),
            // leaving it in AL/AX — so route straight to the store helper rather
            // than the value-first path. Fixture 1808.
            match target {
                AssignTarget::DerefPostMutateParam { param_idx, step } => {
                    crate::codegen::assign::emit_assign_deref_postmutate_param(
                        *param_idx, *step, value, locals, out, fixups);
                    return;
                }
                AssignTarget::DerefPostMutateLocal { local_idx, step } => {
                    crate::codegen::assign::emit_assign_deref_postmutate_local(
                        *local_idx, *step, value, locals, out, fixups);
                    return;
                }
                _ => {}
            }
            // Assignment-as-expression: produce the RHS in AX, store it, leave
            // the value in AX (`<value→ax>; mov [target], ax`). Fixtures
            // 513/1434/2996/3395/555. A literal 0 loads via `sub ax,ax` (the
            // chained-assignment value form, fixture 3334).
            if matches!(value.as_ref(), Expr::IntLit(0)) {
                out.extend_from_slice(&[0x2B, 0xC0]); // sub ax,ax
            } else {
                emit_expr_to_ax(value, locals, out, fixups);
            }
            match target {
                AssignTarget::Local(i) => {
                    let d = locals.disp(*i);
                    out.push(0x89); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
                }
                AssignTarget::Param(i) => {
                    let d = param_disp(*i);
                    out.push(0x89); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
                }
                AssignTarget::Global(g) => {
                    let bo = out.len();
                    out.extend_from_slice(&[0xA3, 0x00, 0x00]); // mov [moffs], ax
                    fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *g } });
                }
                // `(*p = v)` as a value: the pointer base goes into BX after the
                // value is already in AX, then the store reuses AX (`mov bx,
                // [ptr]; mov [bx], ax`). Byte pointees store AL. Fixture 3333.
                AssignTarget::DerefParam(i) => {
                    let d = param_disp(*i);
                    out.push(0x8B); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d); // mov bx,[bp+d]
                    out.extend_from_slice(&[0x89, 0x07]); // mov [bx],ax
                }
                AssignTarget::DerefLocal(i) => {
                    let d = locals.disp(*i);
                    out.push(0x8B); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d);
                    out.extend_from_slice(&[0x89, 0x07]);
                }
                AssignTarget::DerefLocalByte(i) => {
                    let d = locals.disp(*i);
                    if out.last() == Some(&0x98) { out.pop(); } // storing AL, drop cbw
                    out.push(0x8B); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d);
                    out.extend_from_slice(&[0x88, 0x07]); // mov [bx],al
                }
                AssignTarget::DerefGlobal(g) => {
                    out.extend_from_slice(&[0x8B, 0x1E]); // mov bx,[g]
                    let bo = out.len();
                    out.extend_from_slice(&[0x00, 0x00]);
                    fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
                    out.extend_from_slice(&[0x89, 0x07]);
                }
                // `v = (a[K] = e)` — store AX to the const-index array element,
                // leaving the value in AX for the enclosing assignment. Fixture 1986.
                AssignTarget::IndexedLocal { local, byte_off } => {
                    let d = locals.disp(*local) + *byte_off as i16;
                    out.push(0x89); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
                }
                AssignTarget::IndexedLocalByte { local, byte_off } => {
                    let d = locals.disp(*local) + *byte_off as i16;
                    if out.last() == Some(&0x98) { out.pop(); } // storing AL, drop cbw
                    out.push(0x88); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
                }
                AssignTarget::IndexedGlobal { array, byte_off } => {
                    out.extend_from_slice(&[0x89, 0x06]);
                    let bo = out.len(); out.extend_from_slice(&byte_off.to_le_bytes());
                    fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
                }
                AssignTarget::IndexedGlobalByte { array, byte_off } => {
                    if out.last() == Some(&0x98) { out.pop(); }
                    out.extend_from_slice(&[0x88, 0x06]);
                    let bo = out.len(); out.extend_from_slice(&byte_off.to_le_bytes());
                    fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
                }
                other => panic!("AssignExpr target not yet supported: {other:?}"),
            }
        }
        Expr::CastChar { value, unsigned } => {
            // Truncate to a byte then widen back. A constant operand (incl. a
            // foldable arithmetic expression like `a+b` with known a,b) is
            // materialized as the truncated byte `mov al, imm8` (b0), NOT a word
            // `mov ax, imm16` (fixtures 1533/1535). Otherwise MSC loads the low
            // byte of a simple int variable directly (`mov al,[v]`); a general
            // expression is computed into AX (AL is its low byte). Widen:
            // signed → cbw, unsigned → `sub ah,ah`.
            if let Some(k) = value.fold(locals.inits) {
                if k as u8 == 0 {
                    out.extend_from_slice(&[0x2A, 0xC0]); // sub al,al (MSC's zero idiom)
                } else {
                    out.push(0xB0);
                    out.push(k as u8);
                }
            } else {
                // Load just the low byte of a scalar variable operand (incl. a
                // char operand — no inner widening, since we re-widen below; this
                // avoids the double-widen of `(unsigned char)<signed char>`,
                // fixture 3537). Long/float operands fall to the general path.
                match value.as_ref() {
                    Expr::Param(i)
                        if !locals.is_long_param(*i) && !locals.is_float_param(*i) =>
                    {
                        let d = param_disp(*i);
                        out.push(0x8A); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
                    }
                    Expr::Local(i)
                        if !locals.is_long_local(*i) && !locals.is_float_local(*i) =>
                    {
                        let d = locals.disp(*i);
                        out.push(0x8A); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
                    }
                    Expr::Global(g)
                        if !locals.is_long_global(*g) && !locals.is_float_global(*g) =>
                    {
                        let bo = out.len();
                        out.extend_from_slice(&[0xA0, 0x00, 0x00]); // mov al, moffs
                        fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *g } });
                    }
                    // `(char)<long>` truncates to the low byte (lowest address of
                    // the little-endian long) — byte-load it. Fixture 3199.
                    Expr::Param(i) if locals.is_long_param(*i) => {
                        let d = long_param_disp(*i, locals);
                        out.push(0x8A); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
                    }
                    Expr::Local(i) if locals.is_long_local(*i) => {
                        let d = locals.disp(*i);
                        out.push(0x8A); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
                    }
                    _ => emit_expr_to_ax(value, locals, out, fixups),
                }
            }
            if *unsigned {
                out.extend_from_slice(&[0x2A, 0xE4]); // sub ah, ah
            } else {
                out.push(0x98); // cbw
            }
        }
        Expr::Index2D { is_global, base, row, col, cols, elem } => {
            assert!(*is_global, "local 2-D runtime index should have const-folded");
            emit_index2d_regs(row, col, *cols, *elem, locals, out, fixups);
            // mov ax/al, [base + bx + si]  (modrm [bx+si]+disp16 = 0x80)
            let op = if *elem == 1 { 0x8A } else { 0x8B };
            out.push(op);
            let mp = out.len();
            out.push(0x80);
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: mp, kind: FixupKind::GlobalAddr { global_idx: *base } });
            if *elem == 1 { out.push(0x98); } // cbw
        }
        Expr::IntLit(k) => {
            // Foldable path is handled by the caller; this arm only
            // fires if the caller bypassed folding.
            let imm = (*k as u32 & 0xFFFF) as u16;
            out.push(0xB8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Expr::FloatLit(bits, _) => {
            // A float literal reaching int context (e.g. an unfolded
            // `(int)<const>`): load its truncated int value.
            let imm = (f64::from_bits(*bits) as i32 as u32 & 0xFFFF) as u16;
            out.push(0xB8);
            out.extend_from_slice(&imm.to_le_bytes());
        }
        Expr::Local(i) => {
            // Reuse AX when a just-emitted store/load already left local `i`
            // there (MSC's straight-line register liveness).
            emit_load_local_reuse(*i, locals, out);
        }
        Expr::Param(i) => {
            if locals.is_char_param(*i) {
                let disp = param_disp(*i) as u8;
                out.extend_from_slice(&[0x8A, 0x46, disp]); // mov al, [bp+disp]
                if locals.is_unsigned_param(*i) {
                    out.extend_from_slice(&[0x2A, 0xE4]); // sub ah, ah (zero-extend)
                } else {
                    out.push(0x98); // cbw (sign-extend)
                }
            } else {
                emit_load_param_reuse(*i, locals, out);
            }
        }
        // `(int)(long >> 16)`: the result is the long's high word. Read it
        // directly instead of loading the low word and shifting. Fixtures
        // 2170, 231-style high-word extracts.
        Expr::BinOp { op: BinOp::Shr, left, right }
            if long_operand(left, locals) && right.fold(locals.inits) == Some(16) =>
        {
            emit_long_high_word_to_ax(left, locals, out, fixups);
        }
        Expr::BinOp { op, left, right } => {
            emit_binop(*op, left, right, locals, out, fixups);
        }
        Expr::Call { name, args } => {
            // Result lands in AX. Caller consumes from there.
            // Fixture 1220 / 1283 / 1286 — call as part of an
            // arithmetic or assignment expression.
            emit_call(name, args, locals, out, fixups);
        }
        Expr::CallStructField { name, args, byte_off, size, temp_idx } => {
            // `make().field`: call → DX:AX (a ≤4-byte struct), spill both words to
            // the frame temp, then extract the field. The field at offset 0 is
            // already in AX (left live by the spill); offset 2 is in DX → mov ax,dx;
            // otherwise reload from the temp. Fixtures 2629/2634.
            emit_call(name, args, locals, out, fixups);
            let disp = locals.struct_field_temp_disp(*temp_idx);
            out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov [temp],ax
            let dh = disp + 2;
            out.push(0x89); out.push(bp_modrm(0x56, dh)); push_bp_disp(out, dh);     // mov [temp+2],dx
            match *byte_off {
                0 => { if *size == 1 { out.push(0x98); } } // AX live; char field widens
                2 => out.extend_from_slice(&[0x8B, 0xC2]),  // mov ax,dx
                off => {
                    let fd = disp + off as i16;
                    if *size == 1 {
                        out.push(0x8A); out.push(bp_modrm(0x46, fd)); push_bp_disp(out, fd); out.push(0x98);
                    } else {
                        out.push(0x8B); out.push(bp_modrm(0x46, fd)); push_bp_disp(out, fd);
                    }
                }
            }
        }
        Expr::CallPtr { target, args } => {
            // Indirect call through a function-pointer variable; result in AX.
            // Value context (non-tail): always clean up the pushed args.
            crate::codegen::calls::emit_call_ptr(target, args, locals, false, out, fixups);
        }
        Expr::FuncAddr(name) => {
            // `OFFSET _f` as a value in AX: `b8 <off16>` + FuncAddr fixup. The
            // fixup location convention is body_offset+1 (the off16 immediate),
            // so body_offset points at the `b8` opcode.
            out.push(0xB8);
            let body_offset = out.len() - 1;
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset, kind: FixupKind::FuncAddr { target: symbol_name(name) } });
        }
        Expr::StrLit(string_idx) => {
            // `mov ax, <str_addr>` — for use as a pointer value.
            // The address gets resolved to CONST-segment offset via
            // StrLoad fixup. Fixture 1311.
            let body_offset = out.len();
            out.extend_from_slice(&[0xB8, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::StrLoad { string_idx: *string_idx },
            });
        }
        Expr::PtrChainField { base, hops, final_off, final_size } => {
            // Load the base pointer into BX.
            match base.as_ref() {
                Expr::Param(i) => { out.extend_from_slice(&[0x8B, 0x5E, param_disp(*i) as u8]); }
                Expr::Local(i) => { let d = locals.disp(*i); out.push(0x8B); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d); }
                Expr::Global(g) => {
                    out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
                    let bo = out.len() - 2;
                    fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
                }
                // `arr[i]->...` — load the runtime-indexed struct pointer into BX
                // (`mov bx,[i]; shl bx,1; mov bx,_arr[bx]`). Fixtures 3541, 2997.
                Expr::PtrArrayElem { array, index } => {
                    emit_ptr_array_load_bx(*array, index, locals, out, fixups);
                }
                // `a.next->...` — the pointer field of a struct VALUE: load
                // `[bp+disp+byte_off]` into BX. Fixtures 1928, 1419.
                Expr::LocalField { local, byte_off, .. } => {
                    let d = locals.disp(*local) + *byte_off as i16;
                    out.push(0x8B); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d);
                }
                Expr::GlobalField { global, byte_off, .. } => {
                    let bo = out.len();
                    out.push(0x8B); out.push(0x1E); out.extend_from_slice(&byte_off.to_le_bytes());
                    fixups.push(Fixup { body_offset: bo + 1, kind: FixupKind::GlobalAddr { global_idx: *global } });
                }
                other => panic!("unsupported ptr-chain base: {other:?}"),
            }
            // Walk each hop: mov bx,[bx+hop].
            let mov_bx_off = |out: &mut Vec<u8>, off: u16| {
                if off == 0 { out.extend_from_slice(&[0x8B, 0x1F]); }
                else if off < 128 { out.push(0x8B); out.push(0x5F); out.push(off as u8); }
                else { out.push(0x8B); out.push(0x9F); out.extend_from_slice(&off.to_le_bytes()); }
            };
            for &h in hops { mov_bx_off(out, h); }
            // Final field read at [bx + final_off]. final_off is interpreted as a
            // SIGNED i16 so a negative displacement (`(p-1)->x` → [bx-4]) uses the
            // disp8 form. Fixture 3251.
            let read = if *final_size == 1 { 0x8Au8 } else { 0x8B };
            let soff = *final_off as i16;
            if soff == 0 { out.push(read); out.push(0x07); }
            else if (-128..=127).contains(&soff) { out.push(read); out.push(0x47); out.push(soff as i8 as u8); }
            else { out.push(read); out.push(0x87); out.extend_from_slice(&final_off.to_le_bytes()); }
            if *final_size == 1 { out.push(0x98); } // cbw
        }
        Expr::StrLitByte { string_idx, byte_off } => {
            // `mov al, $SG+K; cbw` — CONST byte load via a StrLoad fixup whose
            // placeholder is K (the linker adds the string's CONST address).
            let body_offset = out.len();
            out.push(0xA0); // mov al, moffs8
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset, kind: FixupKind::StrLoad { string_idx: *string_idx } });
            out.push(0x98); // cbw
        }
        Expr::Global(idx) => {
            if locals.is_char_global(*idx) {
                // `a0 00 00` mov al, moffs8, then widen: `98` cbw (signed char,
                // fixture 1092) or `2a e4` sub ah,ah (unsigned char, zero-extend
                // — fixtures 460/466).
                let body_offset = out.len();
                out.extend_from_slice(&[0xA0, 0x00, 0x00]);
                fixups.push(Fixup {
                    body_offset,
                    kind: FixupKind::GlobalAddr { global_idx: *idx },
                });
                if locals.is_unsigned_global(*idx) {
                    out.extend_from_slice(&[0x2A, 0xE4]); // sub ah, ah
                } else {
                    out.push(0x98); // cbw
                }
            } else {
                // Reuse AX when the global was just stored from it (`a3 addr` =
                // mov [g],ax leaves AX intact) — e.g. `g /= b; return g`. The
                // store's address bytes are a placeholder fixed up to the same
                // global, so match on the trailing opcode + last fixup. 949/950.
                let stored_from_ax = out.len() >= 3
                    && out[out.len() - 3] == 0xA3
                    && fixups.last().is_some_and(|f| {
                        f.body_offset == out.len() - 3
                            && matches!(f.kind, FixupKind::GlobalAddr { global_idx } if global_idx == *idx)
                    });
                // Same idea when the global was just stored from DX (`89 16 addr`
                // = mov [g],dx, e.g. `g %= b; return g` leaves the remainder in
                // DX) → reuse via `mov ax,dx`. Fixture 950.
                let stored_from_dx = out.len() >= 4
                    && out[out.len() - 4] == 0x89
                    && out[out.len() - 3] == 0x16
                    && fixups.last().is_some_and(|f| {
                        f.body_offset == out.len() - 3
                            && matches!(f.kind, FixupKind::GlobalAddr { global_idx } if global_idx == *idx)
                    });
                if stored_from_ax {
                    // AX already holds g.
                } else if stored_from_dx {
                    out.extend_from_slice(&[0x8B, 0xC2]); // mov ax, dx
                } else {
                    // `a1 00 00` — mov ax, moffs16.
                    let body_offset = out.len();
                    out.extend_from_slice(&[0xA1, 0x00, 0x00]);
                    fixups.push(Fixup {
                        body_offset,
                        kind: FixupKind::GlobalAddr { global_idx: *idx },
                    });
                }
            }
        }
        Expr::PostMutateLocal { local_idx, step } => {
            // Load the OLD value into AX, then mutate the slot.
            let disp = locals.disp(*local_idx);
            if locals.size(*local_idx) == 1 {
                out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                out.push(0x98); // cbw
            } else {
                out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            }
            emit_postmutate_local(*step, locals.size(*local_idx), disp, out);
        }
        Expr::PreMutateLocal { local_idx, step } => {
            // Mutate first, then load the NEW value into AX.
            let disp = locals.disp(*local_idx);
            emit_postmutate_local(*step, locals.size(*local_idx), disp, out);
            if locals.size(*local_idx) == 1 {
                out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                out.push(0x98); // cbw
            } else {
                out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            }
        }
        Expr::PreMutateParam { param_idx, step } => {
            // Mutate the param's stack slot, then load the NEW value into AX.
            let disp = param_disp(*param_idx);
            let is_char = locals.is_char_param(*param_idx);
            let pointee = locals.param_pointee_size(*param_idx);
            let eff_step = if pointee > 0 { *step * pointee as i32 } else { *step };
            let slot_size = if is_char { 1 } else { 2 };
            crate::codegen::assign::emit_postmutate_local(eff_step, slot_size, disp, out);
            if is_char {
                out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                out.push(0x98); // cbw
            } else {
                out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            }
        }
        Expr::PostMutateParam { param_idx, step } => {
            // Load the OLD value into AX, then mutate the param's stack slot.
            let disp = param_disp(*param_idx);
            let is_char = locals.is_char_param(*param_idx);
            let pointee = locals.param_pointee_size(*param_idx);
            let eff_step = if pointee > 0 { *step * pointee as i32 } else { *step };
            let slot_size = if is_char { 1 } else { 2 };
            if is_char {
                out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                out.push(0x98); // cbw
            } else {
                out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            }
            crate::codegen::assign::emit_postmutate_local(eff_step, slot_size, disp, out);
        }
        Expr::PreMutateIndexedGlobal { array, index, step, is_byte } => {
            // `mov bx,[i]; shl bx,1 (word); <inc/add word/byte _arr[bx]>; mov
            // ax/al,_arr[bx]` (+cbw). Mutate then load NEW value. Fixture 2937.
            emit_load_bx(index, locals, out, fixups);
            if !*is_byte { out.extend_from_slice(&[0xD1, 0xE3]); } // shl bx,1
            // in-place mutate at [_arr + bx]
            let modrm = 0x87u8; // [bx]+disp16
            if *step == 1 || *step == -1 {
                out.push(if *is_byte { 0xFE } else { 0xFF });
                out.push(if *step == 1 { modrm } else { modrm + 8 }); // /0 inc, /1 dec
            } else {
                out.push(if *is_byte { 0x80 } else { 0x83 });
                out.push(modrm); // /0 add
            }
            let bo = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
            if !(*step == 1 || *step == -1) { out.push(*step as i8 as u8); }
            // load the new value
            out.push(if *is_byte { 0x8A } else { 0x8B }); out.push(0x87);
            let bo = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
            if *is_byte { out.push(0x98); }
        }
        Expr::PostMutateIndexedGlobal { array, index, step, is_byte } => {
            // `arr[i]++`: load the OLD element value, then mutate in place.
            if let Some(k) = index.fold(locals.inits) {
                // Const index: moffs forms `mov ax/al,[_a+off]; inc/add [_a+off]`.
                let off = (k as u32).wrapping_mul(if *is_byte { 1 } else { 2 }) as u16;
                let bo = out.len();
                out.push(if *is_byte { 0xA0 } else { 0xA1 }); out.extend_from_slice(&off.to_le_bytes());
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *array } });
                if *is_byte { out.push(0x98); }
                if *step == 1 || *step == -1 {
                    out.push(if *is_byte { 0xFE } else { 0xFF }); out.push(if *step == 1 { 0x06 } else { 0x0E });
                } else {
                    out.push(if *is_byte { 0x80 } else { 0x83 }); out.push(0x06);
                }
                let bo = out.len(); out.extend_from_slice(&off.to_le_bytes());
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
                if !(*step == 1 || *step == -1) { out.push(*step as i8 as u8); }
            } else {
                // Runtime index: `mov bx,[i]; shl bx,1; mov ax,_arr[bx]; inc word _arr[bx]`.
                emit_load_bx(index, locals, out, fixups);
                if !*is_byte { out.extend_from_slice(&[0xD1, 0xE3]); }
                out.push(if *is_byte { 0x8A } else { 0x8B }); out.push(0x87);
                let bo = out.len(); out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
                if *is_byte { out.push(0x98); }
                if *step == 1 || *step == -1 {
                    out.push(if *is_byte { 0xFE } else { 0xFF }); out.push(if *step == 1 { 0x87 } else { 0x8F });
                } else {
                    out.push(if *is_byte { 0x80 } else { 0x83 }); out.push(0x87);
                }
                let bo = out.len(); out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
                if !(*step == 1 || *step == -1) { out.push(*step as i8 as u8); }
            }
        }
        Expr::PostMutateLocalIndex { local, index, step, is_byte } => {
            // `a[K]++` on a local array (constant index): load the OLD element
            // into AX/AL, then mutate the slot in place. Fixture 1418.
            let k = index.fold(locals.inits)
                .expect("runtime local-array post-mutate index not yet supported");
            let elem = if *is_byte { 1 } else { 2 };
            let disp = locals.disp(*local) + (k as i16) * elem;
            // mov ax/al, [bp+disp]
            out.push(if *is_byte { 0x8A } else { 0x8B });
            out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            if *is_byte { out.push(0x98); } // cbw
            // inc/dec or add/sub the slot in place.
            if *step == 1 || *step == -1 {
                out.push(if *is_byte { 0xFE } else { 0xFF });
                out.push(bp_modrm(if *step == 1 { 0x46 } else { 0x4E }, disp));
                push_bp_disp(out, disp);
            } else {
                out.push(if *is_byte { 0x80 } else { 0x83 });
                out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                out.push(*step as i8 as u8);
            }
        }
        Expr::PostIncDeref { ptr, step, is_byte } => {
            // Value of `*p++`: the OLD pointee. Load p into BX, advance [p], then
            // deref BX. Fixture 3102.
            emit_postinc_ptr_to_bx(ptr, *step, locals, out, fixups);
            if *is_byte {
                out.extend_from_slice(&[0x8A, 0x07, 0x98]); // mov al,[bx]; cbw
            } else {
                out.extend_from_slice(&[0x8B, 0x07]); // mov ax,[bx]
            }
        }
        Expr::PreMutateDeref { ptr, step, is_byte } => {
            // `mov bx,[p]; <inc/dec/add> <word|byte> [bx]; mov ax,[bx]` (load the
            // NEW value). For step != ±1, `add word [bx], step`.
            emit_load_bx(ptr, locals, out, fixups); // mov bx, [p]
            match (*step, *is_byte) {
                (1, false) => out.extend_from_slice(&[0xFF, 0x07]),  // inc word [bx]
                (-1, false) => out.extend_from_slice(&[0xFF, 0x0F]), // dec word [bx]
                (1, true) => out.extend_from_slice(&[0xFE, 0x07]),   // inc byte [bx]
                (-1, true) => out.extend_from_slice(&[0xFE, 0x0F]),  // dec byte [bx]
                (k, false) => {
                    out.extend_from_slice(&[0x81, 0x07]); // add word [bx], imm16
                    out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
                }
                (k, true) => out.extend_from_slice(&[0x80, 0x07, (k as u32 & 0xFF) as u8]), // add byte [bx], imm8
            }
            if *is_byte {
                out.extend_from_slice(&[0x8A, 0x07, 0x98]); // mov al,[bx]; cbw
            } else {
                out.extend_from_slice(&[0x8B, 0x07]); // mov ax,[bx]
            }
        }
        Expr::PostMutateDeref { ptr, step, is_byte } => {
            // `(*p)++` — read the OLD pointee into AX, THEN increment `[bx]` in
            // place: `mov bx,[p]; mov ax,[bx]; inc word [bx]`. Fixtures 2857/3107.
            emit_load_bx(ptr, locals, out, fixups);
            if *is_byte {
                out.extend_from_slice(&[0x8A, 0x07, 0x98]); // mov al,[bx]; cbw
            } else {
                out.extend_from_slice(&[0x8B, 0x07]); // mov ax,[bx]
            }
            match (*step, *is_byte) {
                (1, false) => out.extend_from_slice(&[0xFF, 0x07]),  // inc word [bx]
                (-1, false) => out.extend_from_slice(&[0xFF, 0x0F]), // dec word [bx]
                (1, true) => out.extend_from_slice(&[0xFE, 0x07]),   // inc byte [bx]
                (-1, true) => out.extend_from_slice(&[0xFE, 0x0F]),  // dec byte [bx]
                (k, false) => { out.extend_from_slice(&[0x81, 0x07]); out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); }
                (k, true) => out.extend_from_slice(&[0x80, 0x07, (k as u32 & 0xFF) as u8]),
            }
        }
        Expr::PostMutateGlobal { global_idx, step } => {
            // Load the OLD value into AX, then mutate the global.
            if locals.is_char_global(*global_idx) {
                let bo = out.len();
                out.extend_from_slice(&[0xA0, 0x00, 0x00]);  // mov al, [g]
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *global_idx } });
                // For a char global MSC mutates the byte BEFORE widening the old
                // value (`mov al,[g]; inc byte [g]; cbw`). Fixtures 966/972.
                emit_postmutate_global(*step, *global_idx, true, out, fixups);
                out.push(0x98);                                // cbw
            } else {
                let bo = out.len();
                out.extend_from_slice(&[0xA1, 0x00, 0x00]);  // mov ax, [g]
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *global_idx } });
                emit_postmutate_global(*step, *global_idx, false, out, fixups);
            }
        }
        Expr::PreMutateGlobal { global_idx, step } => {
            // Mutate first, then load the NEW value into AX.
            emit_postmutate_global(*step, *global_idx, locals.is_char_global(*global_idx), out, fixups);
            if locals.is_char_global(*global_idx) {
                let bo = out.len();
                out.extend_from_slice(&[0xA0, 0x00, 0x00]);  // mov al, [g]
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *global_idx } });
                out.push(0x98);  // cbw
            } else {
                let bo = out.len();
                out.extend_from_slice(&[0xA1, 0x00, 0x00]);  // mov ax, [g]
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *global_idx } });
            }
        }
        Expr::DerefByte { ptr } => {
            // Phase 1: `*<char-ptr-global>` and `*(<char-ptr> + K)`.
            // Both lower to `mov bx, [p]; mov al, [bx+disp]; cbw`,
            // with disp 0 for the bare deref and K for the
            // constant-offset form (fixtures 4111, 4127).
            // `*p++` for char* locals: load old ptr into BX, advance, byte-deref.
            if let Expr::PostMutateLocal { local_idx, step } = ptr.as_ref() {
                let disp = locals.disp(*local_idx);
                { out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); }  // mov bx,[bp-p]
                emit_postmutate_local(*step, locals.size(*local_idx), disp, out);
                out.extend_from_slice(&[0x8A, 0x07, 0x98]);         // mov al,[bx]; cbw
                return;
            }
            // `*<char-ptr local>`: load p from its slot, byte-deref + widen.
            if let Expr::Local(idx) = ptr.as_ref() {
                let disp = locals.disp(*idx);
                out.push(0x8B);
                out.push(bp_modrm(0x5E, disp));
                push_bp_disp(out, disp); // mov bx, [bp-disp]
                out.extend_from_slice(&[0x8A, 0x07, 0x98]); // mov al, [bx]; cbw
                return;
            }
            // `*<char-ptr param>` / `s[0]` on a char* param: load p from its
            // slot, byte-deref + widen (`mov bx,[bp+pd]; mov al,[bx]; cbw`).
            // Fixtures 2618 / 2919 / 2702.
            if let Expr::Param(idx) = ptr.as_ref() {
                let d = param_disp(*idx);
                out.push(0x8B); out.push(0x5E); out.push(d as u8); // mov bx, [bp+pd]
                out.extend_from_slice(&[0x8A, 0x07, 0x98]); // mov al, [bx]; cbw
                return;
            }
            // `*(base + idx)` byte deref — decayed global array / pointer
            // param/local, constant or runtime index. Fixtures 2546, 3227.
            if let Expr::BinOp { op: BinOp::Add, left, right } = ptr.as_ref()
                && matches!(left.as_ref(), Expr::AddrOfGlobal(_) | Expr::Param(_) | Expr::Local(_))
                && emit_offset_deref(left, right, true, locals, out, fixups)
            {
                return;
            }
            let (ptr_idx, disp) = match ptr.as_ref() {
                Expr::Global(idx) => (*idx, 0i8),
                Expr::BinOp { op: BinOp::Add, left, right }
                | Expr::BinOp { op: BinOp::Add, left: right, right: left }
                    if matches!(left.as_ref(), Expr::Global(_))
                    && matches!(right.as_ref(), Expr::IntLit(_)) =>
                {
                    let Expr::Global(idx) = **left else { unreachable!() };
                    let Expr::IntLit(k) = **right else { unreachable!() };
                    (idx, i8::try_from(k).expect("ptr-add offset fits in i8"))
                }
                other => panic!("byte deref of {other:?} not yet supported"),
            };
            let body_offset = out.len();
            out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset + 1,
                kind: FixupKind::GlobalAddr { global_idx: ptr_idx },
            });
            if disp == 0 {
                out.extend_from_slice(&[0x8A, 0x07, 0x98]);
            } else {
                out.extend_from_slice(&[0x8A, 0x47, disp as u8, 0x98]);
            }
        }
        Expr::DerefWord { ptr } => {
            // Phase 1: `*<int-ptr-param>` (fixture 4125). Pattern is
            // `mov bx, [bp+disp]; mov ax, [bx]`. Future fixtures with
            // a global int-pointer source pick up the BX-from-global
            // load shape.
            match ptr.as_ref() {
                // `*p++` — load old ptr into BX, advance ptr, read from old location.
                Expr::PostMutateLocal { local_idx, step } => {
                    let disp = locals.disp(*local_idx);
                    // If AX still holds p (the immediately-preceding `p = <addr>`
                    // store was `mov [bp-p], ax`), reuse it via `mov bx,ax`
                    // instead of reloading `mov bx,[bp-p]` (fixture 1289).
                    let store = {
                        let mut v = vec![0x89, bp_modrm(0x46, disp)];
                        push_bp_disp(&mut v, disp);
                        v
                    };
                    if out.len() >= store.len() && out[out.len() - store.len()..] == store[..] {
                        out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
                    } else {
                        out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); // mov bx,[bp-p]
                    }
                    emit_postmutate_local(*step, locals.size(*local_idx), disp, out);
                    out.extend_from_slice(&[0x8B, 0x07]);               // mov ax,[bx]
                }
                Expr::Param(i) => {
                    let disp = (4 + (*i * 2)) as i16;
                    { out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); }
                    out.extend_from_slice(&[0x8B, 0x07]);
                }
                Expr::Global(idx) => {
                    let body_offset = out.len();
                    out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
                    fixups.push(Fixup {
                        body_offset: body_offset + 1,
                        kind: FixupKind::GlobalAddr { global_idx: *idx },
                    });
                    out.extend_from_slice(&[0x8B, 0x07]);
                }
                Expr::Local(i) => {
                    // Deref a local pointer. For far/huge pointers use
                    // `les bx,[bp-p]; mov ax,es:[bx]`; for near pointers
                    // `mov bx,[bp-p]; mov ax,[bx]`.
                    let disp = locals.disp(*i) as u8;
                    if locals.is_far_ptr_local(*i) {
                        out.extend_from_slice(&[0xC4, 0x5E, disp]); // les bx,[bp-p]
                        out.extend_from_slice(&[0x26, 0x8B, 0x07]); // mov ax,es:[bx]
                    } else {
                        emit_ptr_local_to_bx(locals.disp(*i), locals, out); // mov bx,[bp-p] / mov bx,ax
                        out.extend_from_slice(&[0x8B, 0x07]);        // mov ax,[bx]
                    }
                }
                Expr::Param(i) => {
                    // `mov bx, [bp+disp]; mov ax, [bx]`.
                    let disp = param_disp(*i);
                    { out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); }
                    out.extend_from_slice(&[0x8B, 0x07]);
                }
                Expr::DerefWord { ptr: inner_ptr } => {
                    // Double-deref (`**p`). MSC keeps the intermediate
                    // value in BX: load pointer → BX, deref to BX, then
                    // deref BX to AX. Avoids an `mov bx, ax` round-trip.
                    match inner_ptr.as_ref() {
                        Expr::Global(idx) => {
                            let body_offset = out.len();
                            out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
                            fixups.push(Fixup {
                                body_offset: body_offset + 1,
                                kind: FixupKind::GlobalAddr { global_idx: *idx },
                            });
                        }
                        Expr::Local(li) => {
                            let disp = locals.disp(*li);
                            { out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); }
                        }
                        Expr::Param(pi) => {
                            let disp = param_disp(*pi);
                            { out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); }
                        }
                        _ => {
                            // Generic fallback: evaluate inner DerefWord
                            // into AX then transfer to BX.
                            emit_expr_to_ax(&Expr::DerefWord { ptr: inner_ptr.clone() }, locals, out, fixups);
                            out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
                            out.extend_from_slice(&[0x8B, 0x07]); // mov ax, [bx]
                            return;
                        }
                    }
                    out.extend_from_slice(&[0x8B, 0x1F]); // mov bx, [bx]
                    out.extend_from_slice(&[0x8B, 0x07]); // mov ax, [bx]
                }
                Expr::BinOp { op: BinOp::Add, left, right } => {
                    // `*(base + idx)` word deref — decayed global array, or a
                    // pointer param/local, with a constant or runtime index.
                    // Fixtures 1152, 1379, 3025, 3189.
                    if emit_offset_deref(left, right, false, locals, out, fixups) {
                        return;
                    }
                    panic!("word deref of {:?} not yet supported", ptr);
                }
                // `*(g.p)` — a global struct-field pointer loads straight into
                // BX (`mov bx,[g+off]`), no AX round-trip. Fixture 2981.
                Expr::GlobalField { size: 2, .. } => {
                    emit_load_bx(ptr, locals, out, fixups);
                    out.extend_from_slice(&[0x8B, 0x07]); // mov ax,[bx]
                }
                other => {
                    // Generic: evaluate the pointer into AX, move to
                    // BX, deref. Covers Call returns (fixture 1343),
                    // Ternary, etc.
                    emit_expr_to_ax(other, locals, out, fixups);
                    out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
                    out.extend_from_slice(&[0x8B, 0x07]);
                }
            }
        }
        Expr::PtrIndexByte { ptr, index } => {
            // Constant index through a global pointer: load it into BX, then
            // read the element. A `char *p` reads a byte + `cbw` (fixture
            // 4123); an `int *p` reads a word and scales the index by 2
            // (`mov bx,_p; mov ax,[bx+K*2]`, fixture 2939). The pointee width
            // comes from the global's element size.
            let k = index.fold(locals.inits).unwrap_or_else(|| {
                panic!("non-constant ptr index not yet supported")
            });
            let elem = locals.global_elem_size(*ptr).max(1) as i32;
            let off = i8::try_from(k * elem).expect("ptr index fits in i8");
            let body_offset = out.len();
            out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset + 1,
                kind: FixupKind::GlobalAddr { global_idx: *ptr },
            });
            let read = if elem == 1 { 0x8Au8 } else { 0x8B }; // mov al / mov ax
            // disp 0 uses the no-displacement modrm `8a 07` (fixture 192).
            if off == 0 {
                out.extend_from_slice(&[read, 0x07]);
            } else {
                out.extend_from_slice(&[read, 0x47, off as u8]);
            }
            if elem == 1 { out.push(0x98); } // cbw
        }
        Expr::StructArrayField { array, index, stride, field_off, size } => {
            // `arr[i].field`: `mov bx,[i]; <scale bx by stride>; mov ax/al,
            // [_arr + bx + field_off]`. The field offset folds into the fixup.
            emit_load_bx(index, locals, out, fixups);
            let s = *stride as usize;
            if s.is_power_of_two() {
                for _ in 0..s.trailing_zeros() { out.extend_from_slice(&[0xD1, 0xE3]); } // shl bx,1
            } else {
                out.push(0x69); out.push(0xDB); out.extend_from_slice(&stride.to_le_bytes()); // imul bx,bx,stride
            }
            out.push(if *size == 1 { 0x8A } else { 0x8B }); out.push(0x87); // mov ax/al,[bx+disp16]
            let bo = out.len();
            out.extend_from_slice(&field_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
            if *size == 1 { out.push(0x98); } // cbw
        }
        Expr::PtrArrayElem { array, index } => {
            // Pointer VALUE at element `index` of an array-of-pointers global.
            if let Some(k) = index.fold(locals.inits) {
                // mov ax, _arr+2k   (a1 <off16>)
                let off = (k as u32).wrapping_mul(2) as u16;
                let body_offset = out.len();
                out.push(0xA1);
                out.extend_from_slice(&off.to_le_bytes());
                fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: *array } });
            } else {
                // mov bx,[i]; shl bx,1; mov ax,[bx+_arr]   (8b 87 <off16>)
                emit_load_bx(index, locals, out, fixups);
                out.extend_from_slice(&[0xD1, 0xE3]); // shl bx,1
                out.push(0x8B); out.push(0x87);
                let bo = out.len();
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
            }
        }
        Expr::PtrArrayDeref { array, index, inner, elem_size } => {
            emit_ptr_array_load_bx(*array, index, locals, out, fixups);
            emit_ptr_array_read_elem(inner, *elem_size, locals, out);
        }
        Expr::IndexByte { array, index } => {
            // Constant index → `a0 <byte_off> 98` (the placeholder is the index;
            // the linker adds the array base). Runtime index → BX-based:
            // `mov bx,[i]; mov al,[bx+&arr]; cbw`. Fixtures 4109, 3231.
            // Unsigned char array → zero-extend (`sub ah,ah`); signed → `cbw`.
            let widen: &[u8] = if locals.is_unsigned_global(*array) { &[0x2A, 0xE4] } else { &[0x98] };
            if let Some(k) = index.fold(locals.inits) {
                let byte_off = (k as u32 & 0xFFFF) as u16;
                let body_offset = out.len();
                out.push(0xA0);
                out.extend_from_slice(&byte_off.to_le_bytes());
                fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: *array } });
                out.extend_from_slice(widen);
            } else {
                emit_load_bx(index, locals, out, fixups);
                out.push(0x8A); out.push(0x87); // mov al, [bx + disp16]
                let bo = out.len();
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
                out.extend_from_slice(widen);
            }
        }
        Expr::Index { array, index } => {
            if let Some(k) = index.fold(locals.inits) {
                // Constant index → `a1 <byte_off>` with FIXUP. The
                // placeholder is `byte_off` (not zero); the linker
                // adds the array's base address. Fixture 4109.
                let byte_off = (k as u32).wrapping_mul(2) as u16;
                let body_offset = out.len();
                out.push(0xA1);
                out.extend_from_slice(&byte_off.to_le_bytes());
                fixups.push(Fixup {
                    body_offset,
                    kind: FixupKind::GlobalAddr { global_idx: *array },
                });
            } else {
                // Variable index → load the variable part into BX, scale ×2 with
                // `shl bx, 1`, then `mov ax, [bx + _arr + koff*2]`. A `± K`
                // constant in the index folds into the FIXUP displacement
                // (`_arr[bx+2]`), not a separate add. Fixtures 4112, 3028, 3033.
                let (var, koff) = split_index_offset(index);
                // A char param index widens via AL/cbw before landing in BX.
                if let Expr::Param(i) = var
                    && locals.is_char_param(*i)
                {
                    let d = param_disp(*i);
                    out.push(0x8A); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); // mov al,[bp+d]
                    out.push(0x98); // cbw
                    out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
                } else {
                    emit_load_bx(var, locals, out, fixups);
                }
                out.extend_from_slice(&[0xD1, 0xE3]); // shl bx,1
                let off = (koff.wrapping_mul(2)) as i16 as u16;
                let body_offset = out.len();
                out.extend_from_slice(&[0x8B, 0x87]);
                out.extend_from_slice(&off.to_le_bytes());
                fixups.push(Fixup {
                    body_offset: body_offset + 1,
                    kind: FixupKind::GlobalAddr { global_idx: *array },
                });
            }
        }
        Expr::AddrOfGlobal(idx) => {
            let body_offset = out.len();
            out.extend_from_slice(&[0xB8, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *idx },
            });
        }
        Expr::AddrOfLocal(idx) => {
            // `lea ax, [bp-disp]` = `8d 46 disp`.
            let disp = locals.disp(*idx);
            out.push(0x8D); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        }
        Expr::AddrOfIndexedGlobal { array, index, elem } => {
            // `&arr[i]` runtime: `<i→ax>; shl ax (×log2 elem); add ax,OFFSET _arr`.
            emit_expr_to_ax(index, locals, out, fixups);
            for _ in 0..(*elem as usize).max(1).trailing_zeros() { out.extend_from_slice(&[0xD1, 0xE0]); } // shl ax,1
            let bo = out.len();
            out.extend_from_slice(&[0x05, 0x00, 0x00]); // add ax, imm16
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *array } });
        }
        Expr::Ternary { cond, then_arm, else_arm } => {
            // When cond is a compile-time constant, MSC skips the branch
            // structure entirely and emits just the selected arm (load,
            // not an inline immediate). Fixture 1038: `a ? b : c` with
            // a=1 → `mov ax, [bp-4]` (load b at runtime).
            if let Some(k) = cond.fold(locals.inits) {
                let arm = if k != 0 { then_arm } else { else_arm };
                emit_expr_to_ax(arm, locals, out, fixups);
                return;
            }
            // "max/min" select-operand idiom: `(L OP R) ? L : R` where L,R are
            // the same simple word bp-relative reads as the comparison's own
            // operands. MSC reuses the AX already loaded with L for the cmp and
            // selects R with a SINGLE forward jcc — no jmp, no second branch.
            //   mov ax,[bp+L]; cmp ax,[bp+R]; jge/jle +szR; mov ax,[bp+R]
            // The jcc uses the "or-equal" variant (a==b ⟹ L,R values equal, so
            // the equality case folds into keep-then). Fixtures 3617/3621/2719.
            if let Expr::BinOp { op, left: cl, right: cr } = cond.as_ref()
                && matches!(op, BinOp::Gt | BinOp::Ge | BinOp::Lt | BinOp::Le)
                && same_var(then_arm, cl)
                && same_var(else_arm, cr)
                && let Some(ld) = simple_word_bp(cl, locals)
                && let Some(rd) = simple_word_bp(cr, locals)
            {
                let jcc = match op { BinOp::Gt | BinOp::Ge => 0x7Du8, _ => 0x7E };
                // mov ax,[bp+ld]
                out.push(0x8B); out.push(bp_modrm(0x46, ld)); push_bp_disp(out, ld);
                // cmp ax,[bp+rd]
                out.push(0x3B); out.push(bp_modrm(0x46, rd)); push_bp_disp(out, rd);
                // else load size = mov ax,[bp+rd]
                let mut else_buf = Vec::new();
                else_buf.push(0x8B); else_buf.push(bp_modrm(0x46, rd)); push_bp_disp(&mut else_buf, rd);
                out.push(jcc);
                out.push(i8::try_from(else_buf.len()).expect("select-operand else fits in rel8") as u8);
                out.extend_from_slice(&else_buf);
                return;
            }
            // Runtime ternary: `cond ? b : c`. MSC emits the condition as a
            // DIRECT comparison + inverted jcc that skips to the else arm —
            // not a materialized 0/1 boolean. Shape (fixture 429
            // `g>0?g:h`):
            //   cmp word [g],0 ; jle else      (emit_cond_skip)
            //   <then>                          value in AX
            //   jmp end                         (eb, skips pad + else)
            //   [90]                            alignment nop (else starts even)
            //   <else>                          value in AX
            // Pre-emit both arms to size the jumps.
            let mut then_buf: Vec<u8> = Vec::new();
            let mut then_fixups: Vec<Fixup> = Vec::new();
            emit_expr_to_ax(then_arm, locals, &mut then_buf, &mut then_fixups);
            let mut else_buf: Vec<u8> = Vec::new();
            let mut else_fixups: Vec<Fixup> = Vec::new();
            emit_expr_to_ax(else_arm, locals, &mut else_buf, &mut else_fixups);
            let cond_c = cond_from_expr((**cond).clone());
            // Size the condition's cmp+jcc to compute the alignment nop.
            let cond_size = {
                let mut buf = Vec::new();
                emit_cond_skip(&cond_c, 0, locals, &mut buf, &mut Vec::new());
                buf.len()
            };
            let then_block = then_buf.len() + 2; // then arm + 2-byte jmp
            let needs_nop = (out.len() + cond_size + then_block) % 2 != 0;
            let skip = i8::try_from(then_block + needs_nop as usize)
                .expect("ternary then-block fits in rel8");
            emit_cond_skip(&cond_c, skip, locals, out, fixups);
            let then_base = out.len();
            for mut f in then_fixups { f.body_offset += then_base; fixups.push(f); }
            out.extend_from_slice(&then_buf);
            // jmp over pad + else.
            out.push(0xEB);
            out.push(i8::try_from(needs_nop as usize + else_buf.len())
                .expect("ternary else arm fits in rel8") as u8);
            if needs_nop { out.push(0x90); }
            let else_base = out.len();
            for mut f in else_fixups { f.body_offset += else_base; fixups.push(f); }
            out.extend_from_slice(&else_buf);
        }
        Expr::Seq { sides, value } => {
            // Evaluate sides for side effects, discard their AX value
            // (statements don't yield), then evaluate value into AX.
            // Fixture 1057, 1114, etc.
            // We need frame/return_int — punt to a wrapper that uses
            // emit_stmt with no return_int (we're inside an expression).
            // Use a noop frame since these stmts shouldn't include
            // returns or `chkstk`.
            for s in sides {
                emit_stmt(s, locals, Frame::BpOnly, true, false, out, fixups);
            }
            emit_expr_to_ax(value, locals, out, fixups);
        }
        Expr::BitField { base, byte_off, bit_off, bit_width } => {
            let mask = (1u32 << *bit_width) - 1;
            // Byte-aligned full-byte field → read the byte directly + zero-extend.
            if *bit_width == 8 && bit_off % 8 == 0 {
                bf_load_byte_al(*base, byte_off + (*bit_off / 8) as u16, locals, out, fixups);
                out.extend_from_slice(&[0x2A, 0xE4]); // sub ah,ah (zero-extend)
            } else {
                // Reuse a live AX left by a preceding store to the same unit
                // (`f.b=K; return f.b` keeps `_f` in AX — no reload). Fixture 2529.
                if !bf_ax_live(*base, *byte_off, locals, out, fixups) {
                    bf_load_word(*base, *byte_off, locals, out, fixups);
                }
                if *bit_off == 1 {
                    out.extend_from_slice(&[0xD1, 0xE8]); // shr ax,1
                } else if *bit_off > 1 {
                    out.push(0xB1); out.push(*bit_off); // mov cl, bit_off
                    out.extend_from_slice(&[0xD3, 0xE8]); // shr ax,cl
                }
                if *bit_width < 16 {
                    out.push(0x25); // and ax, mask
                    out.extend_from_slice(&(mask as u16).to_le_bytes());
                }
            }
        }
        Expr::GlobalField { global, byte_off, size } => {
            // Word field: `a1 byte_off byte_off` + GlobalAddr FIXUP.
            // Byte field: `a0 byte_off byte_off; 98` + FIXUP.
            let body_offset = out.len();
            if *size == 1 {
                out.push(0xA0);
            } else {
                out.push(0xA1);
            }
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *global },
            });
            if *size == 1 {
                out.push(0x98);
            }
        }
        Expr::LocalField { local, byte_off, size } => {
            let disp = locals.disp(*local) + *byte_off as i16;
            if *size == 1 {
                out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                out.push(0x98); // cbw
            } else {
                out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            }
        }
        Expr::ParamField { param, byte_off, size } => {
            // Field of a struct passed by value: `[bp + param_base + byte_off]`.
            let disp = locals.param_base_disp(*param) + *byte_off as i16;
            if *size == 1 {
                out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                out.push(0x98); // cbw
            } else {
                out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            }
        }
        Expr::DerefParamField { ptr_param, byte_off, size } => {
            let p_disp = param_disp(*ptr_param);
            out.push(0x8B);
            out.push(0x5E);
            out.push(p_disp as u8);
            if *size == 1 {
                if *byte_off == 0 {
                    out.extend_from_slice(&[0x8A, 0x07]);
                } else {
                    out.push(0x8A);
                    out.push(0x47);
                    out.push(*byte_off as u8);
                }
                out.push(0x98);
            } else if *byte_off == 0 {
                out.extend_from_slice(&[0x8B, 0x07]);
            } else {
                out.push(0x8B);
                out.push(0x47);
                out.push(*byte_off as u8);
            }
        }
        Expr::DerefLocalField { ptr_local, byte_off, size } => {
            // mov bx, [bp + ptr_disp]; mov ax, [bx + byte_off] (word)
            //                          or mov al, [bx + byte_off]; cbw (byte)
            let p_disp = locals.disp(*ptr_local);
            emit_ptr_local_to_bx(p_disp, locals, out); // mov bx,[bp+disp] / mov bx,ax
            if *size == 1 {
                if *byte_off == 0 {
                    out.extend_from_slice(&[0x8A, 0x07]);
                } else {
                    out.push(0x8A);
                    out.push(0x47);
                    out.push(*byte_off as u8);
                }
                out.push(0x98); // cbw
            } else if *byte_off == 0 {
                out.extend_from_slice(&[0x8B, 0x07]);
            } else {
                out.push(0x8B);
                out.push(0x47);
                out.push(*byte_off as u8);
            }
        }
        Expr::DerefGlobalField { ptr_global, byte_off, size } => {
            // `mov bx, [global]; mov ax/al, [bx+off]; cbw?`.
            out.push(0x8B);
            out.push(0x1E);
            let body_offset = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset - 1,
                kind: FixupKind::GlobalAddr { global_idx: *ptr_global },
            });
            if *size == 1 {
                if *byte_off == 0 {
                    out.extend_from_slice(&[0x8A, 0x07]);
                } else {
                    out.extend_from_slice(&[0x8A, 0x47, *byte_off as u8]);
                }
                out.push(0x98);
            } else if *byte_off == 0 {
                out.extend_from_slice(&[0x8B, 0x07]);
            } else {
                out.extend_from_slice(&[0x8B, 0x47, *byte_off as u8]);
            }
        }
        Expr::ParamIndex { param, index } => {
            let p_disp = param_disp(*param);
            if let Some(k) = index.fold(locals.inits) {
                // Constant K → `mov bx, [bp+param_disp]; mov ax, [bx+2K]`.
                // Fixture 1236.
                out.push(0x8B);
                out.push(0x5E);
                out.push(p_disp as u8);
                let elem_disp = (k as i16) * 2;
                if elem_disp == 0 {
                    out.extend_from_slice(&[0x8B, 0x07]);
                } else {
                    out.push(0x8B);
                    out.push(0x47);
                    out.push(elem_disp as u8);
                }
            } else {
                // Runtime index `a[i]` on an `int *` / `int a[]` param: load the
                // index into BX, scale ×2, load the param pointer into SI, then
                // `mov ax,[bx+si]`. Requires a SI-saving frame. Fixtures 1385,
                // 2849, 2046, 2266, 2454.
                emit_load_bx(index, locals, out, fixups); // mov bx,[i]
                out.extend_from_slice(&[0xD1, 0xE3]); // shl bx,1
                out.push(0x8B); out.push(bp_modrm(0x76, p_disp)); push_bp_disp(out, p_disp); // mov si,[bp+param]
                out.extend_from_slice(&[0x8B, 0x00]); // mov ax,[bx+si]
            }
        }
        Expr::LocalIndex { local, index } => {
            if let Some(k) = index.fold(locals.inits) {
                // Constant K → `mov ax, [bp+disp+2K]`.
                let disp = locals.disp(*local) + (k as i16) * 2;
                out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            } else {
                // Runtime index: load into SI, scale, load element.
                // Requires Frame::WithSlideSi (push si in prologue).
                emit_index_to_si(index, locals, out);
                out.extend_from_slice(&[0xD1, 0xE6]); // shl si, 1
                let base_disp = locals.disp(*local);
                out.push(0x8B); out.push(bp_modrm(0x42, base_disp)); push_bp_disp(out, base_disp); // mov ax,[bp+si+base]
            }
        }
        Expr::LocalIndexByte { local, index } => {
            if let Some(k) = index.fold(locals.inits) {
                // Constant K → `mov al, [bp+disp+K]; cbw`.
                let disp = locals.disp(*local) + (k as i16);
                out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                out.push(0x98);
            } else {
                // Runtime index: load into SI (no scale for bytes), load byte, cbw.
                emit_index_to_si(index, locals, out);
                let base_disp = locals.disp(*local);
                out.push(0x8A); out.push(bp_modrm(0x42, base_disp)); push_bp_disp(out, base_disp); // mov al,[bp+si+base]
                out.push(0x98); // cbw
            }
        }
    }
}
/// `mov ax, word ptr [bp + 4 + 2*idx]` — load a parameter into AX.
pub(crate) fn emit_load_param(idx: usize, out: &mut Vec<u8>) {
    let disp = i8::try_from(4 + (idx * 2)).expect("param disp fits in i8");
    out.push(0x8B);
    out.push(0x46);
    out.push(disp as u8);
}
/// `mov ax/al, word/byte ptr [bp-disp]` — load a local into AX.
pub(crate) fn emit_load_local(idx: usize, locals: &Locals<'_>, out: &mut Vec<u8>) {
    let disp = locals.disp(idx);
    if locals.size(idx) == 1 {
        out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        if locals.is_unsigned_local(idx) {
            // `mov al, [bp-disp]; sub ah, ah` — zero-extend for unsigned char.
            out.extend_from_slice(&[0x2A, 0xE4]); // sub ah, ah
        } else {
            out.push(0x98); // cbw — sign-extend for signed char
        }
    } else {
        out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
    }
}
/// Heuristic AX-liveness reuse: returns true when the trailing bytes of `out`
/// prove AX already holds the word operand whose `mov ax,[op]` encoding is
/// `load` and whose `mov [op],ax` self-store is `store_self`. MSC keeps a value
/// just loaded into / stored from AX live across adjacent straight-line
/// statements and reuses it instead of reloading. Two trailing shapes:
///   (A) out ends with `store_self` — `op` was just written from AX
///       (`x = …` then a read of `x`; fixtures 1970 `sum += t`, 3014 `s = s+x`).
///   (B) out ends with `load` then exactly one store-from-AX to another slot —
///       `op` was loaded then spilled, leaving AX unchanged (3014 `s = a` then
///       `x = a + 1` reuses `a`).
/// A store-from-AX never clobbers AX, so both shapes leave AX == op. This is a
/// byte-pattern heuristic (same approach as the `return <local>` reload
/// elision); it only fires when the defining op is literally the last thing
/// emitted, i.e. straight-line within a basic block.
pub(crate) fn ax_holds_word_operand(out: &[u8], load: &[u8], store_self: &[u8], barrier: usize) -> bool {
    let ends = |pat: &[u8]| out.len() >= pat.len() && out[out.len() - pat.len()..] == *pat;
    // The instruction that established AX's value must be STRAIGHT-LINE (at or past
    // the last branch merge) — otherwise AX isn't reliably live (fixture 1445).
    if ends(store_self) {
        return out.len() - store_self.len() >= barrier;
    }
    // Pattern B: the operand was loaded into AX, then ONE OR MORE stores from AX
    // to other slots followed — none of those modify AX, so AX still holds the
    // operand (e.g. `r.a=v; r.b=v; r.c=v` keeps v live). Strip trailing
    // stores-from-AX until the operand's load is exposed.
    let mut n = out.len();
    loop {
        if n >= load.len() && out[n - load.len()..n] == *load {
            return n - load.len() >= barrier;
        }
        // Length of a trailing store-from-AX: `89 46 d8` / `89 86 d16` (bp-rel)
        // or `a3 o16` (global moffs).
        let store_len = if n >= 3 && out[n - 3] == 0x89 && out[n - 2] == 0x46 {
            3
        } else if n >= 4 && out[n - 4] == 0x89 && out[n - 3] == 0x86 {
            4
        } else if n >= 3 && out[n - 3] == 0xA3 {
            3
        } else {
            return false;
        };
        n -= store_len;
    }
}
/// `mov ax,[local i]`, eliding the load when AX provably already holds local
/// `i` (straight-line reuse — see [`ax_holds_word_operand`]). Plain int slots
/// only; long / float / char fall back to a real load.
pub(crate) fn emit_load_local_reuse(i: usize, locals: &Locals<'_>, out: &mut Vec<u8>) {
    if locals.size(i) == 2 && !locals.is_long_local(i) && !locals.is_float_local(i) {
        let d = locals.disp(i);
        let load = { let mut v = vec![0x8B, bp_modrm(0x46, d)]; push_bp_disp(&mut v, d); v };
        let store_self = { let mut v = vec![0x89, bp_modrm(0x46, d)]; push_bp_disp(&mut v, d); v };
        if ax_holds_word_operand(out, &load, &store_self, locals.last_branch_barrier.get()) {
            return;
        }
    }
    // Char local: when AL already holds the value (the last emit was its own
    // `mov [c],al` store — e.g. `unsigned char c = f(); return (int)c` keeps
    // the call result live), skip the reload and just widen. Fixture 1988.
    if locals.size(i) == 1 {
        let d = locals.disp(i);
        let store_self = { let mut v = vec![0x88, bp_modrm(0x46, d)]; push_bp_disp(&mut v, d); v };
        if out.len() >= store_self.len() && out[out.len() - store_self.len()..] == *store_self {
            if locals.is_unsigned_local(i) { out.extend_from_slice(&[0x2A, 0xE4]); } // sub ah,ah
            else { out.push(0x98); } // cbw
            return;
        }
    }
    emit_load_local(i, locals, out);
}
/// `mov ax,[param i]` with the same AX-reuse elision, for plain int params.
pub(crate) fn emit_load_param_reuse(i: usize, locals: &Locals<'_>, out: &mut Vec<u8>) {
    if !locals.is_long_param(i) && !locals.is_float_param(i) {
        let d = param_disp(i);
        let load = { let mut v = vec![0x8B, bp_modrm(0x46, d)]; push_bp_disp(&mut v, d); v };
        let store_self = { let mut v = vec![0x89, bp_modrm(0x46, d)]; push_bp_disp(&mut v, d); v };
        if ax_holds_word_operand(out, &load, &store_self, locals.last_branch_barrier.get()) {
            return;
        }
    }
    emit_load_param(i, out);
}
/// Load a near pointer LOCAL into BX for a deref, reusing the value still in AX
/// when it provably holds the pointer (the establishing `mov [p],ax` store — or a
/// `mov ax,[p]` load followed only by stores-from-AX — is the last AX-affecting
/// event, gated by the branch barrier). MSC emits `mov bx,ax` instead of reloading
/// `mov bx,[bp+disp]` (fixtures 1278/1775/2852). Far pointers are NOT routed here.
pub(crate) fn emit_ptr_local_to_bx(disp: i16, locals: &Locals<'_>, out: &mut Vec<u8>) {
    let store_self = { let mut v = vec![0x89, bp_modrm(0x46, disp)]; push_bp_disp(&mut v, disp); v };
    let load = { let mut v = vec![0x8B, bp_modrm(0x46, disp)]; push_bp_disp(&mut v, disp); v };
    let barrier = locals.last_branch_barrier.get();
    if ax_holds_word_operand(out, &load, &store_self, barrier)
        || ax_holds_slot_over_imm_stores(out, disp, barrier)
    {
        out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
    } else {
        out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); // mov bx,[bp+disp]
    }
}
/// Like the `store_self` branch of [`ax_holds_word_operand`] but also strips
/// intervening AX-PRESERVING memory writes (immediate stores `mov/add/sub
/// [bp+d],imm`) between the establishing `mov [bp+disp],ax` and now — they leave
/// AX untouched, so it still holds the pointer. Used for `p->f` reads after the
/// folded field stores of an aliased struct pointer (`p=&a; a.x=K; return p->x`,
/// fixtures 1003/136/182). BAILS if a stripped store targets the pointer's own
/// slot (which would change [p] without touching AX). disp8 bp slots only.
fn ax_holds_slot_over_imm_stores(out: &[u8], disp: i16, barrier: usize) -> bool {
    let Ok(dp) = i8::try_from(disp) else { return false };
    let dp = dp as u8;
    let mut n = out.len();
    loop {
        if n < barrier { return false; }
        // Establishing event: AX was stored to (or loaded from) the pointer slot.
        if n >= 3 && out[n-2] == 0x46 && out[n-1] == dp && (out[n-3] == 0x89 || out[n-3] == 0x8B) {
            return n - 3 >= barrier;
        }
        // Strip one AX-preserving trailing instruction; bail on a write to OUR slot.
        // store-from-AX/AL to another bp slot: 89/88 46 d8
        if n >= 3 && out[n-2] == 0x46 && (out[n-3] == 0x89 || out[n-3] == 0x88) {
            n -= 3; continue;
        }
        // store-from-AX to a global: a3 o16
        if n >= 3 && out[n-3] == 0xA3 { n -= 3; continue; }
        // mov [bp+d8],imm16 : c7 46 d8 i16   (collision if d8 == dp)
        if n >= 5 && out[n-5] == 0xC7 && out[n-4] == 0x46 {
            if out[n-3] == dp { return false; }
            n -= 5; continue;
        }
        // mov [bp+d8],imm8 : c6 46 d8 i8
        if n >= 4 && out[n-4] == 0xC6 && out[n-3] == 0x46 {
            if out[n-2] == dp { return false; }
            n -= 4; continue;
        }
        // alu [bp+d8],imm8 : 83 <m> d8 i8  (m: mod=01, rm=110 → (m&0xC7)==0x46)
        if n >= 4 && out[n-4] == 0x83 && (out[n-3] & 0xC7) == 0x46 {
            if out[n-2] == dp { return false; }
            n -= 4; continue;
        }
        // alu [bp+d8],imm16 : 81 <m> d8 i16
        if n >= 6 && out[n-6] == 0x81 && (out[n-5] & 0xC7) == 0x46 {
            if out[n-3] == dp { return false; }
            n -= 6; continue;
        }
        return false;
    }
}
/// True if AX provably holds the constant `k` at the current emit point: the
/// last AX-establishing op (gated by the branch barrier) set AX to exactly `k`
/// (`mov ax,k` / `sub ax,ax` / `xor ax,ax` for 0), with only AX-PRESERVING memory
/// writes since (stores-from-AX and `mov/add/sub [mem],imm`, bp- or global-form,
/// which don't touch AX). Lets `return k` reuse a live constant instead of
/// re-materializing it (fixtures 901/902). The scan finds the actual establishing
/// instruction and checks its value, so a different AX value rejects cleanly.
pub(crate) fn ax_holds_const(out: &[u8], k: i32, barrier: usize) -> bool {
    let target = (k as u32 & 0xFFFF) as u16;
    let mut n = out.len();
    loop {
        if n < barrier { return false; }
        // Establishing AX <- const.
        if k == 0 && n >= 2 && (out[n-2..n] == [0x2B, 0xC0] || out[n-2..n] == [0x33, 0xC0]) {
            return n - 2 >= barrier;
        }
        if n >= 3 && out[n-3] == 0xB8 {
            let v = out[n-2] as u16 | ((out[n-1] as u16) << 8);
            return v == target && n - 3 >= barrier;
        }
        // Strip one AX-preserving trailing memory write.
        // bp store-from-AX/AL: 89/88 46 d8 ; global store-from-AX: a3 o16
        if n >= 3 && out[n-2] == 0x46 && (out[n-3] == 0x89 || out[n-3] == 0x88) { n -= 3; continue; }
        if n >= 4 && out[n-3] == 0x86 && out[n-4] == 0x89 { n -= 4; continue; } // 89 86 d16
        if n >= 3 && out[n-3] == 0xA3 { n -= 3; continue; }
        // bp mem-imm: c7 46 d8 i16 (5) / c6 46 d8 i8 (4) / 83 m46 d8 i8 (4) / 81 m46 d8 i16 (6)
        if n >= 5 && out[n-5] == 0xC7 && out[n-4] == 0x46 { n -= 5; continue; }
        if n >= 4 && out[n-4] == 0xC6 && out[n-3] == 0x46 { n -= 4; continue; }
        if n >= 4 && out[n-4] == 0x83 && (out[n-3] & 0xC7) == 0x46 { n -= 4; continue; }
        if n >= 4 && out[n-4] == 0x80 && (out[n-3] & 0xC7) == 0x46 { n -= 4; continue; } // byte alu [bp+d8],imm8
        if n >= 6 && out[n-6] == 0x81 && (out[n-5] & 0xC7) == 0x46 { n -= 6; continue; }
        // global mem-imm: c7 06 o16 i16 (6) / c6 06 o16 i8 (5) / 80,83 m06 o16 i8 (5) / 81 m06 o16 i16 (6)
        if n >= 6 && out[n-6] == 0xC7 && (out[n-5] & 0xC7) == 0x06 { n -= 6; continue; }
        if n >= 5 && out[n-5] == 0xC6 && (out[n-4] & 0xC7) == 0x06 { n -= 5; continue; }
        if n >= 5 && out[n-5] == 0x83 && (out[n-4] & 0xC7) == 0x06 { n -= 5; continue; }
        if n >= 5 && out[n-5] == 0x80 && (out[n-4] & 0xC7) == 0x06 { n -= 5; continue; } // byte alu [g],imm8
        if n >= 6 && out[n-6] == 0x81 && (out[n-5] & 0xC7) == 0x06 { n -= 6; continue; }
        return false;
    }
}
/// Load a long operand's high word (the upper 16 bits) into AX — used for
/// `(int)(long >> 16)`.
fn emit_long_high_word_to_ax(e: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match e {
        Expr::Local(i) => {
            let d = locals.disp(*i) + 2;
            out.push(0x8B); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
        }
        Expr::Param(i) => {
            let d = long_param_disp(*i, locals) + 2;
            out.push(0x8B); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d);
        }
        Expr::Global(j) => {
            out.push(0xA1); // mov ax, [g+2]
            let off = out.len();
            out.extend_from_slice(&2u16.to_le_bytes());
            fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: *j } });
        }
        _ => unreachable!("long_operand gates these forms"),
    }
}
/// Element-shift count for a pointer-minus-pointer difference, or None
/// when this isn't a same-size pointer subtraction. `p - q` yields an
/// element count, so MSC computes the byte difference then arithmetic-
/// shifts to divide by the (power-of-two) element size: int*→1, char*→0.
/// Only pointer *parameters* are recognized here — local-pointer diffs
/// reuse a live AX from the preceding assignment and are handled
/// elsewhere. Fixtures 3103, 2647, 3377.
fn ptr_diff_shift(left: &Expr, right: &Expr, locals: &Locals<'_>) -> Option<u32> {
    let pointee = |e: &Expr| match e {
        Expr::Param(i) => locals.param_pointee_size(*i),
        Expr::Local(i) => locals.local_pointee_size(*i),
        _ => 0,
    };
    let lsz = pointee(left);
    let rsz = pointee(right);
    if lsz == 0 || lsz != rsz { return None; }
    match lsz {
        1 => Some(0),
        2 => Some(1),
        4 => Some(2),
        _ => None,
    }
}

/// `(pointer_param_pointee_size, scalar_param_disp)` for a `ptr - n`
/// pointer-arithmetic subtraction where the left operand is a pointer param
/// with a power-of-two pointee ≥ 2 and the right operand is a scalar (non-
/// pointer) param. `p - n` yields `p - n*elem`, so the index is scaled by
/// the element size before subtracting. Fixture 3648.
fn ptr_int_sub(left: &Expr, right: &Expr, locals: &Locals<'_>) -> Option<(usize, i16)> {
    let Expr::Param(pi) = left else { return None };
    let Expr::Param(ri) = right else { return None };
    let elem = locals.param_pointee_size(*pi);
    if elem < 2 || !elem.is_power_of_two() { return None; }
    if locals.param_pointee_size(*ri) != 0 { return None; } // right must be scalar
    Some((elem, param_disp(*ri)))
}

/// True when a ternary is the `(L OP R) ? L : R` select-operand idiom that
/// emit_expr_to_ax lowers to a single forward jcc (max/min). emit_return uses
/// this to keep such ternaries on the single-epilogue value path rather than
/// the two-epilogue structure.
pub(crate) fn is_ternary_select_operand(cond: &Expr, then_arm: &Expr, else_arm: &Expr, locals: &Locals<'_>) -> bool {
    if let Expr::BinOp { op, left: cl, right: cr } = cond
        && matches!(op, BinOp::Gt | BinOp::Ge | BinOp::Lt | BinOp::Le)
        && same_var(then_arm, cl)
        && same_var(else_arm, cr)
        && simple_word_bp(cl, locals).is_some()
        && simple_word_bp(cr, locals).is_some()
    {
        return true;
    }
    false
}
/// True when both exprs are the same simple variable read (same Param or same
/// Local index). Used by the ternary select-operand idiom (no PartialEq on Expr).
fn same_var(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Param(i), Expr::Param(j)) => i == j,
        (Expr::Local(i), Expr::Local(j)) => i == j,
        _ => false,
    }
}
/// A signed-byte (char) operand that `emit_expr_to_ax` loads as `mov al,..; cbw`.
/// Drives the CX-save shape for `char OP char` binops. Unsigned char scalars are
/// excluded (they use a different `cl`/`sub ch,ch` load shape).
/// `x ± y` where both x and y are simple word bp-rel operands → (op, ldisp, rdisp).
fn simple_addsub(e: &Expr, locals: &Locals<'_>) -> Option<(BinOp, i16, i16)> {
    let Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } = e else { return None };
    Some((*op, simple_word_bp(left, locals)?, simple_word_bp(right, locals)?))
}
/// Emit `x ± y` into CX (`cx=true`) or AX, both word memory operands.
fn emit_addsub_into(cx: bool, e: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>) {
    let (op, ld, rd) = simple_addsub(e, locals).expect("validated by caller");
    let (mov_reg, op_reg) = if cx { (0x4Eu8, 0x4Eu8) } else { (0x46, 0x46) };
    out.push(0x8B); out.push(bp_modrm(mov_reg, ld)); push_bp_disp(out, ld); // mov reg,[ld]
    let opc = if matches!(op, BinOp::Sub) { 0x2Bu8 } else { 0x03 };
    out.push(opc); out.push(bp_modrm(op_reg, rd)); push_bp_disp(out, rd);   // add/sub reg,[rd]
}
/// `v << k` / `v >> k` where v is a word bp-rel variable and k a constant.
/// Returns (disp, is_shl, k, is_unsigned) for the rotate-idiom codegen.
/// bp-relative disps to build a runtime shift amount in CL: a single word var →
/// `[d]` (mov cl,[d]); a sum of two word vars → `[da, db]` (mov cl,[da]; add
/// cl,[db]). None for other shapes. Word int Param/Local only. Fixture 3634.
fn shift_amount_cl(e: &Expr, locals: &Locals<'_>) -> Option<Vec<i16>> {
    fn word_disp(e: &Expr, locals: &Locals<'_>) -> Option<i16> {
        match e {
            Expr::Param(i) if !locals.is_char_param(*i) && !locals.is_long_param(*i) && !locals.is_float_param(*i) => Some(param_disp(*i)),
            Expr::Local(i) if locals.size(*i) == 2 && !locals.is_long_local(*i) && !locals.is_float_local(*i) => Some(locals.disp(*i)),
            _ => None,
        }
    }
    if let Some(d) = word_disp(e, locals) {
        return Some(vec![d]);
    }
    if let Expr::BinOp { op: BinOp::Add, left, right } = e
        && let (Some(a), Some(b)) = (word_disp(left, locals), word_disp(right, locals))
    {
        return Some(vec![a, b]);
    }
    None
}
fn shift_of_bp(e: &Expr, locals: &Locals<'_>) -> Option<(i16, bool, u8, bool)> {
    let Expr::BinOp { op: op @ (BinOp::Shl | BinOp::Shr), left, right } = e else { return None };
    let Expr::IntLit(k) = right.as_ref() else { return None };
    if *k <= 0 || *k > 15 { return None; }
    let (disp, unsigned) = match left.as_ref() {
        Expr::Param(i) if !locals.is_char_param(*i) && !locals.is_long_param(*i) && !locals.is_float_param(*i) =>
            (param_disp(*i), locals.is_unsigned_param(*i)),
        Expr::Local(i) if locals.size(*i) == 2 && !locals.is_long_local(*i) && !locals.is_float_local(*i) =>
            (locals.disp(*i), locals.is_unsigned_local(*i)),
        _ => return None,
    };
    Some((disp, matches!(op, BinOp::Shl), *k as u8, unsigned))
}
/// Emit `<var> shifted` into AX (`dx=false`) or DX (`dx=true`).
fn emit_shift_into(dx: bool, s: (i16, bool, u8, bool), out: &mut Vec<u8>) {
    let (disp, is_shl, k, unsigned) = s;
    // mov ax/dx, [bp+disp]
    out.push(0x8B); out.push(bp_modrm(if dx { 0x56 } else { 0x46 }, disp)); push_bp_disp(out, disp);
    // /4 shl, /5 shr (logical), /7 sar (arith). reg field: ax=000, dx=010.
    let digit = if is_shl { 4u8 } else if unsigned { 5 } else { 7 };
    let modrm = 0xC0 | (digit << 3) | if dx { 2 } else { 0 };
    if k <= 2 {
        for _ in 0..k { out.push(0xD1); out.push(modrm); } // k× shift reg,1
    } else {
        out.push(0xB1); out.push(k);      // mov cl,k
        out.push(0xD3); out.push(modrm);  // shift reg,cl
    }
}
/// The `[bp+disp]` of an UNSIGNED char param/local read (zero-extends via
/// `sub ch,ch` rather than `cbw`).
fn unsigned_byte_bp(e: &Expr, locals: &Locals<'_>) -> Option<i16> {
    match e {
        Expr::Param(i) if locals.is_char_param(*i) && locals.is_unsigned_param(*i) => Some(param_disp(*i)),
        Expr::Local(i) if locals.size(*i) == 1 && locals.is_unsigned_local(*i) => Some(locals.disp(*i)),
        _ => None,
    }
}
fn signed_byte_load(e: &Expr, locals: &Locals<'_>) -> bool {
    match e {
        Expr::IndexByte { .. } | Expr::LocalIndexByte { .. } | Expr::DerefByte { .. } => true,
        Expr::PtrArrayDeref { elem_size: 1, .. } => true,
        Expr::LocalField { size: 1, .. } | Expr::GlobalField { size: 1, .. }
        | Expr::DerefLocalField { size: 1, .. } | Expr::DerefParamField { size: 1, .. }
        | Expr::DerefGlobalField { size: 1, .. } => true,
        Expr::Param(i) => locals.is_char_param(*i) && !locals.is_unsigned_param(*i),
        Expr::Local(i) => locals.size(*i) == 1 && !locals.is_unsigned_local(*i),
        Expr::Global(g) => locals.is_char_global(*g) && !locals.is_unsigned_global(*g),
        _ => false,
    }
}
/// Element size (pointee bytes) if `e` evaluates to a pointer: a pointer param,
/// or pointer arithmetic `ptr ± int`. Used to scale a trailing `± K` in chained
/// pointer arithmetic like `p + n + 1`. None for non-pointer expressions.
fn pointer_elem_size(e: &Expr, locals: &Locals<'_>) -> Option<usize> {
    match e {
        Expr::Param(p) if locals.param_pointee_size(*p) > 0 => Some(locals.param_pointee_size(*p)),
        Expr::BinOp { op: BinOp::Add, left, right } => {
            pointer_elem_size(left, locals).or_else(|| pointer_elem_size(right, locals))
        }
        Expr::BinOp { op: BinOp::Sub, left, .. } => pointer_elem_size(left, locals),
        _ => None,
    }
}
/// The `[bp+disp]` of a simple word-sized Param/Local read (not char/long/float).
fn simple_word_bp(e: &Expr, locals: &Locals<'_>) -> Option<i16> {
    match e {
        Expr::Param(i)
            if !locals.is_char_param(*i) && !locals.is_long_param(*i) && !locals.is_float_param(*i) =>
        {
            Some(param_disp(*i))
        }
        Expr::Local(i)
            if locals.size(*i) == 2 && !locals.is_long_local(*i) && !locals.is_float_local(*i) =>
        {
            Some(locals.disp(*i))
        }
        _ => None,
    }
}
pub(crate) fn emit_binop(op: BinOp, left: &Expr, right: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // `make().a OP make().b` — two by-value struct-return field reads. Eval both
    // calls left-to-right (each spilling DX:AX to its temp), then combine: left's
    // field is reloaded from its temp (AX was clobbered by the 2nd call), right's
    // word field at offset 2 is still live in DX (`op ax,dx`), else reloaded.
    // Fixture 2682. Word (int) fields with a register-form opcode only.
    if let (Expr::CallStructField { name: ln, args: la, byte_off: lo, size: 2, temp_idx: lt },
            Expr::CallStructField { name: rn, args: ra, byte_off: ro, size: 2, temp_idx: rt }) = (left, right)
        && let Some(op_rm) = match op {
            BinOp::Add => Some(0x03u8), BinOp::Sub => Some(0x2B),
            BinOp::BitAnd => Some(0x23), BinOp::BitOr => Some(0x0B), BinOp::BitXor => Some(0x33),
            _ => None,
        }
    {
        let spill = |disp: i16, out: &mut Vec<u8>| {
            out.push(0x89); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);   // mov [t],ax
            let dh = disp + 2;
            out.push(0x89); out.push(bp_modrm(0x56, dh)); push_bp_disp(out, dh);       // mov [t+2],dx
        };
        emit_call(ln, la, locals, out, fixups);
        let ld = locals.struct_field_temp_disp(*lt);
        spill(ld, out);
        emit_call(rn, ra, locals, out, fixups);
        let rd = locals.struct_field_temp_disp(*rt);
        spill(rd, out);
        let lf = ld + *lo as i16;
        out.push(0x8B); out.push(bp_modrm(0x46, lf)); push_bp_disp(out, lf);           // mov ax,[left field]
        if *ro == 2 {
            out.extend_from_slice(&[op_rm, 0xC2]);                                     // op ax,dx
        } else {
            let rf = rd + *ro as i16;
            out.push(op_rm); out.push(bp_modrm(0x46, rf)); push_bp_disp(out, rf);      // op ax,[right field]
        }
        return;
    }
    // `&local + K` / `&global + K` as a value (`&s.field`, `&a[K]`) → fold the
    // constant into the lea displacement / OFFSET addend rather than computing
    // the base address and adding K at runtime. Fixtures 485, 3262.
    if matches!(op, BinOp::Add)
        && let Expr::IntLit(k) = right
    {
        if let Expr::AddrOfLocal(l) = left {
            let disp = locals.disp(*l) + *k as i16;
            out.push(0x8D); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // lea ax,[bp+disp+k]
            return;
        }
        if let Expr::AddrOfGlobal(g) = left {
            let body_offset = out.len();
            out.push(0xB8);
            out.extend_from_slice(&((*k as u32 & 0xFFFF) as u16).to_le_bytes()); // mov ax, OFFSET g (+K addend)
            fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: *g } });
            return;
        }
    }
    // Unary negation `-x` (parsed as `0 - x`): load x into AX and `neg ax`,
    // not `mov ax,0; sub ax,x` (fixture 3346 `return x<0?-x:x` → `mov ax,[bp+4];
    // neg ax`). Skip literal RHS (folds elsewhere) and long RHS (own path).
    if matches!(op, BinOp::Sub)
        && matches!(left, Expr::IntLit(0))
        && !matches!(right, Expr::IntLit(_))
        && !long_operand(right, locals)
    {
        emit_expr_to_ax(right, locals, out, fixups);
        out.extend_from_slice(&[0xF7, 0xD8]); // neg ax
        return;
    }
    // `a + b` where the two slots were BOTH just stored from the live AX (a
    // chained `a = b = V` leaves AX = a = b) → `add ax,ax`, no reload. Fixtures
    // 2951, 1817, 3513.
    if matches!(op, BinOp::Add)
        && let Some(la) = simple_word_bp(left, locals)
        && let Some(lb) = simple_word_bp(right, locals)
        && la != lb
    {
        let store = |d: i16| { let mut v = vec![0x89, bp_modrm(0x46, d)]; push_bp_disp(&mut v, d); v };
        let (sa, sb) = (store(la), store(lb));
        let ends_with = |pat: &[u8]| out.len() >= pat.len() && out[out.len() - pat.len()..] == *pat;
        let mut ab = sb.clone(); ab.extend_from_slice(&sa); // store b then store a
        let mut ba = sa.clone(); ba.extend_from_slice(&sb);
        if ends_with(&ab) || ends_with(&ba) {
            out.extend_from_slice(&[0x03, 0xC0]); // add ax, ax
            return;
        }
    }
    // Same chained-store `add ax,ax`, but the second summand const-folded to
    // `IntLit(K)` (its value was recorded by the chain const-prop). The chain
    // `mov ax,K; mov [b],ax; mov [a],ax` leaves AX = a = b = K, so `a + K`
    // reuses the live AX. Fixture 2951 (`a = b = 7; return a + b`).
    if matches!(op, BinOp::Add)
        && let Some(la) = simple_word_bp(left, locals)
        && let Expr::IntLit(k) = right
    {
        let k16 = (*k as u32 & 0xFFFF) as u16;
        let storela = { let mut v = vec![0x89, bp_modrm(0x46, la)]; push_bp_disp(&mut v, la); v };
        if out.len() >= storela.len() && out[out.len() - storela.len()..] == *storela {
            // Strip the trailing `mov [bp+d],ax` chain stores, then require the
            // immediate that fed them (`mov ax,K`) — and ≥2 vars in the chain.
            let mut p = out.len();
            let mut stores = 0;
            loop {
                if p >= 4 && out[p - 4] == 0x89 && out[p - 3] == 0x86 { p -= 4; stores += 1; }
                else if p >= 3 && out[p - 3] == 0x89 && out[p - 2] == 0x46 { p -= 3; stores += 1; }
                else { break; }
            }
            let movax = { let mut v = vec![0xB8]; v.extend_from_slice(&k16.to_le_bytes()); v };
            if stores >= 2 && p >= movax.len() && out[p - movax.len()..p] == *movax {
                out.extend_from_slice(&[0x03, 0xC0]); // add ax, ax
                return;
            }
        }
    }
    // `x + x` (same simple variable) → load x, `shl ax,1` (fixture 1991
    // `return x+x` → `mov al,[x]; sub ah,ah; shl ax,1`), not load + add-mem.
    if matches!(op, BinOp::Add) && same_var(left, right) && !long_operand(left, locals) {
        emit_expr_to_ax(left, locals, out, fixups);
        out.extend_from_slice(&[0xD1, 0xE0]); // shl ax, 1
        return;
    }
    // `x * x` (same simple variable) → load x, `imul ax` (one-operand signed
    // multiply, DX:AX = AX*AX), not load + `imul word [x]`. Fixture 2314
    // (`return n * n` → `mov ax,[bp+4]; imul ax`).
    if matches!(op, BinOp::Mul) && same_var(left, right) && !long_operand(left, locals) {
        emit_expr_to_ax(left, locals, out, fixups);
        out.extend_from_slice(&[0xF7, 0xE8]); // imul ax
        return;
    }
    // Pointer difference: emit the raw byte-difference subtraction, then
    // convert bytes → elements with `sar ax,1` per shift step.
    if matches!(op, BinOp::Sub)
        && let Some(shift) = ptr_diff_shift(left, right, locals)
    {
        emit_binop_inner(BinOp::Sub, left, right, locals, out, fixups);
        for _ in 0..shift {
            out.extend_from_slice(&[0xD1, 0xF8]); // sar ax,1
        }
        return;
    }
    // Pointer param ± integer CONSTANT: scale the constant by the element size
    // and emit `add/sub ax, imm16` (not the raw ±1 inc/dec peephole, which would
    // adjust the pointer by 1 byte). `p - 1` (int*) → `mov ax,[p]; sub ax,2`.
    // Fixtures 3256, 3382.
    if matches!(op, BinOp::Add | BinOp::Sub)
        && let Expr::Param(pi) = left
        && let Expr::IntLit(k) = right
        && locals.param_pointee_size(*pi) >= 2
        && locals.param_pointee_size(*pi).is_power_of_two()
    {
        let elem = locals.param_pointee_size(*pi) as i32;
        let imm = ((*k * elem) as u32 & 0xFFFF) as u16;
        emit_expr_to_ax(left, locals, out, fixups); // mov ax, [p]
        out.push(if matches!(op, BinOp::Sub) { 0x2D } else { 0x05 }); // sub/add ax, imm16
        out.extend_from_slice(&imm.to_le_bytes());
        return;
    }
    // Chained pointer arithmetic `(<ptr-expr>) ± K` where the left is itself a
    // pointer-valued expression (e.g. `p + n + 1`): emit the inner pointer expr
    // into AX, then `add/sub ax, K*elem` (the trailing constant is in elements).
    // Fixture 3632 (`p + n + 1` → `mov ax,[n]; shl ax,1; add ax,[p]; add ax,2`).
    if matches!(op, BinOp::Add | BinOp::Sub)
        && let Expr::IntLit(k) = right
        && matches!(left, Expr::BinOp { .. })
        && let Some(elem) = pointer_elem_size(left, locals)
        && elem.is_power_of_two()
    {
        emit_expr_to_ax(left, locals, out, fixups);
        let imm = ((*k * elem as i32) as u32 & 0xFFFF) as u16;
        out.push(if matches!(op, BinOp::Sub) { 0x2D } else { 0x05 }); // sub/add ax, imm16
        out.extend_from_slice(&imm.to_le_bytes());
        return;
    }
    // Pointer param + integer-variable param: load the INDEX, scale it by the
    // element size, then add the pointer. `p + n` (int*) →
    // `mov ax,[n]; shl ax,1; add ax,[p]`. Fixture 3380.
    if matches!(op, BinOp::Add) {
        let pair = match (left, right) {
            (Expr::Param(a), Expr::Param(b))
                if locals.param_pointee_size(*a) >= 2
                    && locals.param_pointee_size(*a).is_power_of_two()
                    && locals.param_pointee_size(*b) == 0 => Some((*a, *b)),
            (Expr::Param(a), Expr::Param(b))
                if locals.param_pointee_size(*b) >= 2
                    && locals.param_pointee_size(*b).is_power_of_two()
                    && locals.param_pointee_size(*a) == 0 => Some((*b, *a)),
            _ => None,
        };
        if let Some((pp, ip)) = pair {
            let elem = locals.param_pointee_size(pp);
            let idisp = param_disp(ip);
            let pdisp = param_disp(pp);
            out.push(0x8B); out.push(bp_modrm(0x46, idisp)); push_bp_disp(out, idisp); // mov ax,[n]
            for _ in 0..elem.trailing_zeros() {
                out.extend_from_slice(&[0xD1, 0xE0]); // shl ax, 1
            }
            out.push(0x03); out.push(bp_modrm(0x46, pdisp)); push_bp_disp(out, pdisp); // add ax,[p]
            return;
        }
    }
    // Pointer minus integer (pointer arithmetic): `p - n` → `p - n*elem`.
    // Load the pointer into AX, the index into CX, scale CX by the element
    // size (`shl cx,1` per power-of-two step), then `sub ax,cx`. Fixture 3648.
    if matches!(op, BinOp::Sub)
        && let Some((elem, rdisp)) = ptr_int_sub(left, right, locals)
    {
        emit_expr_to_ax(left, locals, out, fixups); // mov ax, [p]
        out.push(0x8B);
        out.push(bp_modrm(0x4E, rdisp));
        push_bp_disp(out, rdisp); // mov cx, [n]
        for _ in 0..elem.trailing_zeros() {
            out.extend_from_slice(&[0xD1, 0xE1]); // shl cx, 1
        }
        out.extend_from_slice(&[0x2B, 0xC1]); // sub ax, cx
        return;
    }
    // Chain of 3+ char-pointer-array derefs joined by `+` (e.g.
    // `names[0][0] + names[1][0] + names[2][0]`): MSC evaluates the operands
    // right-to-left, parking each result in CX, DX, BX, loads the leftmost
    // into AX, then adds them back in source order. The save (`mov reg,ax`) is
    // scheduled AFTER the next operand's pointer load (which doesn't touch AX),
    // so it lands between that load and its element read. Fixtures 2231, 2345.
    if matches!(op, BinOp::Add)
        && let Some(leaves) = ptr_deref_add_chain(left, right, locals)
    {
        let n = leaves.len();
        const MOV_FROM_AX: [u8; 3] = [0xC8, 0xD0, 0xD8]; // mov cx/dx/bx, ax
        const ADD_AX_REG: [u8; 3] = [0xC1, 0xC2, 0xC3];  // add ax, cx/dx/bx
        fn parts(o: &Expr) -> (usize, &Expr, &Expr, u8) {
            let Expr::PtrArrayDeref { array, index, inner, elem_size } = o else { unreachable!() };
            (*array, index.as_ref(), inner.as_ref(), *elem_size)
        }
        // Rightmost operand: full load + read into AX.
        let (a, idx, inr, es) = parts(leaves[n - 1]);
        emit_ptr_array_load_bx(a, idx, locals, out, fixups);
        emit_ptr_array_read_elem(inr, es, locals, out);
        // Remaining operands right-to-left: load pointer, save previous AX, read.
        for k in 1..n {
            let (a, idx, inr, es) = parts(leaves[n - 1 - k]);
            emit_ptr_array_load_bx(a, idx, locals, out, fixups);
            out.push(0x8B); out.push(MOV_FROM_AX[k - 1]); // mov cx/dx/bx, ax
            emit_ptr_array_read_elem(inr, es, locals, out);
        }
        // AX = leaves[0]; add back in source order (leaves[j] sits in reg n-1-j).
        for j in 1..n {
            out.push(0x03); out.push(ADD_AX_REG[n - 1 - j]);
        }
        return;
    }
    // Chain of 3-4 signed-byte (char) operands joined by `+`, each a single-
    // instruction load (char param/local/global, IndexByte array element — NOT
    // a two-phase PtrArrayDeref, handled above): evaluate right-to-left, saving
    // each into CX, DX, BX immediately, load the leftmost into AX, then add back
    // in source order. Fixtures 3037, 2095, 2096.
    if matches!(op, BinOp::Add)
        && let Some(leaves) = byte_simple_add_chain(left, right, locals)
    {
        let n = leaves.len();
        const MOV_FROM_AX: [u8; 3] = [0xC8, 0xD0, 0xD8]; // mov cx/dx/bx, ax
        const ADD_AX_REG: [u8; 3] = [0xC1, 0xC2, 0xC3];  // add ax, cx/dx/bx
        for (slot, j) in (1..n).rev().enumerate() {
            emit_expr_to_ax(leaves[j], locals, out, fixups);
            out.push(0x8B); out.push(MOV_FROM_AX[slot]); // mov cx/dx/bx, ax
        }
        emit_expr_to_ax(leaves[0], locals, out, fixups);
        for j in 1..n {
            out.push(0x03); out.push(ADD_AX_REG[n - 1 - j]);
        }
        return;
    }
    // `s[i] + s[j]` — two char-byte subscripts of the SAME char-pointer global:
    // load the pointer into BX once, read `[bx+i]` (LHS) into AX and park in CX,
    // read `[bx+j]` (RHS), then `add ax,cx`. Fixture 2291 (`s[0]+s[1]`).
    if matches!(op, BinOp::Add)
        && let Expr::PtrIndexByte { ptr: lg, index: li } = left
        && let Expr::PtrIndexByte { ptr: rg, index: ri } = right
        && lg == rg
        && let (Some(lk), Some(rk)) = (li.fold(locals.inits), ri.fold(locals.inits))
    {
        let body_offset = out.len();
        out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]); // mov bx,[g]
        fixups.push(Fixup { body_offset: body_offset + 1, kind: FixupKind::GlobalAddr { global_idx: *lg } });
        let mut byte_read = |off: i32, out: &mut Vec<u8>| {
            if off == 0 { out.extend_from_slice(&[0x8A, 0x07]); }
            else { out.push(0x8A); out.push(0x47); out.push(off as u8); }
            out.push(0x98); // cbw
        };
        byte_read(lk, out);                  // mov al,[bx+lk]; cbw  (LHS)
        out.extend_from_slice(&[0x8B, 0xC8]); // mov cx, ax
        byte_read(rk, out);                  // mov al,[bx+rk]; cbw  (RHS)
        out.extend_from_slice(&[0x03, 0xC1]); // add ax, cx
        return;
    }
    // `*p + *(p+K)` — both operands are word derefs of the SAME pointer param at
    // constant offsets. Load `p` into BX once, then `mov ax,[bx+roff]; add ax,
    // [bx+loff]` (RHS into AX, LHS as a memory operand — `+` is commutative).
    // Fixture 3625.
    if matches!(op, BinOp::Add)
        && let Some((lp, loff)) = same_param_word_deref(left)
        && let Some((rp, roff)) = same_param_word_deref(right)
    {
        let emit_bx = |out: &mut Vec<u8>, opc: u8, off: i32| {
            if off == 0 { out.push(opc); out.push(0x07); }
            else { out.push(opc); out.push(0x47); out.push(off as i8 as u8); }
        };
        if lp == rp {
            // Same pointer: load once, RHS offset into AX, LHS as mem operand.
            let pdisp = param_disp(lp);
            out.push(0x8B); out.push(bp_modrm(0x5E, pdisp)); push_bp_disp(out, pdisp); // mov bx,[bp+p]
            emit_bx(out, 0x8B, roff); // mov ax,[bx+roff]
            emit_bx(out, 0x03, loff); // add ax,[bx+loff]
        } else {
            // Different pointers: load LHS ptr, `*lhs` into AX; reload BX with the
            // RHS ptr, `add ax,*rhs`. Fixture 3607 (`*p + *q`).
            let ld = param_disp(lp);
            out.push(0x8B); out.push(bp_modrm(0x5E, ld)); push_bp_disp(out, ld); // mov bx,[p]
            emit_bx(out, 0x8B, loff); // mov ax,[bx+loff]
            let rd = param_disp(rp);
            out.push(0x8B); out.push(bp_modrm(0x5E, rd)); push_bp_disp(out, rd); // mov bx,[q]
            emit_bx(out, 0x03, roff); // add ax,[bx+roff]
        }
        return;
    }
    emit_binop_inner(op, left, right, locals, out, fixups);
}
/// `*p++` setup: load the OLD pointer into BX, then advance `[p]` by `step`.
/// Shared by the value (`return *p++`) and condition (`while(*s++)`) paths.
pub(crate) fn emit_postinc_ptr_to_bx(ptr: &Expr, step: i32, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match ptr {
        Expr::Param(i) => {
            let d = param_disp(*i);
            out.push(0x8B); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d); // mov bx,[bp+d]
            crate::codegen::assign::emit_postmutate_local(step, 2, d, out);
        }
        Expr::Local(i) => {
            let d = locals.disp(*i);
            out.push(0x8B); out.push(bp_modrm(0x5E, d)); push_bp_disp(out, d);
            crate::codegen::assign::emit_postmutate_local(step, 2, d, out);
        }
        Expr::Global(g) => {
            out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]); // mov bx,[g]
            let bo = out.len() - 2;
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
            // advance [g]
            if step == 1 || step == -1 {
                out.push(0xFF); out.push(if step == 1 { 0x06 } else { 0x0E });
            } else {
                out.push(0x83); out.push(0x06);
            }
            let bo = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
            if !(step == 1 || step == -1) { out.push(step as i8 as u8); }
        }
        other => panic!("unsupported *p++ pointer: {other:?}"),
    }
}
/// Load a bit-field's containing 16-bit unit (`base + byte_off`) into AX.
pub(crate) fn bf_load_word(base: BitBase, byte_off: u16, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match base {
        BitBase::Global(g) => {
            let bo = out.len();
            out.push(0xA1); // mov ax, moffs16
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } });
        }
        BitBase::Local(l) => {
            let d = locals.disp(l) + byte_off as i16;
            out.push(0x8B); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); // mov ax,[bp+d]
        }
        BitBase::DerefParam(pp) => {
            // mov bx,[bp+pdisp]; mov ax,[bx+byte_off]
            out.extend_from_slice(&[0x8B, 0x5E, param_disp(pp) as u8]);
            if byte_off == 0 { out.extend_from_slice(&[0x8B, 0x07]); }
            else { out.push(0x8B); out.push(0x47); out.push(byte_off as u8); }
        }
    }
}
/// Load the bit-field unit word at `base + byte_off` into CX. Returns false for
/// bases we don't load into CX (only Local/Global are needed by the read-pair
/// scheduling below). Mirrors `bf_load_word` with the CX reg field.
pub(crate) fn bf_load_word_cx(base: BitBase, byte_off: u16, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) -> bool {
    match base {
        BitBase::Global(g) => {
            out.push(0x8B); out.push(0x0E); // mov cx, [moffs16] (modrm 0E = disp16, reg=cx)
            let bo = out.len() - 1; // points at the 0E byte; address goes at bo+1/bo+2
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } });
            true
        }
        BitBase::Local(l) => {
            let d = locals.disp(l) + byte_off as i16;
            out.push(0x8B); out.push(bp_modrm(0x4E, d)); push_bp_disp(out, d); // mov cx,[bp+d]
            true
        }
        BitBase::DerefParam(_) => false,
    }
}
/// Flatten a left-assoc `+` chain whose leaves are all `Expr::BitField` into a
/// source-ordered list of `(base, byte_off, bit_off, bit_width)`. Returns false
/// (leaving `out` partially filled) the moment a non-BitField, non-Add leaf is hit.
fn collect_bitfield_add_chain(e: &Expr, out: &mut Vec<(BitBase, u16, u8, u8)>) -> bool {
    match e {
        Expr::BitField { base, byte_off, bit_off, bit_width } => {
            out.push((*base, *byte_off, *bit_off, *bit_width));
            true
        }
        Expr::BinOp { op: BinOp::Add, left, right } => {
            collect_bitfield_add_chain(left, out) && collect_bitfield_add_chain(right, out)
        }
        _ => false,
    }
}
/// True when every collected field shares the same Local/Global bit-field base
/// (DerefParam bases are rejected — the multi-field readers only target bp/moffs).
fn same_local_global_base(fields: &[(BitBase, u16, u8, u8)]) -> bool {
    let key = |b: &BitBase| match b {
        BitBase::Local(i) => Some((0u8, *i)),
        BitBase::Global(i) => Some((1u8, *i)),
        BitBase::DerefParam(_) => None,
    };
    match key(&fields[0].0) {
        None => false,
        first => fields.iter().all(|f| key(&f.0) == first),
    }
}
/// Read a WORD bit-field (`base+byte_off`, `bit_off`, `bit_width`) into DX:
/// `mov dx,[unit]` then shift down by bit_off and `and dx,mask`. Used as the
/// middle-operand register in a multi-field sum (CL stays free for the shift).
/// Returns false for bases we don't handle (DerefParam).
pub(crate) fn bf_read_into_dx(base: BitBase, byte_off: u16, bit_off: u8, bit_width: u8, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) -> bool {
    match base {
        BitBase::Global(g) => {
            out.push(0x8B); out.push(0x16); // mov dx,[moffs16]
            let bo = out.len() - 1;
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } });
        }
        BitBase::Local(l) => {
            let d = locals.disp(l) + byte_off as i16;
            out.push(0x8B); out.push(bp_modrm(0x56, d)); push_bp_disp(out, d); // mov dx,[bp+d]
        }
        BitBase::DerefParam(_) => return false,
    }
    if bit_off == 1 {
        out.extend_from_slice(&[0xD1, 0xEA]); // shr dx,1
    } else if bit_off > 1 {
        out.push(0xB1); out.push(bit_off); // mov cl,bit_off
        out.extend_from_slice(&[0xD3, 0xEA]); // shr dx,cl
    }
    if bit_width < 16 {
        let mask = (1u32 << bit_width) - 1;
        out.push(0x81); out.push(0xE2); // and dx,imm16
        out.extend_from_slice(&(mask as u16).to_le_bytes());
    }
    true
}
/// Load the byte at `base + off` into AL.
pub(crate) fn bf_load_byte_al(base: BitBase, off: u16, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match base {
        BitBase::Global(g) => {
            let bo = out.len();
            out.push(0xA0); // mov al, moffs8
            out.extend_from_slice(&off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } });
        }
        BitBase::Local(l) => {
            let d = locals.disp(l) + off as i16;
            out.push(0x8A); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); // mov al,[bp+d]
        }
        BitBase::DerefParam(pp) => {
            out.extend_from_slice(&[0x8B, 0x5E, param_disp(pp) as u8]); // mov bx,[bp+pdisp]
            if off == 0 { out.extend_from_slice(&[0x8A, 0x07]); }
            else { out.push(0x8A); out.push(0x47); out.push(off as u8); }
        }
    }
}
/// Store AX back to the bit-field unit (`base + byte_off`).
pub(crate) fn bf_store_word(base: BitBase, byte_off: u16, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match base {
        BitBase::Global(g) => {
            let bo = out.len();
            out.push(0xA3); // mov moffs16, ax
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } });
        }
        BitBase::Local(l) => {
            let d = locals.disp(l) + byte_off as i16;
            out.push(0x89); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); // mov [bp+d],ax
        }
        BitBase::DerefParam(pp) => {
            out.extend_from_slice(&[0x8B, 0x5E, param_disp(pp) as u8]); // mov bx,[bp+pdisp]
            if byte_off == 0 { out.extend_from_slice(&[0x89, 0x07]); }
            else { out.push(0x89); out.push(0x47); out.push(byte_off as u8); }
        }
    }
}
/// True when `out` already ends with the store of this exact bit-field unit, so
/// AX still holds the unit's value and the load can be skipped (MSC keeps AX
/// live across consecutive bit-field writes to the same struct). Fixture 1691.
pub(crate) fn bf_ax_live(base: BitBase, byte_off: u16, locals: &Locals<'_>, out: &[u8], fixups: &[Fixup]) -> bool {
    match base {
        BitBase::Local(l) => {
            let d = locals.disp(l) + byte_off as i16;
            let mut pat = vec![0x89, bp_modrm(0x46, d)];
            push_bp_disp(&mut pat, d);
            out.len() >= pat.len() && out[out.len() - pat.len()..] == *pat
        }
        BitBase::Global(g) => {
            let mut pat = vec![0xA3];
            pat.extend_from_slice(&byte_off.to_le_bytes());
            out.len() >= pat.len() && out[out.len() - pat.len()..] == *pat
                && matches!(fixups.last(), Some(Fixup { kind: FixupKind::GlobalAddr { global_idx }, .. }) if *global_idx == g)
        }
        // BX-based access — no AX-reuse modeled.
        BitBase::DerefParam(_) => false,
    }
}
/// Split an array index `i ± K` into its variable part and the constant element
/// offset `K` (folded into the addressing displacement). `i`, `i+K`, `K+i`, and
/// `i-K` are recognized; anything else returns `(e, 0)`.
fn split_index_offset(e: &Expr) -> (&Expr, i32) {
    match e {
        Expr::BinOp { op: BinOp::Add, left, right } => {
            if let Expr::IntLit(k) = right.as_ref() { (left, *k) }
            else if let Expr::IntLit(k) = left.as_ref() { (right, *k) }
            else { (e, 0) }
        }
        Expr::BinOp { op: BinOp::Sub, left, right } => {
            if let Expr::IntLit(k) = right.as_ref() { (left, -*k) } else { (e, 0) }
        }
        _ => (e, 0),
    }
}
/// `*p` / `*(p + K)` as a WORD deref of a pointer PARAM → `(param, byte_off)`.
fn same_param_word_deref(e: &Expr) -> Option<(usize, i32)> {
    let Expr::DerefWord { ptr } = e else { return None };
    match ptr.as_ref() {
        Expr::Param(p) => Some((*p, 0)),
        Expr::BinOp { op: BinOp::Add, left, right } => {
            if let (Expr::Param(p), Expr::IntLit(k)) = (left.as_ref(), right.as_ref()) {
                Some((*p, k * 2))
            } else {
                None
            }
        }
        _ => None,
    }
}
/// Flatten a left-associative `a + b + c [+ d]` chain of single-instruction
/// signed-byte loads (char param/local/global, IndexByte) — excluding the
/// two-phase PtrArrayDeref (its own interleaved schedule). 3 or 4 leaves.
fn byte_simple_add_chain<'a>(left: &'a Expr, right: &'a Expr, locals: &Locals<'_>) -> Option<Vec<&'a Expr>> {
    fn ok(e: &Expr, locals: &Locals<'_>) -> bool {
        signed_byte_load(e, locals) && !matches!(e, Expr::PtrArrayDeref { .. })
    }
    fn walk<'a>(e: &'a Expr, locals: &Locals<'_>, out: &mut Vec<&'a Expr>) -> bool {
        match e {
            Expr::BinOp { op: BinOp::Add, left, right } => {
                if !ok(right, locals) { return false; }
                if !walk(left, locals, out) { return false; }
                out.push(right);
                true
            }
            _ => {
                if ok(e, locals) { out.push(e); true } else { false }
            }
        }
    }
    if !ok(right, locals) { return None; }
    let mut leaves = Vec::new();
    if !walk(left, locals, &mut leaves) { return None; }
    leaves.push(right);
    if (3..=4).contains(&leaves.len()) { Some(leaves) } else { None }
}
/// Flatten a left-associative `a + b + c [+ d]` chain where every leaf is a
/// char (`elem_size==1`) pointer-array deref, returning the leaves in source
/// order. `None` unless the chain has 3 or 4 such leaves.
fn ptr_deref_add_chain<'a>(left: &'a Expr, right: &'a Expr, _locals: &Locals<'_>) -> Option<Vec<&'a Expr>> {
    fn is_char_deref(e: &Expr) -> bool { matches!(e, Expr::PtrArrayDeref { elem_size: 1, .. }) }
    fn walk<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) -> bool {
        match e {
            Expr::BinOp { op: BinOp::Add, left, right } => {
                if !is_char_deref(right) { return false; }
                if !walk(left, out) { return false; }
                out.push(right);
                true
            }
            _ => {
                if is_char_deref(e) { out.push(e); true } else { false }
            }
        }
    }
    if !is_char_deref(right) { return None; }
    let mut leaves = Vec::new();
    if !walk(left, &mut leaves) { return None; }
    leaves.push(right);
    if (3..=4).contains(&leaves.len()) { Some(leaves) } else { None }
}

/// A call expression (direct or indirect).
fn is_any_call(e: &Expr) -> bool { matches!(e, Expr::Call { .. } | Expr::CallPtr { .. }) }
/// A call eligible as a NON-rightmost operand of the call-chain scheduler: its
/// argument is built in CX (so ≤1 CX-buildable arg) and, if indirect, its target
/// must be SI-safe (call instruction uses no scratch register).
fn eligible_left_call(e: &Expr, inits: &[Option<i32>]) -> bool {
    let (largs, target) = match e {
        Expr::Call { args, .. } => (args.as_slice(), None),
        Expr::CallPtr { target, args } => (args.as_slice(), Some(target.as_ref())),
        _ => return false,
    };
    if !largs.iter().all(arg_buildable_in_cx) {
        return false;
    }
    target.map(|t| callptr_target_si_safe(t, inits)).unwrap_or(true)
}
/// Collect a left-assoc add/sub chain whose leaves are all calls into
/// `(calls in source order, ops)`; false if `e` isn't such a chain.
fn collect_call_chain<'a>(e: &'a Expr, calls: &mut Vec<&'a Expr>, ops: &mut Vec<BinOp>) -> bool {
    match e {
        Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right } => {
            if !collect_call_chain(left, calls, ops) { return false; }
            ops.push(*op);
            if is_any_call(right) { calls.push(right); true } else { false }
        }
        _ if is_any_call(e) => { calls.push(e); true }
        _ => false,
    }
}
/// If `BinOp{op,left,right}` is a 2- or 3-call add/sub chain handled by the
/// SI/CX(/DI) scheduler, return `(calls, ops)`. All but the rightmost must be
/// `eligible_left_call`. Fixtures 1277/1536 (2), 2305/2343 (3).
pub(crate) fn call_chain<'a>(op: BinOp, left: &'a Expr, right: &'a Expr, inits: &[Option<i32>])
    -> Option<(Vec<&'a Expr>, Vec<BinOp>)>
{
    if !matches!(op, BinOp::Add | BinOp::Sub) || !is_any_call(right) { return None; }
    let mut calls: Vec<&Expr> = Vec::new();
    let mut ops: Vec<BinOp> = Vec::new();
    if !collect_call_chain(left, &mut calls, &mut ops) { return None; }
    ops.push(op);
    calls.push(right);
    if !(2..=3).contains(&calls.len()) { return None; }
    if !calls[..calls.len() - 1].iter().all(|c| eligible_left_call(c, inits)) { return None; }
    Some((calls, ops))
}
/// True for a two-call chain (SI only → an Si frame upgrade).
pub(crate) fn is_two_call_binop(left: &Expr, right: &Expr, inits: &[Option<i32>]) -> bool {
    matches!(call_chain(BinOp::Add, left, right, inits), Some((c, _)) if c.len() == 2)
        || matches!(call_chain(BinOp::Sub, left, right, inits), Some((c, _)) if c.len() == 2)
}
/// True for a 3-call chain (DI+SI parking → a DiSi frame upgrade).
pub(crate) fn is_three_call_binop(left: &Expr, right: &Expr, inits: &[Option<i32>]) -> bool {
    matches!(call_chain(BinOp::Add, left, right, inits), Some((c, _)) if c.len() == 3)
        || matches!(call_chain(BinOp::Sub, left, right, inits), Some((c, _)) if c.len() == 3)
}
/// A fn-ptr call target whose `call WORD PTR <mem>` emission uses no scratch
/// register (so it's safe inside the SI/CX scheduler). Runtime-indexed array
/// elements load SI/BX and are excluded.
fn callptr_target_si_safe(target: &Expr, inits: &[Option<i32>]) -> bool {
    match target {
        Expr::Global(_) | Expr::Param(_) | Expr::Local(_)
        | Expr::LocalField { .. } | Expr::GlobalField { .. } => true,
        Expr::LocalIndex { index, .. } | Expr::Index { index, .. } => index.fold(inits).is_some(),
        _ => false,
    }
}
/// Can this call argument be built directly into CX (without touching AX)?
/// Used by the two-call binop scheduler. Covers the simple shapes the corpus
/// passes as a single call argument: a constant, a string-literal address, a
/// parameter, or `<param> ± K`.
fn arg_buildable_in_cx(e: &Expr) -> bool {
    match e {
        Expr::IntLit(_) | Expr::StrLit(_) | Expr::Param(_) | Expr::FuncAddr(_) => true,
        Expr::BinOp { op: BinOp::Add | BinOp::Sub, left, right } =>
            matches!(left.as_ref(), Expr::Param(_)) && matches!(right.as_ref(), Expr::IntLit(_)),
        _ => false,
    }
}
/// Emit the call argument `e` into CX (mirrors the AX builder for the limited
/// shapes `arg_buildable_in_cx` accepts). Used by the two-call binop scheduler.
fn emit_arg_to_cx(e: &Expr, _locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match e {
        Expr::IntLit(k) => { out.push(0xB9); out.extend_from_slice(&(*k as u16).to_le_bytes()); }
        Expr::StrLit(idx) => {
            let bo = out.len();
            out.extend_from_slice(&[0xB9, 0x00, 0x00]); // mov cx, OFFSET str
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::StrLoad { string_idx: *idx } });
        }
        Expr::FuncAddr(name) => {
            out.push(0xB9); // mov cx, OFFSET _func
            let bo = out.len() - 1;
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::FuncAddr { target: symbol_name(name) } });
        }
        Expr::Param(i) => { let d = param_disp(*i); out.push(0x8B); out.push(bp_modrm(0x4E, d)); push_bp_disp(out, d); }
        Expr::BinOp { op, left, right } => {
            emit_arg_to_cx(left, _locals, out, fixups);
            let Expr::IntLit(k) = right.as_ref() else { unreachable!() };
            match (op, *k) {
                (BinOp::Add, 1) => out.push(0x41),  // inc cx
                (BinOp::Sub, 1) => out.push(0x49),  // dec cx
                (BinOp::Add, k) if (-128..=127).contains(&k) => out.extend_from_slice(&[0x83, 0xC1, k as u8]),
                (BinOp::Sub, k) if (-128..=127).contains(&k) => out.extend_from_slice(&[0x83, 0xE9, k as u8]),
                (BinOp::Add, k) => { out.extend_from_slice(&[0x81, 0xC1]); out.extend_from_slice(&(k as u16).to_le_bytes()); }
                (BinOp::Sub, k) => { out.extend_from_slice(&[0x81, 0xE9]); out.extend_from_slice(&(k as u16).to_le_bytes()); }
                _ => unreachable!(),
            }
        }
        _ => unreachable!(),
    }
}
fn emit_binop_inner(op: BinOp, left: &Expr, right: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // Logical-and/or as a VALUE: short-circuit branches producing 0 or 1 in AX.
    // Pattern: emit_cond_skip(cond, 5); [mov ax,1; jmp +2]; [sub ax,ax]
    // The true block is 5 bytes (B8 01 00 EB 02); jmp +2 skips sub ax,ax.
    if matches!(op, BinOp::LogOr | BinOp::LogAnd) {
        let cond = cond_from_expr(Expr::BinOp {
            op,
            left: Box::new(left.clone()),
            right: Box::new(right.clone()),
        });
        // MSC aligns the false-block (`sub ax,ax`, the jcc target) to an even
        // offset, inserting a NOP after the over-jmp when it would land odd.
        // Fixture 2429. Size the cond onto a clone of `out` for an accurate offset.
        let cond_size = {
            let mut sz = out.clone();
            let base = sz.len();
            emit_cond_skip(&cond, 0i8, locals, &mut sz, &mut Vec::new());
            sz.len() - base
        };
        let needs_nop = (out.len() + cond_size + 5) % 2 != 0; // sub ax,ax lands here
        emit_cond_skip(&cond, 5 + needs_nop as i8, locals, out, fixups);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0xEB, 0x02 + needs_nop as u8]); // mov ax,1; jmp +2/+3
        if needs_nop { out.push(0x90); }
        out.extend_from_slice(&[0x2B, 0xC0]);                      // sub ax,ax
        return;
    }
    // Call-chain binop `f(..) OP g(..) [OP h(..)]` (2 or 3 calls, op add/sub):
    // MSC evaluates the calls RIGHT-to-LEFT, parking each result in SI then DI so
    // the running AX survives. Each non-rightmost call's (single, simple) arg is
    // built in CX. Then it folds left-to-right: `ax OP <reg>` reading the parked
    // results. Fixtures 1277/1536 (2 calls), 2305/2343 (3 calls).
    if let Some((calls, ops)) = call_chain(op, left, right, locals.inits) {
        let n = calls.len();
        // Rightmost call → AX (full call: args pushed, call, cleanup).
        emit_expr_to_ax(calls[n - 1], locals, out, fixups);
        // mov si,ax / mov di,ax park opcodes; add/sub ax,si / ax,di modrm.
        let park_mov = [0xF0u8, 0xF8]; // mov si,ax ; mov di,ax
        let combine_modrm = [0xC6u8, 0xC7]; // op ax,si ; op ax,di
        for k in 0..n - 1 {
            let call = calls[n - 2 - k];
            let (largs, name, target): (&[Expr], Option<&str>, Option<&Expr>) = match call {
                Expr::Call { name, args } => (args, Some(name), None),
                Expr::CallPtr { target, args } => (args, None, Some(target)),
                _ => unreachable!(),
            };
            // Build each arg in CX and push right-to-left (so AX, holding the
            // running result, survives until it's parked below).
            for arg in largs.iter().rev() {
                emit_arg_to_cx(arg, locals, out, fixups);
                out.push(0x51); // push cx
            }
            out.extend_from_slice(&[0x8B, park_mov[k]]); // park the running result
            let char_ret = if let Some(nm) = name {
                let sym = callee_symbol(nm, locals.pascal_fns.contains(nm));
                let bo = out.len();
                out.extend_from_slice(&[0xE8, 0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::TuLocalCall { target: sym.clone() } });
                locals.char_returners.contains(&sym)
            } else {
                crate::codegen::calls::emit_call_ptr_target(target.unwrap(), locals, out, fixups);
                false
            };
            if !largs.is_empty() {
                out.extend_from_slice(&[0x83, 0xC4, (2 * largs.len()) as u8]); // add sp,2N
            }
            if char_ret { out.push(0x98); } // cbw
        }
        // Combine: AX holds calls[0]; fold each op in source order with the reg
        // that parked the corresponding operand (op[j] ↔ park index n-2-j).
        for j in 0..n - 1 {
            let base = if matches!(ops[j], BinOp::Add) { 0x03u8 } else { 0x2B };
            out.extend_from_slice(&[base, combine_modrm[n - 2 - j]]);
        }
        return;
    }
    // Sum of bit-fields of one struct unit (a left-assoc `f0 + f1 + ... + f_{n-1}`
    // chain, all fields of the same Local/Global base): MSC evaluates the operands
    // in REVERSE source order — the last source operand into AX (byte fields widen,
    // high fields shift), the middle operands into DX, and the FIRST source operand
    // (the lowest field, bit_off==0, no shift → CL stays free) into CX — folding
    // each with `add ax,<reg>`. This beats the generic save-to-BX/reload and matches
    // MSC's register choices. Fixtures 2107, 2471 (2 fields); 1691, 1879, 2104 (3,
    // trailing byte field). Add-only (the reverse reorder needs commutativity).
    // Placed early — before the nested-binop-left dispatch claims `(f0+f1)+f2`.
    if op == BinOp::Add {
        let mut fields: Vec<(BitBase, u16, u8, u8)> = Vec::new();
        let all_bitfields = collect_bitfield_add_chain(left, &mut fields)
            & collect_bitfield_add_chain(right, &mut fields);
        if all_bitfields
            && fields.len() >= 2
            && same_local_global_base(&fields)
            // all fields must share the SAME unit word (same byte_off) — the
            // reorder's AX-reuse / CL-shift-freedom only holds within one unit.
            // A `:0` zero-width separator splits fields into different units
            // (fixture 2302), which MSC reads in source order instead.
            && fields.iter().all(|f| f.1 == fields[0].1)
            // first source operand (→CX) must be a low field (no shift)
            && fields[0].2 == 0
            // middle operands (→DX) must be word fields (the DX reader can't widen
            // a byte field); a byte field may only sit last-source (→AX).
            && fields[1..fields.len() - 1].iter().all(|f| !(f.3 == 8 && f.2 % 8 == 0))
        {
            let n = fields.len();
            // Last source operand → AX (handles byte widen / high-field shift+reuse).
            let last = fields[n - 1];
            emit_expr_to_ax(&Expr::BitField { base: last.0, byte_off: last.1, bit_off: last.2, bit_width: last.3 }, locals, out, fixups);
            // Middle operands → DX, add into AX.
            for f in fields[1..n - 1].iter().rev() {
                bf_read_into_dx(f.0, f.1, f.2, f.3, locals, out, fixups);
                out.extend_from_slice(&[0x03, 0xC2]); // add ax,dx
            }
            // First source operand (lowest field) → CX, add into AX.
            let f0 = fields[0];
            bf_load_word_cx(f0.0, f0.1, locals, out, fixups);
            if f0.3 < 16 {
                let mask = (1u32 << f0.3) - 1;
                out.push(0x81); out.push(0xE1); // and cx,imm16
                out.extend_from_slice(&(mask as u16).to_le_bytes());
            }
            out.extend_from_slice(&[0x03, 0xC1]); // add ax,cx
            return;
        }
        // Two low bit-fields in DIFFERENT units (a `:0` zero-width separator put
        // them in separate words): MSC can't reuse a live unit across them, so it
        // reads in SOURCE order — first into AX (fresh `mov ax,[ua]; and ax,mask`),
        // second into CX (`mov cx,[ub]; and cx,mask`), `add ax,cx`. Fixture 2302.
        if all_bitfields
            && fields.len() == 2
            && same_local_global_base(&fields)
            && fields[0].1 != fields[1].1
            && fields[0].2 == 0 && fields[1].2 == 0
        {
            let (a, b) = (fields[0], fields[1]);
            emit_expr_to_ax(&Expr::BitField { base: a.0, byte_off: a.1, bit_off: 0, bit_width: a.3 }, locals, out, fixups);
            bf_load_word_cx(b.0, b.1, locals, out, fixups);
            if b.3 < 16 {
                let mask = (1u32 << b.3) - 1;
                out.push(0x81); out.push(0xE1); // and cx,imm16
                out.extend_from_slice(&(mask as u16).to_le_bytes());
            }
            out.extend_from_slice(&[0x03, 0xC1]); // add ax,cx
            return;
        }
    }
    // `x * 256` over a WORD memory operand: a left-shift by 8 just moves the
    // low byte into the high byte — `mov ah,[x]; sub al,al`. Fixture 3367.
    if matches!(op, BinOp::Mul)
        && matches!(right, Expr::IntLit(256))
    {
        let word_mem = match left {
            Expr::Local(i) => locals.size(*i) == 2 && !locals.is_long_local(*i),
            Expr::Param(i) => !locals.is_char_param(*i) && !locals.is_long_param(*i),
            _ => false,
        };
        if word_mem && let Some(disp) = bp_disp(left, locals) {
            out.push(0x8A); out.push(bp_modrm(0x66, disp)); push_bp_disp(out, disp); // mov ah,[mem]
            out.extend_from_slice(&[0x2A, 0xC0]); // sub al,al
            return;
        }
    }
    // `x * K` for a constant wider than 4 bits over a WORD memory operand: the
    // shift/add chain isn't worth it, so MSC loads the constant into AX and
    // multiplies the memory operand directly (`mov ax,K; imul word [x]`).
    // Fixtures 3623 (×31), 1818 (×100). Threshold: K > 15 (bit-length ≥ 5),
    // excluding powers of two (a plain shl) and 256 (the byte-move above).
    if matches!(op, BinOp::Mul)
        && let Expr::IntLit(k) = right
        && *k > 15
        && !(*k as u32).is_power_of_two()
    {
        let kw = (*k as u32 & 0xFFFF) as u16;
        let word_mem = match left {
            Expr::Local(i) => locals.size(*i) == 2 && !locals.is_long_local(*i),
            Expr::Param(i) => !locals.is_char_param(*i) && !locals.is_long_param(*i),
            _ => false,
        };
        if word_mem && let Some(disp) = bp_disp(left, locals) {
            out.push(0xB8); out.extend_from_slice(&kw.to_le_bytes()); // mov ax,K
            out.push(0xF7); out.push(bp_modrm(0x6E, disp)); push_bp_disp(out, disp); // imul word [bp+disp]
            return;
        }
        if let Expr::Global(g) = left
            && !locals.is_char_global(*g) && !locals.is_long_global(*g)
        {
            out.push(0xB8); out.extend_from_slice(&kw.to_le_bytes()); // mov ax,K
            out.push(0xF7); out.push(0x2E); // imul word [g]
            let bo = out.len(); out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
            return;
        }
    }
    // Comparison ops materialize as 0/1 in AX via cmp+jcc.
    if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
        let cond = Cond::Cmp {
            op: match op {
                BinOp::Eq => RelOp::Eq, BinOp::Ne => RelOp::Ne,
                BinOp::Lt => RelOp::Lt, BinOp::Le => RelOp::Le,
                BinOp::Gt => RelOp::Gt, BinOp::Ge => RelOp::Ge,
                _ => unreachable!(),
            },
            left: left.clone(),
            right: right.clone(),
        };
        // `<cmp>; mov ax, 1; jcc-true +2; sub ax, ax` shape — if condition
        // is TRUE the jcc fires and skips `sub ax,ax`, leaving ax=1.
        // If FALSE it falls through to `sub ax,ax` giving ax=0.
        emit_cond_cmp_inner(&cond, locals, out, fixups);
        out.extend_from_slice(&[0xB8, 0x01, 0x00]);
        out.push(loop_back_jcc(match op {
            BinOp::Eq => RelOp::Eq, BinOp::Ne => RelOp::Ne,
            BinOp::Lt => RelOp::Lt, BinOp::Le => RelOp::Le,
            BinOp::Gt => RelOp::Gt, BinOp::Ge => RelOp::Ge,
            _ => unreachable!(),
        }));
        out.push(0x02); // disp8 = 2 (skip the `sub ax, ax`)
        out.extend_from_slice(&[0x2B, 0xC0]); // sub ax, ax
        return;
    }
    // Runtime shift `x << amt` / `x >> amt` (non-constant amount): load x into AX,
    // compute the amount into CL via byte ops (which leave AX intact), then
    // `shl/shr/sar ax,cl`. Fixture 3634 (`x << (a + b)`). The amount must be a
    // simple var or a sum of two simple vars (the shapes MSC builds in CL).
    if matches!(op, BinOp::Shl | BinOp::Shr)
        && right.fold(locals.inits).is_none()
        && shift_amount_cl(right, locals).is_some()
        && !long_operand(left, locals)
    {
        emit_expr_to_ax(left, locals, out, fixups); // x → AX
        let parts = shift_amount_cl(right, locals).unwrap();
        // First operand: `mov cl,[d]` (8A 4E d); subsequent: `add cl,[d]` (02 4E d).
        for (i, d) in parts.iter().enumerate() {
            out.push(if i == 0 { 0x8A } else { 0x02 });
            out.push(bp_modrm(0x4E, *d));
            push_bp_disp(out, *d);
        }
        let unsigned = match left {
            Expr::Param(i) => locals.is_unsigned_param(*i),
            Expr::Local(i) => locals.is_unsigned_local(*i),
            Expr::Global(g) => locals.is_unsigned_global(*g),
            _ => false,
        };
        let modrm = match op {
            BinOp::Shl => 0xE0,
            BinOp::Shr => if unsigned { 0xE8 } else { 0xF8 },
            _ => unreachable!(),
        };
        out.push(0xD3); out.push(modrm); // shl/shr/sar ax,cl
        return;
    }
    // DX:AX field reuse: `a OP b` where both are word operands at the exact bp
    // slots a preceding DX:AX store just wrote (`mov [a],ax; mov [b],dx`) — e.g.
    // `s.x + s.y` right after a struct copy/receive — reuses the live registers
    // with `<op> ax, dx`. Fixtures 2611, 2614.
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && let Some(ld) = bp_disp(left, locals)
        && let Some(rd) = bp_disp(right, locals)
    {
        let mut pat = vec![0x89, bp_modrm(0x46, ld)];
        push_bp_disp(&mut pat, ld);
        pat.push(0x89); pat.push(bp_modrm(0x56, rd));
        push_bp_disp(&mut pat, rd);
        if out.len() >= pat.len() && out[out.len() - pat.len()..] == *pat {
            let opc = match op {
                BinOp::Add => 0x03, BinOp::Sub => 0x2B, BinOp::BitAnd => 0x23,
                BinOp::BitOr => 0x0B, BinOp::BitXor => 0x33, _ => unreachable!(),
            };
            out.push(opc); out.push(0xC2); // op ax, dx
            return;
        }
    }
    // `(a±b) * (c±d)` — product of two simple add/sub sub-expressions: MSC
    // computes the RHS into CX, the LHS into AX, then `imul cx`. Fixture 3454.
    if matches!(op, BinOp::Mul)
        && simple_addsub(left, locals).is_some()
        && simple_addsub(right, locals).is_some()
    {
        emit_addsub_into(true, right, locals, out);  // RHS → CX
        emit_addsub_into(false, left, locals, out);  // LHS → AX
        out.extend_from_slice(&[0xF7, 0xE9]); // imul cx
        return;
    }
    // Rotate-style `(v sh1 k1) OP (v sh2 k2)` where both operands are constant
    // shifts of word bp-rel variables: MSC computes the LHS in AX and the RHS in
    // DX (no push/pop), then `op ax,dx`. Fixtures 3532, 3533 (`(x<<1)|(x>>15)`).
    if matches!(op, BinOp::BitOr | BinOp::BitXor | BinOp::Add)
        && let Some(l) = shift_of_bp(left, locals)
        && let Some(r) = shift_of_bp(right, locals)
    {
        emit_shift_into(false, l, out); // LHS → AX
        emit_shift_into(true, r, out);  // RHS → DX
        let opc = match op { BinOp::BitOr => 0x0B, BinOp::BitXor => 0x33, BinOp::Add => 0x03, _ => unreachable!() };
        out.push(opc); out.push(0xC2); // op ax, dx
        return;
    }
    // Signed-char LEFT + unsigned-char RIGHT (`s + u`): the signed operand widens
    // into AX (`mov al,[s]; cbw`), the unsigned operand zero-extends into CX
    // (`mov cl,[u]; sub ch,ch`), then `op ax, cx`. Unlike the all-signed path
    // below, the LEFT operand is loaded first. Fixture 2690.
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && signed_byte_load(left, locals)
        && let Some(udisp) = unsigned_byte_bp(right, locals)
    {
        emit_expr_to_ax(left, locals, out, fixups);                          // mov al,[s]; cbw
        out.push(0x8A); out.push(bp_modrm(0x4E, udisp)); push_bp_disp(out, udisp); // mov cl,[u]
        out.extend_from_slice(&[0x2A, 0xED]);                                 // sub ch,ch
        let opc = match op {
            BinOp::Add => 0x03, BinOp::Sub => 0x2B, BinOp::BitAnd => 0x23,
            BinOp::BitOr => 0x0B, BinOp::BitXor => 0x33, _ => unreachable!(),
        };
        out.push(opc); out.push(0xC1); // op ax, cx
        return;
    }
    // Two SIGNED byte (char) operands (`a OP b`, both byte loads): MSC loads the
    // RHS to AX via `mov al,..; cbw`, parks it in CX, loads the LHS the same way,
    // then `op ax, cx`. Covers char params/locals/globals, char-array elements,
    // and size-1 (union/struct) field reads. (The generic path below would wrongly
    // read a char as a word.) Unsigned chars use a different shape (cl/sub ch,ch)
    // and are excluded. Fixtures 2431, 3517, 3667, 2103, 2563.
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && signed_byte_load(left, locals)
        && signed_byte_load(right, locals)
    {
        emit_expr_to_ax(right, locals, out, fixups); // mov al,[r]; cbw
        out.extend_from_slice(&[0x8B, 0xC8]);        // mov cx, ax
        emit_expr_to_ax(left, locals, out, fixups);  // mov al,[l]; cbw
        let opc = match op {
            BinOp::Add => 0x03, BinOp::Sub => 0x2B, BinOp::BitAnd => 0x23,
            BinOp::BitOr => 0x0B, BinOp::BitXor => 0x33, _ => unreachable!(),
        };
        out.push(opc);
        out.push(0xC1); // op ax, cx
        return;
    }
    // Commutative-op swap: when the RHS is a value that needs AX
    // (call, complex expression) and the LHS is a BP-rel operand,
    // MSC emits the RHS first and then uses the BP-rel as a memory
    // operand. Matches the factorial idiom `n * fact(n-1)` → call,
    // then `imul [bp+n_disp]`. Fixtures 1220, 1264, 1277.
    let commutative = matches!(op, BinOp::Add | BinOp::Mul | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor);
    if commutative
        && matches!(right, Expr::Call { .. })
        && let Some(disp) = bp_disp(left, locals)
    {
        emit_expr_to_ax(right, locals, out, fixups);
        emit_mem_op_at(op, disp, out);
        return;
    }
    // Commutative swap: when RIGHT is a char (char param, or a 1-byte field of
    // a by-value struct param) and LEFT is a non-char bp-rel operand, MSC loads
    // the char first (byte+cbw) and uses the other operand as a word mem source.
    // Fixtures 616, 3685, 2866 (`s.a + s.b`, int field + char field).
    let right_char_disp = match right {
        Expr::Param(ri) if locals.is_char_param(*ri) => Some(param_disp(*ri)),
        Expr::ParamField { param, byte_off, size: 1 } => Some(locals.param_base_disp(*param) + *byte_off as i16),
        _ => None,
    };
    if commutative
        && let Some(r_disp) = right_char_disp
        && !matches!(left, Expr::Param(li) if locals.is_char_param(*li))
        && let Some(l_disp) = bp_disp(left, locals)
    {
        out.push(0x8A); out.push(bp_modrm(0x46, r_disp)); push_bp_disp(out, r_disp); // mov al, [bp+r_disp]
        out.push(0x98);  // cbw
        emit_mem_op_at(op, l_disp, out);
        return;
    }
    // Char local /= K: byte division.  MSC hoists `mov cl, K` before the
    // load and uses `idiv cl` (f6 f9) instead of word `idiv cx` (f7 f9).
    // Pattern: `b1 K; 8a 46 disp; 98; f6 f9`.
    if matches!(op, BinOp::Div)
        && let Expr::Local(li) = left
        && locals.size(*li) == 1
        && let Expr::IntLit(k) = right
    {
        let disp = locals.disp(*li);
        let uns = locals.is_unsigned_local(*li);
        out.push(0xB1); // mov cl, K
        out.push((*k as u32 & 0xFF) as u8);
        out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        if uns { out.extend_from_slice(&[0x2A, 0xE4]); } else { out.push(0x98); } // sub ah,ah / cbw
        out.push(0xF6); out.push(if uns { 0xF1 } else { 0xF9 }); // div cl / idiv cl
        return;
    }
    // Signed int division by a power of two 2^m with m >= 2: MSC avoids `idiv`
    // with the abs idiom — `cwd; xor ax,dx; sub ax,dx` (ax = |x|), `mov cx,m;
    // sar ax,cl`, then `xor ax,dx; sub ax,dx` to restore the sign. Division by
    // 2 (m==1) uses the simpler shape handled below. Int-sized signed operands
    // only. Fixtures 3090, 3338.
    if matches!(op, BinOp::Div)
        && let Expr::IntLit(k) = right
        && *k >= 4
        && (*k as u32).is_power_of_two()
        && matches!(left, Expr::Local(_) | Expr::Param(_))
    {
        let is_unsigned = match left {
            Expr::Param(i) => locals.is_unsigned_param(*i),
            Expr::Local(i) => locals.is_unsigned_local(*i),
            _ => false,
        };
        let is_char_sized = match left {
            Expr::Local(i) => locals.size(*i) == 1,
            Expr::Param(i) => locals.is_char_param(*i),
            _ => false,
        };
        if !is_unsigned && !is_char_sized
            && let Some(load) = bp_load(left, locals)
        {
            let m = (*k as u32).trailing_zeros() as u8;
            load(out);
            out.extend_from_slice(&[0x99, 0x33, 0xC2, 0x2B, 0xC2]); // cwd; xor ax,dx; sub ax,dx
            out.extend_from_slice(&[0xB9, m, 0x00]);                // mov cx, m
            out.extend_from_slice(&[0xD3, 0xF8]);                   // sar ax, cl
            out.extend_from_slice(&[0x33, 0xC2, 0x2B, 0xC2]);       // xor ax,dx; sub ax,dx
            return;
        }
    }
    // Unsigned int division by a power of two `x / 2^k` (k>=2) → logical shift
    // right by k. MSC uses `shr ax,1` repeated for a shift of 2, `mov cl,k; shr
    // ax,cl` for 3+. Unsigned int param/local only. Fixtures 3339 (/4), 3369 (/8).
    if matches!(op, BinOp::Div)
        && let Expr::IntLit(k) = right
        && *k >= 4
        && (*k as u32).is_power_of_two()
        && matches!(left, Expr::Local(_) | Expr::Param(_))
    {
        let is_unsigned = match left {
            Expr::Param(i) => locals.is_unsigned_param(*i),
            Expr::Local(i) => locals.is_unsigned_local(*i),
            _ => false,
        };
        let is_char_sized = match left {
            Expr::Local(i) => locals.size(*i) == 1,
            Expr::Param(i) => locals.is_char_param(*i),
            _ => false,
        };
        if is_unsigned && !is_char_sized
            && let Some(load) = bp_load(left, locals)
        {
            let shift = (*k as u32).trailing_zeros();
            load(out);
            if shift <= 2 {
                for _ in 0..shift { out.extend_from_slice(&[0xD1, 0xE8]); } // shr ax,1
            } else {
                out.push(0xB1); out.push(shift as u8);  // mov cl, shift
                out.extend_from_slice(&[0xD3, 0xE8]);    // shr ax, cl
            }
            return;
        }
    }
    // Unsigned int modulo by a power of two `x % 2^k` → `load; and ax, 2^k-1`
    // (the remainder is just the low k bits). Unsigned int param/local only;
    // signed `%` needs a sign-correcting sequence. Fixtures 3092, 3095.
    if matches!(op, BinOp::Mod)
        && let Expr::IntLit(k) = right
        && *k >= 2
        && (*k as u32).is_power_of_two()
        && matches!(left, Expr::Local(_) | Expr::Param(_))
    {
        let is_unsigned = match left {
            Expr::Param(i) => locals.is_unsigned_param(*i),
            Expr::Local(i) => locals.is_unsigned_local(*i),
            _ => false,
        };
        let is_char_sized = match left {
            Expr::Local(i) => locals.size(*i) == 1,
            Expr::Param(i) => locals.is_char_param(*i),
            _ => false,
        };
        if is_unsigned && !is_char_sized
            && let Some(load) = bp_load(left, locals)
        {
            load(out);
            emit_imm_op(BinOp::BitAnd, *k - 1, out); // and ax, k-1
            return;
        }
    }
    // Signed/unsigned int /2 optimization: MSC avoids `idiv` for
    // division by 2.  Signed: `load; cwd; sub ax,dx; sar ax,1`.
    // Unsigned: `load; shr ax,1`.  Only for int-sized (non-char) operands.
    if matches!(op, BinOp::Div)
        && matches!(right, Expr::IntLit(2))
        && matches!(left, Expr::Local(_) | Expr::Param(_))
    {
        let is_unsigned = match left {
            Expr::Param(i) => locals.is_unsigned_param(*i),
            Expr::Local(i) => locals.is_unsigned_local(*i),
            _ => false,
        };
        let is_char_sized = match left {
            Expr::Local(i) => locals.size(*i) == 1,
            Expr::Param(i) => locals.is_char_param(*i),
            _ => false,
        };
        if !is_char_sized {
            if let Some(load) = bp_load(left, locals) {
                load(out);
                if is_unsigned {
                    out.extend_from_slice(&[0xD1, 0xE8]); // shr ax, 1
                } else {
                    out.extend_from_slice(&[0x99, 0x2B, 0xC2, 0xD1, 0xF8]); // cwd; sub ax,dx; sar ax,1
                }
                return;
            }
        }
    }
    // Unsigned int division/modulo by a variable: `a / b` / `a % b` where both
    // are unsigned word operands → `mov ax,[a]; sub dx,dx; div WORD [b]` (`div`,
    // not signed `idiv`; `sub dx,dx` zero-extends instead of `cwd`). Modulo adds
    // `mov ax,dx`. Fixtures 3519, 3627.
    if matches!(op, BinOp::Div | BinOp::Mod) {
        let unsigned_word = |e: &Expr| match e {
            Expr::Param(i) => locals.is_unsigned_param(*i) && !locals.is_char_param(*i),
            Expr::Local(i) => locals.is_unsigned_local(*i) && locals.size(*i) == 2,
            _ => false,
        };
        if unsigned_word(left) && unsigned_word(right)
            && let Some(rdisp) = bp_disp(right, locals)
            && let Some(load) = bp_load(left, locals)
        {
            load(out); // mov ax,[a]
            out.extend_from_slice(&[0x2B, 0xD2]); // sub dx,dx
            out.push(0xF7); out.push(bp_modrm(0x76, rdisp)); push_bp_disp(out, rdisp); // div word [b]
            if matches!(op, BinOp::Mod) { out.extend_from_slice(&[0x8B, 0xC2]); } // mov ax,dx
            return;
        }
    }
    // Left as a BP-rel operand we can load into AX.
    if let Some(load) = bp_load(left, locals) {
        load(out);
        // Right as IntLit → imm form.
        if let Expr::IntLit(k) = right {
            // A SIGNED int right-shift `x >> K` is arithmetic (`sar`), but
            // emit_imm_op only emits the logical `shr`. Route signed shifts to
            // sar here. Fixture 1515 (`int x; ...; return x >> 4`).
            let signed_word = match left {
                Expr::Local(i) => locals.size(*i) == 2 && !locals.is_long_local(*i)
                    && !locals.is_float_local(*i) && !locals.is_unsigned_local(*i),
                Expr::Param(i) => !locals.is_char_param(*i) && !locals.is_long_param(*i)
                    && !locals.is_float_param(*i) && !locals.is_unsigned_param(*i),
                _ => false,
            };
            if matches!(op, BinOp::Shr) && signed_word && *k > 0 {
                if *k == 1 { out.extend_from_slice(&[0xD1, 0xF8]); } // sar ax,1
                else { out.extend_from_slice(&[0xB1, *k as u8, 0xD3, 0xF8]); } // mov cl,k; sar ax,cl
                return;
            }
            emit_imm_op(op, *k, out);
            return;
        }
        // Right as BP-rel → `op ax, [bp+disp]` mem form.
        if let Some(disp) = bp_disp(right, locals) {
            emit_mem_op_at(op, disp, out);
            return;
        }
        // Right as GlobalField (`s.x`) → `op ax, word ptr [g+off]`.
        if let Expr::GlobalField { global, byte_off, size: 2 } = right {
            let opcode = match op {
                BinOp::Add => 0x03,
                BinOp::Sub => 0x2B,
                BinOp::BitAnd => 0x23,
                BinOp::BitOr => 0x0B,
                BinOp::BitXor => 0x33,
                _ => panic!("{op:?} ax, [global+field] not yet supported"),
            };
            out.push(opcode);
            out.push(0x06);
            let body_offset = out.len();
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup {
                body_offset: body_offset - 1,
                kind: FixupKind::GlobalAddr { global_idx: *global },
            });
            return;
        }
        // Right as plain Global → `op ax, word ptr [g]`.
        if let Expr::Global(g) = right {
            let opcode = match op {
                BinOp::Add => 0x03,
                BinOp::Sub => 0x2B,
                BinOp::BitAnd => 0x23,
                BinOp::BitOr => 0x0B,
                BinOp::BitXor => 0x33,
                _ => panic!("{op:?} ax, [global] not yet supported"),
            };
            out.push(opcode);
            out.push(0x06);
            let body_offset = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset - 1,
                kind: FixupKind::GlobalAddr { global_idx: *g },
            });
            return;
        }
    }
    // Left as a global → `mov ax, [g]; <op> ax, ...`. The RHS can
    // be an int literal (fixture 4135), another global via memory
    // operand (`03 06 addr`, fixture 4138), or a local/param via the
    // BP-rel path.
    if let Expr::Global(idx) = left
        && locals.is_char_global(*idx)
    {
        // Char global left: byte load + widen (`a0; cbw` / `a0; sub ah,ah`,
        // via emit_expr_to_ax), NOT a word load. `c + 1` → `mov al,_c; sub
        // ah,ah; inc ax`. Fixture 460. Non-const RHS falls through to the
        // generic left-load/op-right path below (also byte-correct now).
        if let Expr::IntLit(k) = right {
            emit_expr_to_ax(left, locals, out, fixups);
            emit_imm_op(op, *k, out);
            return;
        }
    } else if let Expr::Global(idx) = left {
        let body_offset = out.len();
        out.extend_from_slice(&[0xA1, 0x00, 0x00]);
        fixups.push(Fixup {
            body_offset,
            kind: FixupKind::GlobalAddr { global_idx: *idx },
        });
        if let Expr::IntLit(k) = right {
            emit_imm_op(op, *k, out);
            return;
        }
        if let Expr::Global(idx2) = right {
            // `op ax, word ptr [g2]` — Grp1 r/m16 with mod=00 r/m=110.
            if matches!(op, BinOp::Mul) {
                // `imul word ptr [g2]` (`f7 /5 mem`) — DX:AX = AX *
                // [g2]. AX gets the low word.
                out.push(0xF7);
                out.push(0x2E);
                let body_offset = out.len();
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup {
                    body_offset: body_offset - 1,
                    kind: FixupKind::GlobalAddr { global_idx: *idx2 },
                });
                return;
            }
            let opcode = match op {
                BinOp::Add => 0x03,
                BinOp::Sub => 0x2B,
                BinOp::BitAnd => 0x23,
                BinOp::BitOr => 0x0B,
                BinOp::BitXor => 0x33,
                _ => panic!("{op:?} between two globals not yet supported"),
            };
            out.push(opcode);
            out.push(0x06);
            let body_offset = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset - 1,
                kind: FixupKind::GlobalAddr { global_idx: *idx2 },
            });
            return;
        }
    }
    // Foldable side — recurse with the folded literal substituted.
    // Lets `(2 + x)` collapse to `(<lit> + <local>)` etc. Guard against
    // recursing when the operand is already an IntLit (the fold returns
    // its own value), which would spin forever.
    if !matches!(left, Expr::IntLit(_))
        && let Some(k) = left.fold(locals.inits)
    {
        emit_binop(op, &Expr::IntLit(k), right, locals, out, fixups);
        return;
    }
    if !matches!(right, Expr::IntLit(_))
        && let Some(k) = right.fold(locals.inits)
    {
        emit_binop(op, left, &Expr::IntLit(k), locals, out, fixups);
        return;
    }
    // Both sides are IntLit: fold the whole binop and emit a single
    // `mov ax, K`. The recursion guard above blocks the per-side
    // substitution from doing this.
    if let (Expr::IntLit(_), Expr::IntLit(_)) = (left, right)
        && let Some(k) = (Expr::BinOp { op, left: Box::new(left.clone()), right: Box::new(right.clone()) }).fold(locals.inits)
    {
        let k16 = (k as u32 & 0xFFFF) as u16;
        out.push(0xB8);
        out.extend_from_slice(&k16.to_le_bytes());
        return;
    }
    // `IntLit(K) <op> Local/Param/Global`: for commutative ops, reorder
    // to `<rhs> <op> IntLit(K)` so the BP-rel-on-left path can fire.
    let commut = matches!(op, BinOp::Add | BinOp::Mul | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor);
    if matches!(left, Expr::IntLit(_))
        && commut
        && !matches!(right, Expr::IntLit(_))
    {
        emit_binop(op, right, left, locals, out, fixups);
        return;
    }
    // Non-commutative `K <op> BP-rel`: `mov ax, K; <op> ax, [bp-disp]`.
    if let Expr::IntLit(k) = left
        && bp_load(right, locals).is_some()
    {
        let k16 = (*k as u32 & 0xFFFF) as u16;
        out.push(0xB8);
        out.extend_from_slice(&k16.to_le_bytes());
        emit_mem_op_at(op, bp_disp(right, locals).expect("bp_disp"), out);
        return;
    }
    // `a[I] + a[J]` where both ParamIndex share the same param:
    // load the pointer into BX once, then read RHS into AX and add
    // LHS as a memory operand. Matches MSC's `mov bx, [bp+disp];
    // mov ax, [bx+J*2]; add ax, [bx+I*2]` shape on fixture 1236.
    if let Expr::ParamIndex { param: lp, index: li } = left
        && let Expr::ParamIndex { param: rp, index: ri } = right
        && *lp == *rp
        && let Some(lk) = li.fold(locals.inits)
        && let Some(rk) = ri.fold(locals.inits)
        && matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
    {
        let p_disp = param_disp(*lp);
        out.push(0x8B);
        out.push(0x5E);
        out.push(p_disp as u8);
        // RHS first (matches MSC observation on 1236).
        let r_disp = (rk as i16) * 2;
        if r_disp == 0 {
            out.extend_from_slice(&[0x8B, 0x07]);
        } else {
            out.push(0x8B);
            out.push(0x47);
            out.push(r_disp as u8);
        }
        let opcode = match op {
            BinOp::Add => 0x03,
            BinOp::Sub => 0x2B,
            BinOp::BitAnd => 0x23,
            BinOp::BitOr => 0x0B,
            BinOp::BitXor => 0x33,
            _ => unreachable!(),
        };
        let l_disp = (lk as i16) * 2;
        if l_disp == 0 {
            out.push(opcode);
            out.push(0x07);
        } else {
            out.push(opcode);
            out.push(0x47);
            out.push(l_disp as u8);
        }
        return;
    }
    // Nested binop on the left (`(a + b) + c`, `g[0] + g[1] + g[2]`):
    // compute the left subtree into AX, then fold the right side in.
    // Fixtures 4139 / 1100.
    if let Expr::BinOp { .. } = left {
        emit_expr_to_ax(left, locals, out, fixups);
        return emit_binop_right(op, right, locals, out, fixups);
    }
    // Generic fallback: evaluate the left subtree into AX, then fold
    // the right side in. Covers DerefWord, DerefByte, Call, Ternary,
    // and other compound left operands.
    if matches!(left, Expr::DerefWord { .. } | Expr::DerefByte { .. }
                    | Expr::Call { .. } | Expr::Ternary { .. } | Expr::Seq { .. }
                    | Expr::GlobalField { .. } | Expr::LocalField { .. }
                    | Expr::DerefLocalField { .. } | Expr::DerefParamField { .. }
                    | Expr::LocalIndex { .. } | Expr::Index { .. })
    {
        emit_expr_to_ax(left, locals, out, fixups);
        return emit_binop_right(op, right, locals, out, fixups);
    }
    // Left as a global-array Index with constant K: synthesize a
    // global-load (`a1 byte_off`) and let the right side fold.
    if let Some((array_idx, byte_off)) = const_index_global(left, locals) {
        let body_offset = out.len();
        out.push(0xA1);
        out.extend_from_slice(&byte_off.to_le_bytes());
        fixups.push(Fixup {
            body_offset,
            kind: FixupKind::GlobalAddr { global_idx: array_idx },
        });
        return emit_binop_right(op, right, locals, out, fixups);
    }
    // `<expr> * (++x)` where the RHS pre-increment lands in a word local: MSC
    // evaluates the LHS into AX, mutates+loads the RHS into CX (the `inc [x]`
    // doesn't touch AX), then `imul cx`. Covers `(++i)*(++i)` (fixture 2293).
    if matches!(op, BinOp::Mul)
        && let Expr::PreMutateLocal { local_idx: rb, step: rstep } = right
        && locals.size(*rb) == 2
    {
        emit_expr_to_ax(left, locals, out, fixups); // LHS → AX
        let disp = locals.disp(*rb);
        crate::codegen::assign::emit_postmutate_local(*rstep, 2, disp, out); // inc/dec [x]
        out.push(0x8B); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp); // mov cx,[x]
        out.extend_from_slice(&[0xF7, 0xE9]); // imul cx
        return;
    }
    // `(<assign-expr>) op K` — the assignment has a side effect that must run
    // first (it can't be reordered after loading K into BX), so evaluate the
    // AssignExpr into AX, then fold the constant in place (`inc ax` for +1).
    // Fixture 3333 (`(*p = 5) + 1`).
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && matches!(left, Expr::AssignExpr { .. })
        && let Expr::IntLit(k) = right
    {
        emit_expr_to_ax(left, locals, out, fixups);
        emit_imm_op(op, *k, out);
        return;
    }
    // Generic fallback: evaluate RHS first into AX, save to BX, then
    // evaluate LHS into AX and apply the reg-reg op. Works for any
    // shape that survived the earlier specialized paths.
    emit_expr_to_ax(right, locals, out, fixups);
    out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax (rhs → bx)
    emit_expr_to_ax(left, locals, out, fixups);
    let op_byte = match op {
        BinOp::Add => 0x03,
        BinOp::Sub => 0x2B,
        BinOp::BitAnd => 0x23,
        BinOp::BitOr => 0x0B,
        BinOp::BitXor => 0x33,
        _ => panic!("binop shape not yet supported: {op:?} of {left:?}, {right:?}"),
    };
    out.extend_from_slice(&[op_byte, 0xC3]); // op ax, bx
}
/// After the LHS is in AX, fold the RHS into the same register via
/// the appropriate `op ax, <operand>` form. Used by `emit_binop`
/// once it's resolved the LHS into AX.
pub(crate) fn emit_binop_right(op: BinOp, right: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if let Expr::IntLit(k) = right {
        emit_imm_op(op, *k, out);
        return;
    }
    if let Expr::Global(idx) = right {
        let opcode = match op {
            BinOp::Add => 0x03,
            BinOp::Sub => 0x2B,
            _ => panic!("{op:?} with global rhs not yet supported"),
        };
        out.push(opcode);
        out.push(0x06);
        let body_offset = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx: *idx },
        });
        return;
    }
    if let Some((array_idx, byte_off)) = const_index_global(right, locals) {
        // `imul word [a+off]` (F7 /5, modrm 0x2E) for Mul; `<op> ax,[a+off]`
        // (modrm 0x06) for add/sub/and/or/xor. Fixture 1412 (`a[0] * a[2]`).
        match op {
            BinOp::Mul => { out.push(0xF7); out.push(0x2E); }
            BinOp::Add => { out.push(0x03); out.push(0x06); }
            BinOp::Sub => { out.push(0x2B); out.push(0x06); }
            BinOp::BitAnd => { out.push(0x23); out.push(0x06); }
            BinOp::BitOr => { out.push(0x0B); out.push(0x06); }
            BinOp::BitXor => { out.push(0x33); out.push(0x06); }
            _ => panic!("{op:?} with indexed-global rhs not yet supported"),
        }
        let body_offset = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        fixups.push(Fixup {
            body_offset: body_offset - 1,
            kind: FixupKind::GlobalAddr { global_idx: array_idx },
        });
        return;
    }
    if let Some(disp) = bp_disp(right, locals) {
        emit_mem_op_at(op, disp, out);
        return;
    }
    // Generic fallback for complex RHS (Call, Ternary, deref...):
    // push lhs, evaluate rhs into AX, exchange via BX, `op ax, bx`.
    out.push(0x50); // push ax
    emit_expr_to_ax(right, locals, out, fixups);
    out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax (rhs in bx)
    out.push(0x58); // pop ax (lhs back in ax)
    match op {
        BinOp::Mul => out.extend_from_slice(&[0xF7, 0xEB]), // imul bx (DX:AX = AX * BX)
        BinOp::Div => out.extend_from_slice(&[0x99, 0xF7, 0xFB]), // cwd; idiv bx
        BinOp::Mod => out.extend_from_slice(&[0x99, 0xF7, 0xFB, 0x8B, 0xC2]), // cwd; idiv bx; mov ax, dx
        BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
            let op_byte = match op {
                BinOp::Add => 0x03,
                BinOp::Sub => 0x2B,
                BinOp::BitAnd => 0x23,
                BinOp::BitOr => 0x0B,
                BinOp::BitXor => 0x33,
                _ => unreachable!(),
            };
            out.extend_from_slice(&[op_byte, 0xC3]);
        }
        _ => panic!("binop_right shape not yet supported: {op:?} of {right:?}"),
    }
}
/// If `e` is an `Index { array, IntLit(K) }` (or the IndexByte
/// variant), return (array_idx, byte_off). Used by emit_binop to
/// promote `g[K]` to an `[addr]` operand directly.
pub(crate) fn const_index_global(e: &Expr, locals: &Locals<'_>) -> Option<(usize, u16)> {
    match e {
        Expr::Index { array, index } => {
            let k = index.fold(locals.inits)?;
            let off = u16::try_from((k as i64) * 2).ok()?;
            Some((*array, off))
        }
        Expr::IndexByte { array, index } => {
            let k = index.fold(locals.inits)?;
            let off = u16::try_from(k as i64).ok()?;
            Some((*array, off))
        }
        Expr::GlobalField { global, byte_off, size: 2 } => {
            Some((*global, *byte_off))
        }
        _ => None,
    }
}
/// If `e` is a Local or Param, return a closure that emits the
/// `mov ax, [bp+disp]` load. Otherwise return None. Used by
/// `emit_binop` to handle either operand kind on the left-hand side.
pub(crate) fn bp_load<'a>(e: &'a Expr, locals: &'a Locals<'_>) -> Option<Box<dyn FnOnce(&mut Vec<u8>) + 'a>> {
    match e {
        Expr::Local(i) => Some(Box::new(move |out: &mut Vec<u8>| emit_load_local_reuse(*i, locals, out))),
        Expr::Param(i) => Some(Box::new(move |out: &mut Vec<u8>| {
            if locals.is_char_param(*i) {
                let disp = param_disp(*i) as u8;
                out.extend_from_slice(&[0x8A, 0x46, disp]);  // mov al, [bp+disp]
                if locals.is_unsigned_param(*i) {
                    out.extend_from_slice(&[0x2A, 0xE4]); // sub ah,ah — zero-extend
                } else {
                    out.push(0x98);  // cbw — sign-extend byte param to word
                }
            } else {
                emit_load_param_reuse(*i, locals, out);
            }
        })),
        Expr::ParamField { param, byte_off, size: 2 } => Some(Box::new(move |out: &mut Vec<u8>| {
            let disp = locals.param_base_disp(*param) + *byte_off as i16;
            out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov ax,[bp+disp]
        })),
        Expr::LocalField { local, byte_off, size: 2 } => Some(Box::new(move |out: &mut Vec<u8>| {
            let disp = locals.disp(*local) + *byte_off as i16;
            out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov ax,[bp+disp]
        })),
        _ => None,
    }
}
/// If `e` is a Local or Param, return its bp-relative byte
/// displacement (negative for locals, positive for params).
pub(crate) fn bp_disp(e: &Expr, locals: &Locals<'_>) -> Option<i16> {
    match e {
        Expr::Local(i) => Some(locals.disp(*i)),
        Expr::Param(i) => Some(4 + (*i as i16) * 2),
        // Word field of a by-value struct param: a `[bp+disp]` operand.
        Expr::ParamField { param, byte_off, size: 2 } => Some(locals.param_base_disp(*param) + *byte_off as i16),
        // Word field of a struct local: a `[bp+disp]` operand.
        Expr::LocalField { local, byte_off, size: 2 } => Some(locals.disp(*local) + *byte_off as i16),
        // Word local-array element `a[K]` (constant K) is a `[bp+disp]` operand.
        // (Byte elements need cbw, so they are not bp-addressable here.)
        Expr::LocalIndex { local, index } => index.fold(locals.inits).map(|k| locals.disp(*local) + (k as i16) * 2),
        _ => None,
    }
}
/// Per-operator emit for `<reg-AX> <op> <imm>`. Picks the smallest
/// MSC-equivalent form (single-byte inc/dec, shl, shift-and-add)
/// before falling back to the generic `add/sub ax, imm16`.
pub(crate) fn emit_imm_op(op: BinOp, k: i32, out: &mut Vec<u8>) {
    let k16 = (k as u32 & 0xFFFF) as u16;
    match (op, k16) {
        (BinOp::Add, 1) => out.push(0x40),                  // inc ax
        (BinOp::Sub, 1) => out.push(0x48),                  // dec ax
        (BinOp::Mul, 2) => out.extend_from_slice(&[0xD1, 0xE0]), // shl ax, 1
        // `x * 3` → `mov cx, ax; shl ax, 1; add ax, cx`. Fixture 4088.
        // MSC picks this 6-byte shift-and-add over `imul ax, 3` for
        // single-use *3.
        (BinOp::Mul, 3) => out.extend_from_slice(&[
            0x8B, 0xC8,         // mov cx, ax
            0xD1, 0xE0,         // shl ax, 1
            0x03, 0xC1,         // add ax, cx
        ]),
        // Generic `add/sub ax, imm16` — Phase 2 fixtures will pin
        // down whether MSC ever picks `inc / dec` for K = 2 (BCC
        // does for some shapes; MSC unknown).
        (BinOp::Add, _) => {
            out.push(0x05);                                 // add ax, imm16
            out.extend_from_slice(&k16.to_le_bytes());
        }
        (BinOp::Sub, _) => {
            out.push(0x2D);                                 // sub ax, imm16
            out.extend_from_slice(&k16.to_le_bytes());
        }
        (BinOp::Mul, _) => {
            // MSC targets the 8086, which lacks `imul r,imm` (a 186+ form), so
            // `x * K` strength-reduces. Power-of-two → `shl ax,1` repeated.
            // Otherwise build the product from the binary digits of K: cx = x,
            // then for each bit below the MSB `shl ax,1` and (for a set bit)
            // `add ax,cx`. The initial AX already holds x (the MSB). Fixtures
            // 3366 (×5), 3895 (×7), 3896 (×9), 3898 (×11), 3899 (×4).
            if k == 0 {
                out.extend_from_slice(&[0x2B, 0xC0]); // sub ax,ax
            } else if k == -1 {
                out.extend_from_slice(&[0xF7, 0xD8]); // neg ax
            } else if k > 0 && (k as u32).is_power_of_two() {
                for _ in 0..(k as u32).trailing_zeros() {
                    out.extend_from_slice(&[0xD1, 0xE0]); // shl ax,1
                }
            } else if k > 0 {
                let ku = k as u32;
                let bits = 32 - ku.leading_zeros(); // bit length (≥2 here)
                out.extend_from_slice(&[0x8B, 0xC8]); // mov cx,ax
                for i in (0..bits - 1).rev() {
                    out.extend_from_slice(&[0xD1, 0xE0]); // shl ax,1
                    if (ku >> i) & 1 == 1 {
                        out.extend_from_slice(&[0x03, 0xC1]); // add ax,cx
                    }
                }
            } else {
                // Negative non-(-1) constant: fall back (rare; left for a
                // dedicated decode). 186 imul-imm is wrong for 8086 but these
                // were already failing.
                if let Ok(k8) = i8::try_from(k) {
                    out.extend_from_slice(&[0x6B, 0xC0, k8 as u8]);
                } else {
                    out.push(0x69); out.push(0xC0); out.extend_from_slice(&k16.to_le_bytes());
                }
            }
        }
        // Bitwise imm-AX always uses the accumulator imm16 form (`25/0d/35`),
        // NOT `83 /digit imm8sx`: a sign-extended imm8 would corrupt the high
        // byte for any mask with bit 7 set (e.g. `and ax,0x80` must be 0x0080,
        // not 0xFF80). MSC uses imm16 even for small masks. Fixtures 3683, 3684.
        (BinOp::BitAnd, _) => {
            // `and ax, 0x00XX` forces the high byte to 0, so a `cbw` widening the
            // operand just before is dead — MSC omits it (`char c & 0x0F` →
            // `mov al,[c]; and ax,15`, no cbw). Fixture 3097.
            if k16 <= 0xFF && out.last() == Some(&0x98) {
                out.pop();
            }
            out.push(0x25);
            out.extend_from_slice(&k16.to_le_bytes());
        }
        // OR/XOR with a low-byte-only mask (<= 0xFF) uses the byte accumulator
        // form `or/xor al, imm8` (`0c`/`34`): OR/XOR with 0 in the high byte
        // leaves it unchanged, so only AL is touched (fixture 3684 `x | 0x80`).
        // A wider mask uses `0d`/`35 imm16`. AND cannot do this (it clears the
        // high byte) and always uses the word form above.
        (BinOp::BitOr, _) => {
            if k16 <= 0xFF {
                out.extend_from_slice(&[0x0C, k16 as u8]); // or al, imm8
            } else {
                out.push(0x0D);
                out.extend_from_slice(&k16.to_le_bytes());
            }
        }
        (BinOp::BitXor, 0xFFFF) | (BinOp::BitXor, _) if k16 == 0xFFFF => {
            // `xor ax, -1` → `not ax` (f7 d0). 2 bytes vs 3. Fixture 1120.
            out.extend_from_slice(&[0xF7, 0xD0]);
        }
        (BinOp::BitXor, _) => {
            if k16 <= 0xFF {
                out.extend_from_slice(&[0x34, k16 as u8]); // xor al, imm8
            } else {
                out.push(0x35);
                out.extend_from_slice(&k16.to_le_bytes());
            }
        }
        // 8086 shifts by constant: 1 uses `d1 e0/e8`; larger picks
        // `mov cl, K; shl/shr ax, cl` (4 bytes).
        (BinOp::Shl, 1) => out.extend_from_slice(&[0xD1, 0xE0]),
        (BinOp::Shr, 1) => out.extend_from_slice(&[0xD1, 0xE8]),
        (BinOp::Shl, _) => out.extend_from_slice(&[0xB1, k as u8, 0xD3, 0xE0]),
        (BinOp::Shr, _) => out.extend_from_slice(&[0xB1, k as u8, 0xD3, 0xE8]),
        // Generic `ax /= K` / `ax %= K` for word: `b9 K K cwd f7 f9`
        // (mov cx, K; cwd; idiv cx) — DX:AX = AX / CX, AX gets quotient.
        // Mod: same setup, but the result is in DX, so swap into AX.
        (BinOp::Div, _) => {
            out.push(0xB9);
            out.extend_from_slice(&k16.to_le_bytes());
            out.extend_from_slice(&[0x99, 0xF7, 0xF9]);
        }
        (BinOp::Mod, _) => {
            out.push(0xB9);
            out.extend_from_slice(&k16.to_le_bytes());
            out.extend_from_slice(&[0x99, 0xF7, 0xF9, 0x8B, 0xC2]);
        }
        (op, _) => {
            panic!("imm-op `{op:?}` not yet covered by a fixture (k={k})");
        }
    }
}
/// Per-operator emit for `<reg-AX> <op> word ptr [bp+disp]`. The
/// opcode-prefix byte for memory-source forms: 03=ADD, 2B=SUB.
/// Works for both negative disps (locals) and positive disps
/// (params); fixture 4102 uses param shape.
pub(crate) fn emit_mem_op_at(op: BinOp, disp: i16, out: &mut Vec<u8>) {
    match op {
        BinOp::Mul => {
            // `imul word ptr [bp+disp]` — `F7 /5 r/m`.
            out.push(0xF7);
            out.push(bp_modrm(0x6E, disp));
            push_bp_disp(out, disp);
        }
        BinOp::Div => {
            // `cwd; mov cx, [bp+disp]; idiv cx`
            out.push(0x99);                         // cwd
            out.push(0x8B); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp); // mov cx, [bp+disp]
            out.extend_from_slice(&[0xF7, 0xF9]);   // idiv cx
        }
        BinOp::Mod => {
            // `cwd; mov cx, [bp+disp]; idiv cx; mov ax, dx`
            out.push(0x99);                         // cwd
            out.push(0x8B); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp); // mov cx, [bp+disp]
            out.extend_from_slice(&[0xF7, 0xF9]);   // idiv cx
            out.extend_from_slice(&[0x8B, 0xC2]);   // mov ax, dx
        }
        BinOp::Shl => {
            // `mov cl, byte ptr [bp+disp]; shl ax, cl`
            out.push(0x8A); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp); // mov cl, [bp+disp]
            out.extend_from_slice(&[0xD3, 0xE0]);   // shl ax, cl
        }
        BinOp::Shr => {
            // `mov cl, byte ptr [bp+disp]; sar ax, cl` (signed int >>)
            out.push(0x8A); out.push(bp_modrm(0x4E, disp)); push_bp_disp(out, disp); // mov cl, [bp+disp]
            out.extend_from_slice(&[0xD3, 0xF8]);   // sar ax, cl
        }
        op => {
            let opcode = match op {
                BinOp::Add => 0x03,
                BinOp::Sub => 0x2B,
                BinOp::BitAnd => 0x23,
                BinOp::BitOr => 0x0B,
                BinOp::BitXor => 0x33,
                _ => panic!("memory-source {op:?} not yet covered by a fixture"),
            };
            out.push(opcode);
            out.push(bp_modrm(0x46, disp));
            push_bp_disp(out, disp);
        }
    }
}
