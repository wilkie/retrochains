use crate::*;

/// `<name>(args)` — cdecl call. Args are evaluated in source order
/// but PUSHed right-to-left, then the call lands, then the caller
/// cleans up with `add sp, N`. Fixtures 4100, 4101, 4102.
///
/// 8086 has no `push imm16` opcode (added in 286+), so a constant
/// arg becomes `mov ax, K; push ax` (4 bytes). Local/param args go
/// through `push word ptr [bp+disp]` (3 bytes).
pub(crate) fn emit_call(
    name: &str,
    args: &[Expr],
    locals: &Locals<'_>,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    emit_call_inner(name, args, locals, false, out, fixups);
}
pub(crate) fn emit_call_inner(
    name: &str,
    args: &[Expr],
    locals: &Locals<'_>,
    skip_cleanup: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    let sym = symbol_name(name);
    let param_longs = locals.long_param_funcs.get(&sym);
    // Push args right-to-left (cdecl).
    for (i, arg) in args.iter().enumerate().rev() {
        let is_long_param = param_longs.map(|v| v.get(i).copied().unwrap_or(false)).unwrap_or(false);
        if let Expr::FloatLit(bits, is_double) = arg {
            emit_push_arg_float(*bits, if *is_double { 8 } else { 4 }, out, fixups);
        } else if is_long_param {
            emit_push_arg_long(arg, locals, out, fixups);
        } else {
            emit_push_arg(arg, locals, out, fixups);
        }
    }
    let body_offset = out.len();
    out.extend_from_slice(&[0xE8, 0x00, 0x00]);
    // Both TU-local and external calls record their target name.
    // The classification (intra-segment patch vs OMF FIXUPP record)
    // happens in build_obj once the defined-function set is known.
    fixups.push(Fixup {
        body_offset,
        kind: FixupKind::TuLocalCall { target: sym.clone() },
    });
    let cleanup_bytes: usize = args.iter().enumerate().map(|(i, a)| {
        if let Expr::FloatLit(_, is_double) = a { if *is_double { 8 } else { 4 } }
        else if param_longs.map(|v| v.get(i).copied().unwrap_or(false)).unwrap_or(false) { 4 } else { 2 }
    }).sum();
    if cleanup_bytes > 0 && !skip_cleanup {
        // `add sp, imm8sx` — Grp1 r/m16,imm8sx with /0=ADD,
        // ModR/M mod=11 r/m=100 (SP). 3 bytes for small N.
        out.push(0x83);
        out.push(0xC4);
        out.push(u8::try_from(cleanup_bytes).expect("cleanup fits in u8"));
    }
    // Char-returning callee leaves the byte in AL; widen to int via
    // cbw before the caller treats the value as an int. Fixture 1006.
    if locals.char_returners.contains(&sym) {
        out.push(0x98);
    }
}
/// Indirect call through a function-pointer variable: `call WORD PTR <mem>`
/// (FF /2). Args are pushed right-to-left then cleaned up with `add sp, N`,
/// exactly like a direct cdecl call — only the call instruction differs.
/// `target` is the fnptr lvalue (`Global`/`Param`/`Local`). Fixtures 2818,
/// 2750, 3323.
pub(crate) fn emit_call_ptr(
    target: &Expr,
    args: &[Expr],
    locals: &Locals<'_>,
    skip_cleanup: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    use crate::codegen::func::{bp_modrm, push_bp_disp, param_disp};
    // Push args right-to-left (cdecl).
    for arg in args.iter().rev() {
        if let Expr::FloatLit(bits, is_double) = arg {
            emit_push_arg_float(*bits, if *is_double { 8 } else { 4 }, out, fixups);
        } else {
            emit_push_arg(arg, locals, out, fixups);
        }
    }
    // `call WORD PTR <mem>` — FF /2.
    match target {
        Expr::Global(idx) => {
            // `ff 16 off16` — call through an absolute [moffs]. The offset takes
            // a GlobalAddr fixup (modrm form → body_offset = disp16 pos - 1).
            let body_offset = out.len();
            out.extend_from_slice(&[0xFF, 0x16, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset: body_offset + 1,
                kind: FixupKind::GlobalAddr { global_idx: *idx },
            });
        }
        Expr::Param(i) => {
            let disp = param_disp(*i);
            out.push(0xFF); out.push(bp_modrm(0x56, disp)); push_bp_disp(out, disp);
        }
        Expr::Local(i) => {
            let disp = locals.disp(*i);
            out.push(0xFF); out.push(bp_modrm(0x56, disp)); push_bp_disp(out, disp);
        }
        other => panic!("indirect call through unsupported target {other:?}"),
    }
    // cdecl caller cleanup: `add sp, N`.
    let cleanup_bytes: usize = args.iter().map(|a| {
        if let Expr::FloatLit(_, is_double) = a { if *is_double { 8 } else { 4 } } else { 2 }
    }).sum();
    if cleanup_bytes > 0 && !skip_cleanup {
        out.push(0x83);
        out.push(0xC4);
        out.push(u8::try_from(cleanup_bytes).expect("cleanup fits in u8"));
    }
}
/// Push a long (32-bit) call argument: sign-extend to DX:AX, push DX
/// (high word) then AX (low word). For constants that fit in i16, uses
/// `cwd` for the sign extension; otherwise uses explicit `mov dx, K_hi`.
pub(crate) fn emit_push_arg_long(arg: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if let Some(k) = arg.fold(locals.inits) {
        let lo = (k as u32 & 0xFFFF) as u16;
        out.push(0xB8);
        out.extend_from_slice(&lo.to_le_bytes()); // mov ax, k_lo
        if i16::try_from(k).is_ok() {
            out.push(0x99); // cwd: sign-extend AX into DX
        } else {
            let hi = ((k as u32) >> 16) as u16;
            out.push(0xBA);
            out.extend_from_slice(&hi.to_le_bytes()); // mov dx, k_hi
        }
    } else {
        // Non-constant: load low word into AX, then use cwd or load DX separately.
        emit_expr_to_ax(arg, locals, out, fixups);
        out.push(0x99); // cwd: assume sign extension is appropriate
    }
    out.push(0x52); // push dx (high word)
    out.push(0x50); // push ax (low word)
}
/// Push a float/double literal argument. MSC loads the CONST `$T` temp onto
/// the FPU stack, carves `width` bytes off SP, and stores the value there:
///   9B <D9|DD> 06 <off16>   fld <dword|qword> [$T]   (FIDRQQ + FloatLoad)
///   83 EC <width>           sub sp, width
///   8B DC                   mov bx, sp
///   9B <D9|DD> 1F           fstp <dword|qword> [bx]   (FIDRQQ)
///   [90]                    nop so the next statement is even-aligned
///   9B                      fwait                     (FIWRQQ on the leading byte)
/// SP stays low after the call; the caller's WithSlide epilogue (`mov sp,bp`)
/// reclaims it — there is no `add sp` cleanup.
pub(crate) fn emit_push_arg_float(bits: u64, width: usize, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let op = if width == 4 { 0xD9u8 } else { 0xDDu8 };
    // fld <width> [$T]
    fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
    out.push(0x9B);
    out.push(op);
    out.push(0x06);
    let body_offset = out.len() - 1; // the 06 modrm; off16 at +1
    out.extend_from_slice(&[0x00, 0x00]);
    fixups.push(Fixup { body_offset, kind: FixupKind::FloatLoad { bits, width } });
    // sub sp, width
    out.extend_from_slice(&[0x83, 0xEC, width as u8]);
    // mov bx, sp
    out.extend_from_slice(&[0x8B, 0xDC]);
    // fstp <width> [bx]  (modrm 1F = mod00 /3 r/m=111)
    fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
    out.push(0x9B);
    out.push(op);
    out.push(0x1F);
    // nop (parity) + fwait, marker on the leading byte.
    fixups.push(Fixup { body_offset: out.len(), kind: FixupKind::FloatMarker { target: "FIWRQQ" } });
    if out.len() % 2 == 0 {
        out.push(0x90); // nop
    }
    out.push(0x9B); // fwait
}
/// Push one call argument onto the stack. For Phase 1: constants
/// via `mov ax, K; push ax`; locals/params via direct memory push;
/// string literals via `mov ax, <pool offset>; push ax` with a
/// FIXUP for the linker to fill in the actual offset.
pub(crate) fn emit_push_arg(arg: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match arg {
        Expr::IntLit(k) => {
            let imm = (*k as u32 & 0xFFFF) as u16;
            out.push(0xB8);
            out.extend_from_slice(&imm.to_le_bytes());
            out.push(0x50); // push ax
        }
        Expr::Local(idx) => {
            // `push word ptr [bp-disp]` — `FF /6 r/m`.
            let disp = locals.disp(*idx);
            out.push(0xFF);
            out.push(bp_modrm(0x76, disp));
            push_bp_disp(out, disp);
        }
        Expr::Param(idx) => {
            let disp = i8::try_from(4 + (idx * 2)).expect("param disp fits");
            out.push(0xFF);
            out.push(0x76);
            out.push(disp as u8);
        }
        Expr::StrLit(string_idx) => {
            // `mov ax, 00 00` placeholder; FIXUPP makes the linker
            // write the CONST-segment offset (relative to DGROUP).
            // Fixture 4103.
            let body_offset = out.len();
            out.extend_from_slice(&[0xB8, 0x00, 0x00]);
            out.push(0x50); // push ax
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::StrLoad { string_idx: *string_idx },
            });
        }
        Expr::AddrOfGlobal(idx) => {
            // `mov ax, 00 00` placeholder; FIXUP carries the global's
            // address back into the imm16 at link time. Fixture 4125.
            let body_offset = out.len();
            out.extend_from_slice(&[0xB8, 0x00, 0x00]);
            out.push(0x50); // push ax
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *idx },
            });
        }
        Expr::AddrOfLocal(idx) => {
            // `lea ax, [bp-disp]; push ax`. Fixture 1225.
            let disp = locals.disp(*idx);
            out.push(0x8D);
            out.push(bp_modrm(0x46, disp));
            push_bp_disp(out, disp);
            out.push(0x50);
        }
        Expr::Global(idx) => {
            // `ff 36 <addr>` — push word ptr [imm16]. The placeholder
            // gets the global's _DATA offset; the FIXUP carries the
            // base. Fixture 4131.
            out.push(0xFF);
            let body_offset = out.len();
            out.extend_from_slice(&[0x36, 0x00, 0x00]);
            fixups.push(Fixup {
                body_offset,
                kind: FixupKind::GlobalAddr { global_idx: *idx },
            });
        }
        Expr::PostMutateLocal { local_idx, step } => {
            // Push the OLD value of the local, then mutate it in place.
            // `push word ptr [bp-a]` + inc/dec/add/sub.
            let disp = locals.disp(*local_idx);
            out.push(0xFF);
            out.push(bp_modrm(0x76, disp));
            push_bp_disp(out, disp);
            emit_postmutate_local(*step, locals.size(*local_idx), disp, out);
        }
        Expr::PreMutateLocal { local_idx, step } => {
            // Mutate first, then push the NEW value.
            let disp = locals.disp(*local_idx);
            emit_postmutate_local(*step, locals.size(*local_idx), disp, out);
            out.push(0xFF);
            out.push(bp_modrm(0x76, disp));
            push_bp_disp(out, disp);
        }
        // Word global-array element `a[K]` (incl. `p[K]` via alias) as an
        // argument: `push word ptr [_a+off]` (ff 36) with byte_off as the
        // GlobalAddr addend. Fixture 893.
        Expr::Index { array, index } if matches!(index.as_ref(), Expr::IntLit(_)) => {
            let Expr::IntLit(k) = index.as_ref() else { unreachable!() };
            let byte_off = (*k as u16).wrapping_mul(2);
            out.push(0xFF);
            let body_offset = out.len();
            out.push(0x36);
            out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: *array } });
        }
        Expr::BinOp { .. } | Expr::Call { .. } | Expr::Ternary { .. }
            | Expr::DerefWord { .. } | Expr::DerefByte { .. }
            | Expr::GlobalField { .. } | Expr::LocalField { .. }
            | Expr::DerefLocalField { .. } | Expr::DerefParamField { .. }
            | Expr::Index { .. } | Expr::IndexByte { .. }
            | Expr::LocalIndex { .. } | Expr::LocalIndexByte { .. }
            | Expr::ParamIndex { .. } | Expr::PtrIndexByte { .. }
            | Expr::PostMutateGlobal { .. } | Expr::PreMutateGlobal { .. } | Expr::Seq { .. } => {
            // Computed value: build the result in AX then push.
            // Fixture 4144 (BinOp), 1270 (Call).
            emit_expr_to_ax(arg, locals, out, fixups);
            out.push(0x50);
        }
        other => panic!("argument shape not yet supported: {other:?}"),
    }
}
/// Load a long (32-bit) expression into DX:AX. Used for long global
/// assignments and potentially other long-value sinks. Handles the
/// expression shapes that can appear as a long RHS:
/// - Call result: DX:AX already set by callee, just emit the call
/// - Long local: mov ax, [bp+lo]; mov dx, [bp+hi]
/// - Long param: mov ax, [bp+lo]; mov dx, [bp+hi]
/// - Long global: mov ax, [g]; mov dx, [g+2]
/// - Otherwise: emit_expr_to_ax + cwd (sign-extend int to long)
/// Is `e` a long-typed scalar (param/local/global) loadable into DX:AX?
pub(crate) fn long_operand(e: &Expr, locals: &Locals<'_>) -> bool {
    match e {
        Expr::Param(i) => locals.is_long_param(*i),
        Expr::Local(i) => locals.is_long_local(*i),
        Expr::Global(j) => locals.is_long_global(*j),
        // Long struct field / long array element (emit_long_to_dx_ax loads both
        // words; long_global_rhs_addr supplies the address as an RHS).
        Expr::GlobalField { size: 4, .. } => true,
        Expr::Index { array, .. } => locals.is_long_global(*array),
        _ => false,
    }
}

