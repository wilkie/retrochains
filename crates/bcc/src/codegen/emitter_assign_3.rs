use super::*;

impl<'a> super::FunctionEmitter<'a> {
    pub(crate) fn emit_assign_local(&mut self, loc: LocalLocation, ty: &Type, value: &Expr) {
        // `<fn-ptr-local> = <function-name>` — fold to a direct
        // immediate-to-memory store `mov word ptr [bp-N], offset _f`.
        // The init-form already has this peephole; the assignment
        // form needs the same to match BCC's single-instruction
        // sequence. The explicit `= &f` address-of form is equivalent
        // (function designator vs `&function` both yield the address)
        // and folds the same way. Fixtures 2442, 4198.
        if let LocalLocation::Stack(off) = loc
            && let (ExprKind::Ident(src_name) | ExprKind::AddressOf(src_name)) = &value.kind
            && !self.locals.has(src_name)
            && self.globals.type_of(src_name).is_none()
            && self.signatures.ret_ty_of(src_name).is_some()
        {
            let _ = write!(
                self.out,
                "\tmov\tword ptr {},offset _{src_name}\r\n",
                bp_addr(off),
            );
            return;
        }
        // Far-pointer assignment: `p = &g;` / `p = (T far *)&local;` /
        // `p = (T far *)<stack_array>;`. Mirrors the init-form arms
        // — store segment to upper, offset to lower. Picks DS for
        // globals, SS for stack sources. Fixture 1651
        // (`p = (int far *)a;` for a stack array a + a far pointer
        // p).
        if matches!(ty, Type::FarPointer { .. })
            && let LocalLocation::Stack(off) = loc
        {
            if let ExprKind::AddressOf(sym) = &value.kind
                && self.globals.type_of(sym).is_some()
            {
                let _ = write!(self.out, "\tmov\tword ptr {},ds\r\n", bp_addr(off + 2));
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr {},offset DGROUP:_{sym}\r\n",
                    bp_addr(off),
                );
                return;
            }
            if let Some(addr_expr) = strip_cast(value)
                && let Some((local_name, elem_off)) = match &addr_expr.kind {
                    ExprKind::AddressOf(n) => Some((n.clone(), 0i32)),
                    ExprKind::Ident(n) if self.locals.has(n)
                        && matches!(self.locals.type_of(n), Type::Array { .. }) =>
                        Some((n.clone(), 0i32)),
                    ExprKind::AddressOfArrayElem { array, byte_offset } if self.locals.has(array) =>
                        Some((array.clone(), *byte_offset)),
                    _ => None,
                }
                && self.locals.has(&local_name)
                && let LocalLocation::Stack(local_off) = self.locals.location_of(&local_name)
            {
                let lea_off = i32::from(local_off) + elem_off;
                let lea_off_i16 = i16::try_from(lea_off)
                    .expect("local + elem offset fits in i16");
                let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(lea_off_i16));
                let _ = write!(self.out, "\tmov\tword ptr {},ss\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                return;
            }
        }
        match loc {
            LocalLocation::Stack(off) => {
                // Struct-to-stack copy assign. Three shape branches
                // by source storage and byte size:
                //   - 4-byte from global: inline `mov ax / mov dx`
                //     load + `[bp+off]` store pair (fixture 415).
                //   - 4-byte from stack: same inline pair but both
                //     load and store are bp-relative (fixture 417).
                //   - > 4 bytes: route through `N_SCOPY@`. The
                //     destination far pointer uses `PUSH SS` (segment
                //     for stack-resident memory) instead of `PUSH DS`,
                //     and the offset is loaded via LEA `[bp+off]`
                //     instead of `mov OFFSET _sym`. Source picks SS
                //     vs DS based on whether *it* is stack- or globals-
                //     resident. Fixtures 416 (stack ← global), 418
                //     (stack ← stack).
                if let Type::Struct { .. } = ty
                    && let ExprKind::Ident(src_name) = &value.kind
                {
                    let size = ty.size_bytes();
                    let src_is_global = self.globals.type_of(src_name).map_or(false, |t| t == ty);
                    let src_is_stack = self.locals.has(src_name)
                        && self.locals.type_of(src_name) == ty;
                    if (src_is_global || src_is_stack) && size == 4 {
                        if src_is_global {
                            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                            let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                        } else {
                            let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                            else {
                                panic!("struct local `{src_name}` not stack-resident");
                            };
                            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(src_off + 2));
                            let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(src_off));
                        }
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    if (src_is_global || src_is_stack) && size > 4 {
                        let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
                        self.out.extend_from_slice(b"\tpush\tss\r\n");
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        if src_is_global {
                            let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{src_name}\r\n");
                            self.out.extend_from_slice(b"\tpush\tds\r\n");
                        } else {
                            let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                            else {
                                panic!("struct local `{src_name}` not stack-resident");
                            };
                            let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(src_off));
                            self.out.extend_from_slice(b"\tpush\tss\r\n");
                        }
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                        self.out.extend_from_slice(b"\tcall\tnear ptr N_SCOPY@\r\n");
                        self.helpers.insert("N_SCOPY@".to_string());
                        return;
                    }
                }
                // `struct S a; a = f();` for a 4-byte struct return.
                // Same shape as the global-dest variant (fixture 424):
                // the call leaves DX:AX = high:low, store back to the
                // stack-local destination. Fixture 426.
                if let Type::Struct { .. } = ty
                    && ty.size_bytes() == 4
                    && let ExprKind::Call { name: fname, args } = &value.kind
                    && self.signatures
                        .ret_ty_of(fname)
                        .map_or(false, |t| matches!(t, Type::Struct { .. }) && t.size_bytes() == 4)
                {
                    self.emit_call(fname, args);
                    let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    return;
                }
                // `struct S a; a = f();` for a struct return whose size
                // is ∉ {1, 2, 4} — routes through the hidden-tmp +
                // N_SCOPY@ pattern. The pre-scan reserved a frame slot
                // big enough at `[bp - (stack + tmp)]`; the caller
                // sequence is:
                //   1. lea ax, &a; push ss; push ax  (dest for SCOPY,
                //      kept on stack across the call)
                //   2. push args (normal cdecl/pascal order)
                //   3. push ss; lea ax, &tmp; push ax (hidden ret
                //      ptr — note BCC's ss-then-offset order here)
                //   4. call f
                //   5. cleanup (`pop cx;pop cx` for 4 bytes,
                //      `add sp,N` otherwise) — drops hidden ret ptr
                //      and args, leaves dest on stack
                //   6. lea ax, &tmp; push ss; push ax (src for SCOPY)
                //   7. mov cx, size; call N_SCOPY@ (cleans dest+src)
                // Fixtures 1685 / 1877 (3-int struct), 2207 (4-int +
                // 1 int arg), 2352 (4-int).
                if let Type::Struct { .. } = ty
                    && let ExprKind::Call { name: fname, args } = &value.kind
                    && let Some(ret_ty) = self.signatures.ret_ty_of(fname)
                    && ret_ty == ty
                    && let size = ty.size_bytes()
                    && size != 1 && size != 2 && size != 4
                {
                    let tmp_off = self.struct_call_tmp_offset();
                    let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
                    self.out.extend_from_slice(b"\tpush\tss\r\n");
                    self.out.extend_from_slice(b"\tpush\tax\r\n");
                    self.pending_hidden_ret_ptr_tmp_off = Some(tmp_off);
                    let fname_owned = fname.clone();
                    let args_owned = args.clone();
                    self.emit_call(&fname_owned, &args_owned);
                    let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(tmp_off));
                    self.out.extend_from_slice(b"\tpush\tss\r\n");
                    self.out.extend_from_slice(b"\tpush\tax\r\n");
                    let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_SCOPY@\r\n");
                    self.helpers.insert("N_SCOPY@".to_string());
                    return;
                }
                // `long x; x = K;` — two word stores, high then low.
                // Same shape as the init form (fixture 210/287).
                if ty.is_long_like() {
                    if let Some(v) = try_const_eval(value) {
                        let lo = v & 0xFFFF;
                        let hi = (v >> 16) & 0xFFFF;
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},{hi}\r\n",
                            bp_addr(off + 2),
                        );
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},{lo}\r\n",
                            bp_addr(off),
                        );
                        return;
                    }
                    // `x = g;` from a long-like global — mirror the
                    // init-from-global shape (fixture 286 / 288 family):
                    // load high into AX, low into DX, store back.
                    if let ExprKind::Ident(src_name) = &value.kind
                        && let Some(src_ty) = self.globals.type_of(src_name)
                        && src_ty.is_long_like()
                    {
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `x = f();` — function-call RHS returns DX:AX
                    // (ABI). Store DX → high, AX → low. Same shape as
                    // the init form (fixture 315). Fixture 321.
                    if let ExprKind::Call { .. } = &value.kind {
                        self.emit_expr_to_ax(value);
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x <op> y;` — long stack-local binary
                    // arithmetic (`+`, `-`, `&`, `|`, `^`). Load x
                    // into AX:DX (AX=high, DX=low globals-convention,
                    // since dest is memory). Apply the op pair (with
                    // carry/borrow for `+/-`, same mnemonic per half
                    // for bitwise). Store AX/DX back. Fixtures 329
                    // (add), 330 (sub), 333 (and), 334 (or).
                    if let ExprKind::BinOp { op, left, right } = &value.kind
                        && let Some((lo_op, hi_op)) = long_pair_op(*op)
                        && let ExprKind::Ident(a) = &left.kind
                        && let ExprKind::Ident(b) = &right.kind
                        && self.locals.has(a)
                        && self.locals.has(b)
                        && self.locals.type_of(a).is_long_like()
                        && self.locals.type_of(b).is_long_like()
                    {
                        let (LocalLocation::Stack(a_off), LocalLocation::Stack(b_off)) =
                            (self.locals.location_of(a), self.locals.location_of(b))
                        else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(a_off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(a_off));
                        let _ = write!(self.out, "\t{lo_op}\tdx,word ptr {}\r\n", bp_addr(b_off));
                        let _ = write!(self.out, "\t{hi_op}\tax,word ptr {}\r\n", bp_addr(b_off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x;` — long-from-long-local copy. Load
                    // both halves into AX:DX, store both into z.
                    // Fixture 335.
                    if let ExprKind::Ident(src) = &value.kind
                        && self.locals.has(src)
                        && self.locals.type_of(src).is_long_like()
                    {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src) else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(src_off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(src_off));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x * y;` long stack-local multiply — same
                    // helper convention as the global path: operand
                    // a in CX:BX (high:low), operand b in DX:AX
                    // (high:low). Result returns in DX:AX. Fixture
                    // 336.
                    if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
                        && let ExprKind::Ident(a) = &left.kind
                        && let ExprKind::Ident(b) = &right.kind
                        && self.locals.has(a)
                        && self.locals.has(b)
                        && self.locals.type_of(a).is_long_like()
                        && self.locals.type_of(b).is_long_like()
                    {
                        let (LocalLocation::Stack(a_off), LocalLocation::Stack(b_off)) =
                            (self.locals.location_of(a), self.locals.location_of(b))
                        else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tcx,word ptr {}\r\n", bp_addr(a_off + 2));
                        let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(a_off));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(b_off + 2));
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(b_off));
                        self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                        self.helpers.insert("N_LXMUL@".to_string());
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x / y;` / `z = x % y;` long stack-local
                    // divide/modulo — push operands (rightmost divisor
                    // first, each as high-then-low), call helper.
                    // Fixtures 337, 338.
                    if let ExprKind::BinOp { op, left, right } = &value.kind
                        && matches!(op, BinOp::Div | BinOp::Mod)
                        && let ExprKind::Ident(a) = &left.kind
                        && let ExprKind::Ident(b) = &right.kind
                        && self.locals.has(a)
                        && self.locals.has(b)
                        && self.locals.type_of(a).is_long_like()
                        && self.locals.type_of(b).is_long_like()
                    {
                        let (LocalLocation::Stack(a_off), LocalLocation::Stack(b_off)) =
                            (self.locals.location_of(a), self.locals.location_of(b))
                        else {
                            unreachable!("long is never register-resident");
                        };
                        let unsigned = self.locals.type_of(a).is_unsigned()
                            || self.locals.type_of(b).is_unsigned();
                        let helper = match (op, unsigned) {
                            (BinOp::Div, false) => "N_LDIV@",
                            (BinOp::Mod, false) => "N_LMOD@",
                            (BinOp::Div, true)  => "N_LUDIV@",
                            (BinOp::Mod, true)  => "N_LUMOD@",
                            _ => unreachable!(),
                        };
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(b_off + 2));
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(b_off));
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(a_off + 2));
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(a_off));
                        let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                        self.helpers.insert(helper.to_string());
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x << n;` / `z = x >> n;` long stack-local
                    // shift by a variable count. Load x into DX:AX
                    // (helper-ABI), load shift count into CL as a
                    // byte ptr from n's storage, call helper, store
                    // result. Fixture 341.
                    if let ExprKind::BinOp { op, left, right } = &value.kind
                        && matches!(op, BinOp::Shl | BinOp::Shr)
                        && let ExprKind::Ident(a) = &left.kind
                        && let ExprKind::Ident(n) = &right.kind
                        && self.locals.has(a)
                        && self.locals.has(n)
                        && self.locals.type_of(a).is_long_like()
                    {
                        let LocalLocation::Stack(a_off) = self.locals.location_of(a) else {
                            unreachable!("long is never register-resident");
                        };
                        let unsigned = self.locals.type_of(a).is_unsigned();
                        let helper = match (op, unsigned) {
                            (BinOp::Shl, _)     => "N_LXLSH@",
                            (BinOp::Shr, false) => "N_LXRSH@",
                            (BinOp::Shr, true)  => "N_LXURSH@",
                            _ => unreachable!(),
                        };
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(a_off + 2));
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(a_off));
                        // Load shift count as byte from n's storage
                        // — only the low byte of n is consumed by
                        // the helper.
                        match self.locals.location_of(n) {
                            LocalLocation::Stack(n_off) => {
                                let _ = write!(self.out, "\tmov\tcl,byte ptr {}\r\n", bp_addr(n_off));
                            }
                            LocalLocation::Reg(_reg) => {
                                panic!("register-resident shift count for long shift not yet supported (no fixture)");
                            }
                        }
                        let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                        self.helpers.insert(helper.to_string());
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `z = -x;` long unary negation on a stack local.
                    // BCC's idiom: neg AX / neg DX / sbb AX, 0 — see
                    // "Long unary" in the ASM_OUTPUT spec. Fixture 331.
                    if let ExprKind::Unary { op: UnaryOp::Neg, operand } = &value.kind
                        && let ExprKind::Ident(src) = &operand.kind
                        && self.locals.has(src)
                        && self.locals.type_of(src).is_long_like()
                    {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src) else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(src_off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(src_off));
                        self.out.extend_from_slice(b"\tneg\tax\r\n");
                        self.out.extend_from_slice(b"\tneg\tdx\r\n");
                        self.out.extend_from_slice(b"\tsbb\tax,0\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `z = ~x;` long bitwise complement on a stack
                    // local. Both halves independent: `not dx / not
                    // ax`. Fixture 332.
                    if let ExprKind::Unary { op: UnaryOp::BitNot, operand } = &value.kind
                        && let ExprKind::Ident(src) = &operand.kind
                        && self.locals.has(src)
                        && self.locals.type_of(src).is_long_like()
                    {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src) else {
                            unreachable!("long is never register-resident");
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(src_off + 2));
                        let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(src_off));
                        self.out.extend_from_slice(b"\tnot\tdx\r\n");
                        self.out.extend_from_slice(b"\tnot\tax\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x << 16;` long left-shift by 16 — special
                    // case: move low half into high, zero the low.
                    // Fixture 2586 (`s = s << 16`).
                    if let ExprKind::BinOp { op: BinOp::Shl, left, right } = &value.kind
                        && let Some((_src_hi, src_lo)) = self.long_lvalue_addr_pair(left)
                        && let Some(k) = try_const_eval(right)
                        && k == 16
                    {
                        let _ = write!(self.out, "\tmov\tax,word ptr {src_lo}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},0\r\n", bp_addr(off));
                        return;
                    }
                    // `z = x << K` / `z = x >> K` long shift by a
                    // constant count — same helper as the variable-
                    // count path, but `mov cl, K` instead of loading
                    // from a local. Fixtures 2575, 2579.
                    if let ExprKind::BinOp { op, left, right } = &value.kind
                        && matches!(op, BinOp::Shl | BinOp::Shr)
                        && let Some((src_hi, src_lo)) = self.long_lvalue_addr_pair(left)
                        && let Some(k) = try_const_eval(right)
                    {
                        let unsigned = self.expr_is_unsigned(left);
                        let helper = match (op, unsigned) {
                            (BinOp::Shl, _)     => "N_LXLSH@",
                            (BinOp::Shr, false) => "N_LXRSH@",
                            (BinOp::Shr, true)  => "N_LXURSH@",
                            _ => unreachable!(),
                        };
                        let k8 = (k & 0xFF) as u8;
                        let _ = write!(self.out, "\tmov\tdx,word ptr {src_hi}\r\n");
                        let _ = write!(self.out, "\tmov\tax,word ptr {src_lo}\r\n");
                        let _ = write!(self.out, "\tmov\tcl,{k8}\r\n");
                        let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                        self.helpers.insert(helper.to_string());
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `v = *p++` for long pointee — load both halves
                    // of `*p` through the reg-resident pointer, store
                    // them into v's slot, then advance the pointer by
                    // 4. BCC reads high half first (`mov ax, [reg+2]`)
                    // and low half second (`mov dx, [reg]`); the
                    // post-increment `add reg, 4` fires after both
                    // loads so the result is the pre-update `*p`.
                    // Fixture 2521 (`long *p; long v; v = *p++`).
                    if let ExprKind::Deref(inner) = &value.kind
                        && let ExprKind::Update {
                            target: p_name,
                            op: UpdateOp::Inc,
                            position: UpdatePosition::Post,
                        } = &inner.kind
                        && self.locals.has(p_name)
                        && let Some(pointee) = self.locals.type_of(p_name).pointee()
                        && pointee.is_long_like()
                        && let LocalLocation::Reg(p_reg) = self.locals.location_of(p_name)
                    {
                        let r = p_reg.name();
                        let _ = write!(self.out, "\tmov\tax,word ptr [{r}+2]\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr [{r}]\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        let _ = write!(self.out, "\tadd\t{r},4\r\n");
                        return;
                    }
                    // Fallback: value is an int-typed expression
                    // assigned to a long-typed local — widen via
                    // `cwd` (sign-extend AX to DX:AX) and store both
                    // halves. Unsigned-source uses `xor dx, dx`
                    // instead of cwd. Fixture 3230 (`long n; n = x +
                    // 1;` for int param x).
                    if !self.expr_is_long_like(value) {
                        let unsigned = self.expr_int_is_unsigned(value);
                        self.emit_expr_to_ax(value);
                        if unsigned {
                            self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                        } else {
                            self.out.extend_from_slice(b"\tcwd\t\r\n");
                        }
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `x = ((long)<hi> << 16) | (long)(unsigned int)<lo>`
                    // — the canonical "combine two ints into a long"
                    // idiom. BCC's lowering keeps the hi value in AX
                    // (the *high* half of the result lives in AX in
                    // this peephole) and uses DX for lo (the low
                    // half). After cwd (which would normally
                    // sign-extend, but the value's high half is
                    // about to be overwritten anyway), `xor dx,dx`
                    // zeros DX, then `or dx, lo` puts lo in DX.
                    // `or ax, 0` is a no-op marker for the OR
                    // semantics of the high half. Finally store
                    // AX → off+2 (high) and DX → off (low).
                    // Fixture 1946.
                    if let Some((hi_name, lo_name, lo_unsigned)) =
                        match_combine_long_idiom(value)
                    {
                        let hi_addr = self.named_int_lvalue_addr(&hi_name);
                        let lo_addr = self.named_int_lvalue_addr(&lo_name);
                        if let (Some(hi_addr), Some(lo_addr)) = (hi_addr, lo_addr) {
                            let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                            self.out.extend_from_slice(b"\tcwd\t\r\n");
                            self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                            let _ = write!(self.out, "\tor\tdx,word ptr {lo_addr}\r\n");
                            self.out.extend_from_slice(b"\tor\tax,0\r\n");
                            let _ = write!(
                                self.out,
                                "\tmov\tword ptr {},ax\r\n",
                                bp_addr(off + 2),
                            );
                            let _ = write!(
                                self.out,
                                "\tmov\tword ptr {},dx\r\n",
                                bp_addr(off),
                            );
                            let _ = lo_unsigned;
                            return;
                        }
                    }
                    panic!("non-constant long local assign not yet supported (no fixture)");
                }
                // Char-local store: byte-width immediate. Same byte
                // form as the init path (mov byte ptr [bp-N], K).
                // Fixture 461 (`c = 200;` for a uchar local).
                if ty.is_char_like()
                    && let Some(v) = try_const_eval(value)
                {
                    let v8 = v & 0xFF;
                    let _ = write!(self.out, "\tmov\tbyte ptr {},{v8}\r\n", bp_addr(off));
                    return;
                }
                // Char dest + ternary with both arms constant:
                // `c = cond ? K1 : K2;` — emit `mov al, K1` /
                // `mov al, K2` byte loads instead of `mov ax, K`
                // word loads (saves 1 byte per arm). Fixture 1287
                // (`c = x > 0 ? 'P' : 'N';`).
                if ty.is_char_like()
                    && let ExprKind::Ternary { cond, then_value, else_value } = &value.kind
                    && let Some(t_v) = try_const_eval(then_value)
                    && let Some(e_v) = try_const_eval(else_value)
                {
                    let span_start = value.span.start;
                    let span_end = value.span.end;
                    let base = self.label_plan.base(span_start, span_end);
                    let false_slot = base + 1;
                    let merge_slot = base + 2;
                    let cond_has_top_or = matches!(
                        cond.kind,
                        ExprKind::Logical { op: LogicalOp::Or, .. }
                    );
                    let true_slot = if cond_has_top_or { Some(base) } else { None };
                    self.emit_cond_branch(cond, true_slot, Some(false_slot));
                    if let Some(t) = true_slot {
                        self.emit_label(t);
                    }
                    let t8 = t_v & 0xFF;
                    let _ = write!(self.out, "\tmov\tal,{t8}\r\n");
                    let _ = write!(
                        self.out,
                        "\tjmp\tshort {}\r\n",
                        self.label_ref(merge_slot),
                    );
                    self.emit_label(false_slot);
                    let e8 = e_v & 0xFF;
                    let _ = write!(self.out, "\tmov\tal,{e8}\r\n");
                    self.emit_label(merge_slot);
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    return;
                }
                // Mirror the init form: immediate-store when possible.
                if let Some(v) = try_const_eval(value) {
                    let v16 = v & 0xFFFF;
                    let _ = write!(self.out, "\tmov\tword ptr {},{v16}\r\n", bp_addr(off));
                    return;
                }
                // `v = *p++;` where p is a register-resident pointer
                // local: read through the pointer directly, store the
                // loaded value to v, then advance the pointer. Saves
                // the `mov bx, <reg>` snapshot that `emit_deref_to_ax`'s
                // postinc path emits (since here the read happens
                // before the advance — no need to snapshot). Fixture
                // 2518 (`v = *p++` for `int *p` in SI). Also fires
                // for postdec.
                if !ty.is_char_like()
                    && !ty.is_long_like()
                    && let ExprKind::Deref(inner) = &value.kind
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &inner.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(reg) = self.locals.location_of(target)
                    && let Some(pointee) = self.locals.type_of(target).pointee()
                    && pointee.is_int_like()
                {
                    let reg_name = reg.name();
                    let stride = i32::from(pointee.size_bytes());
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\tmov\tax,word ptr [{reg_name}]\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    for _ in 0..stride {
                        let _ = write!(self.out, "\t{mnem}\t{reg_name}\r\n");
                    }
                    return;
                }
                // `c = *p++` / `c = *p--` for char dest, char *p in a
                // register: read via the reg directly (no BX
                // snapshot), store the byte, then advance the
                // register. No cbw — the dest is char. Fixture 2557
                // (`c = *p++` with `char *p` in SI).
                if ty.is_char_like()
                    && let ExprKind::Deref(inner) = &value.kind
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &inner.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(reg) = self.locals.location_of(target)
                    && let Some(pointee) = self.locals.type_of(target).pointee()
                    && pointee.is_char_like()
                {
                    let reg_name = reg.name();
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\tmov\tal,byte ptr [{reg_name}]\r\n");
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    let _ = write!(self.out, "\t{mnem}\t{reg_name}\r\n");
                    return;
                }
                // `y = ++x;` where x is register-resident — update
                // in place, then store the register direct to the
                // stack slot (skip the AX round-trip). Fixture 530.
                if let ExprKind::Update {
                    target,
                    op,
                    position: crate::ast::UpdatePosition::Pre,
                } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(reg) = self.locals.location_of(target)
                    && !reg.is_byte()
                {
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let rname = reg.name();
                    let _ = write!(self.out, "\t{mnem}\t{rname}\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {},{rname}\r\n", bp_addr(off));
                    return;
                }
                // `y = x++;` (possibly after a leading comma chain) —
                // store the pre-update register direct to the stack
                // slot, then apply the post-update. Skips the AX
                // snapshot. Stride-1 only (int/uint locals). For
                // `y = (a, b, c++)` BCC evaluates a, b for side
                // effect, then assigns c (pre-inc), then increments
                // c. Fixture 1861.
                let final_peek = {
                    let mut cur = value;
                    while let ExprKind::Comma { right, .. } = &cur.kind {
                        cur = right;
                    }
                    cur
                };
                if !ty.is_char_like()
                    && !ty.is_long_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &final_peek.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(reg) = self.locals.location_of(target)
                    && !reg.is_byte()
                    && self.locals.type_of(target).is_int_like()
                {
                    let mut cur = value;
                    while let ExprKind::Comma { left, right } = &cur.kind {
                        self.emit_expr_discard(left);
                        cur = right;
                    }
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let rname = reg.name();
                    let _ = write!(self.out, "\tmov\tword ptr {},{rname}\r\n", bp_addr(off));
                    let _ = write!(self.out, "\t{mnem}\t{rname}\r\n");
                    return;
                }
                // `c = a % b;` on int stack-locals — fold the
                // post-idiv `mov ax, dx` away by storing DX directly
                // into the destination. Fixture 546.
                if let ExprKind::BinOp { op: BinOp::Mod, left, right } = &value.kind
                    && !ty.is_char_like()
                    && !ty.is_long_like()
                {
                    self.emit_arith_setup_for_mod(left, right);
                    let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                    return;
                }
                // `char c = a[K];` — skip the AL→AX widening that
                // `emit_array_index_to_ax` emits for char arrays,
                // since the byte store truncates back anyway. Two
                // shapes:
                //   - global array source: `mov al, byte ptr DGROUP:
                //     _a+K` (fixture 567).
                //   - local array source: `mov al, byte ptr [bp+K]`
                //     (fixture 570).
                if ty.is_char_like()
                    && let ExprKind::ArrayIndex { array, index } = &value.kind
                    && let ExprKind::Ident(arr_name) = &array.kind
                {
                    if let Some(gty) = self.globals.type_of(arr_name)
                        && let Some(const_off) = try_const_array_offset(gty, std::iter::once(&**index))
                            .map(|(o, _leaf)| o)
                        && gty.array_elem().is_some_and(|e| e.is_char_like())
                    {
                        let addr = if const_off == 0 {
                            format!("DGROUP:_{arr_name}")
                        } else {
                            format!("DGROUP:_{arr_name}+{const_off}")
                        };
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                    if self.locals.has(arr_name)
                        && let arr_ty = self.locals.type_of(arr_name).clone()
                        && arr_ty.array_elem().is_some_and(|e| e.is_char_like())
                        && let Some(const_off) =
                            try_const_array_offset(&arr_ty, std::iter::once(&**index))
                                .map(|(o, _leaf)| o)
                        && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
                    {
                        let src_off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
                        let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(src_off));
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                }
                // `char b = s.c;` / `b = s.c;` — char-to-char copy
                // through a struct-member load. Skip the `cbw`-widen
                // that `emit_member_to_ax` would add for the int-
                // promotion path, since the destination is char and
                // the byte store truncates back anyway. Mirrors the
                // char-array-elem peephole just above. Two shapes:
                //   - global struct source: `mov al, byte ptr DGROUP:
                //     _s+K`.
                //   - local struct source: `mov al, byte ptr [bp+K]`.
                // Fixture 1115.
                if ty.is_char_like()
                    && let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } =
                        &value.kind
                    && let Some((src_name, total_off, leaf_ty)) =
                        self.try_member_dot_chain(base, field)
                    && leaf_ty.is_char_like()
                {
                    if self.globals.contains(&src_name) {
                        let addr = if total_off == 0 {
                            format!("DGROUP:_{src_name}")
                        } else {
                            format!("DGROUP:_{src_name}+{total_off}")
                        };
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                    if let LocalLocation::Stack(base_bp) = self.locals.location_of(&src_name) {
                        let src_off =
                            base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                        let _ = write!(
                            self.out,
                            "\tmov\tal,byte ptr {}\r\n",
                            bp_addr(src_off)
                        );
                        let _ = write!(
                            self.out,
                            "\tmov\tbyte ptr {},al\r\n",
                            bp_addr(off)
                        );
                        return;
                    }
                }
                // `c = f();` where c is char, f returns char — call
                // returns value in AL; store the low byte directly,
                // skip the cbw widen. Fixture 2451.
                if ty.is_char_like()
                    && let ExprKind::Call { name, args } = &value.kind
                    && self.signatures.ret_ty_of(name).map_or(false, |t| t.is_char_like())
                {
                    self.emit_call(name, args);
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    return;
                }
                // `c = (char)<int_local>;` — load the low byte of
                // the int directly into AL and store. The cast
                // narrows; for char dest we don't need to widen.
                // Fixture 2455 (`c = (char)i` for int i, char c).
                if ty.is_char_like()
                    && let ExprKind::Cast { ty: cast_ty, operand } = &value.kind
                    && cast_ty.is_char_like()
                    && let ExprKind::Ident(src_name) = &operand.kind
                {
                    let src_addr = if self.locals.has(src_name)
                        && let LocalLocation::Stack(soff) = self.locals.location_of(src_name)
                    {
                        Some(bp_addr(soff))
                    } else if self.globals.type_of(src_name).is_some() {
                        Some(format!("DGROUP:_{src_name}"))
                    } else {
                        None
                    };
                    if let Some(addr) = src_addr {
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                }
                // `c = (char)(<int_local> <op> K);` — byte arithmetic.
                // BCC emits `mov al, [int]; <op> al, K & 0xFF; mov
                // [c], al`. Saves the word load + word op + cbw vs.
                // narrowing at store time. Fixtures 1384, 1535, 1538,
                // 1539, 1540, 1541, 1542, 1543, 1544, 1545, 1546,
                // 1627, 2074.
                if ty.is_char_like()
                    && let ExprKind::Cast { ty: cast_ty, operand } = &value.kind
                    && cast_ty.is_char_like()
                    && let ExprKind::BinOp { op: binop, left, right } = &operand.kind
                    && matches!(binop,
                        BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                    )
                    && let ExprKind::Ident(src_name) = &left.kind
                    && let Some(k) = try_const_eval(right)
                {
                    let src_addr = if self.locals.has(src_name)
                        && let LocalLocation::Stack(soff) = self.locals.location_of(src_name)
                    {
                        Some(bp_addr(soff))
                    } else if self.globals.type_of(src_name).is_some() {
                        Some(format!("DGROUP:_{src_name}"))
                    } else {
                        None
                    };
                    if let Some(addr) = src_addr {
                        let k8 = (k & 0xFF) as u8;
                        let mnem = match binop {
                            BinOp::Add => "add",
                            BinOp::Sub => "sub",
                            BinOp::BitAnd => "and",
                            BinOp::BitOr => "or",
                            BinOp::BitXor => "xor",
                            _ => unreachable!(),
                        };
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        let _ = write!(self.out, "\t{mnem}\tal,{k8}\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                        return;
                    }
                }
                // `c = d;` char-to-char copy through bare ident.
                // Load byte and store byte, no widening. Mirrors the
                // init peephole. Fixture 2685.
                if ty.is_char_like()
                    && let ExprKind::Ident(src_name) = &value.kind
                {
                    let src_is_char = if self.locals.has(src_name) {
                        self.locals.type_of(src_name).is_char_like()
                    } else {
                        self.globals.type_of(src_name).map_or(false, |t| t.is_char_like())
                    };
                    if src_is_char {
                        let src_addr = if self.locals.has(src_name)
                            && let LocalLocation::Stack(soff) = self.locals.location_of(src_name)
                        {
                            Some(bp_addr(soff))
                        } else if self.globals.type_of(src_name).is_some() {
                            Some(format!("DGROUP:_{src_name}"))
                        } else {
                            None
                        };
                        if let Some(addr) = src_addr {
                            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                            let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                            return;
                        }
                    }
                }
                // `<stack-local> = &<global>;` — store the symbol's
                // offset directly into the stack slot. BCC emits this
                // as `C7 46 dd lo hi` with a FIXUPP, saving the
                // intermediate `mov ax, offset ...; mov [bp-N], ax`
                // pair. Fixture 601.
                if !ty.is_char_like()
                    && let ExprKind::AddressOf(sym) = &value.kind
                    && self.globals.contains(sym)
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset DGROUP:_{sym}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // `<pointer-local> = <global-array>;` — array-to-
                // pointer decay. Store the array's symbol offset
                // directly. Same immediate-store shape as `= &g`.
                // Fixtures 2328, 2541.
                if ty.pointee().is_some()
                    && let ExprKind::Ident(sym) = &value.kind
                    && let Some(gty) = self.globals.type_of(sym)
                    && gty.array_elem().is_some()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset DGROUP:_{sym}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // `<stack-int> = <int-global>++` / `--` — BCC loads
                // the pre-update value into AX, stores AX to the
                // stack slot, *then* applies the memory-direct side
                // effect. Order matters: defer the inc/dec until
                // after the use. Fixture 963.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && let Some(gty) = self.globals.type_of(target)
                    && (matches!(gty, Type::Int | Type::UInt) || gty.pointee().is_some())
                {
                    let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{target}\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\tword ptr DGROUP:_{target}\r\n");
                    return;
                }
                // `<stack-int> = <char-global>++` / `--` — load AL,
                // widen via cbw (or mov ah, 0 for uchar), store AX to
                // the stack slot, then defer the memory-direct
                // inc/dec on the byte. Fixture 966.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && let Some(gty) = self.globals.type_of(target)
                    && gty.is_char_like()
                {
                    let unsigned = gty.is_unsigned();
                    let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{target}\r\n");
                    if unsigned {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\tbyte ptr DGROUP:_{target}\r\n");
                    return;
                }
                // `<stack-int> = <reg-int>++` / `--` — store the
                // pre-update register value directly to the stack
                // slot, then apply the side effect. Skips the AX
                // round-trip our generic emit_update_to_ax path
                // takes. Fixture 649.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && !src_reg.is_byte()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},{}\r\n",
                        bp_addr(off),
                        src_reg.name(),
                    );
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\t{}\r\n", src_reg.name());
                    return;
                }
                // Char-src + int-dest postinc: `int r = c++` where c
                // is in a byte register. BCC widens to AX (cbw),
                // stores AX to the int slot, then bumps the source.
                // Different from the generic `emit_update_to_ax`
                // shape which inc'd before the store. Fixture 728.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && src_reg.is_byte()
                {
                    let unsigned = self.locals.type_of(target).is_unsigned();
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", src_reg.name());
                    if unsigned {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\t{}\r\n", src_reg.name());
                    return;
                }
                // Char-src + int-dest preinc: `int r = ++c`. BCC
                // threads through AL: load c, bump AL, write back
                // to c, then widen+store to the int slot. Fixture
                // 729.
                if !ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Pre,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && src_reg.is_byte()
                {
                    let unsigned = self.locals.type_of(target).is_unsigned();
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", src_reg.name());
                    let _ = write!(self.out, "\t{mnem}\tal\r\n");
                    let _ = write!(self.out, "\tmov\t{},al\r\n", src_reg.name());
                    if unsigned {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    return;
                }
                // Char `d = c++` where both d and c are byte. BCC
                // routes the byte through AL without `cbw`-widening,
                // stores to the byte stack slot, then bumps the
                // source register. Pattern: `mov al, <src>; mov
                // byte ptr [bp-N], al; inc <src>`. Fixture 725.
                if ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Post,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && src_reg.is_byte()
                {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", src_reg.name());
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{mnem}\t{}\r\n", src_reg.name());
                    return;
                }
                // Char `d = ++c` where both d and c are byte. BCC
                // works through AL: load c into AL, bump AL, then
                // write back to BOTH c and d. Pattern: `mov al,
                // <src>; inc al; mov <src>, al; mov byte ptr [bp-
                // N], al`. Fixture 727.
                if ty.is_char_like()
                    && let ExprKind::Update {
                        target,
                        op,
                        position: crate::ast::UpdatePosition::Pre,
                    } = &value.kind
                    && self.locals.has(target)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(target)
                    && src_reg.is_byte()
                {
                    let mnem = match op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", src_reg.name());
                    let _ = write!(self.out, "\t{mnem}\tal\r\n");
                    let _ = write!(self.out, "\tmov\t{},al\r\n", src_reg.name());
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    return;
                }
                // Reg-to-mem copy: `<stack-local> = <reg-local>` —
                // direct `mov word ptr [bp-N], <reg>` without the AX
                // round-trip. Restricted to plain int on both sides
                // (no pointers/arrays/chars/longs). Mirror of the
                // mem-to-reg peephole in `emit_store_reg`. Fixture
                // 1145 (`t = a;` with a in SI, t on stack).
                if matches!(ty, Type::Int)
                    && let ExprKind::Ident(src_name) = &value.kind
                    && self.locals.has(src_name)
                    && let LocalLocation::Reg(src_reg) = self.locals.location_of(src_name)
                    && !src_reg.is_byte()
                    && matches!(self.locals.type_of(src_name), Type::Int)
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},{}\r\n",
                        bp_addr(off),
                        src_reg.name()
                    );
                    return;
                }
                // `<stack-ptr-local> = &<global-arr>[K]` — emit a
                // single immediate-store using the linker-resolved
                // address. Saves the AX round-trip vs `mov ax,
                // offset ...; mov [bp-N], ax`. Fixture 2506
                // (`p = &a[7]; q = &a[2]`).
                if ty.pointee().is_some()
                    && let ExprKind::AddressOfArrayElem { array, byte_offset } = &value.kind
                    && self.globals.contains(array)
                {
                    if *byte_offset == 0 {
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},offset DGROUP:_{array}\r\n",
                            bp_addr(off),
                        );
                    } else {
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},offset DGROUP:_{array}+{byte_offset}\r\n",
                            bp_addr(off),
                        );
                    }
                    return;
                }
                // Stack pointer-typed local assigned to `<global-
                // array> + K_const`: scale by element size and
                // emit a single immediate store
                // `mov word ptr [bp-N], offset DGROUP:_<arr>+K*stride`.
                // Same fold as the stack-array init shape one level
                // up, but for global arrays. Fixture 1361 (`int *end;
                // end = a + 3;` with int a[3]; stride 2 → +6).
                if ty.pointee().is_some()
                    && let ExprKind::BinOp { op: BinOp::Add, left, right } = &value.kind
                    && let ExprKind::Ident(arr_name) = &left.kind
                    && let Some(gty) = self.globals.type_of(arr_name)
                    && let Some(elem_ty) = gty.array_elem()
                    && let Some(k) = try_const_eval(right)
                {
                    let stride = u32::from(elem_ty.size_bytes());
                    let scaled = (k as u32).wrapping_mul(stride) & 0xFFFF;
                    if scaled == 0 {
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},offset DGROUP:_{arr_name}\r\n",
                            bp_addr(off),
                        );
                    } else {
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},offset DGROUP:_{arr_name}+{scaled}\r\n",
                            bp_addr(off),
                        );
                    }
                    return;
                }
                self.emit_expr_to_ax(value);
                if ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                } else {
                    let src = if self.try_collapse_lhs_clobber_to_dx() { "dx" } else { "ax" };
                    let _ = write!(self.out, "\tmov\tword ptr {},{src}\r\n", bp_addr(off));
                }
            }
            LocalLocation::Reg(reg) => self.emit_store_reg(reg, value),
        }
    }
    /// Store `expr`'s value into register `reg`. For 16-bit registers
    /// BCC special-cases the zero-init via `xor reg,reg` (one byte
    /// shorter); 8-bit registers use plain `mov reg,0` even for zero
    /// (fixture 050/051).
    pub(crate) fn emit_store_reg(&mut self, reg: Reg, expr: &Expr) {
        if let Some(v) = try_const_eval(expr) {
            if reg.is_byte() {
                let v8 = v & 0xFF;
                let _ = write!(self.out, "\tmov\t{},{v8}\r\n", reg.name());
            } else if v.trailing_zeros() >= 16 {
                let _ = write!(self.out, "\txor\t{0},{0}\r\n", reg.name());
            } else {
                let v16 = v & 0xFFFF;
                let _ = write!(self.out, "\tmov\t{},{v16}\r\n", reg.name());
            }
            return;
        }
        // String-literal init: BCC emits the address as a direct
        // immediate, skipping the AX round-trip used for `&x` (which
        // is a runtime address). Fixture 088: `char *s = "hi";` →
        // `mov si, offset DGROUP:s@`.
        if let ExprKind::StringLit(bytes) = &expr.kind {
            assert!(
                !reg.is_byte(),
                "string-literal address into a byte register is impossible (pointer is 2 bytes)"
            );
            let offset = self.strings.intern(bytes);
            if offset == 0 {
                let _ = write!(self.out, "\tmov\t{},offset DGROUP:s@\r\n", reg.name());
            } else {
                let _ = write!(
                    self.out,
                    "\tmov\t{},offset DGROUP:s@+{offset}\r\n",
                    reg.name(),
                );
            }
            return;
        }
        // `&<global>` direct-to-register: same shape as the string-
        // literal init — a linker-resolved constant, so a direct
        // `mov <reg>, offset DGROUP:_<sym>` works (no AX round-trip).
        // Fixture 308 (`long *p = &g;` with p in SI).
        if let ExprKind::AddressOf(sym) = &expr.kind
            && self.globals.contains(sym)
        {
            assert!(!reg.is_byte(), "global address into a byte register is impossible (pointer is 2 bytes)");
            let _ = write!(self.out, "\tmov\t{},offset DGROUP:_{sym}\r\n", reg.name());
            return;
        }
        // `&<arr>[K]` direct-to-register: linker resolves
        // `offset DGROUP:_<arr>+(K*stride)` to an immediate. Same
        // `mov <reg>, offset ...` shape as `&<global>` above; no
        // AX round-trip. Fixture 2584 (`p = &a[2]` with p in SI).
        if let ExprKind::AddressOfArrayElem { array, byte_offset } = &expr.kind
            && self.globals.contains(array)
        {
            assert!(!reg.is_byte(), "array-element address into a byte register is impossible");
            if *byte_offset == 0 {
                let _ = write!(
                    self.out,
                    "\tmov\t{},offset DGROUP:_{array}\r\n",
                    reg.name(),
                );
            } else {
                let _ = write!(
                    self.out,
                    "\tmov\t{},offset DGROUP:_{array}+{byte_offset}\r\n",
                    reg.name(),
                );
            }
            return;
        }
        // Array decay to a register-resident pointer: `<reg> = <arr>`
        // where `arr` is a global array. Equivalent to `&arr[0]` —
        // and like `&<global>` above, takes the direct `mov <reg>,
        // offset DGROUP:_<sym>` form (no `lea / mov` round-trip).
        // Fixture 313 (`long *p = a;`).
        if let ExprKind::Ident(name) = &expr.kind
            && let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Array { .. })
        {
            assert!(!reg.is_byte(), "array address into a byte register is impossible");
            let _ = write!(self.out, "\tmov\t{},offset DGROUP:_{name}\r\n", reg.name());
            return;
        }
        // Pointer init from `<stack-array> + K_const`: fold the
        // element offset into the LEA's displacement. BCC pattern is
        // `lea ax, [bp+(base + K*stride)]; mov <reg>, ax` — no
        // runtime add of the stride. Fixture 1047 (`int *p = a + 1;`).
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &expr.kind
            && let ExprKind::Ident(arr_name) = &left.kind
            && self.locals.has(arr_name)
            && let Some(elem_ty) = self.locals.type_of(arr_name).array_elem()
            && let Some(k) = try_const_eval(right)
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
        {
            assert!(!reg.is_byte(), "array+const into a byte register is impossible");
            let stride = i32::from(elem_ty.size_bytes());
            let adj_off = i32::from(base_off) + (k as i32) * stride;
            let adj_off_i16 = i16::try_from(adj_off).expect("array+const offset fits in i16");
            let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(adj_off_i16));
            let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
            return;
        }
        // Pointer init from `<stack-array> + <int_lvalue>`: scale
        // the index by stride, then add the array's LEA. BCC's
        // shape: `mov ax, <i>; shl ax (×log2 stride); lea dx, base;
        // add ax, dx; mov <reg>, ax`. Fixture 1278 (`int *p = a +
        // i;` with int array, var i).
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &expr.kind
            && let ExprKind::Ident(arr_name) = &left.kind
            && self.locals.has(arr_name)
            && let Some(elem_ty) = self.locals.type_of(arr_name).array_elem()
            && let Some(idx_src) = self.int_lvalue_addr(right)
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
        {
            assert!(!reg.is_byte(), "array+var into a byte register is impossible");
            let stride = elem_ty.size_bytes();
            let shifts = match stride {
                1 => 0,
                2 => 1,
                4 => 2,
                _ => panic!("unsupported stride {stride} for array+var ptr init"),
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {idx_src}\r\n");
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
            }
            let _ = write!(self.out, "\tlea\tdx,word ptr {}\r\n", bp_addr(base_off));
            self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
            let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
            return;
        }
        // Reg-to-reg copy: `<reg> = <other-reg>` where the RHS is a
        // bare identifier naming another register-resident int
        // local. BCC emits `mov <dest>, <src>` directly, skipping
        // the AX round-trip. Fixture 1143 (`x = y;` with both in
        // SI/DI).
        if let ExprKind::Ident(name) = &expr.kind
            && self.locals.has(name)
            && let LocalLocation::Reg(src_reg) = self.locals.location_of(name)
            && !src_reg.is_byte()
            && !reg.is_byte()
        {
            let _ = write!(self.out, "\tmov\t{},{}\r\n", reg.name(), src_reg.name());
            return;
        }
        // Mem-to-reg copy: `<reg> = <stack-local>` where the RHS is
        // a bare identifier (possibly wrapped in a numeric/pointer
        // cast that doesn't change the bit-width) for a stack-
        // resident int/uint/pointer local. BCC emits `mov <reg>,
        // word ptr [bp-N]` directly, skipping the AX round-trip.
        // Fixture 1145 (`b = t;` int), 2852 (`q = p;` int pointer),
        // 1779 (`int v = (int)p;` cast of pointer-local to int).
        if let Some(name) = match &expr.kind {
            ExprKind::Ident(n) => Some(n.as_str()),
            ExprKind::Cast { operand, ty } if ty.is_int_like() || ty.pointee().is_some() => {
                match &operand.kind {
                    ExprKind::Ident(n) => Some(n.as_str()),
                    _ => None,
                }
            }
            _ => None,
        }
            && self.locals.has(name)
            && let LocalLocation::Stack(src_off) = self.locals.location_of(name)
            && (self.locals.type_of(name).is_int_like() || self.locals.type_of(name).pointee().is_some())
            && !reg.is_byte()
        {
            let _ = write!(
                self.out,
                "\tmov\t{},word ptr {}\r\n",
                reg.name(),
                bp_addr(src_off)
            );
            return;
        }
        // Mem-to-reg copy from a global: `<reg> = <global>` where the
        // RHS is a bare identifier (possibly wrapped in a pointer
        // cast) naming an int- or pointer-shaped global. BCC emits
        // `mov <reg>, word ptr DGROUP:_<sym>` (4 bytes) directly,
        // skipping the AX round-trip (a1 mem16 + mov reg, ax = 5
        // bytes). Fixture 2626 (`ip = (int *)vp;` with vp global).
        if let Some(name) = match &expr.kind {
            ExprKind::Ident(n) => Some(n.as_str()),
            ExprKind::Cast { operand, ty } if ty.pointee().is_some() => match &operand.kind {
                ExprKind::Ident(n) => Some(n.as_str()),
                _ => None,
            },
            _ => None,
        }
            && let Some(gty) = self.globals.type_of(name)
            && (gty.is_int_like() || gty.pointee().is_some())
            && !reg.is_byte()
        {
            let _ = write!(
                self.out,
                "\tmov\t{},word ptr DGROUP:_{name}\r\n",
                reg.name(),
            );
            return;
        }
        // `<reg> = <reg-ptr>-><field>` — load directly from
        // `[<reg-ptr>+<field-off>]` into the destination register,
        // skipping the AX round-trip. Fixture 3343 (`p = p->next`
        // for `struct Node *p` in SI).
        if let ExprKind::Member {
            base,
            field,
            kind: crate::ast::MemberKind::Arrow,
        } = &expr.kind
            && let ExprKind::Ident(base_name) = &base.kind
            && self.locals.has(base_name)
            && let LocalLocation::Reg(base_reg) = self.locals.location_of(base_name)
            && let Some(base_pointee) = self.locals.type_of(base_name).pointee()
            && let Some((field_off, field_ty)) = base_pointee.field(field)
            && (field_ty.is_int_like() || field_ty.pointee().is_some())
            && !reg.is_byte()
        {
            let bx_disp = if field_off == 0 {
                format!("[{}]", base_reg.name())
            } else {
                format!("[{}+{field_off}]", base_reg.name())
            };
            let _ = write!(
                self.out,
                "\tmov\t{},word ptr {bx_disp}\r\n",
                reg.name(),
            );
            return;
        }
        // Non-constant char init: untested. Best guess would be
        // `<compute to AL> / mov <reg>, al`, but until a fixture pins
        // the load-to-AL path, bail.
        assert!(
            !reg.is_byte(),
            "non-constant char init/assign not yet supported (no fixture)"
        );
        self.emit_expr_to_ax(expr);
        // Peephole: if `expr` is `<this-reg> <op> <ax-clobbering-rhs>`,
        // emit_expr_to_ax produces `push ax; mov ax, <reg>; pop dx;
        // <op> ax, dx`. Collapse to `mov dx, <reg>; <op> dx, ax` —
        // result lives in DX, store from DX. Skips the push/pop
        // pair and matches BCC's compound-assign-like shape.
        // Fixture 2397 (`sum = sum + words[i][0]` with sum in DI).
        let src = if !reg.is_byte() && self.try_collapse_lhs_clobber_to_dx() {
            "dx"
        } else {
            "ax"
        };
        let _ = write!(self.out, "\tmov\t{},{src}\r\n", reg.name());
    }
}
