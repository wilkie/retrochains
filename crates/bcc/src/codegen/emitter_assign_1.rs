use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// Emit `name <op>= value;`. Fixtures 067–071 show BCC routes this
    /// through a distinct codegen path that's *tighter* than the
    /// expanded `name = name <op> value` form: when the target sits
    /// in a register, the operation is performed directly on the
    /// register with `<mnemonic> <reg>, <src>` instead of going
    /// through AX. Peepholes:
    ///
    /// - `<reg> += 1` / `<reg> -= 1` → `inc <reg>` / `dec <reg>`
    /// - `<reg> += K` / `<reg> -= K` (K != 1) → `add <reg>, K` / `sub <reg>, K`
    /// - `<reg> += <src>` (src = mem or reg) → `add <reg>, <src>`
    /// - Same shapes for `&=` / `|=` / `^=` with `and` / `or` / `xor`.
    /// - `*=` doesn't have a `reg, imm` form on 8086, so it routes
    ///   through AX via DX: `mov dx, <rhs> / mov ax, <reg> / imul dx
    ///   / mov <reg>, ax`.
    ///
    /// Stack-resident targets are unobserved — every fixture so far
    /// puts the target in a register. Panic until pinned.
    pub(crate) fn emit_compound_assign(&mut self, name: &str, op: BinOp, value: &Expr) {
        // Comma chain on the RHS: emit each leading sub-expression
        // for side effect, then recurse on the final value. Without
        // this unwrap, the compound assign goes through emit_expr_to
        // _ax for the whole comma, materializing the final value in
        // AX even when it's a small constant that would benefit from
        // the ±K inc/dec peephole. Fixture 1378 (`a += (a+1, 2)` —
        // the trailing 2 should fold to `inc si; inc si`).
        if let ExprKind::Comma { left, right } = &value.kind {
            self.emit_expr_discard(left);
            return self.emit_compound_assign(name, op, right);
        }
        // Float / double local `<op>=`: evaluate RHS onto the FPU
        // stack, fadd/fsub/fmul/fdiv against the memory operand
        // (the local), then fstp back to the local. Fixture 2148.
        if let Some(local_ty) = self.locals.has(name).then(|| self.locals.type_of(name).clone())
            && local_ty.is_float_like()
            && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div)
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            let mnem = match op {
                BinOp::Add => "fadd",
                BinOp::Sub => "fsub",
                BinOp::Mul => "fmul",
                BinOp::Div => "fdiv",
                _ => unreachable!(),
            };
            let store_width = if matches!(local_ty, Type::Float) { "dword" } else { "qword" };
            self.emit_float_load_to_fpu(value);
            let _ = write!(
                self.out,
                "\t{mnem}\t{store_width} ptr {}\r\n",
                bp_addr(off),
            );
            let _ = write!(
                self.out,
                "\tfstp\t{store_width} ptr {}\r\n",
                bp_addr(off),
            );
            self.pending_fpu_store_fwait = true;
            return;
        }
        // Long-like global `g <op>= K` with K fitting i8sx (per
        // half): memory-direct read-modify-write on each half. The
        // high-half partner depends on the op family — add/sub need
        // carry/borrow propagation (`adc/sbb high,0`), bitwise ops
        // act independently (the same mnemonic against the high
        // word of K). Distinct from `g = g <op> K` (slice 207) which
        // uses the register-load pattern. Fixtures 251 (`+=`), 252
        // (`-=`), 253 (`&=`).
        // Long-like global `g <op>= rhs` where rhs is another long
        // global (mul/div/mod) — emit the same helper-call shapes
        // as the `g = g <op> rhs` form (slices 231–233). The byte
        // output is identical between `g = g op b` and `g op= b`
        // for these ops. Fixtures 260 (`*=`), 261 (`/=`), 262 (`%=`).
        // `long g += K` / `-= K` / bitwise with constant RHS — use
        // memory-direct two-half form. Saves the AX/DX load + cwd
        // (5-7 bytes) for a 10-byte mem-direct shape. Fixture 251
        // (`long g += 5`). Add/Sub use sign-extended low + adc/sbb
        // with zero for positive K (or -1 for negative). Bitwise
        // uses each half independently.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some(k) = try_const_eval(value)
        {
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            let k_signed = k as i32;
            let lo = (k & 0xFFFF) as u16;
            let hi = ((k >> 16) & 0xFFFF) as u16;
            match op {
                BinOp::Add | BinOp::Sub => {
                    let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                        ("add", "adc")
                    } else {
                        ("sub", "sbb")
                    };
                    let _ = write!(self.out, "\t{lo_op}\tword ptr {lhs_lo},{lo}\r\n");
                    // High-half carry/borrow: 0 for non-negative K
                    // (no carry bits), -1 (0xFFFF) for negative K
                    // sign-extension. Since K is typically small,
                    // hi_k is usually 0 — the adc/sbb still has to
                    // ride the carry/borrow from the low half.
                    let hi_imm = if k_signed < 0 && hi == 0 { 0 } else { hi };
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {lhs_hi},{hi_imm}\r\n");
                }
                BinOp::BitAnd => {
                    let _ = write!(self.out, "\tand\tword ptr {lhs_lo},{lo}\r\n");
                    let _ = write!(self.out, "\tand\tword ptr {lhs_hi},{hi}\r\n");
                }
                BinOp::BitOr => {
                    let _ = write!(self.out, "\tor\tword ptr {lhs_lo},{lo}\r\n");
                    let _ = write!(self.out, "\tor\tword ptr {lhs_hi},{hi}\r\n");
                }
                BinOp::BitXor => {
                    let _ = write!(self.out, "\txor\tword ptr {lhs_lo},{lo}\r\n");
                    let _ = write!(self.out, "\txor\tword ptr {lhs_hi},{hi}\r\n");
                }
                _ => unreachable!(),
            }
            return;
        }
        // Long LHS with int RHS (widening): `long g += int x`. BCC
        // widens the int via `cwd` (signed) into DX:AX, then
        // applies memory-direct add/adc (or sub/sbb, or
        // bitwise-pair) to the LHS. Fixture 755. Also accepts
        // `Type::Char` RHS — `emit_expr_to_ax` emits the `cbw`
        // for the byte-to-int widening, and the same `cwd` then
        // extends to long. Fixture 783. RHS can be Ident,
        // ArrayIndex (fixture 827), or Member (fixture 828).
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::Char)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            let (lo_op, hi_op) = match op {
                BinOp::Add => ("add", "adc"),
                BinOp::Sub => ("sub", "sbb"),
                BinOp::BitAnd => ("and", "and"),
                BinOp::BitOr => ("or", "or"),
                BinOp::BitXor => ("xor", "xor"),
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lhs_lo},ax\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {lhs_hi},dx\r\n");
            return;
        }
        // Long LHS + unsigned-int RHS: zero-extends rather than
        // sign-extends, so BCC skips the cwd and instead uses an
        // immediate-0 operand for the high-half op. `mov ax, <x>;
        // <lo_op> word ptr <lhs_lo>, ax; <hi_op> word ptr
        // <lhs_hi>, 0`. For arith the `0` rides on the carry/
        // borrow from the low half (adc/sbb 0); for bitwise it
        // acts directly (and 0 zeros high, or/xor 0 is a no-op
        // on high). Fixture 767. Also accepts `Type::UChar` RHS
        // — `emit_expr_to_ax` emits `mov ah, 0` for the byte-to-
        // int zero-extension, and the same `<hi_op> 0` finishes
        // the long widening with no further widening register.
        // Fixture 784.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::UInt | Type::UChar)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            let (lo_op, hi_op) = match op {
                BinOp::Add => ("add", "adc"),
                BinOp::Sub => ("sub", "sbb"),
                BinOp::BitAnd => ("and", "and"),
                BinOp::BitOr => ("or", "or"),
                BinOp::BitXor => ("xor", "xor"),
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lhs_lo},ax\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {lhs_hi},0\r\n");
            return;
        }
        // Long LHS `*= int x` (signed widening). BCC can't load
        // both AX:DX (LHS) and BX:CX (widened RHS) simultaneously
        // since `cwd` clobbers DX, so it routes the widened RHS
        // through the stack: `mov ax, <x>; cwd; push ax; push dx;
        // mov dx, <lhs_hi>; mov ax, <lhs_lo>; pop cx; pop bx; call
        // N_LXMUL@; store`. The push/pop dance places RHS-high in
        // CX and RHS-low in BX — matching the helper's
        // convention. Fixture 762. Also accepts `Type::Char` —
        // `emit_expr_to_ax` emits the `cbw` byte-to-int step,
        // and the same `cwd` finishes the long-widening. Fixture
        // 785.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::Char)
            && matches!(op, BinOp::Mul)
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
            self.out.extend_from_slice(b"\tpop\tcx\r\n");
            self.out.extend_from_slice(b"\tpop\tbx\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `*= uchar c` (unsigned-byte widening). Same
        // `xor cx, cx` zero-extension as `*= uint`, but the uchar
        // is materialized in AX via `mov al; mov ah, 0` — so AX
        // is occupied. BCC inserts a `push ax; ...; pop bx`
        // shuffle to free AX for the LHS-low load while
        // preserving the widened RHS for BX:
        // `mov al, <c>; mov ah, 0; xor cx, cx; mov dx, <lhs_hi>;
        // push ax; mov ax, <lhs_lo>; pop bx; call N_LXMUL@;
        // store`. Different from the `*= uint` arm (fixture 772)
        // which loads BX directly from a 16-bit memory operand.
        // Fixture 786.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::UChar)
            && matches!(op, BinOp::Mul)
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\txor\tcx,cx\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
            self.out.extend_from_slice(b"\tpop\tbx\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `*= uint x` (unsigned widening). Zero-extension
        // means CX can be cleared with `xor cx, cx` without
        // disturbing DX, so BCC loads BX directly from the uint
        // and skips the push/pop dance the signed path needs:
        // `mov bx, <x>; xor cx, cx; mov dx, <lhs_hi>; mov ax,
        // <lhs_lo>; call N_LXMUL@; store`. Fixture 772.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::UInt)
            && matches!(op, BinOp::Mul)
        {
            let _ = ty_lhs;
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            let rhs_addr = if self.globals.contains(b) {
                format!("DGROUP:_{b}")
            } else {
                let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                    unreachable!();
                };
                bp_addr(off)
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr {rhs_addr}\r\n");
            self.out.extend_from_slice(b"\txor\tcx,cx\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `/= int x` / `%= int x` (signed widening). BCC
        // widens via `cwd`, pushes both halves of the widened RHS
        // (DX then AX, high then low), then pushes the two halves
        // of the LHS, calls the helper. Same push convention as
        // the both-globals path. Fixture 763. Also accepts
        // `Type::Char` — `emit_expr_to_ax` emits the `cbw` byte-
        // to-int step, and the same `cwd` finishes the long-
        // widening. Fixture 787.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::Char)
            && matches!(op, BinOp::Div | BinOp::Mod)
        {
            let unsigned = ty_lhs.is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `/= uchar c` / `%= uchar c` (unsigned-byte
        // widening). uchar materializes in AX via `mov ah, 0`, so
        // unlike the `/= uint` arm BCC can't use AX to source the
        // pushed `0` for the widened RHS high half — it zeroes DX
        // instead: `mov al, <c>; mov ah, 0; xor dx, dx; push dx;
        // push ax; push <lhs_hi>; push <lhs_lo>; call <helper>`.
        // Helper still picked from LHS signedness. Fixture 788.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::UChar)
            && matches!(op, BinOp::Div | BinOp::Mod)
        {
            let unsigned = ty_lhs.is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS `/= uint x` / `%= uint x` (unsigned widening).
        // Zero-extension lets BCC push a literal 0 (`xor ax, ax;
        // push ax`) for the widened RHS high half, then push the
        // uint directly via `push word ptr <rhs>` without going
        // through AX. The helper consumes the same four words off
        // the stack as the signed path. Fixture 773.
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::UInt)
            && matches!(op, BinOp::Div | BinOp::Mod)
        {
            let unsigned = ty_lhs.is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            let rhs_addr = if self.globals.contains(b) {
                format!("DGROUP:_{b}")
            } else {
                let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                    unreachable!();
                };
                bp_addr(off)
            };
            self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {rhs_addr}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lhs_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long LHS shift by an int/char RHS — same helper-call
        // shape as `long <<= long h`, with the shift count read as
        // `byte ptr` out of the RHS's storage. `mov cl, byte ptr
        // <addr>` works regardless of RHS width (CL only needs the
        // low byte) and regardless of RHS signedness (the shift
        // count is bounded by long width anyway). Fixture 760
        // (int/uint), fixture 789 (char/uchar).
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt | Type::Char | Type::UChar)
            && matches!(op, BinOp::Shl | BinOp::Shr)
        {
            let unsigned = ty_lhs.is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Shl, _)     => "N_LXLSH@",
                (BinOp::Shr, false) => "N_LXRSH@",
                (BinOp::Shr, true)  => "N_LXURSH@",
                _ => unreachable!(),
            };
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            let rhs_lo_byte = if self.globals.contains(b) {
                format!("byte ptr DGROUP:_{b}")
            } else {
                let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                    unreachable!();
                };
                format!("byte ptr {}", bp_addr(off))
            };
            let _ = write!(self.out, "\tmov\tcl,{rhs_lo_byte}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
            return;
        }
        // Long compound on a long LHS (global or stack local) with
        // a long RHS (global or stack local), but not both globals
        // (which keeps the existing branch). Mul/Div/Mod use the
        // long helper; Add/Sub/Bit* use the inline memory-direct
        // shape. Fixtures 744-746 (Add/And), 747 (Mul).
        if let Some(ty_lhs) = self.lhs_long_type(name)
            && ty_lhs.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && let Some(ty_rhs) = self.rhs_long_type_of_ident(b)
            && ty_rhs.is_long_like()
            && !(self.globals.contains(name) && self.globals.contains(b))
        {
            let unsigned = ty_lhs.is_unsigned() || ty_rhs.is_unsigned();
            let (rhs_lo, rhs_hi) = self.long_halves_of(b);
            let (lhs_lo, lhs_hi) = self.long_halves_of(name);
            match op {
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let _ = write!(self.out, "\tmov\tax,word ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {rhs_lo}\r\n");
                    let (lo_op, hi_op) = match op {
                        BinOp::Add => ("add", "adc"),
                        BinOp::Sub => ("sub", "sbb"),
                        BinOp::BitAnd => ("and", "and"),
                        BinOp::BitOr => ("or", "or"),
                        BinOp::BitXor => ("xor", "xor"),
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\t{lo_op}\tword ptr {lhs_lo},dx\r\n");
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {lhs_hi},ax\r\n");
                    return;
                }
                // Long `g *= h` — RHS into CX:BX, LHS into DX:AX,
                // helper call, write back. Same shape as the both-
                // globals path. Fixture 747.
                BinOp::Mul => {
                    let _ = write!(self.out, "\tmov\tcx,word ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tbx,word ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                    self.helpers.insert("N_LXMUL@".to_string());
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
                    return;
                }
                // Long `g <<= h` / `g >>= h` (mixed location): same
                // helper-call shape, CL loaded from RHS low byte
                // (which lives at `byte ptr [bp+off]` for a stack
                // RHS or `byte ptr DGROUP:_<sym>` for a global).
                // Fixture 749 (global LHS + stack RHS).
                BinOp::Shl | BinOp::Shr => {
                    let helper = match (op, unsigned) {
                        (BinOp::Shl, _)     => "N_LXLSH@",
                        (BinOp::Shr, false) => "N_LXRSH@",
                        (BinOp::Shr, true)  => "N_LXURSH@",
                        _ => unreachable!(),
                    };
                    // `rhs_lo` is already an address (sans `word
                    // ptr`); reuse for the byte form.
                    let _ = write!(self.out, "\tmov\tcl,byte ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {lhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr {lhs_lo}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
                    return;
                }
                // Long `g /= h` / `g %= h` — push both halves of
                // RHS then LHS, helper call, write back. Helper
                // selection matches the both-globals path.
                BinOp::Div | BinOp::Mod => {
                    let helper = match (op, unsigned) {
                        (BinOp::Div, false) => "N_LDIV@",
                        (BinOp::Mod, false) => "N_LMOD@",
                        (BinOp::Div, true)  => "N_LUDIV@",
                        (BinOp::Mod, true)  => "N_LUMOD@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tpush\tword ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {lhs_hi}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {lhs_lo}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_hi},dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {lhs_lo},ax\r\n");
                    return;
                }
                _ => {}
            }
        }
        // Long-RHS variant accepting ArrayIndex (const index) and
        // Member in addition to plain Ident global. Same Mul/Div/
        // Mod/Add/Sub/Bit* shapes; only the RHS address strings
        // differ. Fixture 829 (`long_array[0]`).
        if let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
            && let Some((rhs_lo, rhs_hi, rhs_ty)) = self.long_rhs_halves(&value.kind)
            && rhs_ty.is_long_like()
            && !matches!(&value.kind, ExprKind::Ident(b) if self.globals.contains(b))
        {
            let unsigned = ty.is_unsigned() || rhs_ty.is_unsigned();
            match op {
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let _ = write!(self.out, "\tmov\tax,word ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {rhs_lo}\r\n");
                    let (lo_op, hi_op) = match op {
                        BinOp::Add => ("add", "adc"),
                        BinOp::Sub => ("sub", "sbb"),
                        BinOp::BitAnd => ("and", "and"),
                        BinOp::BitOr => ("or", "or"),
                        BinOp::BitXor => ("xor", "xor"),
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{name},dx\r\n");
                    let _ = write!(self.out, "\t{hi_op}\tword ptr DGROUP:_{name}+2,ax\r\n");
                    let _ = unsigned;
                    return;
                }
                BinOp::Mul => {
                    let _ = write!(self.out, "\tmov\tcx,word ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tbx,word ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                    self.helpers.insert("N_LXMUL@".to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                BinOp::Div | BinOp::Mod => {
                    let helper = match (op, unsigned) {
                        (BinOp::Div, false) => "N_LDIV@",
                        (BinOp::Mod, false) => "N_LMOD@",
                        (BinOp::Div, true)  => "N_LUDIV@",
                        (BinOp::Mod, true)  => "N_LUMOD@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tpush\tword ptr {rhs_hi}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {rhs_lo}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                _ => {}
            }
        }
        if let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
            && let ExprKind::Ident(b) = &value.kind
            && self.globals.type_of(b).map_or(false, |t| t.is_long_like())
        {
            let unsigned = ty.is_unsigned()
                || self.globals.type_of(b).map_or(false, |t| t.is_unsigned());
            match op {
                BinOp::Mul => {
                    let _ = write!(self.out, "\tmov\tcx,word ptr DGROUP:_{b}+2\r\n");
                    let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{b}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                    self.helpers.insert("N_LXMUL@".to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                BinOp::Div | BinOp::Mod => {
                    let helper = match (op, unsigned) {
                        (BinOp::Div, false) => "N_LDIV@",
                        (BinOp::Mod, false) => "N_LMOD@",
                        (BinOp::Div, true)  => "N_LUDIV@",
                        (BinOp::Mod, true)  => "N_LUMOD@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{b}+2\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{b}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                // Long `g <<= h` / `g >>= h` (both globals). Same
                // helper-call shape as the K-constant K>1 path
                // (slices 263/264), but the shift count comes from
                // h's low byte: `mov cl, byte ptr DGROUP:_h`.
                // Fixture 739 (`g <<= h`).
                BinOp::Shl | BinOp::Shr => {
                    let helper = match (op, unsigned) {
                        (BinOp::Shl, _)     => "N_LXLSH@",
                        (BinOp::Shr, false) => "N_LXRSH@",
                        (BinOp::Shr, true)  => "N_LXURSH@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tmov\tcl,byte ptr DGROUP:_{b}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}+2\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                    return;
                }
                // Long `g += h` / `g -= h` / `g &= h` / `g |= h` /
                // `g ^= h` (both globals). BCC loads h's two halves
                // into AX:DX (AX=high, DX=low) — the same convention
                // used for long-to-int truncation reads — then
                // applies the op memory-direct to g, with carry/borrow
                // propagation via `adc/sbb` for arith. Fixture 734
                // (`+=`), 735 (`-=`), 736 (`&=`).
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{b}+2\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{b}\r\n");
                    let (lo_op, hi_op) = match op {
                        BinOp::Add => ("add", "adc"),
                        BinOp::Sub => ("sub", "sbb"),
                        BinOp::BitAnd => ("and", "and"),
                        BinOp::BitOr => ("or", "or"),
                        BinOp::BitXor => ("xor", "xor"),
                        _ => unreachable!(),
                    };
                    let _ = write!(
                        self.out,
                        "\t{lo_op}\tword ptr DGROUP:_{name},dx\r\n",
                    );
                    let _ = write!(
                        self.out,
                        "\t{hi_op}\tword ptr DGROUP:_{name}+2,ax\r\n",
                    );
                    return;
                }
                _ => {}
            }
        }
        // Long-like global compound shifts. Two shapes:
        //   K=1: inlined as `shl/sar/shr` + `rcl/rcr` (same as the
        //        `=` form, slices 227/229/243). Fixtures 265, 266.
        //   K>1: helper call, but with `mov cl, K` emitted BEFORE
        //        the operand loads — distinct from the `=` form
        //        (slices 228/230) where mov cl lands after the
        //        operands. Fixtures 263, 264.
        if let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && k >= 1
            && k <= 255
        {
            let unsigned = ty.is_unsigned();
            if k == 1 {
                let hi_op = match (op, unsigned) {
                    (BinOp::Shl, _)     => "shl",
                    (BinOp::Shr, false) => "sar",
                    (BinOp::Shr, true)  => "shr",
                    _ => unreachable!(),
                };
                let lo_op = if matches!(op, BinOp::Shl) { "rcl" } else { "rcr" };
                // Convention: AX=high, DX=low (the `=` form's
                // pattern). For `<<` the low-half op runs first
                // (shl dx), then rotate carries into high (rcl ax).
                // For `>>` the high runs first (sar ax), then
                // rotate down into low (rcr dx).
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}\r\n");
                if matches!(op, BinOp::Shl) {
                    let _ = write!(self.out, "\tshl\tdx,1\r\n");
                    let _ = write!(self.out, "\trcl\tax,1\r\n");
                } else {
                    let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                    let _ = write!(self.out, "\t{lo_op}\tdx,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // K > 1: helper, with `mov cl, K` FIRST (compound-form
            // reorder).
            let helper = match (op, unsigned) {
                (BinOp::Shl, _)     => "N_LXLSH@",
                (BinOp::Shr, false) => "N_LXRSH@",
                (BinOp::Shr, true)  => "N_LXURSH@",
                _ => unreachable!(),
            };
            let k_u8 = k as u8;
            let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{name}+2\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            return;
        }
        if let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
            && let Some(k) = try_const_eval(value)
        {
            let k_lo = (k & 0xFFFF) as i32;
            let k_hi = (k >> 16) as i32;
            // Arithmetic uses `83 /n` (imm8sx) so each half must fit
            // i8sx; bitwise uses `81 /n` (imm16) which fits anything
            // in 16 bits — no further restriction. Either way, k_hi
            // for arith is always 0 (the partner is `adc/sbb 0`).
            match op {
                BinOp::Add | BinOp::Sub => {
                    let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                        ("add", "adc")
                    } else {
                        ("sub", "sbb")
                    };
                    // imm8sx-fits: emit compact `83 06 ... ii` (5 bytes)
                    // — slice 251. Otherwise: wider `81 06 ... lo hi`
                    // (6 bytes) — fixture 276. The high partner is
                    // always `adc/sbb 0` (carry comes from low).
                    if let Ok(lo_i8) = i8::try_from(k_lo) {
                        let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{name},{lo_i8}\r\n");
                    } else {
                        let lo_u16 = k_lo as u16;
                        let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{name},{lo_u16}\r\n");
                    }
                    let _ = write!(self.out, "\t{hi_op}\tword ptr DGROUP:_{name}+2,0\r\n");
                    return;
                }
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let mnem = match op {
                        BinOp::BitAnd => "and",
                        BinOp::BitOr => "or",
                        BinOp::BitXor => "xor",
                        _ => unreachable!(),
                    };
                    let lo = (k_lo as i64) & 0xFFFF;
                    let hi = (k_hi as i64) & 0xFFFF;
                    let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{name},{lo}\r\n");
                    let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{name}+2,{hi}\r\n");
                    return;
                }
                _ => {}
            }
        }
        // Long-like stack local compound assigns — memory-direct,
        // same byte-width selection as the global path: arithmetic
        // uses `83` (imm8sx, 4 bytes per half on stack), bitwise uses
        // `81` (imm16, 5 bytes per half). Fixtures 288 (`+=`), 289
        // (`&=`).
        if self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && let Some(k) = try_const_eval(value)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long is never register-resident");
            };
            let k_lo = (k & 0xFFFF) as i32;
            let k_hi = (k >> 16) as i32;
            match op {
                BinOp::Add | BinOp::Sub => {
                    let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                        ("add", "adc")
                    } else {
                        ("sub", "sbb")
                    };
                    if let Ok(lo_i8) = i8::try_from(k_lo) {
                        let _ = write!(self.out, "\t{lo_op}\tword ptr {},{lo_i8}\r\n", bp_addr(off));
                    } else {
                        let lo_u16 = k_lo as u16;
                        let _ = write!(self.out, "\t{lo_op}\tword ptr {},{lo_u16}\r\n", bp_addr(off));
                    }
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {},0\r\n", bp_addr(off + 2));
                    return;
                }
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let mnem = match op {
                        BinOp::BitAnd => "and",
                        BinOp::BitOr => "or",
                        BinOp::BitXor => "xor",
                        _ => unreachable!(),
                    };
                    let lo = (k_lo as i64) & 0xFFFF;
                    let hi = (k_hi as i64) & 0xFFFF;
                    let _ = write!(self.out, "\t{mnem}\tword ptr {},{lo}\r\n", bp_addr(off));
                    let _ = write!(self.out, "\t{mnem}\tword ptr {},{hi}\r\n", bp_addr(off + 2));
                    return;
                }
                BinOp::Shl | BinOp::Shr if k >= 1 && k <= 255 => {
                    // Long stack-local compound shift. Two shapes
                    // by K — mirrors the long-global compound shift
                    // path (fixtures 263–266) but stores back to
                    // `[bp+N]` instead of `DGROUP:_g+N`. K=1 inlines
                    // shift+rotate against AX:DX (memory-dest
                    // convention: AX=high, DX=low). K>1 routes
                    // through the helper, which forces the helper
                    // convention (DX=high, AX=low) for the load —
                    // BCC's register-pair choice tracks the
                    // intermediate operation, not the final memory
                    // store. The `mov cl, K` lands FIRST (compound-
                    // form reorder). Fixtures 383 (K=1 `<<`),
                    // 384 (K=1 `>>` signed), 385 (K>1 `<<`).
                    let unsigned = self.locals.type_of(name).is_unsigned();
                    if k == 1 {
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off));
                        if matches!(op, BinOp::Shl) {
                            self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                            self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                        } else {
                            let hi_op = if unsigned { "shr" } else { "sar" };
                            let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                            self.out.extend_from_slice(b"\trcr\tdx,1\r\n");
                        }
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                    } else {
                        let helper = match (op, unsigned) {
                            (BinOp::Shl, _)     => "N_LXLSH@",
                            (BinOp::Shr, false) => "N_LXRSH@",
                            (BinOp::Shr, true)  => "N_LXURSH@",
                            _ => unreachable!(),
                        };
                        let k_u8 = (k & 0xFF) as u8;
                        let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                        let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                        self.helpers.insert(helper.to_string());
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    }
                    return;
                }
                _ => {}
            }
        }
        // Long stack-local compound `+=` / `-=` / `&=` / `|=` / `^=`
        // with a long stack-local RHS (non-constant). Load y into
        // AX:DX (AX=high, DX=low — globals convention since dest is
        // memory), then memory-direct store with `<op> [mem], reg`.
        // Arith uses carry/borrow propagation, bitwise repeats the
        // same mnemonic. Fixtures 339, 340, 342, 343, 344.
        if self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && let Some((lo_op, hi_op)) = long_pair_op(op)
            && let ExprKind::Ident(rhs_name) = &value.kind
            && self.locals.has(rhs_name)
            && self.locals.type_of(rhs_name).is_long_like()
        {
            let (LocalLocation::Stack(x_off), LocalLocation::Stack(y_off)) =
                (self.locals.location_of(name), self.locals.location_of(rhs_name))
            else {
                unreachable!("long is never register-resident");
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(y_off + 2));
            let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(y_off));
            let _ = write!(self.out, "\t{lo_op}\tword ptr {},dx\r\n", bp_addr(x_off));
            let _ = write!(self.out, "\t{hi_op}\tword ptr {},ax\r\n", bp_addr(x_off + 2));
            return;
        }
        // Long stack-local compound `*=` with a long stack-local RHS.
        // Helper convention swaps from the `z = x * y` shape: here
        // the destination is `x`, so x goes to DX:AX (where the
        // helper deposits the result) and y goes to CX:BX. Fixture
        // 345.
        if self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && matches!(op, BinOp::Mul)
            && let ExprKind::Ident(rhs_name) = &value.kind
            && self.locals.has(rhs_name)
            && self.locals.type_of(rhs_name).is_long_like()
        {
            let (LocalLocation::Stack(x_off), LocalLocation::Stack(y_off)) =
                (self.locals.location_of(name), self.locals.location_of(rhs_name))
            else {
                unreachable!("long is never register-resident");
            };
            let _ = write!(self.out, "\tmov\tcx,word ptr {}\r\n", bp_addr(y_off + 2));
            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(y_off));
            let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(x_off + 2));
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(x_off));
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(x_off + 2));
            let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(x_off));
            return;
        }
        // Long stack-local compound `/=` / `%=` with a long stack-
        // local RHS. Same push convention as the `z = x / y` shape
        // (fixtures 337/338) but result lands back in x. Fixtures
        // 346, 347.
        if self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let ExprKind::Ident(rhs_name) = &value.kind
            && self.locals.has(rhs_name)
            && self.locals.type_of(rhs_name).is_long_like()
        {
            let (LocalLocation::Stack(x_off), LocalLocation::Stack(y_off)) =
                (self.locals.location_of(name), self.locals.location_of(rhs_name))
            else {
                unreachable!("long is never register-resident");
            };
            let unsigned = self.locals.type_of(name).is_unsigned()
                || self.locals.type_of(rhs_name).is_unsigned();
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(y_off + 2));
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(y_off));
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(x_off + 2));
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(x_off));
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(x_off + 2));
            let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(x_off));
            return;
        }
        // Int/uint global compound shift with constant RHS. K in
        // 1..=3 unrolls into K `<shl|sar|shr> word ptr DGROUP:_g, 1`
        // instructions (each 4 bytes); K >= 4 switches to the CL form
        // `mov cl, K; <shl|sar|shr> word ptr DGROUP:_g, cl` (6 bytes
        // total — wins at K=4 where unroll cost is 16 bytes). Fixtures
        // 539 (K=2 unroll), 3374 (K=4 CL form).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && k >= 1
        {
            let signed = !gty.is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            if k <= 3 {
                for _ in 0..k {
                    let _ = write!(
                        self.out,
                        "\t{mnem}\tword ptr DGROUP:_{name},1\r\n",
                    );
                }
            } else {
                let k8 = (k & 0xFF) as u8;
                let _ = write!(self.out, "\tmov\tcl,{k8}\r\n");
                let _ = write!(
                    self.out,
                    "\t{mnem}\tword ptr DGROUP:_{name},cl\r\n",
                );
            }
            return;
        }
        // Int/uint global compound add/sub with another global as
        // RHS — `mov ax, [_b]; <add|sub> word ptr DGROUP:_a, ax`.
        // Fixture 571 (`a += b;`). The store-back uses Grp1 r/m16,
        // r16 (`01 06` or `29 06`) — no IR change needed, the asm
        // syntax `add word ptr DGROUP:_a, ax` is already routed.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Add | BinOp::Sub)
            && let ExprKind::Ident(rhs_name) = &value.kind
            && let Some(rhs_ty) = self.globals.type_of(rhs_name)
            && matches!(rhs_ty, Type::Int | Type::UInt)
        {
            let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{rhs_name}\r\n");
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr DGROUP:_{name},ax\r\n",
            );
            return;
        }
        // Int/uint global compound add/sub with constant RHS —
        // memory-direct `add|sub word ptr DGROUP:_g, K`. Fixture
        // 519 (`g += 5`). TASM picks the imm8sx form when K fits a
        // signed byte; the asm syntax doesn't differ.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Add | BinOp::Sub)
            && let Some(v) = try_const_eval(value)
        {
            // K=1 peephole: `inc/dec word ptr [_g]` (4 bytes) instead
            // of `add/sub word ptr [_g], 1` (5 bytes). Fixture 3497.
            let v_masked = v & 0xFFFF;
            if v_masked == 1 {
                let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{name}\r\n");
                return;
            }
            let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr DGROUP:_{name},{v_masked}\r\n",
            );
            return;
        }
        // Int/uint global compound bitwise op with constant RHS —
        // memory-direct `<op> word ptr DGROUP:_g, K`. Fixture 517
        // (`g &= 15`). BCC always emits the imm16 form here; the
        // imm8sx peephole is not used for bitwise ops.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && let Some(v) = try_const_eval(value)
        {
            let mnem = match op {
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let v16 = v & 0xFFFF;
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr DGROUP:_{name},{v16}\r\n",
            );
            return;
        }
        // Int/uint global compound add/sub/bit* with a non-const
        // RHS (int/uint, or char/uchar that widens through AX).
        // RHS can be a local, another global, or an array element
        // — `emit_expr_to_ax` handles all of them and emits
        // `cbw` / `mov ah, 0` for the byte-to-int widening. The
        // same memory-direct `<op> word ptr DGROUP:_<name>, ax`
        // finishes the int compound. Fixtures 794 (`g += char c`),
        // 799 (int local RHS), 812 (char global RHS), 821
        // (`g += a[1]` int array element).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt | Type::Char | Type::UChar)
        {
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr DGROUP:_{name},ax\r\n",
            );
            return;
        }
        // Int/uint global compound `*=` with an int/uint local
        // RHS. No widening needed, so BCC uses `imul word ptr
        // [bp+N]` directly (the F7 6E reg=5 form): `mov ax, _g;
        // imul word ptr <rhs>; mov _g, ax`. Fixture 802.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul)
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                unreachable!();
            };
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\timul\tword ptr {}\r\n", bp_addr(off));
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            return;
        }
        // Int/uint global compound `*=` / `/=` / `%=` with a
        // constant RHS. For `*=`, BCC materializes the constant in
        // DX and uses `imul dx`. For `/=` and `%=`, the divisor goes
        // into BX (DX would be clobbered by cwd/xor). Fixtures 3494
        // (`g *= 3`), 3495 (`g /= 4`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let Some(k) = try_const_eval(value)
        {
            let k16 = k & 0xFFFF;
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\tmov\tdx,{k16}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                self.out.extend_from_slice(b"\timul\tdx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tbx,{k16}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
                let (widen, mnem) = if gty.is_unsigned() {
                    (&b"\txor\tdx,dx\r\n"[..], "div")
                } else {
                    (&b"\tcwd\t\r\n"[..], "idiv")
                };
                self.out.extend_from_slice(widen);
                let _ = write!(self.out, "\t{mnem}\tbx\r\n");
                let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
                );
            }
            return;
        }
        // Int/uint global compound `*=` / `/=` / `%=` with `*p`
        // where `p` is a register-resident pointer (typically SI
        // for int*). `imul`/`idiv word ptr [si]` uses the deref-
        // through-register addressing form. Fixture 825.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let ExprKind::Deref(inner) = &value.kind
            && let ExprKind::Ident(p_name) = &inner.kind
            && !self.globals.contains(p_name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
            && !reg.is_byte()
            && let Some(pty) = self.locals.type_of(p_name).pointee()
            && matches!(pty, Type::Int | Type::UInt)
        {
            let reg_name = reg.name();
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr [{reg_name}]\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tidiv\tword ptr [{reg_name}]\r\n");
                let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
                );
            }
            return;
        }
        // Int/uint global compound `*=` / `/=` / `%=` with another
        // int/uint value in a DGROUP slot — `imul`/`idiv word ptr
        // <group>:<sym>[+offset]`. Same shape as the local-RHS
        // path, just with a DGROUP operand. Accepts plain
        // identifiers (fixture 809, 810), constant array indices
        // (`a[K]` — fixture 824), and struct members (`s.x` —
        // fixture 826's sibling).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let Some((rhs_addr, rhs_ty)) = self.global_int_rhs_addr(&value.kind)
            && matches!(rhs_ty, Type::Int | Type::UInt)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr {rhs_addr}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            } else {
                // Unsigned LHS: `xor dx, dx; div` instead of `cwd;
                // idiv`. Fixture 949.
                let (widen, mnem) = if gty.is_unsigned() {
                    (&b"\txor\tdx,dx\r\n"[..], "div")
                } else {
                    (&b"\tcwd\t\r\n"[..], "idiv")
                };
                self.out.extend_from_slice(widen);
                let _ = write!(self.out, "\t{mnem}\tword ptr {rhs_addr}\r\n");
                let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
                );
            }
            return;
        }
        // Int/uint global compound `<<=` / `>>=` with int/uint/char/
        // uchar RHS in any memory slot (local, global, array elem,
        // struct member). CL is loaded from the low byte of the
        // shift count; the shift acts memory-direct on the global.
        // Fixture 805 (local), 811 (global), 826 (member).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt | Type::Char | Type::UChar)
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let unsigned = gty.is_unsigned();
            let mnem = match (op, unsigned) {
                (BinOp::Shl, _) => "shl",
                (BinOp::Shr, false) => "sar",
                (BinOp::Shr, true) => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tcl,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{name},cl\r\n");
            return;
        }
        // Int/uint global compound `/=` / `%=` with an int/uint
        // local RHS. Same mem-direct shape as Mul, but with
        // `cwd` for the dividend sign-extension and `idiv word
        // ptr [bp+N]`: `mov ax, _g; cwd; idiv word ptr <rhs>;
        // mov _g, {ax|dx}`. Fixture 803.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::UInt)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                unreachable!();
            };
            // Unsigned LHS: `xor dx, dx; div` instead of `cwd; idiv`.
            // Fixture 949.
            let (widen, mnem) = if gty.is_unsigned() {
                (&b"\txor\tdx,dx\r\n"[..], "div")
            } else {
                (&b"\tcwd\t\r\n"[..], "idiv")
            };
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            self.out.extend_from_slice(widen);
            let _ = write!(self.out, "\t{mnem}\tword ptr {}\r\n", bp_addr(off));
            let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
            );
            return;
        }
        // Int/uint global compound `*=` with a char/uchar RHS
        // (local or global). `emit_expr_to_ax` materializes the
        // widened byte in AX, but AX is needed for the LHS load
        // (which feeds `imul`). BCC inserts a `push ax; ...; pop
        // dx` shuffle to park the widened RHS in DX while AX
        // takes the LHS. `imul dx` then computes DX:AX = AX * DX
        // (signed); the low-16 store back ignores DX. Note BCC
        // uses signed `imul` even for `uchar` — the zero-extended
        // dividend is positive so the low-16 product is
        // identical. Fixture 796 (local), 815 (global).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul)
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Char | Type::UChar)
            && let ExprKind::Ident(_) = &value.kind
        {
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            self.out.extend_from_slice(b"\tpop\tdx\r\n");
            self.out.extend_from_slice(b"\timul\tdx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            return;
        }
        // Int/uint global compound `/=` / `%=` with a char/uchar
        // RHS (local or global). Similar register-pressure dance
        // as the Mul arm, but BCC parks the widened RHS in BX
        // (Div uses BX by convention; Mul used DX). The LHS load
        // now needs both AX (dividend low) and DX (sign-extend
        // via cwd), so the push/pop must stash AX before the cwd:
        // `mov al, <c>; cbw; push ax; mov ax, <lhs>; cwd; pop
        // bx; idiv bx; mov <lhs>, ax` (or `, dx` for `%=`).
        // Signed `idiv` works for `uchar` RHS too — the
        // zero-extended divisor is positive. Fixture 798 (local),
        // 816 (global).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some(ty_rhs) = self.rhs_type_for_long_widening(&value.kind)
            && matches!(ty_rhs, Type::Char | Type::UChar)
            && let ExprKind::Ident(_) = &value.kind
        {
            let _ = ty_rhs;
            self.emit_expr_to_ax(value);
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tpop\tbx\r\n");
            self.out.extend_from_slice(b"\tidiv\tbx\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},{result_reg}\r\n",
            );
            return;
        }
        // Char/uchar global compound `*= K` — two shapes:
        //  - K is power of two: unroll `shl al, 1` log2(K) times
        //    around an AL load-modify-store. Fixture 690.
        //  - otherwise: widen via `cbw` then 16-bit signed multiply
        //    through DX (`mov dx, K; imul dx`). Note BCC picks DX
        //    as the multiplier register here while `/=` uses BX —
        //    presumably because `imul dx` doesn't touch a register
        //    `div bx` wouldn't already need free. Fixture 693
        //    (`g *= 3` → `mov al, _g; cbw; mov dx, 3; imul dx;
        //    mov _g, al`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Mul)
            && let Some(k) = try_const_eval(value)
        {
            let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
            if k > 0 && (k & (k - 1)) == 0 && k <= 256 {
                let shifts = k.trailing_zeros();
                if shifts <= 3 {
                    for _ in 0..shifts {
                        self.out.extend_from_slice(b"\tshl\tal,1\r\n");
                    }
                } else {
                    let _ = write!(self.out, "\tmov\tcl,{shifts}\r\n");
                    self.out.extend_from_slice(b"\tshl\tal,cl\r\n");
                }
            } else {
                let v16 = k & 0xFFFF;
                self.out.extend_from_slice(b"\tcbw\t\r\n");
                let _ = write!(self.out, "\tmov\tdx,{v16}\r\n");
                self.out.extend_from_slice(b"\timul\tdx\r\n");
            }
            let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
            return;
        }
        // Char/uchar global compound `/=` / `%=` with constant K.
        // BCC widens the global to AX, loads K into BX,
        // sign-extends DX:AX with cwd, then `idiv bx`. For `/=`
        // stores AL (quotient) back; for `%=` stores DL (low byte
        // of remainder). Fixture 691 (signed `g /= 4`) and
        // fixture 694 (unsigned `g /= 4`).
        //
        // Signed widening uses `cbw`; unsigned uses `mov ah, 0`.
        // Interestingly BCC keeps the `cwd; idiv bx` (signed
        // divide) sequence even for `unsigned char` — the
        // zero-extended dividend fits in [0, 255] which is well
        // within the positive `idiv` range, so signed division
        // gives the right answer.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some(v) = try_const_eval(value)
        {
            let v16 = v & 0xFFFF;
            let unsigned = gty.is_unsigned();
            let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            }
            let _ = write!(self.out, "\tmov\tbx,{v16}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tidiv\tbx\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "al" } else { "dl" };
            let _ = write!(
                self.out,
                "\tmov\tbyte ptr DGROUP:_{name},{result_reg}\r\n",
            );
            return;
        }
        // Char/uchar global compound `<<=` / `>>=` with constant K.
        // BCC unrolls into K memory-direct shift-by-1 instructions
        // (one per shift): `shl|sar|shr byte ptr _g, 1`. Signedness
        // picks SAR vs SHR for `>>=` (signed char → SAR). Fixture
        // 688 (`g <<= 2` → two `shl byte ptr _g, 1`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && k >= 1
            && k <= 8
        {
            let unsigned = gty.is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if unsigned => "shr",
                BinOp::Shr => "sar",
                _ => unreachable!(),
            };
            for _ in 0..k {
                let _ = write!(
                    self.out,
                    "\t{mnem}\tbyte ptr DGROUP:_{name},1\r\n",
                );
            }
            return;
        }
        // Char/uchar global compound with constant byte RHS, two
        // shapes:
        //  - Arith (`+=` / `-=`): load-modify-store through AL —
        //    `mov al, _g; <add|sub> al, K; mov _g, al`. BCC
        //    canonicalizes `c -= K` as `add al, (256 - K)` (matches
        //    the broader add-neg-over-sub-const pattern). Fixtures
        //    683 / 684.
        //  - Bitwise (`&=` / `|=` / `^=`): memory-direct
        //    `<op> byte ptr _g, K` (one instruction). Fixture 685.
        //    Asymmetry vs the int-global path (which uses
        //    memory-direct for arith too) is empirical; BCC seems to
        //    pick mem-direct for bitwise but always load-modify-
        //    store for byte arith.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some(v) = try_const_eval(value)
        {
            let v8 = (v & 0xFF) as u8;
            if matches!(op, BinOp::Add | BinOp::Sub) {
                let imm = if matches!(op, BinOp::Add) { v8 } else { v8.wrapping_neg() };
                let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
                // K=±1: BCC uses inc/dec al (2 bytes) instead of
                // add al, K (2 bytes too, but BCC's preference).
                // Fixture 2891 (`char g; g += 1;` → `mov al, [g];
                // inc al; mov [g], al`).
                if imm == 1 {
                    self.out.extend_from_slice(b"\tinc\tal\r\n");
                } else if imm == 0xFF {
                    self.out.extend_from_slice(b"\tdec\tal\r\n");
                } else {
                    let _ = write!(self.out, "\tadd\tal,{imm}\r\n");
                }
                let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
            } else {
                let mnem = match op {
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let _ = write!(
                    self.out,
                    "\t{mnem}\tbyte ptr DGROUP:_{name},{v8}\r\n",
                );
            }
            return;
        }
        // Char/uchar global compound `*=` with variable byte RHS.
        // BCC widens through AL only (no sign-extension needed for
        // 8-bit multiply), then 8-bit `imul byte ptr <src>` and
        // store the low byte AL back. Fixture 695 (`g *= d` →
        // `mov al, _g; imul byte ptr [bp-1]; mov _g, al`). 8-bit
        // multiply doesn't differentiate signed/unsigned at the
        // low-byte level, so BCC picks `imul` for both.
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Mul)
            && try_const_eval(value).is_none()
        {
            let src = self.resolve_operand_source(value);
            let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
            self.out.extend_from_slice(b"\timul\t");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
            return;
        }
        // Char/uchar global compound `/=` / `%=` with variable byte
        // RHS. Same 8-bit divide pattern as the local form
        // (fixtures 673 / 677): signed uses `cbw; idiv byte ptr
        // <src>`, unsigned uses `mov ah, 0; div al, byte ptr
        // <src>` with explicit AL accumulator in the TASM listing.
        // Store quotient (AL) for `/=`, remainder (AH) for `%=`.
        // Fixture 696 (signed `g /= d`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Div | BinOp::Mod)
            && try_const_eval(value).is_none()
        {
            let unsigned = gty.is_unsigned();
            let src = self.resolve_operand_source(value);
            let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                self.out.extend_from_slice(b"\tdiv\tal,");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
                self.out.extend_from_slice(b"\tidiv\t");
            }
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "al" } else { "ah" };
            let _ = write!(
                self.out,
                "\tmov\tbyte ptr DGROUP:_{name},{result_reg}\r\n",
            );
            return;
        }
        // Char/uchar global compound `<<=` / `>>=` with variable
        // byte RHS. BCC loads the shift count into CL then issues a
        // memory-direct `<shl|sar|shr> byte ptr _g, cl` — no AL
        // detour (the global stays in memory across the op).
        // Fixture 697 (`g <<= d` → `mov cl, byte ptr [bp-1]; shl
        // byte ptr _g, cl`).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && try_const_eval(value).is_none()
        {
            let unsigned = gty.is_unsigned();
            let src = self.resolve_operand_source(value);
            self.out.extend_from_slice(b"\tmov\tcl,");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if unsigned => "shr",
                BinOp::Shr => "sar",
                _ => unreachable!(),
            };
            let _ = write!(
                self.out,
                "\t{mnem}\tbyte ptr DGROUP:_{name},cl\r\n",
            );
            return;
        }
        // Char/uchar global compound `+=` / `-=` / `&=` / `|=` /
        // `^=` with a non-constant byte RHS. BCC loads the RHS into
        // AL and then applies the op memory-direct to the global:
        // `mov al, byte ptr <src>; <op> byte ptr DGROUP:_<g>, al`
        // (fixtures 680/681/682).
        if let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Char | Type::UChar)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && try_const_eval(value).is_none()
        {
            let src = self.resolve_operand_source(value);
            self.out.extend_from_slice(b"\tmov\tal,");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(
                self.out,
                "\t{mnem}\tbyte ptr DGROUP:_{name},al\r\n",
            );
            return;
        }
        // Int/uint stack-local compound `+=` / `-=` / `&=` / `|=` /
        // `^=` with a constant RHS — same memory-direct shape as
        // the global path. Uses imm8sx (`83 /op disp8 ii`) when K
        // fits a signed byte; tasm picks the encoding. Fixture 1216
        // (`unsigned a -= 3`).
        let local_ty = self.locals.type_of(name).clone();
        if local_ty.is_int_like()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some(v) = try_const_eval(value)
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            let v16 = v & 0xFFFF;
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr {},{v16}\r\n",
                bp_addr(off),
            );
            return;
        }
        // Stack-resident int local + simple lvalue rhs: load rhs
        // into AX, then memory-op against the local. Mirrors what
        // BCC emits when neither operand made it into a register.
        // Fixture 1980 (`c += d` with both on stack).
        if local_ty.is_int_like()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
            && let Some(rhs_addr) = self.int_lvalue_addr(value)
        {
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {rhs_addr}\r\n");
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr {},ax\r\n",
                bp_addr(off),
            );
            return;
        }
        // Stack-resident int local += reg-resident int local: emit
        // `<mnem> word ptr [bp-N], <reg>` directly — BCC collapses
        // the round-trip through AX when the rhs is already in a
        // 16-bit register. Fixture 1980 (`e += a` with e stack, a
        // in SI).
        if local_ty.is_int_like()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
            && let ExprKind::Ident(rhs_name) = &value.kind
            && self.locals.has(rhs_name)
            && self.locals.type_of(rhs_name).is_int_like()
            && let LocalLocation::Reg(rhs_reg) = self.locals.location_of(rhs_name)
            && !rhs_reg.is_byte()
        {
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr {},{}\r\n",
                bp_addr(off),
                rhs_reg.name(),
            );
            return;
        }
        // Stack-resident int local += general expression rhs: evaluate
        // the rhs into AX (which is free to clobber BX/DX), then op it
        // against the local in memory (`<mnem> word ptr [bp-N], ax`).
        // This is the catch-all for rhs forms that aren't a constant, a
        // simple lvalue, or a register-resident ident — e.g. an indexed
        // pointer deref `(*p)[c]`. Fixture 4217 (`sum += (*p)[c]`).
        if local_ty.is_int_like()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            self.emit_expr_to_ax(value);
            let _ = write!(
                self.out,
                "\t{mnem}\tword ptr {},ax\r\n",
                bp_addr(off),
            );
            return;
        }
        // Stack-resident CHAR local compound with a constant rhs. BCC codes both
        // arith (`+=`/`-=`) and bitwise (`&=`/`|=`/`^=`) AL-through for a char
        // STACK local (`mov al,[c]; <op> al,K; mov [c],al`), with the ±1 inc/dec
        // peephole for arith. (Char GLOBALs differ — bitwise is memory-direct
        // there — so this is stack-local-specific.) Fixtures 4320 (`c = c + 1`),
        // 4321 (`c = c | 8`).
        if local_ty.is_char_like()
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
            && let Some(v) = try_const_eval(value)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let dest = bp_addr(off);
            let vm = (v & 0xFF) as u8;
            let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
            match op {
                BinOp::Add if vm == 1 => self.out.extend_from_slice(b"\tinc\tal\r\n"),
                BinOp::Sub if vm == 1 => self.out.extend_from_slice(b"\tdec\tal\r\n"),
                BinOp::Add => {
                    let _ = write!(self.out, "\tadd\tal,{vm}\r\n");
                }
                BinOp::Sub => {
                    let _ = write!(self.out, "\tadd\tal,{}\r\n", vm.wrapping_neg());
                }
                BinOp::BitAnd => {
                    let _ = write!(self.out, "\tand\tal,{vm}\r\n");
                }
                BinOp::BitOr => {
                    let _ = write!(self.out, "\tor\tal,{vm}\r\n");
                }
                BinOp::BitXor => {
                    let _ = write!(self.out, "\txor\tal,{vm}\r\n");
                }
                _ => unreachable!(),
            }
            let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
            return;
        }
        let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
            panic!(
                "compound assignment on stack-resident `{name}` not yet supported (no fixture)"
            );
        };
        // Char compound on a byte-register local.
        //
        // BCC splits two ways:
        //  - `+=` / `-=`: round-trip through AL so the 2-byte AL
        //    accumulator forms (`04/2C ii`) can be used. With the
        //    AL ±1 peephole (`fe c0/c8`) the total is still 6 bytes.
        //  - `&=` / `|=` / `^=`: direct `<and|or|xor> <reg>, K`
        //    (`80 /4|/1|/6 reg ii`, 3 bytes). Fixture 556 (`c &= 31`
        //    on DL) shows the direct form is preferred for bitwise.
        if reg.is_byte()
            && matches!(op, BinOp::Add | BinOp::Sub)
            && let Some(v) = try_const_eval(value)
        {
            let v8 = v & 0xFF;
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            if v8 == 1 {
                let inc_mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                let _ = write!(self.out, "\t{inc_mnem}\tal\r\n");
            } else if matches!(op, BinOp::Sub) {
                // BCC canonicalizes `c -= K` (char, K != 1) as `add
                // al, -K` rather than `sub al, K` (same length, same
                // result mod 256). Fixture 623 (`c -= 3` → `04 FD`).
                let neg = (0u32.wrapping_sub(v8 as u32)) & 0xFF;
                let neg_i8 = neg as i8;
                let _ = write!(self.out, "\tadd\tal,{neg_i8}\r\n");
            } else {
                let _ = write!(self.out, "\tadd\tal,{v8}\r\n");
            }
            let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            return;
        }
        if reg.is_byte()
            && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && let Some(v) = try_const_eval(value)
        {
            let v8 = v & 0xFF;
            let mnem = match op {
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\t{},{v8}\r\n", reg.name());
            return;
        }
        // Char compound shift on a byte-register local: unroll into K
        // `<shl|sar|shr> <reg>, 1` instructions directly on the
        // register — no AL round-trip. Fixture 535 (`char c <<= 2`
        // → two `shl dl, 1`). The 8086 has no `r/m8, imm8` shift, so
        // BCC always unrolls for small K and switches to a CL-loop
        // for larger K (threshold not yet pinned).
        if reg.is_byte()
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && k >= 1
            && k <= 8
        {
            let signed = !self.locals.type_of(name).is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            for _ in 0..k {
                let _ = write!(self.out, "\t{mnem}\t{},1\r\n", reg.name());
            }
            return;
        }
        // Char compound `*= K` where K is a small power of two —
        // round-trip through AL and unroll `shl al, 1`. Fixture 633
        // (`c *= 4` → `mov al, dl; shl al, 1; shl al, 1; mov dl, al`).
        if reg.is_byte()
            && matches!(op, BinOp::Mul)
            && let Some(k) = try_const_eval(value)
            && k > 0
            && (k & (k - 1)) == 0
            && k <= 256
        {
            let shifts = k.trailing_zeros();
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            if shifts <= 3 {
                for _ in 0..shifts {
                    self.out.extend_from_slice(b"\tshl\tal,1\r\n");
                }
            } else {
                let _ = write!(self.out, "\tmov\tcl,{shifts}\r\n");
                self.out.extend_from_slice(b"\tshl\tal,cl\r\n");
            }
            let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            return;
        }
        // Char compound `/= K` / `%= K` — widen char to AX (cbw),
        // load divisor into BX, then signed idiv. For `/=` store
        // AL back; for `%=` store DL (the remainder's low byte).
        // Fixture 640 (`c /= 4` → `mov al, cl; cbw; mov bx, 4;
        // cwd; idiv bx; mov cl, al`). Shift unroll wouldn't match
        // signed semantics (rounding differs for negative).
        if reg.is_byte()
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some(v) = try_const_eval(value)
        {
            let v16 = v & 0xFFFF;
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            self.out.extend_from_slice(b"\tcbw\t\r\n");
            let _ = write!(self.out, "\tmov\tbx,{v16}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            self.out.extend_from_slice(b"\tidiv\tbx\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "al" } else { "dl" };
            let _ = write!(self.out, "\tmov\t{},{result_reg}\r\n", reg.name());
            return;
        }
        // Char compound `*= K` (constant RHS): load the byte to AL,
        // widen via `cbw`, materialize K in DX, signed `imul dx`,
        // then store AL back. Fixture 1295 (`c *= 3` for char c in
        // DL).
        if reg.is_byte()
            && matches!(op, BinOp::Mul)
            && let Some(k) = try_const_eval(value)
        {
            let k16 = k & 0xFFFF;
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            self.out.extend_from_slice(b"\tcbw\t\r\n");
            let _ = write!(self.out, "\tmov\tdx,{k16}\r\n");
            self.out.extend_from_slice(b"\timul\tdx\r\n");
            let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            return;
        }
        // Char compound `*=` with a non-constant byte RHS: load the
        // dst into AL, then 8-bit `imul byte ptr <src>` (AX = AL *
        // src), then store AL back to the byte register. Fixture
        // 672 (`c *= d` → `mov al, dl; imul byte ptr [bp-1]; mov
        // dl, al`).
        if reg.is_byte() && matches!(op, BinOp::Mul) {
            let src = self.resolve_operand_source(value);
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            self.out.extend_from_slice(b"\timul\t");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            return;
        }
        // Char compound `/=` / `%=` with a non-constant byte RHS.
        // Signed: load dst into AL, `cbw` to sign-extend, 8-bit
        // `idiv byte ptr <src>` (AL=quotient, AH=remainder), then
        // store the quotient (or AH for `%=`) back. Fixture 673
        // (`c /= d` → `mov al, dl; cbw; idiv byte ptr [bp-1]; mov
        // dl, al`).
        //
        // Unsigned: zero-extend via `mov ah, 0`, then 8-bit `div
        // al, byte ptr <src>` — note BCC emits the explicit `al,`
        // operand in the TASM listing. Fixture 677 (`c /= d` with
        // unsigned char → `mov al, bl; mov ah, 0; div al, byte
        // ptr [bp-1]; mov bl, al`).
        if reg.is_byte() && matches!(op, BinOp::Div | BinOp::Mod) {
            let unsigned = self.locals.type_of(name).is_unsigned();
            let src = self.resolve_operand_source(value);
            let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                self.out.extend_from_slice(b"\tdiv\tal,");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
                self.out.extend_from_slice(b"\tidiv\t");
            }
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let result_reg = if matches!(op, BinOp::Div) { "al" } else { "ah" };
            let _ = write!(self.out, "\tmov\t{},{result_reg}\r\n", reg.name());
            return;
        }
        // Char compound `<<=` / `>>=` with a non-constant RHS:
        // load the RHS byte into CL with `mov cl, byte ptr <src>`,
        // then shift the byte register by CL (`sar dl, cl` for
        // signed `>>=`, `shr` for unsigned, `shl` for `<<=`).
        // Fixture 670 (`c >>= d` with c in DL, d at [bp-1]).
        if reg.is_byte() && matches!(op, BinOp::Shl | BinOp::Shr) {
            let signed = !self.locals.type_of(name).is_unsigned();
            let src = self.resolve_operand_source(value);
            self.out.extend_from_slice(b"\tmov\tcl,");
            self.out.extend_from_slice(src.byte().as_bytes());
            self.out.extend_from_slice(b"\r\n");
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\t{},cl\r\n", reg.name());
            return;
        }
        // Char compound `+=` / `-=` / `&=` / `|=` / `^=` with a
        // non-constant RHS. BCC's pattern depends on the RHS's type:
        //  - Char RHS (`c += d` for two char locals): load the RHS
        //    byte into AL, then `<op> <c>, al` directly. Fixtures
        //    665 (`c += d`), 666–669.
        //  - Int-lvalue RHS (`c += n` where n is int): load c into
        //    AL, then `<op> al, byte ptr <n>` against the int's low
        //    byte, then store AL back. Fixture 1213.
        if reg.is_byte()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && !matches!(&value.kind, ExprKind::BinOp { .. })
        {
            // BCC uses the "load c into AL then op against rhs-low-
            // byte then store back" pattern ONLY for Add/Sub. Bitwise
            // operations work byte-wise: BCC keeps the shorter
            // `mov al, [rhs_low]; <op> <c>, al` shape. Fixture 1254
            // (`c |= n`).
            let rhs_is_int_lvalue = if let ExprKind::Ident(rhs_name) = &value.kind {
                self.locals.has(rhs_name) && self.locals.type_of(rhs_name).is_int_like()
            } else {
                false
            } && matches!(op, BinOp::Add | BinOp::Sub);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let src = self.resolve_operand_source(value);
            if rhs_is_int_lvalue {
                // Load c to AL, operate against RHS's byte form,
                // store AL back.
                let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                self.out.extend_from_slice(b"\t");
                self.out.extend_from_slice(mnem.as_bytes());
                self.out.extend_from_slice(b"\tal,");
                self.out.extend_from_slice(src.byte().as_bytes());
                self.out.extend_from_slice(b"\r\n");
                let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
            } else {
                // Char-char pattern: load RHS byte, op against c.
                self.out.extend_from_slice(b"\tmov\tal,");
                self.out.extend_from_slice(src.byte().as_bytes());
                self.out.extend_from_slice(b"\r\n");
                let _ = write!(self.out, "\t{mnem}\t{},al\r\n", reg.name());
            }
            return;
        }
        // Char compound `+=` / `-=` / `&=` / `|=` / `^=` with a
        // non-char RHS expression (typically a BinOp like `a * b`).
        // BCC routes the int result through AX (via the usual
        // expr-to-AX paths) and then computes the byte result via
        // `mov dl, <c_reg>; <op> dl, al; mov <c_reg>, dl`. Fixture
        // 1314 (`c += a * b`, `c` char in BL).
        if reg.is_byte()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && matches!(&value.kind, ExprKind::BinOp { .. })
        {
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            // Byte-narrow peephole: when the RHS is `<int-lvalue>
            // <Mul-by-pow2 or Shl> K`, BCC computes the multiply in
            // byte form (`mov al, byte ptr <src>; shl al, 1...`)
            // rather than word form. Multiplication / left-shift mod
            // 256 commute with the eventual byte-truncation at the
            // char store, so the byte path is correctness-preserving
            // and the same total bytes. Fixture 1430 (`c += a * 2`).
            let byte_narrow_done = if let ExprKind::BinOp { op: inner_op, left, right } = &value.kind
                && let Some((src_name, src_off, _)) = self.try_lvalue_chain_addr(left)
                && let Some(src_addr) = self.resolve_chain_addr(&src_name, src_off)
                && let Some(k) = try_const_eval(right)
                && k >= 1
                && k <= 7
            {
                let shifts: Option<u32> = match inner_op {
                    BinOp::Shl => Some(k as u32),
                    BinOp::Mul if k.is_power_of_two() => Some(k.trailing_zeros()),
                    _ => None,
                };
                if let Some(s) = shifts
                    && s >= 1
                    && s <= 3
                {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {src_addr}\r\n");
                    for _ in 0..s {
                        self.out.extend_from_slice(b"\tshl\tal,1\r\n");
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !byte_narrow_done {
                self.emit_expr_to_ax(value);
            }
            let _ = write!(self.out, "\tmov\tdl,{}\r\n", reg.name());
            let _ = write!(self.out, "\t{mnem}\tdl,al\r\n");
            let _ = write!(self.out, "\tmov\t{},dl\r\n", reg.name());
            return;
        }
        assert!(
            !reg.is_byte(),
            "compound assignment on a char (byte-register) target not yet supported (no fixture)"
        );
        // Complex RHS that resolve_operand_source can't reduce to a
        // single memory/register operand: evaluate it into AX first,
        // then apply the op via `<mnem> <reg>, ax`. Covers:
        //   - `s += a * b` / `a |= (1 << b)` / `a -= b - 1` — RHS is
        //     a BinOp (fixtures 1255, 1258, 1315).
        //   - `s += a[i]` where a is a global array and i is variable
        //     — RHS is variable-indexed ArrayIndex (fixtures 1385,
        //     1462, etc.).
        // Restricted to ops where AX-as-RHS is unambiguous:
        // Add/Sub/BitAnd/BitOr/BitXor. Mul/Shl/Shr/Div/Mod use AX/CL/
        // DX implicitly and route through their own arms below.
        // `<reg> += <global-arr>[<var>]` — variable-indexed global
        // array element as the RHS of a compound add. Emit the
        // bx-indexed memory load + add directly (`03 (mod=10 reg=<r>
        // r/m=111) lo hi`, 4 bytes) instead of the load-to-ax + add
        // pair (6 bytes). Fixture 1462 (`s += a[i]` for int global
        // array, var index, reg-resident s).
        if matches!(op, BinOp::Add)
            && let ExprKind::ArrayIndex { array, index } = &value.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && let Some(gty) = self.globals.type_of(arr_name)
            && let Some(elem_ty) = gty.array_elem()
            && elem_ty.is_int_like()
            && !reg.is_byte()
        {
            let elem_ty = elem_ty.clone();
            self.emit_index_into_bx(index, &elem_ty);
            let _ = write!(
                self.out,
                "\tadd\t{},word ptr DGROUP:_{arr_name}[bx]\r\n",
                reg.name(),
            );
            return;
        }
        // `<reg> += <global-struct-arr>[<var>].<field>` — variable-
        // indexed global array-of-struct field as the RHS of a
        // compound add. Mirrors the rvalue read shape (slice in
        // emitter_members: `mov ax, word ptr DGROUP:_arr+<off>[bx]`)
        // but folds the load straight into the add, avoiding the
        // load-to-AX + add pair (saves 2 bytes). Fixture 4210
        // (`sum += pts[i].y` for a global `struct Pt pts[3]`).
        if matches!(op, BinOp::Add)
            && !reg.is_byte()
            && let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } =
                &value.kind
            && let ExprKind::ArrayIndex { array, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && !self.locals.has(arr_name)
            && let Some(arr_ty) = self.globals.type_of(arr_name)
            && let Some(elem_ty) = arr_ty.array_elem()
            && let Type::Struct { fields, .. } = elem_ty.clone()
            && let Some(field_info) = fields.iter().find(|f| f.name == *field)
            && field_info.ty.is_int_like()
            && try_const_eval(index).is_none()
        {
            let field_off = field_info.offset;
            let elem_ty = elem_ty.clone();
            self.emit_index_into_bx(index, &elem_ty);
            let addr = if field_off == 0 {
                format!("DGROUP:_{arr_name}[bx]")
            } else {
                format!("DGROUP:_{arr_name}+{field_off}[bx]")
            };
            let _ = write!(self.out, "\tadd\t{},word ptr {addr}\r\n", reg.name());
            return;
        }
        // `<reg> += <stack-arr>[K_const]` — constant-indexed stack
        // array element folds to a single bp-relative add.
        // `add <reg>, word ptr [bp+(base+K*stride)]`. Fixture 1336
        // (`b += a[1]` for stack int array a, b in SI).
        if matches!(op, BinOp::Add)
            && let ExprKind::ArrayIndex { array, index } = &value.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let arr_ty = self.locals.type_of(arr_name).clone()
            && let Some(elem_ty) = arr_ty.array_elem()
            && elem_ty.is_int_like()
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
            && let Some(k) = try_const_eval(index)
            && !reg.is_byte()
        {
            let stride = i32::from(elem_ty.size_bytes());
            let elem_off = i32::from(base_off) + (k as i32) * stride;
            let elem_off_i16 = i16::try_from(elem_off).expect("elem offset fits in i16");
            let _ = write!(
                self.out,
                "\tadd\t{},word ptr {}\r\n",
                reg.name(),
                bp_addr(elem_off_i16),
            );
            return;
        }
        // `<reg> += <int-ptr-stack>[<var>]` — pointer param on the
        // stack ([bp+N]), variable index. Compute &p[i] into BX:
        // scale index in AX, then `mov bx, [bp+N]; add bx, ax`, then
        // `add <reg>, word ptr [bx]` directly. Same memory-direct
        // shape as the global-array and stack-array variants above;
        // only the BX load source differs. Fixture 1385 (`s += a[i]`
        // for `int sum(int *a, int n)`).
        if matches!(op, BinOp::Add)
            && let ExprKind::ArrayIndex { array, index } = &value.kind
            && let ExprKind::Ident(p_name) = &array.kind
            && self.locals.has(p_name)
            && let Some(pointee) = self.locals.type_of(p_name).pointee()
            && pointee.is_int_like()
            && let LocalLocation::Stack(p_off) = self.locals.location_of(p_name)
            && !reg.is_byte()
        {
            let stride = u16::from(pointee.size_bytes());
            self.emit_expr_to_ax(index);
            if stride == 2 {
                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
            } else if stride != 1 {
                let _ = write!(self.out, "\tmov\tdx,{stride}\r\n");
                self.out.extend_from_slice(b"\timul\tdx\r\n");
            }
            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(p_off));
            self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
            let _ = write!(
                self.out,
                "\tadd\t{},word ptr [bx]\r\n",
                reg.name(),
            );
            return;
        }
        // `<reg> += <stack-arr>[<var>]` — same shape but the array
        // base is bp-relative. Compute &arr[i] into BX (matches the
        // stack-array assign helper), then `add <reg>, word ptr [bx]`
        // directly. Fixtures 1807, 1822, 1933 (`sum += a[i]` for
        // stack int array, var index, reg-resident sum).
        if matches!(op, BinOp::Add)
            && let ExprKind::ArrayIndex { array, index } = &value.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let arr_ty = self.locals.type_of(arr_name).clone()
            && let Some(elem_ty) = arr_ty.array_elem()
            && elem_ty.is_int_like()
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
            && !reg.is_byte()
        {
            let elem_size = elem_ty.size_bytes();
            self.emit_array_addr_to_bx(arr_name, index, base_off, elem_size);
            let _ = write!(
                self.out,
                "\tadd\t{},word ptr [bx]\r\n",
                reg.name(),
            );
            return;
        }
        // `<reg> += *<reg-ptr>++` / etc.: read directly through the
        // pointer's reg (`add <dst>, word ptr [<ptr>]`), then advance
        // the pointer by stride. The natural-postinc shape avoids the
        // `mov bx, <ptr>; inc <ptr>; mov ax, [bx]; add <dst>, ax`
        // bounce. Fixture 1551 (`s += *a++` in `sum_n`).
        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && !reg.is_byte()
            && let ExprKind::Deref(inner) = &value.kind
            && let ExprKind::Update {
                target: ptr_name,
                op: upd_op,
                position: crate::ast::UpdatePosition::Post,
            } = &inner.kind
            && self.locals.has(ptr_name)
            && let LocalLocation::Reg(ptr_reg) = self.locals.location_of(ptr_name)
            && !ptr_reg.is_byte()
            && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
            && pointee.is_int_like()
        {
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let upd_mnem = match upd_op {
                crate::ast::UpdateOp::Inc => "inc",
                crate::ast::UpdateOp::Dec => "dec",
            };
            let ptr_reg_name = ptr_reg.name();
            let _ = write!(
                self.out,
                "\t{mnem}\t{},word ptr [{ptr_reg_name}]\r\n",
                reg.name(),
            );
            let stride = i32::from(pointee.size_bytes());
            for _ in 0..stride {
                let _ = write!(self.out, "\t{upd_mnem}\t{ptr_reg_name}\r\n");
            }
            return;
        }
        // `<reg> += <other-reg>++` / `--<other-reg>`: emit the op
        // using the current register value (memory-direct add), then
        // apply the post/pre update separately. Skips the AX
        // snapshot. Fixtures 1347 (`a += b++`), 1348 (`a += ++b`).
        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && let ExprKind::Update {
                target: upd_name,
                op: upd_op,
                position,
            } = &value.kind
            && self.locals.has(upd_name)
            && let LocalLocation::Reg(upd_reg) = self.locals.location_of(upd_name)
            && !upd_reg.is_byte()
            && self.locals.type_of(upd_name).is_int_like()
        {
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let upd_mnem = match upd_op {
                crate::ast::UpdateOp::Inc => "inc",
                crate::ast::UpdateOp::Dec => "dec",
            };
            let upd_reg_name = upd_reg.name();
            let dst_reg_name = reg.name();
            // Pre-inc: increment first, then op.
            // Post-inc: op with current value, then increment.
            match position {
                crate::ast::UpdatePosition::Pre => {
                    let _ = write!(self.out, "\t{upd_mnem}\t{upd_reg_name}\r\n");
                    let _ = write!(self.out, "\t{mnem}\t{dst_reg_name},{upd_reg_name}\r\n");
                }
                crate::ast::UpdatePosition::Post => {
                    let _ = write!(self.out, "\t{mnem}\t{dst_reg_name},{upd_reg_name}\r\n");
                    let _ = write!(self.out, "\t{upd_mnem}\t{upd_reg_name}\r\n");
                }
            }
            return;
        }
        // `<reg> += (int)<long-lvalue>` — the cast keeps the low half,
        // which is just a word-sized memory operand at the low addr.
        // Direct `<mnem> reg, word ptr <lo_addr>` instead of the
        // load-to-AX + add. Fixture 1969 (`sum += (int)a` with sum
        // in SI and a a long stack local).
        if matches!(
            op,
            BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
        ) && let ExprKind::Cast { ty: cast_ty, operand } = &value.kind
            && matches!(cast_ty, Type::Int | Type::UInt)
            && let Some((_hi, lo)) = self.long_lvalue_addr_pair(operand)
        {
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\t{},word ptr {lo}\r\n", reg.name());
            return;
        }
        if matches!(
            op,
            BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
        ) && try_const_eval(value).is_none()
            && self.value_needs_ax_route(value)
        {
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            // `<reg> <op>= <binop>`: BCC's pattern is the AX-route
            // shape `mov dx, <reg>; <op> dx, ax; mov <reg>, dx`
            // rather than the shorter `<op> <reg>, ax` direct
            // form. Adopt it when the value's last emitted line is
            // a load (not a single-byte op like `and ax, imm` that
            // already lives in AX with no scratch shuffle) so
            // existing fixtures still match. Specifically: trigger
            // when the value emission tail ends with an `and/or/
            // xor/add/sub ax, imm` peephole — that's BCC's exact
            // popcount shape (`n += x & 1`). Fixture 2271
            // (`while (x) { n += x & 1; x >>= 1; }`).
            let collapsed = self.try_collapse_lhs_clobber_to_dx();
            let src = if collapsed { "dx" } else { "ax" };
            // The AX-route widens the compound by 4 bytes vs the
            // direct `add <reg>, ax` shape, so it has to match a
            // BCC-specific trigger to be worth it. Trigger: tail is
            // a bare immediate op (`and ax, 1`, `add ax, K`, …) AND
            // the value didn't contain a call (BCC keeps the direct
            // shape when RHS evaluation already passed through
            // `call` — fixture 1441's `a += two() + 3`).
            if !collapsed
                && matches!(op, BinOp::Add)
                && tail_is_ax_imm_op(self.out)
                && !expr_has_call(value)
            {
                let reg_name = reg.name();
                let _ = write!(self.out, "\tmov\tdx,{reg_name}\r\n");
                self.out.extend_from_slice(b"\tadd\tdx,ax\r\n");
                let _ = write!(self.out, "\tmov\t{reg_name},dx\r\n");
                return;
            }
            let _ = write!(self.out, "\t{mnem}\t{},{src}\r\n", reg.name());
            return;
        }
        // `<reg> *= <int_lv> <op> <int_lv>` where `<op>` is a
        // non-clobbering binop (Add/Sub/BitAnd/BitOr/BitXor): compute
        // the RHS directly into DX, skipping the `mov dx, ax` shuffle.
        // Both operands must be int-typed memory operands (stack or
        // global). Fixture 1390 (`a *= (b+c)` with a in SI, b/c stack).
        if matches!(op, BinOp::Mul)
            && let ExprKind::BinOp { op: rop, left: rl, right: rr } = &value.kind
            && matches!(rop, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && let Some(l_src) = self.int_lvalue_addr(rl)
            && let Some(r_src) = self.int_lvalue_addr(rr)
        {
            let mnem = match rop {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tdx,word ptr {l_src}\r\n");
            let _ = write!(self.out, "\t{mnem}\tdx,word ptr {r_src}\r\n");
            let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
            self.out.extend_from_slice(b"\timul\tdx\r\n");
            let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
            return;
        }
        // Mul / Div / Mod with nested-binop (or Cast/Ternary/Call)
        // RHS: evaluate RHS to AX (which clobbers AX), then perform
        // the op with the dst register as the source operand
        // (imul/idiv/div take a single r/m argument and use AX as
        // the implicit accumulator). For Mul we want `dst * ax →
        // dst`; BCC's shape is `mov dx, ax; mov ax, dst; imul dx;
        // mov dst, ax`. For Div the accumulator must be the dividend
        // (dst), so we move RHS into BX first, then load dst into
        // AX, cwd, idiv bx. Fixtures 1390 (`a *= (b+c)`), 1393
        // (`a %= b*c`).
        if matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && try_const_eval(value).is_none()
            && self.value_needs_ax_route(value)
        {
            self.emit_expr_to_ax(value);
            match op {
                BinOp::Mul => {
                    self.out.extend_from_slice(b"\tmov\tdx,ax\r\n");
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\timul\tdx\r\n");
                }
                BinOp::Div | BinOp::Mod => {
                    // BCC's shape: `push ax` (save RHS divisor),
                    // `mov ax, dst` (load dividend), `cwd`, `pop
                    // bx` (recover divisor), `idiv bx`. Same
                    // 2-byte total as `mov bx, ax` + `mov ax, dst`
                    // but matches BCC's exact sequence.
                    // Fixture 1393 (`a %= b * c`).
                    let unsigned = self.locals.type_of(name).is_unsigned();
                    self.out.extend_from_slice(b"\tpush\tax\r\n");
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    if unsigned {
                        self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcwd\t\r\n");
                    }
                    self.out.extend_from_slice(b"\tpop\tbx\r\n");
                    let mnem = if unsigned { "div" } else { "idiv" };
                    let _ = write!(self.out, "\t{mnem}\tbx\r\n");
                }
                _ => unreachable!(),
            }
            let result = if matches!(op, BinOp::Mod) { "dx" } else { "ax" };
            let _ = write!(self.out, "\tmov\t{},{result}\r\n", reg.name());
            return;
        }
        match op {
            BinOp::Add | BinOp::Sub => {
                // Pointer compound add/sub: scale the RHS by the
                // pointee's size in bytes (C pointer arithmetic).
                // Fixture 542 (`int *p; p += 2` → `add si, 4` since
                // `sizeof(int)==2`).
                let stride = self
                    .locals
                    .type_of(name)
                    .pointee()
                    .map_or(1u32, |p| u32::from(p.size_bytes()));
                if let Some(v) = try_const_eval(value) {
                    let scaled = (v & 0xFFFF).wrapping_mul(stride) & 0xFFFF;
                    if scaled == 1 {
                        let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                        let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
                        return;
                    }
                    // Stride-2 pointer +=1 / -=1 collapses to a pair
                    // of inc/dec (2 bytes vs 3 for `add reg, 2`).
                    // BCC's asymmetric ±2 rule applies here too: +2
                    // uses double-inc, -2 uses `sub <reg>, 2` (3
                    // bytes) — see emit_op_with_source. Fixture 1983
                    // (`int *ip; ip += 1` for stride-2 → `inc di;
                    // inc di`).
                    if scaled == 2 && matches!(op, BinOp::Add) {
                        let _ = write!(self.out, "\tinc\t{}\r\n", reg.name());
                        let _ = write!(self.out, "\tinc\t{}\r\n", reg.name());
                        return;
                    }
                    let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
                    let _ = write!(self.out, "\t{mnem}\t{},{scaled}\r\n", reg.name());
                    return;
                }
                // Char-typed RHS needs widening: read the byte into
                // AL, sign-extend to AX, then `add <reg>, ax`. The
                // memory-direct add would otherwise read garbage
                // from the high byte. Covers:
                //   - Char local ident (fixture 1234: `a += c`).
                //   - Deref of a char pointer (fixture 1690: `n +=
                //     *s` where `s: char *`).
                let rhs_is_char_lvalue = match &value.kind {
                    ExprKind::Ident(rhs_name) => {
                        self.locals.has(rhs_name)
                            && self.locals.type_of(rhs_name).is_char_like()
                    }
                    ExprKind::Deref(inner) => match &inner.kind {
                        ExprKind::Ident(p_name) => {
                            self.locals.has(p_name)
                                && self
                                    .locals
                                    .type_of(p_name)
                                    .pointee()
                                    .is_some_and(|p| p.is_char_like())
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if rhs_is_char_lvalue {
                    self.emit_expr_to_ax(value);
                    let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
                    let _ = write!(self.out, "\t{mnem}\t{},ax\r\n", reg.name());
                    return;
                }
                let src = self.resolve_operand_source(value);
                let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
                let _ = write!(self.out, "\t{mnem}\t{},{}\r\n", reg.name(), src.word());
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                let mnem = match op {
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                // Char-typed RHS: widen via AL/cbw before the bitwise
                // op (same correctness rationale as the Add/Sub arm).
                if let ExprKind::Ident(rhs_name) = &value.kind
                    && (self.locals.has(rhs_name) && self.locals.type_of(rhs_name).is_char_like())
                {
                    self.emit_expr_to_ax(value);
                    let _ = write!(self.out, "\t{mnem}\t{},ax\r\n", reg.name());
                    return;
                }
                let src = self.resolve_operand_source(value);
                let _ = write!(self.out, "\t{mnem}\t{},{}\r\n", reg.name(), src.word());
            }
            BinOp::Mul => {
                // `imul reg, imm` is 80186+; BCC uses single-operand
                // `imul <src>` with AX. For a constant RHS the
                // divisor materializes in DX first (fixture 069).
                // For a memory-resident RHS (stack local or global)
                // BCC uses `imul <mem>` directly — fixture 651 (`x
                // *= y` with y at `[bp-2]` → `mov ax, si; imul word
                // ptr [bp-2]; mov si, ax`).
                if let Some(v) = try_const_eval(value) {
                    let v16 = v & 0xFFFF;
                    let _ = write!(self.out, "\tmov\tdx,{v16}\r\n");
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\timul\tdx\r\n");
                } else {
                    // Char-typed RHS needs widening: BCC's pattern is
                    // `mov al, [c]; cbw; push ax; mov ax, <reg>; pop
                    // dx; imul dx; mov <reg>, ax`. The word-form
                    // `imul word ptr <c>` would read garbage from
                    // the byte past c. Fixture 1388 (`a *= c` for
                    // int a in SI, char c at [bp-1]).
                    let rhs_is_char_lvalue = match &value.kind {
                        ExprKind::Ident(rhs_name) => {
                            self.locals.has(rhs_name)
                                && self.locals.type_of(rhs_name).is_char_like()
                        }
                        ExprKind::Deref(inner) => match &inner.kind {
                            ExprKind::Ident(p_name) => {
                                self.locals.has(p_name)
                                    && self
                                        .locals
                                        .type_of(p_name)
                                        .pointee()
                                        .is_some_and(|p| p.is_char_like())
                            }
                            _ => false,
                        },
                        _ => false,
                    };
                    if rhs_is_char_lvalue {
                        self.emit_expr_to_ax(value);
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                        self.out.extend_from_slice(b"\tpop\tdx\r\n");
                        self.out.extend_from_slice(b"\timul\tdx\r\n");
                        let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
                        return;
                    }
                    let src = self.resolve_operand_source(value);
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    match &src {
                        OperandSource::Local(_)
                        | OperandSource::Global(_)
                        | OperandSource::GlobalOffset { .. } => {
                            let _ = write!(self.out, "\timul\t{}\r\n", src.word());
                        }
                        OperandSource::Reg(rhs_reg) => {
                            // `imul <reg16>` directly, no DX roundtrip
                            // (matches BCC's shape for `r *= i` with
                            // both r and i in registers — fixture
                            // 1411).
                            let _ = write!(self.out, "\timul\t{}\r\n", rhs_reg.name());
                        }
                        _ => {
                            let _ = write!(self.out, "\tmov\tdx,{}\r\n", src.word());
                            self.out.extend_from_slice(b"\timul\tdx\r\n");
                        }
                    }
                }
                let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
            }
            BinOp::Shl | BinOp::Shr => {
                // `<int-reg> <<= K` / `>>= K` — small K (1, 2, 3)
                // unrolls into repeated single-bit shifts (`<mnem>
                // <reg>, 1`) since each shift is 2 bytes (`D1 /r`)
                // vs 5 bytes for the `mov cl, K; <mnem> <reg>, cl`
                // pair (4 bytes for K, but 5 total). K >= 4 uses
                // the CL load. Same threshold BCC uses in
                // expression context (fixture 626). Fixtures 537
                // (K=4, CL form) and 1022 (K=2, unrolled).
                let signed = !self.locals.type_of(name).is_unsigned();
                let mnem = match op {
                    BinOp::Shl => "shl",
                    BinOp::Shr if signed => "sar",
                    BinOp::Shr => "shr",
                    _ => unreachable!(),
                };
                if let Some(k) = try_const_eval(value) {
                    let k = k as u16;
                    if (1..=3).contains(&k) {
                        for _ in 0..k {
                            let _ = write!(self.out, "\t{mnem}\t{},1\r\n", reg.name());
                        }
                        return;
                    }
                    let k8 = k & 0xFF;
                    let _ = write!(self.out, "\tmov\tcl,{k8}\r\n");
                    let _ = write!(self.out, "\t{mnem}\t{},cl\r\n", reg.name());
                    return;
                }
                // Non-constant shift count — load the low byte of
                // the RHS into CL via the same `mov cl, byte ptr
                // ...` shape we use for constants (but with the
                // operand source instead of an immediate). Fixture
                // 658 (`x <<= y` → `mov cl, byte ptr [bp-2]; shl
                // si, cl`).
                let src = self.resolve_operand_source(value);
                let _ = write!(self.out, "\tmov\tcl,{}\r\n", src.byte());
                let _ = write!(self.out, "\t{mnem}\t{},cl\r\n", reg.name());
            }
            BinOp::Div | BinOp::Mod => {
                // `<int-reg> /= K` (or `%= K`) — load divisor into
                // BX (DX is clobbered by `cwd`), then `mov ax, <reg>;
                // cwd; idiv bx`. `/=` stores AX back, `%=` stores DX
                // (the remainder). Fixtures 584 (`/=`) and 585 (`%=`).
                // For a memory-resident variable RHS BCC uses `idiv
                // <mem>` directly — fixture 653 (`x /= y` → `mov ax,
                // si; cwd; idiv word ptr [bp-2]; mov si, ax`).
                if let Some(v) = try_const_eval(value) {
                    let v16 = v & 0xFFFF;
                    let _ = write!(self.out, "\tmov\tbx,{v16}\r\n");
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                    self.out.extend_from_slice(b"\tidiv\tbx\r\n");
                } else {
                    let src = self.resolve_operand_source(value);
                    let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                    match &src {
                        OperandSource::Local(_)
                        | OperandSource::Global(_)
                        | OperandSource::GlobalOffset { .. } => {
                            let _ = write!(self.out, "\tidiv\t{}\r\n", src.word());
                        }
                        _ => {
                            let _ = write!(self.out, "\tmov\tbx,{}\r\n", src.word());
                            self.out.extend_from_slice(b"\tidiv\tbx\r\n");
                        }
                    }
                }
                let result_reg = if matches!(op, BinOp::Div) { "ax" } else { "dx" };
                let _ = write!(self.out, "\tmov\t{},{result_reg}\r\n", reg.name());
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                unreachable!("comparison ops are not compound-assignable in C")
            }
        }
    }
}