/// Whether a long operand is unsigned (selects SHR vs SAR for `>>`).
pub(crate) fn long_operand_unsigned(e: &Expr, locals: &Locals<'_>) -> bool {
    match e {
        Expr::Param(i) => locals.is_unsigned_param(*i),
        Expr::Local(i) => locals.is_unsigned_local(*i),
        Expr::Global(j) => locals.is_unsigned_global(*j),
        _ => false,
    }
}

/// BP-relative displacement of param `idx`'s low word, accounting for the
/// 4-byte width of any preceding long params (cdecl: first param at [bp+4],
/// args pushed right-to-left so earlier params sit at lower offsets). The
/// plain `param_disp` assumes 2 bytes per param, which is wrong once a long
/// precedes the one being addressed.
pub(crate) fn long_param_disp(idx: usize, locals: &Locals<'_>) -> i16 {
    let mut d = 4i16;
    for k in 0..idx {
        d += if locals.is_long_param(k) { 4 } else { 2 };
    }
    d
}

/// A long RHS that lives in memory we can fold into an `op ax,[..]; op dx,[..]`
/// pair (bp-relative param/local). Globals are handled elsewhere later.
fn long_rhs_mem(e: &Expr, locals: &Locals<'_>) -> bool {
    matches!(e, Expr::Param(i) if locals.is_long_param(*i))
        || matches!(e, Expr::Local(i) if locals.is_long_local(*i))
        || long_global_rhs_addr(e, locals).is_some()
}
/// For a long RHS living at a fixed global address (long global, long struct
/// field, or long array element with a constant index), return its (global index,
/// low-word in-segment offset). The high word is at offset+2.
fn long_global_rhs_addr(e: &Expr, locals: &Locals<'_>) -> Option<(usize, u16)> {
    match e {
        Expr::Global(g) if locals.is_long_global(*g) => Some((*g, 0)),
        Expr::GlobalField { global, byte_off, size: 4 } => Some((*global, *byte_off)),
        Expr::Index { array, index } if locals.is_long_global(*array) => {
            let k = index.fold(locals.inits)?;
            Some((*array, (k as u32 & 0xFFFF) as u16 * 4))
        }
        _ => None,
    }
}

