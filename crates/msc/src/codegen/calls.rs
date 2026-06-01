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
        if is_long_param {
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
    let cleanup_bytes: usize = args.iter().enumerate().map(|(i, _)| {
        if param_longs.map(|v| v.get(i).copied().unwrap_or(false)).unwrap_or(false) { 4 } else { 2 }
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
pub(crate) fn emit_long_to_dx_ax(value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match value {
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
            let lo = param_disp(*i) as u8;
            let hi = (param_disp(*i) + 2) as u8;
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
        _ => {
            emit_expr_to_ax(value, locals, out, fixups);
            out.push(0x99); // cwd: sign-extend AX into DX
        }
    }
}
