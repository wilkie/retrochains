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
            // Assignment-as-expression: produce the RHS in AX, store it, leave
            // the value in AX (`<value→ax>; mov [target], ax`). Fixtures
            // 513/1434/2996/3395/555.
            emit_expr_to_ax(value, locals, out, fixups);
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
                out.push(0xB0);
                out.push(k as u8);
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
            emit_load_local(*i, locals, out);
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
                emit_load_param(*i, out);
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
        Expr::CallPtr { target, args } => {
            // Indirect call through a function-pointer variable; result in AX.
            crate::codegen::calls::emit_call_ptr(target, args, locals, out, fixups);
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
        Expr::Global(idx) => {
            if locals.is_char_global(*idx) {
                // `a0 00 00` mov al, moffs8 + `98` cbw — read a char
                // global with sign extension. Fixture 1092.
                let body_offset = out.len();
                out.extend_from_slice(&[0xA0, 0x00, 0x00]);
                fixups.push(Fixup {
                    body_offset,
                    kind: FixupKind::GlobalAddr { global_idx: *idx },
                });
                out.push(0x98);
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
        Expr::PostMutateGlobal { global_idx, step } => {
            // Load the OLD value into AX, then mutate the global.
            if locals.is_char_global(*global_idx) {
                let bo = out.len();
                out.extend_from_slice(&[0xA0, 0x00, 0x00]);  // mov al, [g]
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *global_idx } });
                out.push(0x98);                                // cbw
            } else {
                let bo = out.len();
                out.extend_from_slice(&[0xA1, 0x00, 0x00]);  // mov ax, [g]
                fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *global_idx } });
            }
            emit_postmutate_global(*step, *global_idx, out, fixups);
        }
        Expr::PreMutateGlobal { global_idx, step } => {
            // Mutate first, then load the NEW value into AX.
            emit_postmutate_global(*step, *global_idx, out, fixups);
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
                        out.extend_from_slice(&[0x8B, 0x5E, disp]); // mov bx,[bp-p]
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
            // Constant index: load pointer global into BX, then
            // `mov al, [bx + disp]` and `cbw`. `disp` is the byte
            // index. Fixture 4123. Phase 1 keeps it disp8.
            let k = index.fold(locals.inits).unwrap_or_else(|| {
                panic!("non-constant char-ptr index not yet supported")
            });
            let disp = i8::try_from(k).expect("ptr index fits in i8");
            let body_offset = out.len();
            out.extend_from_slice(&[0x8B, 0x1E, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset + 1,
                kind: FixupKind::GlobalAddr { global_idx: *ptr },
            });
            // disp 0 uses the no-displacement modrm `8a 07` (fixture 192).
            if disp == 0 {
                out.extend_from_slice(&[0x8A, 0x07, 0x98]);
            } else {
                out.extend_from_slice(&[0x8A, 0x47, disp as u8, 0x98]);
            }
        }
        Expr::IndexByte { array, index } => {
            // Constant index → `a0 <byte_off> 98` (the placeholder is the index;
            // the linker adds the array base). Runtime index → BX-based:
            // `mov bx,[i]; mov al,[bx+&arr]; cbw`. Fixtures 4109, 3231.
            if let Some(k) = index.fold(locals.inits) {
                let byte_off = (k as u32 & 0xFFFF) as u16;
                let body_offset = out.len();
                out.push(0xA0);
                out.extend_from_slice(&byte_off.to_le_bytes());
                fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: *array } });
                out.push(0x98);
            } else {
                emit_load_bx(index, locals, out, fixups);
                out.push(0x8A); out.push(0x87); // mov al, [bx + disp16]
                let bo = out.len();
                out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
                out.push(0x98); // cbw
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
                // Variable index → load it into BX, scale ×2 with
                // `shl bx, 1`, then `mov ax, [bx+addr]` with FIXUP.
                // Fixture 4112.
                match index.as_ref() {
                    Expr::Param(i) => {
                        let disp = param_disp(*i);
                        if locals.is_char_param(*i) {
                            // char param: byte-load + cbw + mov bx, ax
                            out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
                            out.push(0x98); // cbw
                            out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
                        } else {
                            out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp);
                        }
                    }
                    Expr::Local(i) => {
                        let disp = locals.disp(*i);
                        { out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); }
                    }
                    other => panic!(
                        "non-const, non-param/local array index not supported: {other:?}"
                    ),
                }
                out.extend_from_slice(&[0xD1, 0xE3]);
                let body_offset = out.len();
                out.extend_from_slice(&[0x8B, 0x87, 0x00, 0x00]);
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
            out.push(0x8B); out.push(bp_modrm(0x5E, p_disp)); push_bp_disp(out, p_disp);
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
            // Constant K → `mov bx, [bp+param_disp]; mov ax, [bx+2K]`.
            // Fixture 1236.
            let k = index.fold(locals.inits).unwrap_or_else(|| {
                panic!("non-constant param-index not yet supported")
            });
            let p_disp = param_disp(*param);
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
    let lsz = match left { Expr::Param(i) => locals.param_pointee_size(*i), _ => 0 };
    let rsz = match right { Expr::Param(i) => locals.param_pointee_size(*i), _ => 0 };
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
    // `x + x` (same simple variable) → load x, `shl ax,1` (fixture 1991
    // `return x+x` → `mov al,[x]; sub ah,ah; shl ax,1`), not load + add-mem.
    if matches!(op, BinOp::Add) && same_var(left, right) && !long_operand(left, locals) {
        emit_expr_to_ax(left, locals, out, fixups);
        out.extend_from_slice(&[0xD1, 0xE0]); // shl ax, 1
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
    emit_binop_inner(op, left, right, locals, out, fixups);
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
        emit_cond_skip(&cond, 5, locals, out, fixups);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0xEB, 0x02]);  // mov ax,1; jmp +2
        out.extend_from_slice(&[0x2B, 0xC0]);                      // sub ax,ax
        return;
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
    // Two char global-array elements (`a[i] OP b[j]`, byte loads): MSC loads
    // the RHS to AX via `mov al,..; cbw`, parks it in CX, loads the LHS the same
    // way, then `op ax, cx`. (The generic indexed-global path below would wrongly
    // emit word loads for byte elements.) Fixture 2431.
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
        && matches!(left, Expr::IndexByte { .. })
        && matches!(right, Expr::IndexByte { .. })
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
    // Commutative swap: when RIGHT is a char param and LEFT is a non-char
    // bp-rel operand, MSC loads the char first (byte+cbw) and uses the
    // other operand as a word mem source. Fixtures 616, 3685.
    if commutative
        && let Expr::Param(ri) = right
        && locals.is_char_param(*ri)
        && !matches!(left, Expr::Param(li) if locals.is_char_param(*li))
        && let Some(l_disp) = bp_disp(left, locals)
    {
        let r_disp = param_disp(*ri) as u8;
        out.extend_from_slice(&[0x8A, 0x46, r_disp]);  // mov al, [bp+r_disp]
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
        out.push(0xB1); // mov cl, K
        out.push((*k as u32 & 0xFF) as u8);
        out.push(0x8A); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
        out.push(0x98); // cbw
        out.push(0xF6); // idiv cl
        out.push(0xF9);
        return;
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
    // Left as a BP-rel operand we can load into AX.
    if let Some(load) = bp_load(left, locals) {
        load(out);
        // Right as IntLit → imm form.
        if let Expr::IntLit(k) = right {
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
    if let Expr::Global(idx) = left {
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
        let opcode = match op {
            BinOp::Add => 0x03,
            BinOp::Sub => 0x2B,
            _ => panic!("{op:?} with indexed-global rhs not yet supported"),
        };
        out.push(opcode);
        out.push(0x06);
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
        Expr::Local(i) => Some(Box::new(move |out: &mut Vec<u8>| emit_load_local(*i, locals, out))),
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
                emit_load_param(*i, out);
            }
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
            // `imul ax, ax, imm8sx` (`6b c0 imm8`) or `imul ax, ax, imm16` (`69 c0 imm16`).
            // Generic multiplication by any constant.
            if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x6B, 0xC0, k8 as u8]);
            } else {
                out.push(0x69);
                out.push(0xC0);
                out.extend_from_slice(&k16.to_le_bytes());
            }
        }
        // Bitwise imm-AX: prefer 3-byte `83 /digit imm8sx` for small K,
        // 3-byte `op_byte imm16` (form `25/0d/35`) otherwise.
        (BinOp::BitAnd, _) => {
            if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x83, 0xE0, k8 as u8]);
            } else {
                out.push(0x25);
                out.extend_from_slice(&k16.to_le_bytes());
            }
        }
        (BinOp::BitOr, _) => {
            if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x83, 0xC8, k8 as u8]);
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
            if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x83, 0xF0, k8 as u8]);
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