const fn long_arith_opcodes(op: BinOp) -> Option<(u8, u8)> {
    // (low-word op, high-word op): high word carries for add/sub.
    match op {
        BinOp::Add => Some((0x03, 0x13)),    // add ax,..; adc dx,..
        BinOp::Sub => Some((0x2B, 0x1B)),    // sub ax,..; sbb dx,..
        BinOp::BitOr => Some((0x0B, 0x0B)),  // or  ax,..; or  dx,..
        BinOp::BitAnd => Some((0x23, 0x23)), // and ax,..; and dx,..
        BinOp::BitXor => Some((0x33, 0x33)), // xor ax,..; xor dx,..
        _ => None,
    }
}

/// `e` is `<long> <arith> <long-in-memory>` — an inline 2-word arithmetic op.
pub(crate) fn is_long_arith_mem(e: &Expr, locals: &Locals<'_>) -> bool {
    matches!(e, Expr::BinOp { op, left, right }
        if long_arith_opcodes(*op).is_some()
            && long_operand(left, locals)
            && long_rhs_mem(right, locals))
}

/// The runtime helper for an expression-context long mul/div/mod (operands
/// pushed on the stack, result returned in DX:AX). `None` for other ops.
fn long_muldiv_helper(op: BinOp, unsigned: bool) -> Option<&'static str> {
    Some(match (op, unsigned) {
        (BinOp::Mul, false) => "__aNlmul",
        (BinOp::Mul, true) => "__aNulmul",
        (BinOp::Div, false) => "__aNldiv",
        (BinOp::Div, true) => "__aNuldiv",
        (BinOp::Mod, false) => "__aNlrem",
        (BinOp::Mod, true) => "__aNulrem",
        _ => return None,
    })
}

