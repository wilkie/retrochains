use crate::*;

/// `<local> = <expr>;`. Phase 1 supports the peephole
/// `<local> = <same-local> ± 1;` → `inc/dec word ptr [bp-disp]`
/// (fixture 4096: `x = x - 1;` in a while body). The general path
/// — `mov ax, <expr>; mov [bp-disp], ax` — is reserved for a
/// future fixture that exercises a non-peephole shape.
pub(crate) fn emit_assign(target: AssignTarget, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
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
        AssignTarget::IndexedGlobal { array, byte_off } => {
            return emit_assign_indexed_global(array, byte_off, value, locals, out, fixups);
        }
        AssignTarget::IndexedGlobalByte { array, byte_off } => {
            return emit_assign_indexed_global_byte(array, byte_off, value, locals, out, fixups);
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
        AssignTarget::DerefLocalField { ptr_local, byte_off, size } => {
            return emit_assign_deref_local_field(ptr_local, byte_off, size, value, locals, out, fixups);
        }
        AssignTarget::GlobalField { global, byte_off, size } => {
            return emit_assign_global_field(global, byte_off, size, value, locals, out, fixups);
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
        AssignTarget::DerefPostMutateLocal { local_idx, step } => {
            return emit_assign_deref_postmutate_local(local_idx, step, value, locals, out, fixups);
        }
    };
    let disp = locals.disp(local_idx);
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
            (BinOp::Shr, k) if k > 0 && k < 16 => (Some(0x7Eu8), k as u8),
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
                } else {
                    // AND with 0x00xx or other: word form to correctly affect high byte.
                    out.push(0x81); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp);
                    out.extend_from_slice(&imm16.to_le_bytes());
                }
            }
            BinOp::BitOr | BinOp::BitXor => {
                if imm16 <= 0xFF {
                    // Small non-negative: byte form (high byte OR/XOR 0x00 = identity).
                    out.push(0x80); out.push(bp_modrm(modrm, disp)); push_bp_disp(out, disp); out.push(imm8);
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
            // Long local: two word stores — low half at disp, high half at disp+2.
            let low = (k as u32 & 0xFFFF) as u16;
            let high = (((k as i32) >> 16) as u32 & 0xFFFF) as u16;
            out.push(0xC7); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp);
            out.extend_from_slice(&low.to_le_bytes());
            let hi_disp = disp + 2;
            out.push(0xC7); out.push(bp_modrm(0x46, hi_disp)); push_bp_disp(out, hi_disp);
            out.extend_from_slice(&high.to_le_bytes());
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
/// `mov bx, [bp+pdisp]; mov [bx], imm/ax`. We don't track param
/// pointee size yet; emit word store by default, which is correct
/// for `int *` and the low byte of `char *` (the high byte is
/// untouched but ignored). For `char *p; *p = K;` the right shape
/// would be `c6 07 imm8` — let's match that.
pub(crate) fn emit_assign_deref_param(param_idx: usize, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let pdisp = param_disp(param_idx);
    out.push(0x8B);
    out.push(0x5E);
    out.push(pdisp as u8);
    if let Some(k) = value.fold(locals.inits) {
        // `c7 07 imm16` mov word [bx], imm. We don't know if dest is
        // byte/word — default to word; fixture 1225 has char and expects
        // byte store. Pick byte when fits in i8 to favor the common
        // small-K char case.
        if let Ok(k8) = i8::try_from(k) {
            out.extend_from_slice(&[0xC6, 0x07, k8 as u8]);
        } else {
            out.push(0xC7);
            out.push(0x07);
            let imm = (k as u32 & 0xFFFF) as u16;
            out.extend_from_slice(&imm.to_le_bytes());
        }
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        // `mov [bx], ax`.
        out.extend_from_slice(&[0x89, 0x07]);
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
        let helper = match (op, locals.is_unsigned_global(global_idx)) {
            (BinOp::Mul, _) => "__aNNalmul",     // multiply is sign-agnostic
            (BinOp::Div, false) => "__aNNaldiv",
            (BinOp::Div, true) => "__aNNauldiv",
            (BinOp::Mod, false) => "__aNNalrem",
            (BinOp::Mod, true) => "__aNNaurem",
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
/// `<global>[K] = <expr>;` — write at a constant array index. The
/// placeholder address is `byte_off`, which the linker adds to the
/// global's base. Constant RHS → `c7 06 byte_off imm16`; general RHS
/// → `<expr-to-ax>; a3 byte_off`. Fixture 4119.
pub(crate) fn emit_assign_indexed_global(global_idx: usize, byte_off: u16, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
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
    emit_index_to_si(index, locals, out);
    out.extend_from_slice(&[0xD1, 0xE6]); // shl si, 1
    emit_expr_to_ax(value, locals, out, fixups);
    let base_disp = locals.disp(local_idx);
    out.push(0x89); out.push(bp_modrm(0x42, base_disp)); push_bp_disp(out, base_disp); // mov [bp+si+base], ax
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
pub(crate) fn emit_assign_global_field(global_idx: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
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
pub(crate) fn emit_assign_local_field(local_idx: usize, byte_off: u16, size: u8, value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
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
    { out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); }    // mov bx, [bp-p]
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
    // Near pointer: `mov bx, [bp-disp]`.
    out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp);
    if let Some(k) = value.fold(locals.inits) {
        let imm = (k as u32 & 0xFFFF) as u16;
        out.extend_from_slice(&[0xC7, 0x07]);
        out.extend_from_slice(&imm.to_le_bytes());
    } else {
        emit_expr_to_ax(value, locals, out, fixups);
        out.extend_from_slice(&[0x89, 0x07]);
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
