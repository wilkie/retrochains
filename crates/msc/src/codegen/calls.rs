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
    // A `pascal` callee (detected by its bare C name) takes the UPPERCASE,
    // underscore-free OMF symbol; cdecl callees get `_name`.
    let is_pascal_call = locals.pascal_fns.contains(name);
    let sym = callee_symbol_full(name, is_pascal_call, locals.static_fns.contains(name));
    let param_longs = locals.long_param_funcs.get(&sym);
    let param_structs = locals.struct_param_funcs.get(&sym);
    let struct_bytes_at = |i: usize| -> usize {
        param_structs.and_then(|v| v.get(i).copied()).unwrap_or(0)
    };
    // A variadic callee gives no prototype for its vararg positions, so a long
    // argument there is pushed as two words by its natural width rather than
    // truncated. Fixtures 2197/3983.
    let is_variadic = locals.variadic_fns.contains(&sym);
    let push_long_at = |i: usize| -> bool {
        param_longs.map(|v| v.get(i).copied().unwrap_or(false)).unwrap_or(false)
            || (is_variadic && long_operand(&args[i], locals))
    };
    // cdecl pushes args right-to-left; the `pascal` convention pushes them
    // left-to-right (and the callee cleans the stack via `ret N`).
    let order: Vec<usize> = if is_pascal_call {
        (0..args.len()).collect()
    } else {
        (0..args.len()).rev().collect()
    };
    for &i in &order {
        let arg = &args[i];
        let is_long_param = push_long_at(i);
        let struct_bytes = struct_bytes_at(i);
        if struct_bytes > 0 {
            emit_push_struct_arg(arg, struct_bytes, locals, out, fixups);
        } else if let Expr::FloatLit(bits, is_double) = arg {
            emit_push_arg_float(*bits, if *is_double { 8 } else { 4 }, out, fixups);
        } else if is_long_param {
            emit_push_arg_long(arg, locals, out, fixups);
        } else {
            emit_push_arg(arg, locals, out, fixups);
        }
    }
    // A same-segment `far` callee is reached with a near call preceded by
    // `push cs`, so its `retf` finds CS:IP on the stack (fixtures 1654/2060/2251).
    if locals.far_fns.contains(name) {
        out.push(0x0E); // push cs
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
        let sb = struct_bytes_at(i);
        if sb > 0 { sb }
        else if let Expr::FloatLit(_, is_double) = a { if *is_double { 8 } else { 4 } }
        else if push_long_at(i) { 4 } else { 2 }
    }).sum();
    // The single last call before a slide-frame epilogue elides its cleanup
    // (the `mov sp,bp` teardown restores SP) — signalled by the function view's
    // elide_call_cleanup flag, set in emit_function.
    // A pascal callee pops the arguments itself (`ret N`), so the caller emits
    // no `add sp` cleanup.
    let skip_cleanup = skip_cleanup || locals.elide_call_cleanup.get() || is_pascal_call;
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
/// Emit just the `call WORD PTR <mem>` instruction (FF /2) for a
/// function-pointer target, with no argument push or cleanup. Shared by
/// emit_call_ptr and the two-call binop scheduler.
pub(crate) fn emit_call_ptr_target(target: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    use crate::codegen::func::{bp_modrm, push_bp_disp, param_disp};
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
        // `op.fn(args)` — call through a function-pointer struct field of a local.
        Expr::LocalField { local, byte_off, .. } => {
            let disp = locals.disp(*local) + *byte_off as i16;
            out.push(0xFF); out.push(bp_modrm(0x56, disp)); push_bp_disp(out, disp);
        }
        // `g.fn(args)` — call through a fn-ptr field of a global struct.
        Expr::GlobalField { global, byte_off, .. } => {
            out.push(0xFF); out.push(0x16);
            let bo = out.len(); out.extend_from_slice(&byte_off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *global } });
        }
        // `ops[i](args)` — call through a fn-ptr ARRAY element. A constant index
        // folds to `call [bp+disp]`; a runtime index scales into SI and calls
        // `call [bp+si+disp]` (FF 52 disp). disp = ops[0]'s slot. Fixture 2435.
        Expr::LocalIndex { local, index } => {
            let base = locals.disp(*local);
            if let Some(k) = index.fold(locals.inits) {
                let disp = base + (k as i16) * 2;
                out.push(0xFF); out.push(bp_modrm(0x56, disp)); push_bp_disp(out, disp);
            } else {
                crate::codegen::expr::emit_load_si(index, locals, out, fixups);
                out.extend_from_slice(&[0xD1, 0xE6]); // shl si,1
                // call WORD [bp+si+disp]  (FF /2, modrm rm=010 [bp+si], base=0x52)
                out.push(0xFF); out.push(bp_modrm(0x52, base)); push_bp_disp(out, base);
            }
        }
        // `ops[i](args)` — call through a fn-ptr GLOBAL array element. Const
        // index → `call [_ops + K*2]` (FF 16); runtime → `mov bx,[i]; shl bx,1;
        // call [bx+_ops]` (FF 97 off16). Fixtures 2944, 3696.
        Expr::Index { array, index } => {
            if let Some(k) = index.fold(locals.inits) {
                let off = (k as u32).wrapping_mul(2) as u16;
                out.push(0xFF); out.push(0x16);
                let bo = out.len(); out.extend_from_slice(&off.to_le_bytes());
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
            } else {
                crate::codegen::expr::emit_load_bx(index, locals, out, fixups);
                out.extend_from_slice(&[0xD1, 0xE3]); // shl bx,1
                out.push(0xFF); out.push(0x97);
                let bo = out.len(); out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
            }
        }
        other => panic!("indirect call through unsupported target {other:?}"),
    }
}
pub(crate) fn emit_call_ptr(
    target: &Expr,
    args: &[Expr],
    locals: &Locals<'_>,
    skip_cleanup: bool,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // Push args right-to-left (cdecl).
    for arg in args.iter().rev() {
        if let Expr::FloatLit(bits, is_double) = arg {
            emit_push_arg_float(*bits, if *is_double { 8 } else { 4 }, out, fixups);
        } else {
            emit_push_arg(arg, locals, out, fixups);
        }
    }
    // `call WORD PTR <mem>` — FF /2.
    emit_call_ptr_target(target, locals, out, fixups);
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
/// Push a struct argument passed BY VALUE: its words, highest stack address
/// first (so the struct lands in memory order on the downward-growing stack).
/// The struct arg is a struct local. The first (highest-word) push reuses AX
/// when the preceding store left that word there (`mov [w],ax` → `push ax`).
/// Fixtures 3197, 2866.
/// True when `out` ends with `store` (a `mov [w],ax`) followed by zero or more
/// AX-preserving `push WORD [mem]` instructions — so AX still holds the value
/// last stored. Lets a struct-by-value arg push `push ax` even when later args
/// were already pushed from memory in between (RTL order). Fixture 3272.
fn ax_live_after_mem_pushes(out: &[u8], store: &[u8]) -> bool {
    let sl = store.len();
    let n = out.len();
    let all_push_mem = |s: &[u8]| -> bool {
        let mut i = 0;
        while i < s.len() {
            if i + 3 <= s.len() && s[i] == 0xFF && s[i + 1] == 0x76 { i += 3; }          // push [bp+d8]
            else if i + 4 <= s.len() && s[i] == 0xFF && (s[i + 1] == 0x36 || s[i + 1] == 0xB6) { i += 4; } // push [addr16]/[bp+d16]
            else { return false; }
        }
        true
    };
    let mut p = n.saturating_sub(sl);
    loop {
        if out[p..p + sl] == *store && all_push_mem(&out[p + sl..]) {
            return true;
        }
        if p == 0 { return false; }
        p -= 1;
    }
}
pub(crate) fn emit_push_struct_arg(arg: &Expr, bytes: usize, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let nwords = bytes / 2;
    match arg {
        Expr::Local(i) => {
            let disp = locals.disp(*i);
            for w in (0..nwords).rev() {
                let wdisp = disp + (w as i16) * 2;
                if w == nwords - 1 {
                    // Reuse AX if the highest word was just stored from it (possibly
                    // across already-pushed later args, which preserve AX).
                    let store = { let mut v = vec![0x89, bp_modrm(0x46, wdisp)]; push_bp_disp(&mut v, wdisp); v };
                    if out.len() >= store.len() && ax_live_after_mem_pushes(out, &store) {
                        out.push(0x50); // push ax
                        continue;
                    }
                }
                out.push(0xFF); out.push(bp_modrm(0x76, wdisp)); push_bp_disp(out, wdisp); // push WORD [bp+wdisp]
            }
        }
        Expr::Global(g) => {
            // `push WORD [g + off]` (ff 36 <off16>) for each word, high addr first.
            for w in (0..nwords).rev() {
                let off = (w as u16) * 2;
                out.push(0xFF); out.push(0x36);
                let p = out.len();
                out.extend_from_slice(&off.to_le_bytes());
                fixups.push(Fixup { body_offset: p - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
            }
        }
        _ => {} // other struct arg sources NYI
    }
}
pub(crate) fn emit_push_arg_long(arg: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if let Some(k) = arg.fold(locals.inits) {
        // Constant long: materialize into DX:AX, push high then low.
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
        out.push(0x52); // push dx (high word)
        out.push(0x50); // push ax (low word)
        return;
    }
    // An addressable long operand is pushed directly from memory, high word
    // then low — `push WORD [hi]; push WORD [lo]` — without going through
    // DX:AX. Fixture 3252 (`take_long(x)`, x a long param).
    match arg {
        Expr::Param(i) if locals.is_long_param(*i) => {
            let lo = long_param_disp(*i, locals);
            let hi = lo + 2;
            out.push(0xFF); out.push(bp_modrm(0x76, hi)); push_bp_disp(out, hi);
            out.push(0xFF); out.push(bp_modrm(0x76, lo)); push_bp_disp(out, lo);
            return;
        }
        Expr::Local(i) if locals.is_long_local(*i) => {
            let lo = locals.disp(*i);
            let hi = lo + 2;
            out.push(0xFF); out.push(bp_modrm(0x76, hi)); push_bp_disp(out, hi);
            out.push(0xFF); out.push(bp_modrm(0x76, lo)); push_bp_disp(out, lo);
            return;
        }
        _ => {
            // A long global, long struct field, or long global-array element
            // with a constant index: push both words straight from memory,
            // high then low. Fixtures 3252-family + 328 (`f(a[1])`).
            if let Some((g, base)) = long_global_rhs_addr(arg, locals) {
                out.push(0xFF); out.push(0x36); // push WORD [g+base+2]
                let off = out.len(); out.extend_from_slice(&(base + 2).to_le_bytes());
                fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: g } });
                out.push(0xFF); out.push(0x36); // push WORD [g+base]
                let off = out.len(); out.extend_from_slice(&base.to_le_bytes());
                fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: g } });
                return;
            }
        }
    }
    // Fallback: a computed long expression — evaluate into DX:AX, push both.
    emit_long_to_dx_ax(arg, locals, out, fixups);
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
        Expr::IntLit(0) => {
            // MSC zeroes AX with `sub ax,ax` (its zero idiom), then pushes.
            // Fixture 3988. A consecutive 0 arg (args push RTL, so two trailing
            // 0 args emit back-to-back) reuses the still-live AX with a bare
            // `push ax` rather than re-zeroing. Fixtures 3915/3935.
            if out.ends_with(&[0x2B, 0xC0, 0x50]) {
                out.push(0x50); // push ax
            } else {
                out.extend_from_slice(&[0x2B, 0xC0, 0x50]); // sub ax,ax; push ax
            }
        }
        Expr::IntLit(k) => {
            let imm = (*k as u32 & 0xFFFF) as u16;
            // A run of identical constant args (args push RTL) all reuse the live
            // AX: AX still holds K iff the only thing emitted since `mov ax,K` was
            // a run of `push ax` (0x50). Scan back over that run; if `B8 lo hi`
            // precedes it, emit a bare `push ax`. Fixtures 1260 (2 args), 2327
            // (a longer run of equal args).
            let mut p = out.len();
            while p > 0 && out[p - 1] == 0x50 { p -= 1; }
            let holds_k = p < out.len()
                && p >= 3
                && out[p - 3] == 0xB8
                && out[p - 2] == (imm & 0xFF) as u8
                && out[p - 1] == (imm >> 8) as u8;
            if holds_k {
                out.push(0x50); // push ax
            } else {
                out.push(0xB8);
                out.extend_from_slice(&imm.to_le_bytes());
                out.push(0x50); // push ax
            }
        }
        Expr::Local(idx) => {
            let disp = locals.disp(*idx);
            // Reuse AX when the local was just stored from it (`mov [l],ax`
            // immediately precedes) — `push ax` instead of reloading. The
            // `mov [l],ax` left the value live. Fixture 3975 (`p = malloc();
            // free(p)`).
            let self_store = { let mut v = vec![0x89, bp_modrm(0x46, disp)]; push_bp_disp(&mut v, disp); v };
            if locals.size(*idx) == 2 && !locals.is_long_local(*idx)
                && out.len() >= self_store.len() && out[out.len() - self_store.len()..] == *self_store
            {
                out.push(0x50); // push ax
            } else {
                // `push word ptr [bp-disp]` — `FF /6 r/m`.
                out.push(0xFF);
                out.push(bp_modrm(0x76, disp));
                push_bp_disp(out, disp);
            }
        }
        Expr::Param(idx) => {
            let disp = i8::try_from(crate::codegen::func::param_disp(*idx)).expect("param disp fits");
            if locals.is_char_param(*idx) {
                // A char param passed as an int arg widens first: `mov al,[c];
                // cbw|sub ah,ah; push ax` (pushing the raw word would carry the
                // high-byte garbage). Fixture 2836.
                out.extend_from_slice(&[0x8A, 0x46, disp as u8]); // mov al,[bp+disp]
                if locals.is_unsigned_param(*idx) { out.extend_from_slice(&[0x2A, 0xE4]); } // sub ah,ah
                else { out.push(0x98); } // cbw
                out.push(0x50); // push ax
            } else {
                out.push(0xFF);
                out.push(0x76);
                out.push(disp as u8);
            }
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
            let disp = locals.disp(*local_idx);
            if locals.size(*local_idx) == 1 {
                // A char local passed as an int arg widens the OLD value:
                // `mov al,[c]; inc/dec byte [c]; cbw|sub ah,ah; push ax`.
                // Fixtures 731/733.
                out.extend_from_slice(&[0x8A, bp_modrm(0x46, disp)]); push_bp_disp(out, disp);
                emit_postmutate_local(*step, 1, disp, out);
                if locals.is_unsigned_local(*local_idx) { out.extend_from_slice(&[0x2A, 0xE4]); }
                else { out.push(0x98); }
                out.push(0x50);
            } else {
                // `push word ptr [bp-a]` + inc/dec/add/sub.
                out.push(0xFF);
                out.push(bp_modrm(0x76, disp));
                push_bp_disp(out, disp);
                emit_postmutate_local(*step, locals.size(*local_idx), disp, out);
            }
        }
        Expr::PreMutateLocal { local_idx, step } => {
            // Mutate first, then push the NEW value.
            let disp = locals.disp(*local_idx);
            if locals.size(*local_idx) == 1 {
                // char local: `inc/dec byte [c]; mov al,[c]; cbw|sub ah,ah;
                // push ax`. Fixture 732.
                emit_postmutate_local(*step, 1, disp, out);
                out.extend_from_slice(&[0x8A, bp_modrm(0x46, disp)]); push_bp_disp(out, disp);
                if locals.is_unsigned_local(*local_idx) { out.extend_from_slice(&[0x2A, 0xE4]); }
                else { out.push(0x98); }
                out.push(0x50);
            } else {
                emit_postmutate_local(*step, locals.size(*local_idx), disp, out);
                out.push(0xFF);
                out.push(bp_modrm(0x76, disp));
                push_bp_disp(out, disp);
            }
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
        Expr::BinOp { .. } | Expr::Call { .. } | Expr::CallPtr { .. } | Expr::Ternary { .. }
            | Expr::DerefWord { .. } | Expr::DerefByte { .. }
            | Expr::GlobalField { .. } | Expr::LocalField { .. }
            | Expr::DerefLocalField { .. } | Expr::DerefParamField { .. }
            | Expr::Index { .. } | Expr::IndexByte { .. }
            | Expr::LocalIndex { .. } | Expr::LocalIndexByte { .. }
            | Expr::ParamIndex { .. } | Expr::PtrIndexByte { .. }
            | Expr::FuncAddr(..)
            | Expr::PostMutateGlobal { .. } | Expr::PreMutateGlobal { .. } | Expr::Seq { .. }
            | Expr::PostIncDeref { .. } | Expr::PostMutateLocal { .. } | Expr::PostMutateParam { .. }
            | Expr::PtrChainField { .. } | Expr::AddrOfIndexedGlobal { .. }
            | Expr::StructArrayField { .. } | Expr::PreMutateIndexedGlobal { .. }
            | Expr::PostMutateIndexedGlobal { .. } | Expr::PostMutateLocalIndex { .. }
            | Expr::AssignExpr { .. } => {
            // Computed value: build the result in AX then push.
            // Fixture 4144 (BinOp), 1270 (Call), 3626 (`putchar(*s++)`).
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
        // Long field of a struct local / by-value struct param (`s.v` / `b.v`).
        Expr::LocalField { size: 4, .. } | Expr::ParamField { size: 4, .. } => true,
        // `(long)<int>` widening cast — a long value.
        Expr::CastLong { .. } => true,
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
        Expr::CastLong { unsigned, .. } => *unsigned,
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

/// `e` is `<long> + <int>` / `<int> + <long>` — exactly one operand is a long in
/// memory and the other is a non-constant RUNTIME int. Lowered by promoting the
/// int to a long then two-word adding. A constant int operand is excluded — it
/// folds into the long arithmetic instead (`add ax,K; adc dx,0`). Fixture 3291.
pub(crate) fn is_long_plus_int(e: &Expr, locals: &Locals<'_>) -> bool {
    let Expr::BinOp { op: BinOp::Add, left, right } = e else { return false };
    let (long_side, int_side) = match (long_operand(left, locals), long_operand(right, locals)) {
        (true, false) => (left.as_ref(), right.as_ref()),
        (false, true) => (right.as_ref(), left.as_ref()),
        _ => return false,
    };
    long_rhs_mem(long_side, locals)
        && int_side.fold(locals.inits).is_none()
        && matches!(int_side,
            Expr::Param(i) if !locals.is_long_param(*i) && !locals.is_char_param(*i) && !locals.is_float_param(*i))
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
            && (long_operand(right, locals) || right.fold(locals.inits).is_some())
            && long_shl_amount(*op, right, locals).is_none())
}

/// Shift-left amount for a long `<<` / `* 2^n`: `v << k` (1..=31) or
/// `v * 2^n` both lower to a left shift by that many bits.
/// Two expressions denote the SAME scalar lvalue operand (same Local/Param/
/// Global index) — used to recognize `x + x`, `x & x`, etc.
fn same_long_operand(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Local(i), Expr::Local(j))
        | (Expr::Param(i), Expr::Param(j))
        | (Expr::Global(i), Expr::Global(j)) => i == j,
        _ => false,
    }
}
pub(crate) fn long_shl_amount(op: BinOp, right: &Expr, locals: &Locals<'_>) -> Option<u8> {
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

/// `e` is a long left shift by a VARIABLE (non-constant) count (`v << n`), which
/// emit_long_to_dx_ax lowers to a cl-counted shift loop. Fixture 3430.
pub(crate) fn is_long_shl_var(e: &Expr, locals: &Locals<'_>) -> bool {
    matches!(e, Expr::BinOp { op: BinOp::Shl, left, right }
        if long_operand(left, locals)
            && long_shl_amount(BinOp::Shl, right, locals).is_none()
            && right.fold(locals.inits).is_none())
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
        if matches!(right.fold(locals.inits), Some(k) if (1..16).contains(&k))
            && long_operand(left, locals))
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
/// Long right shift by `k` (DX:AX): high word first so the carry threads into
/// the low word. `sar` (signed) / `shr` (unsigned) on DX, then `rcr ax,1`. For
/// k>1 MSC uses a cl-counted loop (`dec cl; jne`). Fixtures 230/3179/3180.
fn emit_long_shr_k(k: u8, unsigned: bool, out: &mut Vec<u8>) {
    let hi: [u8; 2] = if unsigned { [0xD1, 0xEA] } else { [0xD1, 0xFA] }; // shr|sar dx,1
    if k == 1 {
        out.extend_from_slice(&hi);
        out.extend_from_slice(&[0xD1, 0xD8]); // rcr ax,1
    } else {
        out.extend_from_slice(&[0xB1, k]); // mov cl, k
        out.extend_from_slice(&hi);
        out.extend_from_slice(&[0xD1, 0xD8, 0xFE, 0xC9, 0x75, 0xF8]); // rcr ax,1; dec cl; jne -8
    }
}

/// Push a long operand's two words (high then low) from memory, as MSC does
/// for the helper-call calling convention.
pub(crate) fn push_long_operand(e: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
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
        // A constant long operand (`2L`, `10L`) — sign-extend the low word into
        // DX:AX and push both: `mov ax,K; cwd; push dx; push ax`. Fixtures
        // 3176 (`v / 2L`), 3303 (`a * 10L`).
        _ if e.fold(locals.inits).is_some() => {
            let k = e.fold(locals.inits).unwrap();
            out.push(0xB8); out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); // mov ax,K
            if ((k as i32) >> 16) == if k < 0 { -1 } else { 0 } {
                out.push(0x99); // cwd (high word is the sign extension)
            } else {
                out.push(0xBA); out.extend_from_slice(&(((k as i32) >> 16) as u16).to_le_bytes()); // mov dx, hi
            }
            out.push(0x52); out.push(0x50); // push dx; push ax
        }
        // `(long)<int>` operand: load the int, widen to DX:AX, push both words.
        Expr::CastLong { value, unsigned } => {
            crate::codegen::expr::emit_expr_to_ax(value, locals, out, fixups);
            if *unsigned { out.extend_from_slice(&[0x2B, 0xD2]); } else { out.push(0x99); }
            out.push(0x52); out.push(0x50); // push dx; push ax
        }
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

/// Apply a bitwise op of a 16-bit constant `word` to AX (low half, `is_high`
/// false) or DX (high half, `is_high` true) using MSC's byte-op idiom: a word
/// whose byte equals the op's identity (0 for OR/XOR, 0xFF for AND) is reduced
/// to a single-byte op on the other byte, and an all-identity word is elided.
fn emit_word_const_bitop(op: BinOp, word: u16, is_high: bool, out: &mut Vec<u8>) {
    let hi = (word >> 8) as u8;
    let lo = word as u8;
    let is_and = matches!(op, BinOp::BitAnd);
    if (!is_and && word == 0) || (is_and && word == 0xFFFF) { return; } // identity
    let grp = match op { BinOp::BitOr => 1u8, BinOp::BitAnd => 4, BinOp::BitXor => 6, _ => unreachable!() };
    let byte_id: u8 = if is_and { 0xFF } else { 0x00 };
    // ModRM (mod=11) r/m for the dl/dh vs al/ah register pick.
    let rm_lo = if is_high { 0x02u8 } else { 0x00 }; // dl / al
    let rm_hi = if is_high { 0x06u8 } else { 0x04 }; // dh / ah
    if hi == byte_id {
        // op the low byte. AL has an accumulator short form; DL uses 80 /grp.
        if is_high {
            out.extend_from_slice(&[0x80, 0xC0 | (grp << 3) | rm_lo, lo]);
        } else {
            let opc = match op { BinOp::BitOr => 0x0Cu8, BinOp::BitAnd => 0x24, BinOp::BitXor => 0x34, _ => unreachable!() };
            out.extend_from_slice(&[opc, lo]);
        }
    } else if lo == byte_id {
        // op the high byte (ah / dh) — always the 80 /grp form.
        out.extend_from_slice(&[0x80, 0xC0 | (grp << 3) | rm_hi, hi]);
    } else if is_high {
        out.push(0x81); out.push(0xC0 | (grp << 3) | rm_lo); out.extend_from_slice(&word.to_le_bytes());
    } else {
        let opc = match op { BinOp::BitOr => 0x0Du8, BinOp::BitAnd => 0x25, BinOp::BitXor => 0x35, _ => unreachable!() };
        out.push(opc); out.extend_from_slice(&word.to_le_bytes());
    }
}

/// `e` is `<long> <&|`|^> <const>` — a long operand combined with a foldable
/// constant via a bitwise op. Lowered by loading DX:AX then word-wise const ops.
pub(crate) fn is_long_const_bitop(e: &Expr, locals: &Locals<'_>) -> bool {
    matches!(e, Expr::BinOp { op: BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor, left, right }
        if long_operand(left, locals) && right.fold(locals.inits).is_some())
}

/// Materialize a known 32-bit long constant into DX:AX the way MSC does:
/// `mov ax,lo; cwd` when the high word is the sign-extension of the low word
/// (the common small-value case), else `mov ax,lo; mov dx,hi`.
pub(crate) fn emit_long_const_to_dx_ax(k: i32, out: &mut Vec<u8>) {
    let lo = (k as u32 & 0xFFFF) as u16;
    let hi = ((k >> 16) as u32 & 0xFFFF) as u16;
    out.push(0xB8); out.extend_from_slice(&lo.to_le_bytes()); // mov ax, lo
    let sign_ext = if (lo as i16) < 0 { 0xFFFFu16 } else { 0x0000 };
    if hi == sign_ext {
        out.push(0x99); // cwd
    } else {
        out.push(0xBA); out.extend_from_slice(&hi.to_le_bytes()); // mov dx, hi
    }
}
/// `((long)A << 16) | (long)B` — assemble a 32-bit value from two 16-bit ints,
/// `A` the high word and `B` the low word. Returns `(A, B)` (the inner int
/// operands). Matches either OR order. Fixture 1946 (`make_long`).
pub(crate) fn long_from_two_ints(value: &Expr) -> Option<(&Expr, &Expr)> {
    fn hi_of(e: &Expr) -> Option<&Expr> {
        if let Expr::BinOp { op: BinOp::Shl, left: l, right: r } = e
            && matches!(r.as_ref(), Expr::IntLit(16))
            && let Expr::CastLong { value: inner, .. } = l.as_ref()
        { Some(inner.as_ref()) } else { None }
    }
    fn lo_of(e: &Expr) -> Option<&Expr> {
        if let Expr::CastLong { value: inner, .. } = e { Some(inner.as_ref()) } else { None }
    }
    let Expr::BinOp { op: BinOp::BitOr, left, right } = value else { return None };
    if let (Some(hi), Some(lo)) = (hi_of(left), lo_of(right)) { return Some((hi, lo)); }
    if let (Some(hi), Some(lo)) = (hi_of(right), lo_of(left)) { return Some((hi, lo)); }
    None
}
/// `mov <reg>,[bp+disp]` for a word int Param/Local operand; None otherwise.
/// `reg` is the modrm reg field (ax=0 → 0x46, bx=3 → 0x5E).
fn int_operand_disp(e: &Expr, locals: &Locals<'_>) -> Option<i16> {
    match e {
        Expr::Param(i) if !locals.is_char_param(*i) && !locals.is_long_param(*i)
            && !locals.is_float_param(*i) => Some(param_disp(*i)),
        Expr::Local(i) if locals.size(*i) == 2 && !locals.is_long_local(*i)
            && !locals.is_float_local(*i) && !locals.is_array_local(*i) => Some(locals.disp(*i)),
        _ => None,
    }
}
pub(crate) fn emit_long_to_dx_ax(value: &Expr, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // `((long)hi << 16) | (long)lo` builds a long with hi in the high word and lo
    // in the low word. MSC loads lo into AX, hi into BX, then `mov dx,bx` (the
    // `<<16` realized as a low→high register move). Fixture 1946.
    if let Some((hi, lo)) = long_from_two_ints(value)
        && let (Some(hd), Some(ld)) = (int_operand_disp(hi, locals), int_operand_disp(lo, locals))
    {
        out.push(0x8B); out.push(bp_modrm(0x46, ld)); push_bp_disp(out, ld); // mov ax,[lo]
        out.push(0x8B); out.push(bp_modrm(0x5E, hd)); push_bp_disp(out, hd); // mov bx,[hi]
        out.extend_from_slice(&[0x8B, 0xD3]);                                // mov dx,bx
        return;
    }
    match value {
        // Expression-context long mul/div/mod: push both operands as longs
        // (RHS then LHS, each high-then-low), call the runtime helper, result
        // comes back in DX:AX. Fixtures 231-233, 3173, 3181, ...
        Expr::BinOp { op, left, right }
            if long_muldiv_helper(*op, false).is_some()
                && long_operand(left, locals)
                && (long_operand(right, locals) || right.fold(locals.inits).is_some())
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
        // Long left shift by a VARIABLE count `v << n`: load DX:AX, `mov cl,<n>`,
        // then a test-first cl-counted loop (n may be 0):
        //   jmp short L2; nop; L1: shl ax,1; rcl dx,1; dec cl; L2: or cl,cl; jne L1
        // Fixture 3430 (`long a << n`).
        Expr::BinOp { op: BinOp::Shl, left, right }
            if long_operand(left, locals)
                && long_shl_amount(BinOp::Shl, right, locals).is_none()
                && right.fold(locals.inits).is_none() =>
        {
            emit_long_to_dx_ax(left, locals, out, fixups);
            match right.as_ref() {
                Expr::Param(i) => { let d = long_param_disp(*i, locals); out.push(0x8A); out.push(bp_modrm(0x4E, d)); push_bp_disp(out, d); }
                Expr::Local(i) => { let d = locals.disp(*i); out.push(0x8A); out.push(bp_modrm(0x4E, d)); push_bp_disp(out, d); }
                _ => { crate::codegen::expr::emit_expr_to_ax(right, locals, out, fixups); out.extend_from_slice(&[0x8A, 0xC8]); } // mov cl,al
            }
            out.extend_from_slice(&[0xEB, 0x07, 0x90]);                   // jmp short L2; nop
            out.extend_from_slice(&[0xD1, 0xE0, 0xD1, 0xD2, 0xFE, 0xC9]); // L1: shl ax,1; rcl dx,1; dec cl
            out.extend_from_slice(&[0x0A, 0xC9, 0x75, 0xF6]);             // L2: or cl,cl; jne L1
        }
        // Long right shift by a constant `v >> K` (1..=15): high word first so
        // the carry threads into the low word; SAR vs SHR by signedness; k>1 uses
        // a cl-counted loop. Fixtures 2885/2886/3300/378 (>>1), 230/3179/3180 (>>K).
        Expr::BinOp { op: BinOp::Shr, left, right }
            if matches!(right.fold(locals.inits), Some(k) if (1..16).contains(&k))
                && long_operand(left, locals) =>
        {
            let k = right.fold(locals.inits).unwrap() as u8;
            emit_long_to_dx_ax(left, locals, out, fixups);
            emit_long_shr_k(k, long_operand_unsigned(left, locals), out);
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
        // `<long> + <int>` / `<int> + <long>` (commutative Add, exactly one
        // operand long): promote the INT operand to a long — MSC loads it FIRST
        // into AX and sign-extends with `cwd` (zero-extend `sub dx,dx` if the int
        // is unsigned) — then two-word adds the long operand from memory. Fixture
        // 3291 (`long a + int b`).
        Expr::BinOp { left, right, .. } if is_long_plus_int(value, locals) => {
            let (long_side, int_side) = if long_operand(left, locals) {
                (left.as_ref(), right.as_ref())
            } else {
                (right.as_ref(), left.as_ref())
            };
            // int → AX with the long-aware displacement (a preceding long param
            // shifts an int param: `int b` after `long a` is at [bp+8], not +6).
            let Expr::Param(i) = int_side else { unreachable!("is_long_plus_int gates Param") };
            let d = long_param_disp(*i, locals);
            out.push(0x8B); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); // mov ax,[bp+d]
            if locals.is_unsigned_param(*i) { out.extend_from_slice(&[0x2B, 0xD2]); } // sub dx,dx
            else { out.push(0x99); } // cwd
            emit_long_op_mem(BinOp::Add, long_side, locals, out, fixups);
        }
        // `<long> <&|`|^> <const>`: load the long into DX:AX, then apply the
        // constant word-wise with MSC's byte-op idiom (`a | 0x100L` → `or ah,1`,
        // the all-identity high word elided). Fixture 2876.
        Expr::BinOp { op, left, right }
            if matches!(op, BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor)
                && long_operand(left, locals)
                && right.fold(locals.inits).is_some() =>
        {
            let k = right.fold(locals.inits).expect("folded const");
            emit_long_to_dx_ax(left, locals, out, fixups);
            emit_word_const_bitop(*op, (k as u32 & 0xFFFF) as u16, false, out); // AX (low)
            emit_word_const_bitop(*op, ((k as u32) >> 16) as u16, true, out);   // DX (high)
        }
        // `x + x` (long, the SAME operand on both sides) is `x * 2` → strength-
        // reduce to a single in-place left shift `shl ax,1; rcl dx,1` instead of
        // re-loading x and two-word adding. Fixture 4037.
        Expr::BinOp { op: BinOp::Add, left, right }
            if long_operand(left, locals) && same_long_operand(left, right) =>
        {
            emit_long_to_dx_ax(left, locals, out, fixups);
            out.extend_from_slice(&[0xD1, 0xE0, 0xD1, 0xD2]); // shl ax,1; rcl dx,1
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
        // `(long)<int>` — materialize the inner int into AX, then widen to DX:AX:
        // signed → `cwd`, unsigned → `sub dx,dx`. Fixture 1683.
        Expr::CastLong { value, unsigned } => {
            crate::codegen::expr::emit_expr_to_ax(value, locals, out, fixups);
            if *unsigned { out.extend_from_slice(&[0x2B, 0xD2]); } else { out.push(0x99); }
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
        // `g++` / `g--` (long global) used as a value: load the OLD long into
        // DX:AX, then increment/decrement the 4-byte global in place — `add
        // word[g],step; adc word[g+2],0` (sub/sbb for --). Fixture 3294.
        Expr::PostMutateGlobal { global_idx, step } if locals.is_long_global(*global_idx) => {
            let g = *global_idx;
            let bo = out.len();
            out.extend_from_slice(&[0xA1, 0x00, 0x00]); // mov ax, [g]
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } });
            let bo = out.len() + 1;
            out.extend_from_slice(&[0x8B, 0x16, 0x02, 0x00]); // mov dx, [g+2]
            fixups.push(Fixup { body_offset: bo, kind: FixupKind::GlobalAddr { global_idx: g } });
            let (lo_op, hi_op) = if *step >= 0 { (0x06u8, 0x16u8) } else { (0x2Eu8, 0x1Eu8) }; // add/adc | sub/sbb
            let mag = step.unsigned_abs() as u16;
            out.push(0x83); out.push(lo_op); // add/sub word [g], step_lo (imm8sx)
            let bo = out.len(); out.extend_from_slice(&[0x00, 0x00]); out.push(mag as u8);
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: g } });
            out.push(0x83); out.push(hi_op); // adc/sbb word [g+2], 0
            let bo = out.len(); out.extend_from_slice(&2u16.to_le_bytes()); out.push(0x00);
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: g } });
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
        // Long field of a struct local / by-value struct param: low word at the
        // field's bp offset, high word at +2. Fixture 2592 (`b.v`, b a by-value
        // struct param).
        Expr::LocalField { local, byte_off, size: 4 } => {
            let lo = locals.disp(*local) + *byte_off as i16;
            let hi = lo + 2;
            out.push(0x8B); out.push(bp_modrm(0x46, lo)); push_bp_disp(out, lo); // mov ax,[bp+lo]
            out.push(0x8B); out.push(bp_modrm(0x56, hi)); push_bp_disp(out, hi); // mov dx,[bp+hi]
        }
        Expr::ParamField { param, byte_off, size: 4 } => {
            let lo = locals.param_base_disp(*param) + *byte_off as i16;
            let hi = lo + 2;
            out.push(0x8B); out.push(bp_modrm(0x46, lo)); push_bp_disp(out, lo); // mov ax,[bp+lo]
            out.push(0x8B); out.push(bp_modrm(0x56, hi)); push_bp_disp(out, hi); // mov dx,[bp+hi]
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
        // Runtime-index long LOCAL array element `arr[i]`: scale the index ×4
        // through SI and read both words at [bp+si+disp] / [bp+si+disp+2].
        // Fixture 2798 (`return arr[i]`).
        Expr::LocalIndex { local, index } if locals.is_long_local(*local) => {
            let (var, koff) = crate::codegen::expr::split_index_offset(index);
            crate::codegen::expr::emit_load_si(var, locals, out, fixups); // mov si,[i]
            out.extend_from_slice(&[0xD1, 0xE6, 0xD1, 0xE6]); // shl si,1; shl si,1
            let lo = locals.disp(*local) + (koff.wrapping_mul(4)) as i16;
            let hi = lo + 2;
            out.push(0x8B); out.push(bp_modrm(0x42, lo)); push_bp_disp(out, lo); // mov ax,[bp+si+lo]
            out.push(0x8B); out.push(bp_modrm(0x52, hi)); push_bp_disp(out, hi); // mov dx,[bp+si+hi]
        }
        // Runtime-index long array element `a[i]`: scale the index ×4 (two
        // `shl bx,1`) and read both pointee words at [bx+a] / [bx+a+2]. Fixture
        // 3288 (`return arr[i]`).
        Expr::Index { array, index } if locals.is_long_global(*array) => {
            let (var, koff) = crate::codegen::expr::split_index_offset(index);
            crate::codegen::expr::emit_load_bx(var, locals, out, fixups); // mov bx,[i]
            out.extend_from_slice(&[0xD1, 0xE3, 0xD1, 0xE3]); // shl bx,1; shl bx,1
            let off = (koff.wrapping_mul(4)) as i16 as u16;
            out.extend_from_slice(&[0x8B, 0x87]); // mov ax,[bx+a+off]
            let bo = out.len(); out.extend_from_slice(&off.to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
            out.extend_from_slice(&[0x8B, 0x97]); // mov dx,[bx+a+off+2]
            let bo = out.len(); out.extend_from_slice(&off.wrapping_add(2).to_le_bytes());
            fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
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
        // `*p` / `*p++` where p is a long* (pointee_size 4): read BOTH pointee
        // words into DX:AX (`mov bx,[p]; mov ax,[bx]; mov dx,[bx+2]`). For `*p++`
        // advance p by its stride first, keeping the OLD pointer in BX. Fixture 2521.
        Expr::DerefWord { ptr } if matches!(ptr.as_ref(),
                Expr::PostMutateLocal { local_idx, .. } if locals.local_pointee_size(*local_idx) == 4)
            || matches!(ptr.as_ref(), Expr::Local(l) if locals.local_pointee_size(*l) == 4)
            || matches!(ptr.as_ref(), Expr::Param(pi) if locals.param_pointee_size(*pi) == 4) =>
        {
            match ptr.as_ref() {
                Expr::PostMutateLocal { local_idx, step } => {
                    let d = locals.disp(*local_idx);
                    out.extend_from_slice(&[0x8B, 0x5E, d as u8]); // mov bx,[bp-p]
                    crate::codegen::assign::emit_postmutate_local(*step, 2, d, out); // add word [p],stride
                }
                Expr::Local(l) => { out.extend_from_slice(&[0x8B, 0x5E, locals.disp(*l) as u8]); }
                Expr::Param(pi) => { out.extend_from_slice(&[0x8B, 0x5E, crate::codegen::func::param_disp(*pi) as u8]); }
                _ => unreachable!(),
            }
            out.extend_from_slice(&[0x8B, 0x07]);       // mov ax,[bx]
            out.extend_from_slice(&[0x8B, 0x57, 0x02]); // mov dx,[bx+2]
        }
        _ => {
            emit_expr_to_ax(value, locals, out, fixups);
            out.push(0x99); // cwd: sign-extend AX into DX
        }
    }
}