/// `e` is `<long> * / % <long>` — an expression-context helper-call op.
/// Power-of-two multiplies are excluded (they strength-reduce to a shift).
pub(crate) fn is_long_muldiv(e: &Expr, locals: &Locals<'_>) -> bool {
    matches!(e, Expr::BinOp { op, left, right }
        if long_muldiv_helper(*op, false).is_some()
            && long_operand(left, locals)
            && long_operand(right, locals)
            && long_shl_amount(*op, right, locals).is_none())
}

/// Shift-left amount for a long `<<` / `* 2^n`: `v << k` (1..=31) or
/// `v * 2^n` both lower to a left shift by that many bits.
fn long_shl_amount(op: BinOp, right: &Expr, locals: &Locals<'_>) -> Option<u8> {
    let k = right.fold(locals.inits)?;
    match op {
        BinOp::Shl if (1..=31).contains(&k) => Some(k as u8),
        BinOp::Mul if k >= 2 && (k & (k - 1)) == 0 => Some(k.trailing_zeros() as u8),
        _ => None,
    }
}

/// `e` is a long left shift (`v << k`) or power-of-two multiply (`v * 2^n`).
pub(crate) fn is_long_shl(e: &Expr, locals: &Locals<'_>) -> bool {
    matches!(e, Expr::BinOp { op, left, right }
        if long_operand(left, locals) && long_shl_amount(*op, right, locals).is_some())
}

