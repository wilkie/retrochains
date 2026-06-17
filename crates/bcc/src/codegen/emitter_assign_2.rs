use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// `*<ptr>` in rvalue position. The inner pointer expression can
    /// be a bare `Ident(p)` or — for fixtures 091, 092, 094 — a
    /// `BinOp(Add, Ident(p), <offset>)` (and presumably Sub later).
    /// Both lower to a `<width> ptr [<addressing-mode>]` load:
    ///
    /// - **`*<ident>`** → `[<reg>]` (the pointer must be enregistered;
    ///   stack-resident pointers don't have an addressing form like
    ///   `[[bp-N]]` so we'd need a temp load — no fixture yet).
    /// - **`*(<ident> + K)`** with K constant → `[<reg> + K*stride]`
    ///   (fixture 091: `*(p + 1)` with `p: int *` → `[si+2]`).
    /// - **`*(<ident> + <i>)`** with i variable → the load/shl/add
    ///   sequence with the result in BX (fixture 092). Both pointer
    ///   and index can be either register- or stack-resident; only
    ///   the all-stack form is captured today.
    pub(crate) fn emit_deref_to_ax(&mut self, ptr: &Expr) {
        // `*++p` / `*--p` — pre-increment/decrement the pointer (a side effect
        // on the pointer variable, scaled by the pointee stride), then
        // dereference the *updated* value. (Postfix `*p++` derefs the old value
        // and is handled in the update-expression path.) Fixture 4282.
        if let ExprKind::Update { target, op, position: UpdatePosition::Pre } = &ptr.kind {
            self.emit_update_in_place(target, *op, UpdatePosition::Pre);
            // A `char` pointee bounces the deref through BX after the update
            // (`mov bx,si; mov al,[bx]; cbw`) — a BCC quirk; an `int` derefs the
            // pointer register directly. Fixtures 4282 (int), 4284 (char).
            let pointee = self.locals.type_of(target).pointee().cloned();
            if let Some(pointee) = pointee
                && pointee.is_char_like()
                && let LocalLocation::Reg(reg) = self.locals.location_of(target)
            {
                let reg_name = reg.name();
                let _ = write!(self.out, "\tmov\tbx,{reg_name}\r\n");
                self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                self.emit_widen_al(&pointee);
                return;
            }
            let updated = Expr { kind: ExprKind::Ident(target.clone()), span: ptr.span };
            self.emit_deref_to_ax(&updated);
            return;
        }
        // `*p` where p is a seg-qualified pointer (`int _ss/_es/_cs/_ds *`).
        // The qualifier becomes a TASM `<seg>:` operand prefix on the
        // load. DS is the default segment and is elided. Fixtures
        // 4064 (_ss), 4066 (_cs), 4067 (_ds, prefix elided).
        if let ExprKind::Ident(p_name) = &ptr.kind
            && self.locals.has(p_name)
            && let p_ty = self.locals.type_of(p_name).clone()
            && let Some(seg) = p_ty.seg_qualifier()
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
        {
            let pointee = p_ty.pointee().expect("SegPointer has a pointee");
            let reg_name = reg.name();
            let seg_prefix = if matches!(seg, crate::ast::SegReg::Ds) {
                String::new()
            } else {
                format!("{}:", seg.name())
            };
            if pointee.is_char_like() {
                let _ = write!(
                    self.out,
                    "\tmov\tal,byte ptr {seg_prefix}[{reg_name}]\r\n",
                );
                self.emit_widen_al(pointee);
            } else {
                let _ = write!(
                    self.out,
                    "\tmov\tax,word ptr {seg_prefix}[{reg_name}]\r\n",
                );
            }
            return;
        }
        // `*(T *)<inner>` — pointer cast around the dereferenced
        // pointer. The cast determines the effective pointee width
        // (e.g. `*(char *)p` reads a byte even if `p` is `int *`).
        // Recurse with the inner, but if the cast narrows the
        // pointee to a different type, emit through BX with the
        // cast-derived width. Fixture 3163.
        if let ExprKind::Cast { ty: cast_ty, operand } = &ptr.kind
            && let Type::Pointer(cast_pointee) = cast_ty
            && let ExprKind::Ident(p_name) = &operand.kind
            && self.locals.has(p_name)
        {
            let cast_pointee = (**cast_pointee).clone();
            let p_ty = self.locals.type_of(p_name).clone();
            // Only kick in when the cast actually changes the pointee
            // size (char vs int); same-size casts produce identical
            // bytes regardless.
            if let Some(orig_pointee) = p_ty.pointee()
                && orig_pointee.size_bytes() != cast_pointee.size_bytes()
            {
                match self.locals.location_of(p_name) {
                    LocalLocation::Reg(reg) => {
                        let addr_reg = reg.name();
                        if cast_pointee.is_char_like() {
                            let _ = write!(self.out, "\tmov\tal,byte ptr [{addr_reg}]\r\n");
                            self.emit_widen_al(&cast_pointee);
                        } else {
                            let _ = write!(self.out, "\tmov\tax,word ptr [{addr_reg}]\r\n");
                        }
                    }
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                        if cast_pointee.is_char_like() {
                            self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                            self.emit_widen_al(&cast_pointee);
                        } else {
                            self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
                        }
                    }
                }
                return;
            }
        }
        // `*<call>()` — deref of a function-call's pointer return.
        // Call the function (result in AX = pointer), copy to BX,
        // read `[bx]`. Fixture 1343 (`*nextp("ab")`).
        if let ExprKind::Call { name: fname, args } = &ptr.kind
            && let Some(ret_ty) = self.signatures.ret_ty_of(fname)
            && let Some(pointee) = ret_ty.pointee()
        {
            let pointee = pointee.clone();
            self.emit_call(fname, args);
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
            if pointee.is_char_like() {
                self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                self.emit_widen_al(&pointee);
            } else {
                self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
            }
            return;
        }
        // `*(<struct>.<field>)` / `*<struct>-><field>` where the
        // field is a pointer: walk the lvalue chain to a constant
        // address, load that into BX, then read `[bx]`. Fixture 2981.
        if let ExprKind::Member { base, field, kind } = &ptr.kind {
            let _ = kind;
            let _ = base;
            let _ = field;
            if let Some((name, total_off, leaf_ty)) = self.try_lvalue_chain_addr(ptr)
                && let Some(addr) = self.resolve_chain_addr(&name, total_off)
                && let Some(pointee) = leaf_ty.pointee()
            {
                let pointee = pointee.clone();
                let _ = write!(self.out, "\tmov\tbx,word ptr {addr}\r\n");
                if pointee.is_char_like() {
                    self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                    self.emit_widen_al(&pointee);
                } else {
                    self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
                }
                return;
            }
        }
        // `*arr[K]` where arr is a pointer-array (global or local).
        // Load arr[K] into BX, then read `[bx]`. Pointee width
        // picks byte/word. Fixtures 2470, 2608.
        if let ExprKind::ArrayIndex { array, index } = &ptr.kind
            && let ExprKind::Ident(arr_name) = &array.kind
        {
            // Resolve the array's element type to know what pointee
            // we're dereferencing. Both global and stack arrays of
            // pointers route through the same shape.
            let elem_ty = if let Some(g_ty) = self.globals.type_of(arr_name) {
                g_ty.array_elem().cloned()
            } else if self.locals.has(arr_name) {
                self.locals.type_of(arr_name).array_elem().cloned()
            } else {
                None
            };
            if let Some(elem_ty) = elem_ty
                && let Some(pointee) = elem_ty.pointee()
            {
                let pointee = pointee.clone();
                let stride = u32::from(elem_ty.size_bytes());
                if let Some(k) = try_const_eval(index) {
                    let off = k.wrapping_mul(stride);
                    let load = if self.globals.contains(arr_name) {
                        if off == 0 {
                            format!("DGROUP:_{arr_name}")
                        } else {
                            format!("DGROUP:_{arr_name}+{off}")
                        }
                    } else if let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name) {
                        let elem_off = i32::from(base_off) + off as i32;
                        let off16 = i16::try_from(elem_off).unwrap_or(i16::MAX);
                        bp_addr(off16)
                    } else {
                        unreachable!()
                    };
                    let _ = write!(self.out, "\tmov\tbx,word ptr {load}\r\n");
                    if pointee.is_char_like() {
                        self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                        self.emit_widen_al(&pointee);
                    } else {
                        self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
                    }
                    return;
                }
                // Variable index on a global pointer array: scale i
                // into BX, load the element pointer via the indexed
                // form, then read through [bx]. Fixture 3592.
                if self.globals.contains(arr_name) {
                    let elem_ty_clone = elem_ty.clone();
                    self.emit_index_into_bx(index, &elem_ty_clone);
                    let _ = write!(
                        self.out,
                        "\tmov\tbx,word ptr DGROUP:_{arr_name}[bx]\r\n",
                    );
                    if pointee.is_char_like() {
                        self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                        self.emit_widen_al(&pointee);
                    } else {
                        self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
                    }
                    return;
                }
            }
        }
        // `*(a + i)` where `a` is a global array (or char array): the
        // `a + i` is array-decay + variable offset. Same byte shape
        // as the array-index path: scale i into BX, then read
        // through `[bx + _a]`. Fixture 1379.
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &ptr.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(gty) = self.globals.type_of(name)
            && let Some(elem_ty) = gty.array_elem()
        {
            let elem_ty = elem_ty.clone();
            let width = ptr_width(&elem_ty);
            self.emit_index_into_bx(right, &elem_ty);
            if elem_ty.is_char_like() {
                let _ = write!(
                    self.out,
                    "\tmov\tal,byte ptr DGROUP:_{name}[bx]\r\n",
                );
                self.emit_widen_al(&elem_ty);
            } else {
                let _ = write!(
                    self.out,
                    "\tmov\tax,{width} ptr DGROUP:_{name}[bx]\r\n",
                );
            }
            return;
        }
        // `*((<ptr-cast>)&<lvalue>)` — pure-type cast of an address.
        // Equivalent to reading the underlying lvalue but typed as
        // the cast's pointee. We honor the cast's WIDTH (byte for
        // char* casts, word otherwise) and skip the address load
        // entirely. Fixture 2430 (`*(char *)&i`). No `cbw` for
        // char-cast: BCC stores AL straight into a char target,
        // matching the observed shape.
        if let ExprKind::Cast { ty: cast_ty, operand: inner } = &ptr.kind
            && let crate::ast::Type::Pointer(cast_pointee) = cast_ty
            && let ExprKind::AddressOf(sym) = &inner.kind
        {
            let cast_pointee = (**cast_pointee).clone();
            if self.locals.has(sym)
                && let LocalLocation::Stack(off) = self.locals.location_of(sym)
            {
                if cast_pointee.is_char_like() {
                    let _ = write!(
                        self.out,
                        "\tmov\tal,byte ptr {}\r\n",
                        bp_addr(off),
                    );
                } else {
                    let _ = write!(
                        self.out,
                        "\tmov\tax,word ptr {}\r\n",
                        bp_addr(off),
                    );
                }
                return;
            }
        }
        // `*(p + i + j)` for a pointer p and two var offsets: load i,
        // scale, add to p in BX, then add j*stride to BX in place,
        // then read through BX. Mirrors BCC's actual sequence (no
        // intermediate AX save). Fixture 3468.
        if let ExprKind::BinOp { op: BinOp::Add, left: outer_l, right: outer_r } = &ptr.kind
            && let ExprKind::BinOp { op: BinOp::Add, left: inner_l, right: inner_r } = &outer_l.kind
            && let ExprKind::Ident(name) = &inner_l.kind
            && self.locals.has(name)
            && let Some(pointee) = self.locals.type_of(name).pointee()
        {
            let pointee = pointee.clone();
            let stride = u32::from(pointee.size_bytes());
            if stride == 1 || stride == 2 {
                // First half: emit i scaled into AX, then load p into
                // BX and add AX.
                self.emit_expr_to_ax(inner_r);
                if stride == 2 {
                    self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                }
                match self.locals.location_of(name) {
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                    }
                    LocalLocation::Reg(reg) => {
                        let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                    }
                }
                self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
                // Second half: j scaled into AX, then add to BX.
                self.emit_expr_to_ax(outer_r);
                if stride == 2 {
                    self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                }
                self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
                if pointee.is_char_like() {
                    self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                    self.emit_widen_al(&pointee);
                } else {
                    self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
                }
                return;
            }
        }
        // `*(p + <offset>)` where p is a `_seg` segment-only pointer:
        // load the segment into ES, then read via `es:[<offset>]`.
        // Offset can be a constant (folded into displacement) or
        // a near-pointer local (loaded into SI first). Fixtures
        // 4070 (offset 0), 4071 (const offset), 4073 (seg + near).
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &ptr.kind
            && let ExprKind::Ident(name) = &left.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_seg_selector()
        {
            let p_ty = self.locals.type_of(name).clone();
            let pointee = p_ty.pointee().expect("SegSelector has a pointee").clone();
            self.emit_seg_selector_deref_read(name, &pointee, right);
            return;
        }
        // `*(p + offset)` shapes go through a shared helper that
        // builds the addressing mode.
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &ptr.kind
            && let ExprKind::Ident(name) = &left.kind
            && self.locals.has(name)
        {
            let ty = self.locals.type_of(name).clone();
            if let Some(pointee) = ty.pointee() {
                return self.emit_deref_pointer_plus_offset(name, pointee.clone(), right);
            }
        }
        // `*p++` / `*p--`: post-update inside a deref (fixture 199).
        // BCC saves the pre-update pointer in BX, advances the
        // register-resident pointer by `stride` 1-byte `inc`/`dec`
        // ops (when stride ≤ 2), then reads through `[bx]`.
        if let ExprKind::Update { target, op, position: UpdatePosition::Post } = &ptr.kind {
            let ty = self.locals.type_of(target).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{target}++`: not a pointer type");
            };
            let LocalLocation::Reg(reg) = self.locals.location_of(target) else {
                panic!("stack-resident pointer in `*p++` not yet supported (no fixture)");
            };
            let reg_name = reg.name();
            let stride = pointee.size_bytes();
            let mnemonic = match op {
                UpdateOp::Inc => "inc",
                UpdateOp::Dec => "dec",
            };
            // BX / SI / DI are the only 8086 base registers usable
            // in r/m=`[reg]`. Pointers parked in DX or CX (the
            // extended-pool slots) need a `mov bx, <reg>` before
            // the `[bx]` load. Fixture 1808 (`*s++` with s in DX).
            let needs_bx_indirect = !matches!(reg, Reg::Bx | Reg::Si | Reg::Di);
            if pointee.is_char_like() && !self.in_arg_expr && !needs_bx_indirect {
                // Char dereference (non-arg context): BCC reads
                // through the pointer first, then defers the
                // increment until after the consumer of AL/AX. We
                // emit the read here and stash the pending inc so
                // the next statement boundary flushes it. Fixture
                // 2000 (`sum = *p++` chain with char pointer).
                let _ = write!(self.out, "\tmov\tal,byte ptr [{reg_name}]\r\n");
                self.out.extend_from_slice(b"\tcbw\t\r\n");
                if stride == 1 || stride == 2 {
                    self.pending_post_update = Some((
                        reg_name.to_string(),
                        stride as u8,
                        mnemonic,
                    ));
                } else {
                    panic!("`*p++` (char) with pointee stride > 2 not yet supported");
                }
            } else {
                let _ = write!(self.out, "\tmov\tbx,{reg_name}\r\n");
                if stride == 1 || stride == 2 {
                    for _ in 0..stride {
                        let _ = write!(self.out, "\t{mnemonic}\t{reg_name}\r\n");
                    }
                } else {
                    panic!("`*p++` with pointee stride > 2 not yet supported (no fixture)");
                }
                if pointee.is_char_like() {
                    self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                } else {
                    let width = ptr_width(pointee);
                    let _ = write!(self.out, "\tmov\tax,{width} ptr [bx]\r\n");
                }
            }
            return;
        }
        let (base_name, depth) = deref_chain_root(ptr);
        // Single-deref of a stack/register-resident local stays on
        // the original fast path (`mov al,byte ptr [si]` etc.) so
        // SI/DI-resident pointers don't bounce through BX.
        let is_global = self.globals.type_of(base_name).is_some();
        if depth == 0 && !is_global {
            let ty = self.locals.type_of(base_name).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{base_name}`: not a pointer type");
            };
            let width = ptr_width(pointee);
            match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) => {
                    let addr_reg = reg.name();
                    if pointee.is_char_like() {
                        let _ = write!(self.out, "\tmov\tal,byte ptr [{addr_reg}]\r\n");
                        self.emit_widen_al(pointee);
                    } else {
                        let _ = write!(self.out, "\tmov\tax,{width} ptr [{addr_reg}]\r\n");
                    }
                }
                LocalLocation::Stack(off) => {
                    if matches!(ty, Type::FarPointer { .. }) {
                        // Far-pointer deref: `les bx, [bp+off]`
                        // loads the 4-byte (offset, segment) pair
                        // into BX (offset) and ES (segment) in one
                        // step. The follow-up read uses the ES
                        // override prefix. Fixtures 1649, 1652,
                        // 2058, 2250 (int read);
                        // future `char far *` slices add the AL
                        // variant. Width is always the pointee's
                        // natural size since `les` already
                        // dispatched the segment.
                        let _ = write!(self.out, "\tles\tbx,word ptr {}\r\n", bp_addr(off));
                        if pointee.is_char_like() {
                            self.out.extend_from_slice(b"\tmov\tal,byte ptr es:[bx]\r\n");
                            self.emit_widen_al(pointee);
                        } else {
                            self.out.extend_from_slice(b"\tmov\tax,word ptr es:[bx]\r\n");
                        }
                    } else {
                        // Stack-resident near pointer: load into BX,
                        // then read through [bx]. Fixture 1932.
                        let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                        if pointee.is_char_like() {
                            self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                            self.emit_widen_al(pointee);
                        } else {
                            let _ = write!(self.out, "\tmov\tax,{width} ptr [bx]\r\n");
                        }
                    }
                }
            };
            return;
        }
        // Far-pointer global deref `*p` (depth=0) — pointer promotion
        // in compact / large turned `int *p` into `int far *p`, so
        // the slot is 4 bytes and the read goes through `les bx +
        // mov es:[bx]`. Fixtures 3900 / 3901.
        if depth == 0
            && let Some(gty) = self.globals.type_of(base_name)
            && matches!(gty, Type::FarPointer { .. })
            && let Some(pointee) = gty.pointee()
        {
            let pointee = pointee.clone();
            let _ = write!(
                self.out,
                "\tles\tbx,dword ptr DGROUP:_{base_name}\r\n"
            );
            if pointee.is_char_like() {
                self.out.extend_from_slice(b"\tmov\tal,byte ptr es:[bx]\r\n");
                self.emit_widen_al(&pointee);
            } else {
                self.out.extend_from_slice(b"\tmov\tax,word ptr es:[bx]\r\n");
            }
            return;
        }
        // Chain path: land the address-to-be-deref'd-once-more in BX,
        // then do the final load. Fixture 195 (`int **p` → `**p`)
        // hits depth=1; fixture 193 hits depth=0 on a global.
        let final_ty = self.emit_chain_to_bx(base_name, depth);
        if final_ty.is_char_like() {
            self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
            self.emit_widen_al(&final_ty);
        } else {
            let width = ptr_width(&final_ty);
            let _ = write!(self.out, "\tmov\tax,{width} ptr [bx]\r\n");
        }
    }
    pub(crate) fn emit_deref_pointer_plus_offset(
        &mut self,
        ptr_name: &str,
        pointee: Type,
        offset: &Expr,
    ) {
        let stride = u32::from(pointee.size_bytes());
        let load_byte = pointee.is_char_like();
        if let Some(k) = try_const_eval(offset) {
            // Constant offset — fold to indexed addressing on the pointer
            // register. A stack-resident pointer (`-r-`, or one BCC couldn't
            // promote) is loaded into `bx` first, then indexed through it —
            // identical codegen to the `p[K]` subscript path. Fixture 4276
            // (`return *(p + 2);`).
            let reg_name = match self.locals.location_of(ptr_name) {
                LocalLocation::Reg(reg) => reg.name(),
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                    "bx"
                }
            };
            let byte_off = k * stride;
            let addr = if byte_off == 0 {
                format!("[{reg_name}]")
            } else {
                format!("[{reg_name}+{byte_off}]")
            };
            if load_byte {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
            return;
        }
        // Variable offset. Fixture 092 (both p and i on the stack):
        //   mov ax, word ptr [bp-i]
        //   shl ax, 1               ; * stride (stride=2 for int)
        //   mov bx, word ptr [bp-p]
        //   add bx, ax
        //   mov ax, word ptr [bx]
        // Reg-resident variants are inferred but unobserved.
        //
        // Char-stride (1) memory-direct add: when the offset is a
        // simple int lvalue, BCC skips the AX route and adds the
        // index memory directly to BX. Fixture 3227 (`*(p + i)` for
        // `char *p, int i`).
        if stride == 1
            && let Some(idx_addr) = self.int_lvalue_addr(offset)
        {
            match self.locals.location_of(ptr_name) {
                LocalLocation::Stack(off) => {
                    let _ = write!(
                        self.out,
                        "\tmov\tbx,word ptr {}\r\n",
                        bp_addr(off),
                    );
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                }
            }
            let _ = write!(self.out, "\tadd\tbx,word ptr {idx_addr}\r\n");
            if load_byte {
                self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                if !self.skip_widen {
                    if pointee.is_unsigned() {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                }
            } else {
                self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
            }
            return;
        }
        self.emit_expr_to_ax(offset);
        if stride == 2 {
            self.out.extend_from_slice(b"\tshl\tax,1\r\n");
        } else if stride != 1 {
            panic!("non-1/2 pointer stride not yet supported (no fixture)");
        }
        match self.locals.location_of(ptr_name) {
            LocalLocation::Stack(off) => {
                let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
            }
            LocalLocation::Reg(reg) => {
                let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
            }
        }
        self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
        if load_byte {
            self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        } else {
            self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
        }
    }
    /// Store AX through an arbitrary lvalue, leaving AX intact for
    /// the surrounding expression to consume. Used by the
    /// `<lvalue> = <value>` expression form (the statement form
    /// goes through `emit_deref_assign` / `emit_array_assign`,
    /// which assume AX is scratch). Today only `*<reg-ptr>` /
    /// `*<stack-ptr>` are supported — enough for the fixtures that
    /// exercise lvalue-assign in a value context. Fixture 3333.
    /// Attempt the "address-first, value-second" emission shape for
    /// `<lvalue> = <value>` in expression position. Returns true if
    /// it handled the assign (caller skips the value-first path).
    /// Used for lvalues whose address computation needs AX as
    /// scratch — chiefly `<stack-arr>[<var>]` with a variable index
    /// (`lea ax, [bp-N]; add bx, ax`). Fixture 1986.
    pub(crate) fn try_emit_assign_lvalue_addr_first(
        &mut self,
        target: &Expr,
        value: &Expr,
    ) -> bool {
        let ExprKind::ArrayIndex { array, index } = &target.kind else {
            return false;
        };
        let ExprKind::Ident(arr_name) = &array.kind else {
            return false;
        };
        if !self.locals.has(arr_name) {
            return false;
        }
        let arr_ty = self.locals.type_of(arr_name).clone();
        let Some(elem_ty) = arr_ty.array_elem() else {
            return false;
        };
        if !matches!(elem_ty, Type::Int | Type::UInt) {
            return false;
        }
        let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
        else {
            return false;
        };
        if try_const_eval(index).is_some() {
            // Const-index path is the regular `mov [bp-N+K*2], ax`
            // store, handled by the value-first fallback (no AX
            // pressure during address computation).
            return false;
        }
        // Variable index: load i into BX, scale to byte offset,
        // then add the array's `lea` base.
        let ExprKind::Ident(idx_name) = &index.kind else {
            return false;
        };
        if !self.locals.has(idx_name) {
            return false;
        }
        let LocalLocation::Reg(idx_reg) = self.locals.location_of(idx_name)
        else {
            return false;
        };
        let _ = write!(self.out, "\tmov\tbx,{}\r\n", idx_reg.name());
        self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
        let _ = write!(
            self.out,
            "\tlea\tax,word ptr {}\r\n",
            bp_addr(base_off),
        );
        self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
        // Now BX = &arr[i]; load value into AX, then store.
        self.emit_expr_to_ax(value);
        self.out.extend_from_slice(b"\tmov\tword ptr [bx],ax\r\n");
        true
    }
    pub(crate) fn emit_store_ax_to_lvalue(&mut self, target: &Expr) {
        if let ExprKind::Deref(inner) = &target.kind
            && let ExprKind::Ident(name) = &inner.kind
            && self.locals.has(name)
        {
            let ty = self.locals.type_of(name).clone();
            let pointee = ty.pointee().expect("`*p =` needs pointer type").clone();
            let width = if pointee.is_char_like() { "byte" } else { "word" };
            let src = if pointee.is_char_like() { "al" } else { "ax" };
            match self.locals.location_of(name) {
                LocalLocation::Reg(reg) => {
                    let _ = write!(
                        self.out,
                        "\tmov\t{width} ptr [{}],{src}\r\n",
                        reg.name(),
                    );
                }
                LocalLocation::Stack(off) => {
                    let _ = write!(
                        self.out,
                        "\tmov\tbx,word ptr {}\r\n",
                        bp_addr(off),
                    );
                    let _ = write!(self.out, "\tmov\t{width} ptr [bx],{src}\r\n");
                }
            }
            return;
        }
        // `*d++ = AX` — post-increment deref-assign through a
        // reg-resident pointer. Mirror the read-side `*p++` shape:
        // copy the pre-update pointer into BX, then store through
        // `[bx]`, then increment the pointer by stride. For DX/CX
        // pointers the BX indirection is mandatory because those
        // regs aren't 8086 base registers. Fixture 1808
        // (`*d++ = *s++` in the strcpy loop).
        if let ExprKind::Deref(inner) = &target.kind
            && let ExprKind::Update {
                target: p_name,
                op: UpdateOp::Inc,
                position: UpdatePosition::Post,
            } = &inner.kind
            && self.locals.has(p_name)
            && let LocalLocation::Reg(p_reg) = self.locals.location_of(p_name)
            && let Some(pointee) = self.locals.type_of(p_name).pointee()
        {
            let stride = pointee.size_bytes();
            let r = p_reg.name();
            let width = if pointee.is_char_like() { "byte" } else { "word" };
            let src = if pointee.is_char_like() { "al" } else { "ax" };
            // BCC emits `mov bx, <reg>; inc <reg>; mov [bx],
            // <src>` — increment lands BEFORE the store. The
            // semantic is the same either way (BX holds the
            // pre-update pointer), but byte-matching needs the
            // BCC order.
            let _ = write!(self.out, "\tmov\tbx,{r}\r\n");
            for _ in 0..stride {
                let _ = write!(self.out, "\tinc\t{r}\r\n");
            }
            let _ = write!(self.out, "\tmov\t{width} ptr [bx],{src}\r\n");
            return;
        }
        panic!(
            "emit_store_ax_to_lvalue: target shape not supported yet (no fixture): {target:?}"
        );
    }
    /// `*<target> = <value>;` — indirect store. Pattern (fixture 081):
    /// ```text
    ///   mov word ptr [si], <value>
    /// ```
    /// where SI holds the pointer.
    pub(crate) fn emit_deref_assign(&mut self, target: &Expr, value: &Expr) {
        // `*(<seg-selector> + <offset>) = v;` — write through a
        // `_seg` segment-only pointer. Loads the segment into ES,
        // then stores via `es:[<offset>]`. Constant RHS folds to
        // `mov <width> ptr es:[<off>], imm`; non-constant goes
        // through AX/AL. Fixture 4072.
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &target.kind
            && let ExprKind::Ident(p_name) = &left.kind
            && self.locals.has(p_name)
            && self.locals.type_of(p_name).is_seg_selector()
        {
            let p_ty = self.locals.type_of(p_name).clone();
            let pointee = p_ty.pointee().expect("SegSelector has a pointee").clone();
            self.emit_seg_selector_deref_write(p_name, &pointee, right, value);
            return;
        }
        // `*<seg-qual-ptr> = v;` — write through a segment-qualified
        // pointer. The qualifier becomes a TASM `<seg>:` operand
        // prefix (DS elided as the default segment). Constant RHS
        // folds to a single `mov <width> ptr <seg>:[<reg>], imm`;
        // non-constant RHS evaluates to AX first then stores.
        // Fixtures 4063 (_ss write), 4065 (_es write), 4068 (_ss
        // write in large model — pointer stays near).
        if let ExprKind::Ident(p_name) = &target.kind
            && self.locals.has(p_name)
            && let ty = self.locals.type_of(p_name).clone()
            && let Some(seg) = ty.seg_qualifier()
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
        {
            let pointee = ty.pointee().expect("SegPointer has a pointee");
            let reg_name = reg.name();
            let seg_prefix = if matches!(seg, crate::ast::SegReg::Ds) {
                String::new()
            } else {
                format!("{}:", seg.name())
            };
            let width = ptr_width(pointee);
            if let Some(v) = try_const_eval(value) {
                let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(
                    self.out,
                    "\tmov\t{width} ptr {seg_prefix}[{reg_name}],{v_masked}\r\n",
                );
                return;
            }
            self.emit_expr_to_ax(value);
            let src = if pointee.is_char_like() { "al" } else { "ax" };
            let _ = write!(
                self.out,
                "\tmov\t{width} ptr {seg_prefix}[{reg_name}],{src}\r\n",
            );
            return;
        }
        // `*(<ptr> + K) = v;` — write through a pointer with a
        // constant offset. Folds to `mov <width> ptr [<reg>+K*stride],
        // <value>` (reg-resident) or loads p into BX first (stack-
        // resident). Fixture 3591.
        if let ExprKind::BinOp { op: BinOp::Add, left, right } = &target.kind
            && let ExprKind::Ident(p_name) = &left.kind
            && self.locals.has(p_name)
            && let Some(pointee) = self.locals.type_of(p_name).pointee()
            && let Some(k) = try_const_eval(right)
        {
            let pointee = pointee.clone();
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let addr_reg = match self.locals.location_of(p_name) {
                LocalLocation::Reg(reg) => reg.name().to_owned(),
                LocalLocation::Stack(p_off) => {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(p_off));
                    "bx".to_owned()
                }
            };
            let bx_disp = if off == 0 {
                format!("[{addr_reg}]")
            } else if off > 0 {
                format!("[{addr_reg}+{off}]")
            } else {
                format!("[{addr_reg}-{}]", -off)
            };
            let width = ptr_width(&pointee);
            if let Some(v) = try_const_eval(value) {
                let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(self.out, "\tmov\t{width} ptr {bx_disp},{v_masked}\r\n");
                return;
            }
            self.emit_expr_to_ax(value);
            if pointee.is_char_like() {
                let _ = write!(self.out, "\tmov\tbyte ptr {bx_disp},al\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tword ptr {bx_disp},ax\r\n");
            }
            return;
        }
        // `*arr[K] = v;` — assign through an element of a
        // pointer-array (global or stack). Load arr[K] into BX,
        // then store through `[bx]`. Constant-index form only.
        // Fixture 2470.
        if let ExprKind::ArrayIndex { array, index } = &target.kind
            && let ExprKind::Ident(arr_name) = &array.kind
        {
            let elem_ty = if let Some(g_ty) = self.globals.type_of(arr_name) {
                g_ty.array_elem().cloned()
            } else if self.locals.has(arr_name) {
                self.locals.type_of(arr_name).array_elem().cloned()
            } else {
                None
            };
            if let Some(elem_ty) = elem_ty
                && let Some(pointee) = elem_ty.pointee()
            {
                let pointee = pointee.clone();
                let stride = u32::from(elem_ty.size_bytes());
                if let Some(k) = try_const_eval(index) {
                    let off = k.wrapping_mul(stride);
                    let load = if self.globals.contains(arr_name) {
                        if off == 0 {
                            format!("DGROUP:_{arr_name}")
                        } else {
                            format!("DGROUP:_{arr_name}+{off}")
                        }
                    } else if let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name) {
                        let elem_off = i32::from(base_off) + off as i32;
                        let off16 = i16::try_from(elem_off).unwrap_or(i16::MAX);
                        bp_addr(off16)
                    } else {
                        unreachable!()
                    };
                    let _ = write!(self.out, "\tmov\tbx,word ptr {load}\r\n");
                    let width = ptr_width(&pointee);
                    if let Some(v) = try_const_eval(value) {
                        let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                        let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
                    } else {
                        self.emit_expr_to_ax(value);
                        if pointee.is_char_like() {
                            self.out.extend_from_slice(b"\tmov\tbyte ptr [bx],al\r\n");
                        } else {
                            self.out.extend_from_slice(b"\tmov\tword ptr [bx],ax\r\n");
                        }
                    }
                    return;
                }
            }
        }
        // `*<call>() = v;` — assigning through a call-returned
        // pointer. Call the function (result in AX = pointer), move
        // to BX, then store through `[bx]`. Fixture 1322
        // (`*getp() = 7` where getp returns `int *`).
        if let ExprKind::Call { name: fname, args } = &target.kind
            && let Some(ret_ty) = self.signatures.ret_ty_of(fname)
            && let Some(pointee) = ret_ty.pointee()
        {
            let pointee = pointee.clone();
            self.emit_call(fname, args);
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
            let width = ptr_width(&pointee);
            if let Some(v) = try_const_eval(value) {
                let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
            } else {
                self.emit_expr_to_ax(value);
                if pointee.is_char_like() {
                    self.out.extend_from_slice(b"\tmov\tbyte ptr [bx],al\r\n");
                } else {
                    self.out.extend_from_slice(b"\tmov\tword ptr [bx],ax\r\n");
                }
            }
            return;
        }
        // `*p++ = v;` — postfix increment of a register-resident pointer
        // in lvalue position. BCC stores first (using the pre-increment
        // address) then advances the pointer by sizeof(*p). Fixture 501.
        if let ExprKind::Update {
            target: name,
            op: crate::ast::UpdateOp::Inc,
            position: crate::ast::UpdatePosition::Post,
        } = &target.kind
        {
            let ty = self.locals.type_of(name).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{name}++ = v`: not a pointer type");
            };
            let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
                panic!("stack-resident pointer in `*p++ = v` not supported");
            };
            let reg = reg.name();
            let width = ptr_width(pointee);
            // `*dst++ = *src++` (or *--src etc.) — when both sides
            // have register-resident pointers, BCC reads source
            // directly through [src-reg], writes through [dst-reg],
            // then advances BOTH pointers. No BX snapshot needed
            // for the source. Fixture 3528 (`while(n--) *dst++ =
            // *src++`).
            if let ExprKind::Deref(rhs_inner) = &value.kind
                && let ExprKind::Update {
                    target: src_name,
                    op: src_op,
                    position: src_pos,
                } = &rhs_inner.kind
                && self.locals.has(src_name)
                && let LocalLocation::Reg(src_reg) = self.locals.location_of(src_name)
                && !src_reg.is_byte()
                && let Some(src_pointee) = self.locals.type_of(src_name).pointee()
                && src_pointee.is_int_like()
                && pointee.is_int_like()
            {
                let src_reg_name = src_reg.name();
                let src_stride = src_pointee.size_bytes();
                let src_mnem = match src_op {
                    crate::ast::UpdateOp::Inc => "inc",
                    crate::ast::UpdateOp::Dec => "dec",
                };
                // Pre-inc src: advance first, then read.
                // Post-inc src: read first, then advance.
                if matches!(src_pos, crate::ast::UpdatePosition::Pre) {
                    for _ in 0..src_stride {
                        let _ = write!(self.out, "\t{src_mnem}\t{src_reg_name}\r\n");
                    }
                }
                let _ = write!(self.out, "\tmov\tax,word ptr [{src_reg_name}]\r\n");
                let _ = write!(self.out, "\tmov\tword ptr [{reg}],ax\r\n");
                if matches!(src_pos, crate::ast::UpdatePosition::Post) {
                    for _ in 0..src_stride {
                        let _ = write!(self.out, "\t{src_mnem}\t{src_reg_name}\r\n");
                    }
                }
                // Now the dst post-update.
                let stride = pointee.size_bytes();
                for _ in 0..stride {
                    let _ = write!(self.out, "\tinc\t{reg}\r\n");
                }
                return;
            }
            // `*dst++ = *src++` for char* / char* — same paired
            // postinc shape, but byte width: read AL directly via
            // [src-reg], store via [dst-reg], no cbw/widen, then
            // advance both. Fixture 1346 (`*d++ = *s++` for char*).
            if pointee.is_char_like()
                && let ExprKind::Deref(rhs_inner) = &value.kind
                && let ExprKind::Update {
                    target: src_name,
                    op: src_op,
                    position: src_pos,
                } = &rhs_inner.kind
                && self.locals.has(src_name)
                && let LocalLocation::Reg(src_reg) = self.locals.location_of(src_name)
                && !src_reg.is_byte()
                && let Some(src_pointee) = self.locals.type_of(src_name).pointee()
                && src_pointee.is_char_like()
            {
                let src_reg_name = src_reg.name();
                let src_mnem = match src_op {
                    crate::ast::UpdateOp::Inc => "inc",
                    crate::ast::UpdateOp::Dec => "dec",
                };
                if matches!(src_pos, crate::ast::UpdatePosition::Pre) {
                    let _ = write!(self.out, "\t{src_mnem}\t{src_reg_name}\r\n");
                }
                let _ = write!(self.out, "\tmov\tal,byte ptr [{src_reg_name}]\r\n");
                let _ = write!(self.out, "\tmov\tbyte ptr [{reg}],al\r\n");
                if matches!(src_pos, crate::ast::UpdatePosition::Post) {
                    let _ = write!(self.out, "\t{src_mnem}\t{src_reg_name}\r\n");
                }
                let _ = write!(self.out, "\tinc\t{reg}\r\n");
                return;
            }
            // Non-constant RHS: evaluate to AX (or AL for char dst),
            // then `mov <width> ptr [<reg>], al/ax`, then advance the
            // dest pointer. Fixture 1346 (`*d++ = *s++`).
            if try_const_eval(value).is_none() {
                self.emit_expr_to_ax(value);
                if pointee.is_char_like() {
                    let _ = write!(self.out, "\tmov\tbyte ptr [{reg}],al\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tword ptr [{reg}],ax\r\n");
                }
                let stride = pointee.size_bytes();
                for _ in 0..stride {
                    let _ = write!(self.out, "\tinc\t{reg}\r\n");
                }
                return;
            }
            let v = try_const_eval(value).unwrap();
            let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr [{reg}],{v_masked}\r\n");
            let stride = pointee.size_bytes();
            for _ in 0..stride {
                let _ = write!(self.out, "\tinc\t{reg}\r\n");
            }
            return;
        }
        let (base_name, depth) = deref_chain_root(target);
        // Single-deref of a register-resident local pointer keeps the
        // original fast path (`mov word ptr [si], v` etc.). Anything
        // beyond that — globals, deeper chains — bounces through BX
        // via the shared chain helper.
        let is_global = self.globals.type_of(base_name).is_some();
        if depth == 0 && !is_global {
            let ty = self.locals.type_of(base_name).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{base_name} = v`: not a pointer type");
            };
            // Stack-resident far pointer write: `les bx, [bp+lo]`
            // brings the 4-byte (offset, segment) pair into ES:BX in
            // one shot, then the store uses the ES-override prefix.
            // Constant RHS folds to `mov es:[bx], imm`; everything
            // else evaluates the RHS to AX/AL first. Fixture 1650
            // (`*p = 99` for `int far *p = (int far *)&x`).
            if matches!(ty, Type::FarPointer { .. })
                && let LocalLocation::Stack(p_off) = self.locals.location_of(base_name)
            {
                let _ = write!(self.out, "\tles\tbx,word ptr {}\r\n", bp_addr(p_off));
                if let Some(v) = try_const_eval(value) {
                    if pointee.is_char_like() {
                        let v8 = v & 0xFF;
                        let _ = write!(self.out, "\tmov\tbyte ptr es:[bx],{v8}\r\n");
                    } else {
                        let v16 = v & 0xFFFF;
                        let _ = write!(self.out, "\tmov\tword ptr es:[bx],{v16}\r\n");
                    }
                } else {
                    self.emit_expr_to_ax(value);
                    if pointee.is_char_like() {
                        self.out.extend_from_slice(b"\tmov\tbyte ptr es:[bx],al\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tmov\tword ptr es:[bx],ax\r\n");
                    }
                }
                return;
            }
            let addr_reg = match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) => reg.name(),
                // A stack-resident near pointer: load it into bx, then store
                // through `[bx]` (the register-resident path's `addr_reg`).
                // Fixture 4271 (`*p = 5` with p on the stack → `mov bx,[bp-N];
                // mov word ptr [bx],5`).
                LocalLocation::Stack(p_off) => {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(p_off));
                    "bx"
                }
            };
            // Long pointee: store both halves through `[reg]` /
            // `[reg+2]`. High first, then low (matches all other
            // long memory-direct stores). Fixture 308.
            if pointee.is_long_like() {
                if let Some(v) = try_const_eval(value) {
                    let lo = (v & 0xFFFF) as u16;
                    let hi = ((v >> 16) & 0xFFFF) as u16;
                    let _ = write!(self.out, "\tmov\tword ptr [{addr_reg}+2],{hi}\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr [{addr_reg}],{lo}\r\n");
                    return;
                }
                // Non-constant long rhs from an int / long lvalue
                // (stack or global): load high to AX, low to DX, then
                // store hi → [reg+2] and lo → [reg]. Fixture 3287.
                if let Some((src_hi, src_lo)) = self.long_lvalue_addr_pair(value) {
                    let _ = write!(self.out, "\tmov\tax,word ptr {src_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {src_lo}\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr [{addr_reg}+2],ax\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr [{addr_reg}],dx\r\n");
                    return;
                }
                panic!("non-constant rhs in long `*p = v` not yet supported (no fixture)");
            }
            // 4-byte struct pointee, RHS = `*<reg-ptr>`: copy both
            // halves through AX (hi) and DX (lo). Same shape as
            // 4-byte struct return / assign-from-stack. Fixtures
            // 2495, 3093 (`*dst = *src` for `struct { int x; int y; }`).
            if let Type::Struct { .. } = pointee
                && pointee.size_bytes() == 4
                && let ExprKind::Deref(src_inner) = &value.kind
                && let ExprKind::Ident(src_name) = &src_inner.kind
                && self.locals.has(src_name)
                && let LocalLocation::Reg(src_reg) = self.locals.location_of(src_name)
                && self.locals.type_of(src_name).pointee().is_some()
            {
                let src_reg_name = src_reg.name();
                let _ = write!(self.out, "\tmov\tax,word ptr [{src_reg_name}+2]\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr [{src_reg_name}]\r\n");
                let _ = write!(self.out, "\tmov\tword ptr [{addr_reg}+2],ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr [{addr_reg}],dx\r\n");
                return;
            }
            let width = ptr_width(pointee);
            if let Some(v) = try_const_eval(value) {
                let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(self.out, "\tmov\t{width} ptr [{addr_reg}],{v_masked}\r\n");
                return;
            }
            // Direct-from-register-local peephole: when the RHS is
            // a register-resident int local, skip the AX round-trip
            // and store the register directly through the address
            // register. Fixture 628 (`*p = x` with p in DI, x in SI
            // → `mov [di], si`).
            if !pointee.is_char_like()
                && let ExprKind::Ident(src_name) = &value.kind
                && self.locals.has(src_name)
                && let LocalLocation::Reg(src_reg) = self.locals.location_of(src_name)
                && !src_reg.is_byte()
            {
                let _ = write!(
                    self.out,
                    "\tmov\t{width} ptr [{addr_reg}],{}\r\n",
                    src_reg.name(),
                );
                return;
            }
            // `*p = &<symbol>` — the symbol address is a 16-bit
            // immediate (with a SegRelGroupTarget relocation), so
            // BCC emits the single `c7 04 ...` immediate-to-memory
            // store rather than a load-to-AX + store pair. Fixture
            // 1932 (`*pp = &storage`).
            if !pointee.is_char_like()
                && let ExprKind::AddressOf(sym_name) = &value.kind
                && self.globals.contains(sym_name)
            {
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr [{addr_reg}],offset DGROUP:_{sym_name}\r\n",
                );
                return;
            }
            // Char-to-char direct copy peephole: `*p = *q` where both
            // are register-resident char pointers. BCC skips the
            // `cbw` widening since AL flows directly into the byte
            // store. Fixture 3529 (`cswap` body: `*a = *b`).
            if pointee.is_char_like()
                && let ExprKind::Deref(src_inner) = &value.kind
                && let ExprKind::Ident(src_name) = &src_inner.kind
                && self.locals.has(src_name)
                && let Some(src_pointee) = self.locals.type_of(src_name).pointee()
                && src_pointee.is_char_like()
                && let LocalLocation::Reg(src_reg) = self.locals.location_of(src_name)
                && !src_reg.is_byte()
            {
                let src_reg_name = src_reg.name();
                let _ = write!(self.out, "\tmov\tal,byte ptr [{src_reg_name}]\r\n");
                let _ = write!(self.out, "\tmov\tbyte ptr [{addr_reg}],al\r\n");
                return;
            }
            // Char direct-from-stack peephole: `*p = c` where `c` is
            // a stack-resident char local. Skip cbw — AL is enough.
            // Fixture 3529 (`*b = t` where t is on the stack).
            if pointee.is_char_like()
                && let ExprKind::Ident(src_name) = &value.kind
                && self.locals.has(src_name)
                && self.locals.type_of(src_name).is_char_like()
                && let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
            {
                let _ = write!(
                    self.out,
                    "\tmov\tal,byte ptr {}\r\n",
                    bp_addr(src_off),
                );
                let _ = write!(self.out, "\tmov\tbyte ptr [{addr_reg}],al\r\n");
                return;
            }
            // Non-constant RHS: materialize the value in AX/AL,
            // then store through the address register. Fixture 595
            // (`*p = *p + 1` → `mov ax, [si]; inc ax; mov [si], ax`).
            self.emit_expr_to_ax(value);
            let reg_name = if pointee.is_char_like() { "al" } else { "ax" };
            let _ = write!(self.out, "\tmov\t{width} ptr [{addr_reg}],{reg_name}\r\n");
            return;
        }
        // Chain path: same prefix as the read side (fixtures 194 /
        // 196), then a `mov <width> ptr [bx], <imm|reg>` store.
        // Constant-rhs is the original shape; the non-constant
        // shape evaluates RHS into AX/AL first, then chains to BX,
        // then stores AX through [bx]. Fixture 2680 (\`**pp = v\`).
        let final_ty_known = self.peek_chain_leaf_ty(base_name, depth);
        if let Some(v) = try_const_eval(value) {
            let final_ty = self.emit_chain_to_bx(base_name, depth);
            let width = ptr_width(&final_ty);
            let v_masked = if final_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
            return;
        }
        let _ = final_ty_known;
        // BCC emits the chain-to-BX sequence first, then loads
        // value into AX, then stores AX through [bx]. The chain
        // produces only BX so the value load doesn't clobber it.
        // Fixture 2680.
        let final_ty = self.emit_chain_to_bx(base_name, depth);
        self.emit_expr_to_ax(value);
        let width = ptr_width(&final_ty);
        let reg_name = if final_ty.is_char_like() { "al" } else { "ax" };
        let _ = write!(self.out, "\tmov\t{width} ptr [bx],{reg_name}\r\n");
    }
    /// `*<target> <op>= <value>;` — read-modify-write through a
    /// dereferenced pointer. Same shape as `emit_deref_assign` for
    /// address resolution, then emits `<op> <width> ptr [reg],imm`
    /// directly (fixture 183).
    pub(crate) fn emit_deref_compound_assign(
        &mut self,
        target: &Expr,
        op: BinOp,
        value: &Expr,
        from_postfix: bool,
    ) {
        let (base_name, depth) = deref_chain_root(target);
        // Long pointee + register-resident pointer: route through the
        // shared long-compound-to-memory helper. Picks up variable
        // RHS (fixture 398: `*p += y`) for free since the helper
        // already knows the destination addressing. Const RHS still
        // falls through to the existing const-only fast paths
        // immediately below.
        let is_global = self.globals.type_of(base_name).is_some();
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && pointee.is_long_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
        {
            let r = reg.name();
            let lo_addr = format!("[{r}]");
            let hi_addr = format!("[{r}+2]");
            self.emit_long_compound_to_mem(
                &lo_addr,
                &hi_addr,
                op,
                value,
                pointee.is_unsigned(),
            );
            return;
        }
        // Postfix `lv++` / `lv--` (discarded) through a char pointer:
        // BCC emits memory-direct `inc|dec byte ptr [reg]` rather
        // than the AL detour used for prefix `++lv` / explicit
        // `lv += 1`. Same pre-vs-post asymmetry as char-global
        // (fixture 702 `g++`). Fixture 714 (`(*p)++` standalone).
        if depth == 0
            && !is_global
            && from_postfix
            && matches!(op, BinOp::Add | BinOp::Sub)
            && try_const_eval(value) == Some(1)
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && pointee.is_char_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
        {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\tbyte ptr [{}]\r\n", reg.name());
            return;
        }
        // Char-pointee `*p <op>= d` (variable RHS): load RHS into
        // AL, then memory-direct `<op> byte ptr [reg], al`. Mirrors
        // the char-global var-RHS pattern (batch 121). Fixture 713
        // (`*p += d` with p in SI, d at [bp-1]).
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && pointee.is_char_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
            && try_const_eval(value).is_none()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let r = reg.name();
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
            let _ = write!(self.out, "\t{mnem}\tbyte ptr [{r}],al\r\n");
            return;
        }
        // Int-pointee `*p <op>= y` (variable RHS): load RHS into
        // AX (with widening if RHS is byte), then memory-direct
        // `<op> word ptr [reg], ax`. Pointer must be register-
        // resident. Fixture 838 (`*p += y` with p in SI, y at
        // [bp-4]).
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && matches!(pointee, Type::Int | Type::UInt)
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
            && try_const_eval(value).is_none()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let r = reg.name();
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tword ptr [{r}],ax\r\n");
            return;
        }
        // Int-pointee `*p *= y` / `/= y` / `%= y` with non-const
        // local RHS: `mov ax, word ptr [r]; imul/idiv word ptr
        // [bp+N]; mov word ptr [r], ax|dx`. Fixture 839.
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && matches!(pointee, Type::Int | Type::UInt)
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
            && try_const_eval(value).is_none()
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                panic!("non-stack RHS in deref compound Mul/Div not yet supported (no fixture)");
            };
            let r = reg.name();
            let _ = write!(self.out, "\tmov\tax,word ptr [{r}]\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr {}\r\n", bp_addr(off));
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tidiv\tword ptr {}\r\n", bp_addr(off));
            }
            let result_reg = if matches!(op, BinOp::Mul | BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(self.out, "\tmov\tword ptr [{r}],{result_reg}\r\n");
            return;
        }
        // Int-pointee `*p <<= y` / `>>= y` with non-const RHS:
        // `mov cl, byte ptr <rhs>; shl|sar|shr word ptr [r], cl`.
        // Needs new IR variants for `<sh> word ptr [si], cl`.
        // Fixture 840.
        if depth == 0
            && !is_global
            && let ty = self.locals.type_of(base_name).clone()
            && let Some(pointee) = ty.pointee()
            && matches!(pointee, Type::Int | Type::UInt)
            && let LocalLocation::Reg(reg) = self.locals.location_of(base_name)
            && try_const_eval(value).is_none()
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let r = reg.name();
            let unsigned = pointee.is_unsigned();
            let mnem = match (op, unsigned) {
                (BinOp::Shl, _) => "shl",
                (BinOp::Shr, false) => "sar",
                (BinOp::Shr, true) => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tcl,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tword ptr [{r}],cl\r\n");
            return;
        }
        let Some(v) = try_const_eval(value) else {
            panic!("non-constant rhs in `*p <op>= v` not yet supported (no fixture)");
        };
        let mnemonic = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::BitAnd => "and",
            BinOp::BitOr => "or",
            BinOp::BitXor => "xor",
            _ => panic!("compound op `{op:?}` on `*p` not yet supported (no fixture)"),
        };
        // Single-deref local stays on the original fast path so a
        // register-resident pointer (SI/DI) can drive the operand
        // directly. Fixture 183 (`*p += K` for local `p` in SI).
        let is_global = self.globals.type_of(base_name).is_some();
        if depth == 0 && !is_global {
            let ty = self.locals.type_of(base_name).clone();
            let Some(pointee) = ty.pointee() else {
                panic!("`*{base_name} <op>= v`: not a pointer type");
            };
            let addr_reg = match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) => reg.name(),
                LocalLocation::Stack(_) => {
                    panic!(
                        "stack-resident pointer in `*p <op>= v` not yet supported (no fixture)"
                    );
                }
            };
            // Long pointee: emit memory-direct read-modify-write pair
            // through `[reg]` / `[reg+2]`. Same byte-width rule as
            // the long-global compound assigns — arith uses imm8sx,
            // bitwise uses imm16. Fixture 311.
            if pointee.is_long_like() {
                let k_lo = (v as i64) & 0xFFFF;
                let k_hi = ((v as i64) >> 16) & 0xFFFF;
                match op {
                    BinOp::Add | BinOp::Sub => {
                        let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                            ("add", "adc")
                        } else {
                            ("sub", "sbb")
                        };
                        if let Ok(lo_i8) = i8::try_from(k_lo as i32) {
                            let _ = write!(self.out, "\t{lo_op}\tword ptr [{addr_reg}],{lo_i8}\r\n");
                        } else {
                            let lo_u16 = k_lo as u16;
                            let _ = write!(self.out, "\t{lo_op}\tword ptr [{addr_reg}],{lo_u16}\r\n");
                        }
                        let _ = write!(self.out, "\t{hi_op}\tword ptr [{addr_reg}+2],0\r\n");
                        return;
                    }
                    BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                        let _ = write!(self.out, "\t{mnemonic}\tword ptr [{addr_reg}],{k_lo}\r\n");
                        let _ = write!(self.out, "\t{mnemonic}\tword ptr [{addr_reg}+2],{k_hi}\r\n");
                        return;
                    }
                    _ => {}
                }
            }
            let store_byte = pointee.is_char_like();
            let width = if store_byte { "byte" } else { "word" };
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
            // Char-pointee arith follows the BCC byte-through-AL
            // pattern: `mov al, byte ptr [reg]; add al, K (or
            // inc/dec for K=1); mov byte ptr [reg], al`. Bitwise
            // ops stay memory-direct. Fixture 711 (`*p += 5` with
            // p in SI → `mov al, [si]; add al, 5; mov [si], al`).
            if store_byte && matches!(op, BinOp::Add | BinOp::Sub) {
                let imm8 = if matches!(op, BinOp::Add) {
                    (v_masked & 0xFF) as u8
                } else {
                    ((v_masked & 0xFF) as u8).wrapping_neg()
                };
                let _ = write!(self.out, "\tmov\tal,byte ptr [{addr_reg}]\r\n");
                if v_masked == 1 && matches!(op, BinOp::Add) {
                    self.out.extend_from_slice(b"\tinc\tal\r\n");
                } else if v_masked == 1 && matches!(op, BinOp::Sub) {
                    self.out.extend_from_slice(b"\tdec\tal\r\n");
                } else {
                    let _ = write!(self.out, "\tadd\tal,{imm8}\r\n");
                }
                let _ = write!(self.out, "\tmov\tbyte ptr [{addr_reg}],al\r\n");
                return;
            }
            // `*pp <add|sub>= K` where *pp itself is a pointer:
            // scale K by sizeof(pointee-of-pointee) for C pointer
            // arithmetic. The inc/dec peephole below assumes
            // stride=1; pointer-of-pointer with non-1 stride must
            // emit `add word ptr [reg], K*stride`. Fixture 3647
            // (`*pp += 1` where pp is `struct Pt**`, stride=4).
            if let Some(inner) = pointee.pointee()
                && matches!(op, BinOp::Add | BinOp::Sub)
                && inner.size_bytes() > 1
            {
                let stride = i32::from(inner.size_bytes());
                let sign = if matches!(op, BinOp::Add) { 1i32 } else { -1 };
                let bytes = sign.wrapping_mul(v_masked as i32).wrapping_mul(stride);
                let imm16 = bytes as i16;
                let _ = write!(self.out, "\tadd\tword ptr [{addr_reg}],{imm16}\r\n");
                return;
            }
            // Int-pointee K=1 peephole: `inc word ptr [reg]` / `dec
            // word ptr [reg]` (2 bytes) instead of `add word ptr
            // [reg], 1` (3 bytes). Fixture 1302 (`++(*p)` with int p
            // in SI).
            if !store_byte && v_masked == 1 && matches!(op, BinOp::Add | BinOp::Sub) {
                let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                let _ = write!(self.out, "\t{mnem}\tword ptr [{addr_reg}]\r\n");
                return;
            }
            let _ = write!(
                self.out,
                "\t{mnemonic}\t{width} ptr [{addr_reg}],{v_masked}\r\n",
            );
            return;
        }
        // Chain path: same prefix as the read/write counterparts
        // (fixtures 194 / 196), then `<op> word ptr [bx],<imm>` in
        // place. Fixture 197 (`*p += 5` for global `p`).
        let final_ty = self.emit_chain_to_bx(base_name, depth);
        let store_byte = final_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
        let _ = write!(
            self.out,
            "\t{mnemonic}\t{width} ptr [bx],{v_masked}\r\n",
        );
    }
    /// Assign to a file-scope variable: `<width> ptr DGROUP:_<name>`
    /// is both the lvalue and the rvalue address. Fixture 085:
    /// `g = 7;` → `mov word ptr DGROUP:_g, 7`.
    /// `_AX = K;` / `_AH = K;` / etc. — assignment to a pseudo-register
    /// identifier. Bypasses locals/globals; emits a direct `mov <reg>,
    /// <imm>` for constant RHS. Fixtures 4051 (`_AX = 0xabcd;`) and
    /// 4053 (`_AH = 0x80;`).
    pub(crate) fn emit_assign_pseudo_register(&mut self, name: &str, value: &Expr) {
        let reg = pseudo_register_operand(name)
            .expect("caller verified pseudo-register name");
        let v = try_const_eval(value).unwrap_or_else(|| {
            panic!("non-const RHS for pseudo-register `{name}` assignment not yet supported")
        });
        let v_masked = if is_byte_pseudo_register(name) { v & 0xFF } else { v & 0xFFFF };
        let _ = write!(self.out, "\tmov\t{reg},{v_masked}\r\n");
    }
    pub(crate) fn emit_assign_global(&mut self, name: &str, value: &Expr) {
        let ty = self
            .globals
            .type_of(name)
            .cloned()
            .expect("caller already checked");
        // Float / double global LHS: evaluate the RHS onto the FPU
        // stack, then fstp directly into the global's DGROUP
        // address. Mirrors the stack-local init path's fstp tail.
        // Fixture 1757.
        if ty.is_float_like() {
            self.emit_float_load_to_fpu(value);
            let width = if matches!(ty, Type::Float) { "dword" } else { "qword" };
            let _ = write!(
                self.out,
                "\tfstp\t{width} ptr DGROUP:_{name}\r\n",
            );
            self.pending_fpu_store_fwait = true;
            return;
        }
        // `long g = K;` — two word stores, **high word first** then
        // low word (fixture 205). Both `long` and `unsigned long`
        // share the same byte-level emission for arithmetic and
        // bitwise ops; only shifts (sar vs shr) and comparisons
        // (signed vs unsigned jumps) need to branch on signedness.
        // Struct-to-struct copy assign at file scope. Two emission
        // shapes by size:
        //   - **4 bytes**: BCC inlines a high-first AX:DX load/store
        //     pair — byte-identical to a long-to-long copy (fixture
        //     211). Source-level type is invisible at the byte level
        //     (a `struct { int x; int y; }` and a `struct { long x; }`
        //     produce the same bytes). Fixtures 410, 412.
        //   - **>4 bytes**: BCC calls the runtime helper `N_SCOPY@`,
        //     passing far pointers to dest and src (DS:offset, dest
        //     pushed first) and the byte count in CX. Fixtures 413
        //     (6-byte), 414 (8-byte).
        // 1-byte and 2-byte struct copies still take the generic
        // single-word path (fixture 411) — same byte output as a
        // plain int copy.
        if let Type::Struct { .. } = &ty
            && let ExprKind::Ident(src_name) = &value.kind
            && self.globals.type_of(src_name).map_or(false, |t| t == &ty)
        {
            let size = ty.size_bytes();
            if size == 4 {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            if size > 4 {
                let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{name}\r\n");
                self.out.extend_from_slice(b"\tpush\tds\r\n");
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{src_name}\r\n");
                self.out.extend_from_slice(b"\tpush\tds\r\n");
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_SCOPY@\r\n");
                self.helpers.insert("N_SCOPY@".to_string());
                return;
            }
        }
        // `a = f();` for `f` returning a 4-byte struct. Same shape
        // as `g = f();` for a long-returning callee — the call
        // leaves DX:AX = high:low and we store back to the struct
        // destination. Byte-identical to the long-return store
        // (fixture 214) for the 4-byte case. Fixture 424.
        if let Type::Struct { .. } = &ty
            && ty.size_bytes() == 4
            && let ExprKind::Call { name: fname, args } = &value.kind
            && self.signatures.ret_ty_of(fname).map_or(false, |t| t == &ty)
        {
            self.emit_call(fname, args);
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
            return;
        }
        if ty.is_long_like() {
            if let Some(v) = try_const_eval(value) {
                let lo = v & 0xFFFF;
                let hi = (v >> 16) & 0xFFFF;
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name}+2,{hi}\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},{lo}\r\n",
                );
                return;
            }
            // `g = h;` long-to-long copy between two long globals.
            // Load h into AX:DX (high→AX, low→DX), then store into
            // g. Fixture 211.
            if let ExprKind::Ident(src_name) = &value.kind
                && let Some(src_ty) = self.globals.type_of(src_name)
                && src_ty.is_long_like()
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = x;` long-from-stack-local copy. Same in-memory
            // convention as global-to-global (high→AX, low→DX), with
            // bp-relative loads. Fixture 218 (`g = <long param>`).
            if let ExprKind::Ident(src_name) = &value.kind
                && self.locals.has(src_name)
                && self.locals.type_of(src_name).is_long_like()
            {
                let LocalLocation::Stack(off) = self.locals.location_of(src_name) else {
                    panic!("register-resident long source not yet supported (no fixture)");
                };
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off));
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = f();` where `f` returns long. Call returns DX:AX
            // (high:low) per the standard ABI; store directly back
            // into the long global. Fixture 214.
            if let ExprKind::Call { name: fname, args } = &value.kind
                && self.signatures.ret_ty_of(fname).map_or(false, |t| t.is_long_like())
            {
                self.emit_call(fname, args);
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = ~a;` between two long globals. Independent per
            // half (no carry), so it's just `not` on each register
            // after the load. Fixture 225.
            if let ExprKind::Unary { op: UnaryOp::BitNot, operand } = &value.kind
                && let ExprKind::Ident(a) = &operand.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tnot\tdx\r\n");
                let _ = write!(self.out, "\tnot\tax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a * 2;` long times constant 2 — BCC peepholes
            // this to the same shl/rcl pattern as `g << 1` (slice
            // 227), skipping the N_LXMUL@ helper. Fixture 283. For
            // other small power-of-2 multipliers, BCC's behavior
            // is unprobed (likely helper-call); not yet handled.
            if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
                && let ExprKind::Ident(a) = &left.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
                && try_const_eval(right) == Some(2)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tshl\tdx,1\r\n");
                let _ = write!(self.out, "\trcl\tax,1\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a << 1;` long left-shift-by-one. BCC inlines as
            // shl on the low half (CF gets the high bit) and rcl on
            // the high half (rotates CF into the LSB). Note the
            // AX=high/DX=low convention here matches the rest of the
            // long-arith block; for shift counts >1 BCC switches to
            // the `N_LXLSH@` helper and the standard DX:AX=high:low
            // ABI convention (see the >1 path below). Fixture 227.
            if let ExprKind::BinOp { op: BinOp::Shl, left, right } = &value.kind
                && let ExprKind::Ident(a) = &left.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
                && try_const_eval(right) == Some(1)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tshl\tdx,1\r\n");
                let _ = write!(self.out, "\trcl\tax,1\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a >> 1;` long right-shift-by-one. Mirror of the
            // `<< 1` path: high gets `sar`/`shr` (signed/unsigned),
            // low gets `rcr` (CF threads from high LSB into low MSB).
            // Register convention is AX=high, DX=low. Fixtures 229
            // (signed), 243 (unsigned).
            if let ExprKind::BinOp { op: BinOp::Shr, left, right } = &value.kind
                && let ExprKind::Ident(a) = &left.kind
                && let Some(a_ty) = self.globals.type_of(a)
                && a_ty.is_long_like()
                && try_const_eval(right) == Some(1)
            {
                let hi_op = if a_ty.is_unsigned() { "shr" } else { "sar" };
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                let _ = write!(self.out, "\trcr\tdx,1\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a / b;` / `g = a % b;` long division and modulo.
            // BCC calls helpers:
            //   signed   /  → `N_LDIV@`   (fixture 232)
            //   signed   %  → `N_LMOD@`   (fixture 233)
            //   unsigned /  → `N_LUDIV@`  (fixture 245)
            //   unsigned %  → (likely `N_LUMOD@`; not yet fixtured)
            // Operands passed on the STACK (cdecl order — b pushed
            // first, so a sits at the lowest pushed address). High
            // word pushed before low for each operand: push b+2, b,
            // a+2, a. Result in DX:AX. Helper self-cleans the
            // stack (no `add sp,8` after).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && matches!(op, BinOp::Div | BinOp::Mod)
                && let ExprKind::Ident(a) = &left.kind
                && let ExprKind::Ident(b) = &right.kind
                && let Some(a_ty) = self.globals.type_of(a)
                && a_ty.is_long_like()
                && let Some(b_ty) = self.globals.type_of(b)
                && b_ty.is_long_like()
            {
                let unsigned = a_ty.is_unsigned() || b_ty.is_unsigned();
                let helper = match (op, unsigned) {
                    (BinOp::Div, false) => "N_LDIV@",
                    (BinOp::Mod, false) => "N_LMOD@",
                    (BinOp::Div, true)  => "N_LUDIV@",
                    (BinOp::Mod, true)  => "N_LUMOD@",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{b}+2\r\n");
                let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{b}\r\n");
                let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = a * b;` long multiplication. BCC calls the runtime
            // helper `N_LXMUL@`. Calling convention: operand a in
            // (CX:BX)=(high:low), operand b in (DX:AX)=(high:low),
            // result returned in (DX:AX)=(high:low). Note the order
            // of register loads is high before low for both operands.
            // Fixture 231.
            if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
                && let ExprKind::Ident(a) = &left.kind
                && let ExprKind::Ident(b) = &right.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
                && self.globals.type_of(b).map_or(false, |t| t.is_long_like())
            {
                let _ = write!(self.out, "\tmov\tcx,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{b}+2\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{b}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                self.helpers.insert("N_LXMUL@".to_string());
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = a << K;` / `g = a >> K;` for K > 1 — BCC calls a
            // runtime helper: `N_LXLSH@` for left-shift (fixture
            // 228), `N_LXRSH@` for signed right-shift (fixture 230),
            // `N_LXURSH@` for unsigned right-shift (fixture 244).
            // The register convention SWITCHES to the standard
            // 32-bit ABI: DX=high, AX=low (input *and* output). CL
            // holds the shift count. The helper is declared
            // `extrn <name>:far` in the tail.
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && matches!(op, BinOp::Shl | BinOp::Shr)
                && let ExprKind::Ident(a) = &left.kind
                && let Some(a_ty) = self.globals.type_of(a)
                && a_ty.is_long_like()
                && let Some(k) = try_const_eval(right)
                && k > 1
                && k <= 255
            {
                let helper = match (op, a_ty.is_unsigned()) {
                    (BinOp::Shl, _)        => "N_LXLSH@",
                    (BinOp::Shr, false)    => "N_LXRSH@",
                    (BinOp::Shr, true)     => "N_LXURSH@",
                    _ => unreachable!(),
                };
                let k_u8 = k as u8;
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = -a;` long unary minus. 32-bit two's-complement
            // negate: neg high, neg low (sets CF iff low != 0), sbb
            // high,0 to fold the low-half carry back into the high.
            // The high `neg` comes BEFORE the low `neg` so the carry
            // generated by the low half is the one consumed by sbb.
            // Fixture 226.
            if let ExprKind::Unary { op: UnaryOp::Neg, operand } = &value.kind
                && let ExprKind::Ident(a) = &operand.kind
                && self.globals.type_of(a).map_or(false, |t| t.is_long_like())
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{a}+2\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{a}\r\n");
                let _ = write!(self.out, "\tneg\tax\r\n");
                let _ = write!(self.out, "\tneg\tdx\r\n");
                let _ = write!(self.out, "\tsbb\tax,0\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // Long-to-long arithmetic/bitwise between two long globals:
            // `g = <lvalue_a> <op> <lvalue_b>;` for two long lvalues.
            // Same skeleton: load a into (AX=high, DX=low), apply
            // the op's pair to b's halves, store back. Add/Sub need
            // carry/borrow; bitwise ops repeat the same mnemonic.
            // Both lvalues can be any long ident (global/stack),
            // struct field (dot-chain), array element (const index),
            // or `*p` (register pointer). Fixtures 219, 220, 221,
            // 222, 224 (globals-globals); 355 (struct fields).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && let Some((lo_op, hi_op)) = long_pair_op(*op)
                && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
                && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
                let _ = write!(self.out, "\t{lo_op}\tdx,word ptr {b_lo}\r\n");
                let _ = write!(self.out, "\t{hi_op}\tax,word ptr {b_hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = <long-lvalue> + K;` or `g = <long-lvalue> - K;` —
            // load the lvalue's halves into (AX=high, DX=low) globals
            // convention (since dest is the memory global `g`), then
            // add/sub the low half and adc/sbb the high (carry=0 for
            // Add, -1 for Sub). The lvalue can be any long ident
            // (global or stack), struct field, array element (const
            // index), or `*p` for a register-resident long pointer.
            // Fixtures 207 / 208 (self-modify g), 275 (wide K), 352
            // (struct field source), 353 (array element source), 354
            // (deref source).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && (matches!(op, BinOp::Add) || matches!(op, BinOp::Sub))
                && let Some((hi_addr, lo_addr)) = self.long_lvalue_addr_pair(left)
                && let Some(k) = try_const_eval(right)
            {
                let signed = k as i32;
                let (delta, carry) = if matches!(op, BinOp::Add) {
                    (signed, 0i16)
                } else {
                    (-signed, -1i16)
                };
                // imm8sx-fits emits `add dx, K_i8` (slice 207);
                // otherwise emits the wider `add dx, K_i16`
                // (fixture 275). Either way the high partner is
                // `adc ax, carry` (carry=0 for Add, -1 for Sub).
                let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                if let Ok(delta_i8) = i8::try_from(delta) {
                    let _ = write!(self.out, "\tadd\tdx,{delta_i8}\r\n");
                } else {
                    let delta_u16 = (delta as i32) as u16;
                    let _ = write!(self.out, "\tadd\tdx,{delta_u16}\r\n");
                }
                let _ = write!(self.out, "\tadc\tax,{carry}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = i + g;` int-LHS plus long-RHS, where the long
            // RHS happens to be the assign target. BCC widens i
            // into DX:AX (mov ax,_i / cwd), then uses MEMORY-direct
            // add/adc on the long — no BX:CX scratch needed. The
            // result lands directly in DX:AX (the widened-int
            // registers) and stores back. Fixture 281.
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && matches!(op, BinOp::Add)
                && let ExprKind::Ident(i_name) = &left.kind
                && let Some(i_ty) = self.globals.type_of(i_name)
                && matches!(i_ty, Type::Int)
                && let ExprKind::Ident(rhs_name) = &right.kind
                && rhs_name == name
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{i_name}\r\n");
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tadd\tax,word ptr DGROUP:_{name}\r\n");
                let _ = write!(self.out, "\tadc\tdx,word ptr DGROUP:_{name}+2\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = g <op> i;` long-self <op> int-global, for
            // add/sub/and/or/xor. BCC widens i first (mov ax,
            // _i / cwd to DX:AX), then loads the long accumulator
            // into BX:CX (high:low — DX:AX is busy with the
            // widened int), does the operation per half, and stores
            // back. Arithmetic uses add/adc or sub/sbb for carry
            // propagation; bitwise repeats the same mnemonic per
            // half since they're independent. Fixtures 257 (`+`),
            // 258 (`-`), 259 (`&`).
            if let ExprKind::BinOp { op, left, right } = &value.kind
                && let ExprKind::Ident(lhs_name) = &left.kind
                && lhs_name == name
                && let ExprKind::Ident(i_name) = &right.kind
                && let Some(i_ty) = self.globals.type_of(i_name)
                && matches!(i_ty, Type::Int)
                && let Some((lo_op, hi_op)) = long_pair_op(*op)
            {
                let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{i_name}\r\n");
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{name}+2\r\n");
                let _ = write!(self.out, "\tmov\tcx,word ptr DGROUP:_{name}\r\n");
                let _ = write!(self.out, "\t{lo_op}\tcx,ax\r\n");
                let _ = write!(self.out, "\t{hi_op}\tbx,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,bx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},cx\r\n");
                return;
            }
            // `long g = i;` / `long g = u;` / `long g = (long)i;` —
            // widen an int-family global to long. Signed int
            // sign-extends via `cwd` (fixture 254); `unsigned int`
            // zero-extends by storing 0 directly into the high half
            // (fixture 255). Either way: load into AX first, store
            // high, then low. Peels an explicit `(long)` cast if
            // present (fixture 279); BCC emits identical bytes for
            // implicit and explicit forms.
            let widening_src = match &value.kind {
                ExprKind::Ident(name) => Some(name.as_str()),
                ExprKind::Cast { ty: Type::Long, operand } => {
                    if let ExprKind::Ident(name) = &operand.kind { Some(name.as_str()) } else { None }
                }
                _ => None,
            };
            if let Some(src_name) = widening_src
                && let Some(src_ty) = self.globals.type_of(src_name)
                && matches!(src_ty, Type::Int | Type::UInt | Type::Char)
            {
                match src_ty {
                    Type::Char => {
                        // Signed char widens via cbw (byte→word)
                        // then cwd (word→dword). Fixture 271.
                        let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{src_name}\r\n");
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                        self.out.extend_from_slice(b"\tcwd\t\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    }
                    Type::UInt => {
                        // Zero-extend: store 0 directly into high.
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,0\r\n");
                    }
                    Type::Int => {
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}\r\n");
                        self.out.extend_from_slice(b"\tcwd\t\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,dx\r\n");
                    }
                    _ => unreachable!(),
                }
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},ax\r\n");
                return;
            }
            // `g = a[K];` for a long-element STACK array — load high
            // (`[bp+base+K*4+2]`) then low (`[bp+base+K*4]`) into
            // AX:DX (globals convention), then store. Fixture 306.
            if let ExprKind::ArrayIndex { array: arr_expr, index } = &value.kind
                && let ExprKind::Ident(arr_name) = &arr_expr.kind
                && self.locals.has(arr_name)
                && let Some(elem) = self.locals.type_of(arr_name).array_elem()
                && elem.is_long_like()
                && let Some(k) = try_const_eval(index)
            {
                let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name) else {
                    unreachable!("array is stack-resident");
                };
                let off = base_off + i16::try_from((k as i32) * 4).expect("offset fits");
                let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(off));
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = a[K];` / `g = a[i];` for a long-element GLOBAL array RHS.
            // Const index folds to `_a+K*4` / `_a+K*4+2`; var index
            // uses bx-indexed addressing on the global. Fixtures 301
            // (const index), 303 (var index).
            if let ExprKind::ArrayIndex { array: arr_expr, index } = &value.kind
                && let ExprKind::Ident(arr_name) = &arr_expr.kind
                && let Some(arr_ty) = self.globals.type_of(arr_name)
                && let Some(elem) = arr_ty.array_elem()
                && elem.is_long_like()
            {
                let arr_name = arr_name.clone();
                if let Some(k) = try_const_eval(index) {
                    let byte_off = (k as i32) * 4;
                    let lo_addr = global_offset_addr(&arr_name, byte_off);
                    let hi_addr = global_offset_addr(&arr_name, byte_off + 2);
                    let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                    let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                    return;
                }
                // Variable index — load `i` into BX, scale by 4 with
                // two `shl bx, 1`s, then read both halves via
                // `<sym>[bx+disp]`. Fixtures 303, 307.
                self.emit_index_into_bx_long_stride(index);
                let _ = write!(
                    self.out,
                    "\tmov\tax,word ptr DGROUP:_{arr_name}[bx+2]\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tmov\tdx,word ptr DGROUP:_{arr_name}[bx]\r\n",
                );
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = *p;` for `p: long *` register-resident — load
            // high through `[reg+2]` and low through `[reg]` into
            // AX:DX (globals convention), then store. Fixture 309.
            if let ExprKind::Deref(operand) = &value.kind
                && let ExprKind::Ident(ptr_name) = &operand.kind
                && self.locals.has(ptr_name)
                && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
                && pointee.is_long_like()
                && let LocalLocation::Reg(reg) = self.locals.location_of(ptr_name)
            {
                let r = reg.name();
                let _ = write!(self.out, "\tmov\tax,word ptr [{r}+2]\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr [{r}]\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            // `g = s.x;` / `g = a[K].x;` etc. — long field of a
            // dot-chain lvalue. Resolves to a constant offset within
            // some base storage (global, stack); load both halves
            // memory-direct, then store. Fixture 317.
            if let ExprKind::Member { base: mem_base, field, kind: crate::ast::MemberKind::Dot } = &value.kind
                && let Some((src, total_off, leaf_ty)) = self.try_member_dot_chain(mem_base, field)
                && leaf_ty.is_long_like()
            {
                let (lo_addr, hi_addr) = if self.globals.contains(&src) {
                    (
                        global_offset_addr(&src, total_off),
                        global_offset_addr(&src, total_off + 2),
                    )
                } else {
                    let LocalLocation::Stack(base_bp) = self.locals.location_of(&src) else {
                        panic!("struct local `{src}` not stack-resident");
                    };
                    let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                    (bp_addr(off), bp_addr(off + 2))
                };
                let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name}+2,ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},dx\r\n");
                return;
            }
            panic!("non-constant long assignment to global not yet supported (no fixture)");
        }
        let width = if ty.is_char_like() { "byte" } else { "word" };
        if let Some(v) = try_const_eval(value) {
            let v_masked = if ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(
                self.out,
                "\tmov\t{width} ptr DGROUP:_{name},{v_masked}\r\n",
            );
            return;
        }
        // Register-resident int local on the RHS: store directly
        // from the register (`mov [_g], si` — 89 36 disp16, 4 bytes)
        // instead of bouncing through AX (`mov ax, si / mov [_g],
        // ax` — 2+3 = 5 bytes). Fixture 477 (`g = x` where x is in
        // SI).
        if !ty.is_char_like()
            && let ExprKind::Ident(src) = &value.kind
            && self.locals.has(src)
            && let LocalLocation::Reg(reg) = self.locals.location_of(src)
            && !reg.is_byte()
        {
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},{}\r\n",
                reg.name(),
            );
            return;
        }
        // `<ptr-global> = &<global>;` — emit the direct immediate-
        // store form `mov word ptr DGROUP:_p, offset DGROUP:_x`
        // (`C7 06 <p-disp> <x-imm>`, 6 bytes with two FIXUPPs)
        // instead of the AX-bounce `mov ax, offset _x / mov [_p],
        // ax` (5 bytes — yes, shorter, but oracle prefers the
        // single immediate-store form). Fixture 480.
        if !ty.is_char_like()
            && let ExprKind::AddressOf(src) = &value.kind
            && self.globals.contains(src)
        {
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},offset DGROUP:_{src}\r\n",
            );
            return;
        }
        // `<ptr-global> = <arr-global>;` — global array decays to
        // its base address. Same `mov word ptr [_p], offset _a`
        // form as `p = &a;`. Fixture 561 (`int a[3]; int *p; p = a;`).
        if !ty.is_char_like()
            && let ExprKind::Ident(src) = &value.kind
            && let Some(src_ty) = self.globals.type_of(src)
            && matches!(src_ty, Type::Array { .. })
        {
            let _ = write!(
                self.out,
                "\tmov\tword ptr DGROUP:_{name},offset DGROUP:_{src}\r\n",
            );
            return;
        }
        // `<ptr-global> = &<arr>[K];` — same shape as the
        // `&<global>` immediate-store above but with `+offset` on
        // the source symbol. Fixture 483.
        if !ty.is_char_like()
            && let ExprKind::AddressOfArrayElem { array, byte_offset } = &value.kind
            && self.globals.contains(array)
        {
            if *byte_offset == 0 {
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},offset DGROUP:_{array}\r\n",
                );
            } else {
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{name},offset DGROUP:_{array}+{byte_offset}\r\n",
                );
            }
            return;
        }
        // Non-constant: compute into AX, then store.
        self.emit_expr_to_ax(value);
        if ty.is_char_like() {
            let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
        } else {
            let src = if self.try_collapse_lhs_clobber_to_dx() { "dx" } else { "ax" };
            let _ = write!(self.out, "\tmov\tword ptr DGROUP:_{name},{src}\r\n");
        }
    }
}