/// `e` is a long-valued expression that emit_long_to_dx_ax can load directly:
/// a long struct field, a long array element (const index), or `<long> ± <const>`.
pub(crate) fn is_long_field_elem_or_const_arith(e: &Expr, locals: &Locals<'_>) -> bool {
    match e {
        Expr::GlobalField { size: 4, .. } => true,
        Expr::Index { array, index } => locals.is_long_global(*array) && index.fold(locals.inits).is_some(),
        Expr::BinOp { op: BinOp::Add | BinOp::Sub, left, right } =>
            long_operand(left, locals) && !long_operand(right, locals) && right.fold(locals.inits).is_some(),
        _ => false,
    }
}
/// `e` is a long right shift by one (`v >> 1`).
pub(crate) fn is_long_shr1(e: &Expr, locals: &Locals<'_>) -> bool {
    matches!(e, Expr::BinOp { op: BinOp::Shr, left, right }
        if right.fold(locals.inits) == Some(1) && long_operand(left, locals))
}

/// `e` is a long negate `-x` (parsed as `0 - x`).
pub(crate) fn is_long_neg(e: &Expr, locals: &Locals<'_>) -> bool {
    matches!(e, Expr::BinOp { op: BinOp::Sub, left, right }
        if matches!(left.as_ref(), Expr::IntLit(0)) && long_operand(right, locals))
}

/// `e` is a long bitwise-not `~x` (parsed as `x ^ -1`).
pub(crate) fn is_long_not(e: &Expr, locals: &Locals<'_>) -> bool {
    matches!(e, Expr::BinOp { op: BinOp::BitXor, left, right }
        if matches!(right.as_ref(), Expr::IntLit(-1)) && long_operand(left, locals))
}

/// Emit a long shift-left-by-k on DX:AX. k==1 is a single shl/rcl; k>=2 uses
/// MSC's cl-counted loop: `mov cl,k; shl ax,1; rcl dx,1; dec cl; jnz -8`.
fn emit_long_shl_k(k: u8, out: &mut Vec<u8>) {
    if k == 1 {
        out.extend_from_slice(&[0xD1, 0xE0, 0xD1, 0xD2]);
    } else {
        out.extend_from_slice(&[0xB1, k]); // mov cl, k
        out.extend_from_slice(&[0xD1, 0xE0, 0xD1, 0xD2, 0xFE, 0xC9, 0x75, 0xF8]);
    }
}

/// Push a long operand's two words (high then low) from memory, as MSC does
/// for the helper-call calling convention.
fn push_long_operand(e: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let mut push_global_word = |word_off: i16, j: usize, out: &mut Vec<u8>| {
        out.push(0xFF);
        out.push(0x36); // push word [disp16]
        let modrm_pos = out.len() - 1;
        out.extend_from_slice(&(word_off as u16).to_le_bytes());
        fixups.push(Fixup { body_offset: modrm_pos, kind: FixupKind::GlobalAddr { global_idx: j } });
    };
    let mut push_bp_word = |disp: i16, out: &mut Vec<u8>| {
        out.push(0xFF);
        out.push(bp_modrm(0x76, disp)); // push word [bp+disp]
        push_bp_disp(out, disp);
    };
    match e {
        Expr::Global(j) => { push_global_word(2, *j, out); push_global_word(0, *j, out); }
        Expr::Param(i) => { let d = long_param_disp(*i, locals); push_bp_word(d + 2, out); push_bp_word(d, out); }
        Expr::Local(i) => { let d = locals.disp(*i); push_bp_word(d + 2, out); push_bp_word(d, out); }
        _ => unreachable!("long_operand gates these forms"),
    }
}

/// Emit `op ax,[rhs_lo]; op2 dx,[rhs_hi]` for a bp-relative long RHS, combining
/// it into DX:AX (which already holds the left operand).
fn emit_long_op_mem(op: BinOp, rhs: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let (lo_op, hi_op) = long_arith_opcodes(op).expect("long arith opcode");
    // Global-address RHS (long global / struct field / array element): disp16 form
    // `<op> ax,[g+lo]; <op> dx,[g+hi]` with GlobalAddr fixups.
    if let Some((gidx, lo)) = long_global_rhs_addr(rhs, locals) {
        out.push(lo_op); out.push(0x06); // reg = ax, [disp16]
        let p = out.len(); out.extend_from_slice(&lo.to_le_bytes());
        fixups.push(Fixup { body_offset: p - 1, kind: FixupKind::GlobalAddr { global_idx: gidx } });
        out.push(hi_op); out.push(0x16); // reg = dx, [disp16]
        let p = out.len(); out.extend_from_slice(&(lo + 2).to_le_bytes());
        fixups.push(Fixup { body_offset: p - 1, kind: FixupKind::GlobalAddr { global_idx: gidx } });
        return;
    }
    let disp = match rhs {
        Expr::Param(i) => long_param_disp(*i, locals),
        Expr::Local(i) => locals.disp(*i),
        _ => unreachable!("long_rhs_mem gates this"),
    };
    out.push(lo_op);
    out.push(bp_modrm(0x46, disp)); // reg = ax
    push_bp_disp(out, disp);
    out.push(hi_op);
    out.push(bp_modrm(0x56, disp + 2)); // reg = dx
    push_bp_disp(out, disp + 2);
}

pub(crate) fn emit_long_to_dx_ax(value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match value {
        // Expression-context long mul/div/mod: push both operands as longs
        // (RHS then LHS, each high-then-low), call the runtime helper, result
        // comes back in DX:AX. Fixtures 231-233, 3173, 3181, ...
        Expr::BinOp { op, left, right }
            if long_muldiv_helper(*op, false).is_some()
                && long_operand(left, locals)
                && long_operand(right, locals)
                && long_shl_amount(*op, right, locals).is_none() =>
        {
            let unsigned = long_operand_unsigned(left, locals)
                || long_operand_unsigned(right, locals);
            let helper = long_muldiv_helper(*op, unsigned).expect("muldiv helper");
            push_long_operand(right, locals, out, fixups);
            push_long_operand(left, locals, out, fixups);
            let call = out.len();
            out.extend_from_slice(&[0xE8, 0x00, 0x00]); // call helper
            fixups.push(Fixup { body_offset: call, kind: FixupKind::ExtCall { target: helper.to_owned() } });
        }
        // Long left shift `v << k` or power-of-two multiply `v * 2^n`: load
        // DX:AX, then shift left by that many bits. k==1 is a single
        // shl/rcl; k>=2 uses MSC's cl-counted loop. Fixtures 2878, 377,
        // 3170 (*2), 3175 (*4), 3177 (*8).
        Expr::BinOp { op, left, right }
            if long_operand(left, locals)
                && long_shl_amount(*op, right, locals).is_some() =>
        {
            let n = long_shl_amount(*op, right, locals).expect("shl amount");
            emit_long_to_dx_ax(left, locals, out, fixups);
            emit_long_shl_k(n, out);
        }
        // Long right shift by one `v >> 1`: high word first so the carry
        // threads into the low word; SAR vs SHR by signedness. Fixtures
        // 2885, 2886, 3300, 378.
        Expr::BinOp { op: BinOp::Shr, left, right }
            if right.fold(locals.inits) == Some(1) && long_operand(left, locals) =>
        {
            emit_long_to_dx_ax(left, locals, out, fixups);
            if long_operand_unsigned(left, locals) {
                out.extend_from_slice(&[0xD1, 0xEA, 0xD1, 0xD8]); // shr dx,1; rcr ax,1
            } else {
                out.extend_from_slice(&[0xD1, 0xFA, 0xD1, 0xD8]); // sar dx,1; rcr ax,1
            }
        }
        // Long negate `-x` (parsed as `0 - x`): neg ax; adc dx,0; neg dx.
        // Fixtures 2673, 2877.
        Expr::BinOp { op: BinOp::Sub, left, right }
            if matches!(left.as_ref(), Expr::IntLit(0)) && long_operand(right, locals) =>
        {
            emit_long_to_dx_ax(right, locals, out, fixups);
            out.extend_from_slice(&[0xF7, 0xD8, 0x83, 0xD2, 0x00, 0xF7, 0xDA]);
        }
        // Long bitwise-not `~x` (parsed as `x ^ -1`): not ax; not dx.
        // Fixtures 3290, 372.
        Expr::BinOp { op: BinOp::BitXor, left, right }
            if matches!(right.as_ref(), Expr::IntLit(-1)) && long_operand(left, locals) =>
        {
            emit_long_to_dx_ax(left, locals, out, fixups);
            out.extend_from_slice(&[0xF7, 0xD0, 0xF7, 0xD2]);
        }
        // Inline 2-word arithmetic: `a <op> b` where both are long. Load a
        // into DX:AX, then combine b (in memory) word-wise: add/adc, sub/sbb,
        // or/or, and/and, xor/xor. Fixtures 2870/2872/2873/285/...
        Expr::BinOp { op, left, right }
            if long_arith_opcodes(*op).is_some()
                && long_operand(left, locals)
                && long_rhs_mem(right, locals) =>
        {
            emit_long_to_dx_ax(left, locals, out, fixups);
            emit_long_op_mem(*op, right, locals, out, fixups);
        }
        Expr::Call { name, args } => {
            emit_call(name, args, locals, out, fixups);
            // DX:AX already set by callee returning long
        }
        Expr::Local(i) if locals.is_long_local(*i) => {
            let disp = locals.disp(*i);
            out.extend_from_slice(&[0x8B, 0x46, disp as u8]);     // mov ax, [bp+lo]
            out.extend_from_slice(&[0x8B, 0x56, (disp+2) as u8]); // mov dx, [bp+hi]
        }
        Expr::Param(i) if locals.is_long_param(*i) => {
            let lo = long_param_disp(*i, locals) as u8;
            let hi = (long_param_disp(*i, locals) + 2) as u8;
            out.extend_from_slice(&[0x8B, 0x46, lo]); // mov ax, [bp+lo]
            out.extend_from_slice(&[0x8B, 0x56, hi]); // mov dx, [bp+hi]
        }
        Expr::Global(j) if locals.is_long_global(*j) => {
            let body_offset = out.len();
            out.extend_from_slice(&[0xA1, 0x00, 0x00]); // mov ax, [g]
            fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: *j } });
            let body_offset = out.len() + 1;
            out.extend_from_slice(&[0x8B, 0x16, 0x02, 0x00]); // mov dx, [g+2]
            fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: *j } });
        }
        // Long struct field `s.f` / long array element `a[K]` (constant K): load the
        // low word at the byte offset and the high word at +2. Fixtures 363, 364.
        Expr::GlobalField { global, byte_off, size: 4 } => {
            let lo = *byte_off;
            let bo = out.len(); out.push(0xA1); out.extend_from_slice(&lo.to_le_bytes()); // mov ax, [g+lo]
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *global } });
            let bo = out.len() + 1; out.extend_from_slice(&[0x8B, 0x16]); out.extend_from_slice(&(lo + 2).to_le_bytes()); // mov dx, [g+lo+2]
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *global } });
        }
        Expr::Index { array, index }
            if locals.is_long_global(*array) && index.fold(locals.inits).is_some() =>
        {
            let k = index.fold(locals.inits).unwrap();
            let lo = (k as u32 & 0xFFFF) as u16 * 4;
            let bo = out.len(); out.push(0xA1); out.extend_from_slice(&lo.to_le_bytes()); // mov ax, [a+lo]
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *array } });
            let bo = out.len() + 1; out.extend_from_slice(&[0x8B, 0x16]); out.extend_from_slice(&(lo + 2).to_le_bytes()); // mov dx, [a+lo+2]
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: *array } });
        }
        // Long `<long> ± <const>`: load DX:AX, then add/sub the immediate across
        // the two words (`add ax,lo; adc dx,hi` / `sub ax,lo; sbb dx,hi`). Fixture 362.
        Expr::BinOp { op: op @ (BinOp::Add | BinOp::Sub), left, right }
            if long_operand(left, locals)
                && !long_operand(right, locals)
                && right.fold(locals.inits).is_some() =>
        {
            let k = right.fold(locals.inits).unwrap();
            emit_long_to_dx_ax(left, locals, out, fixups);
            let lo = (k as u32 & 0xFFFF) as u16;
            let hi = ((k as i32) >> 16) as i32;
            if matches!(op, BinOp::Add) {
                out.push(0x05); out.extend_from_slice(&lo.to_le_bytes()); // add ax, lo
                if let Ok(h8) = i8::try_from(hi) { out.extend_from_slice(&[0x83, 0xD2, h8 as u8]); } // adc dx, hi (sx)
                else { out.push(0x81); out.push(0xD2); out.extend_from_slice(&((hi as u32 & 0xFFFF) as u16).to_le_bytes()); }
            } else {
                out.push(0x2D); out.extend_from_slice(&lo.to_le_bytes()); // sub ax, lo
                if let Ok(h8) = i8::try_from(hi) { out.extend_from_slice(&[0x83, 0xDA, h8 as u8]); } // sbb dx, hi (sx)
                else { out.push(0x81); out.push(0xDA); out.extend_from_slice(&((hi as u32 & 0xFFFF) as u16).to_le_bytes()); }
            }
        }
        _ => {
            emit_expr_to_ax(value, locals, out, fixups);
            out.push(0x99); // cwd: sign-extend AX into DX
        }
    }
}
