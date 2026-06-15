use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// `(high-addr, low-addr)` text for an arbitrary long-valued
    /// lvalue expression. Covers: bare ident (global or stack),
    /// dot-chain (`s.x`, `a[K].x`, nested), array index with
    /// constant subscript (global or stack), and pointer deref
    /// for a register-resident long pointer (`*p`).
    ///
    /// Returns `None` if the lvalue isn't a shape we know how to
    /// fold into a constant address pair (e.g. variable array index,
    /// stack-resident pointer).
    /// Load an array-index expression into BX, pre-scaled by the
    /// element type's stride. The common shape BCC uses when the
    /// index is a non-constant expression and the result will be
    /// used in a `[bx+<symbol>]` addressing form.
    ///
    /// Lowering rules:
    ///   - int index: `mov bx, <idx>` then shl bx, 1 (× stride/2)
    ///     repeatedly. For int stride=2 → one shl; long stride=4 →
    ///     two shls; char stride=1 → no shifts.
    ///   - char index: `mov al, <idx-byte>`, then `cbw` (or `mov
    ///     ah,0` for unsigned), `shl ax, ...`, `mov bx, ax`.
    pub(crate) fn emit_index_into_bx(&mut self, idx: &Expr, elem_ty: &Type) {
        let stride = elem_ty.size_bytes();
        let shifts = match stride {
            1 => 0,
            2 => 1,
            4 => 2,
            _ => panic!("unsupported element stride {stride} for variable-indexed array"),
        };
        // Char-typed index: widen AL → AX with CBW (signed) or
        // mov ah,0 (unsigned), then scale, then move into BX. Fixture
        // 1493.
        let idx_is_char = matches!(&idx.kind, ExprKind::Ident(n)
            if (self.locals.has(n) && self.locals.type_of(n).is_char_like())
            || self.globals.type_of(n).map_or(false, |t| t.is_char_like()));
        if idx_is_char
            && let ExprKind::Ident(name) = &idx.kind
        {
            let unsigned = if self.locals.has(name) {
                self.locals.type_of(name).is_unsigned()
            } else {
                self.globals.type_of(name).map_or(false, |t| t.is_unsigned())
            };
            let src_addr = if self.locals.has(name) {
                let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                    panic!("char index `{name}` should be stack-resident");
                };
                bp_addr(off)
            } else {
                format!("DGROUP:_{name}")
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {src_addr}\r\n");
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            }
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
            }
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
            return;
        }
        // `<int-lvalue> << K` index: load lvalue into BX, then apply
        // K extra shifts followed by the stride shifts. Avoids the
        // AX route. Fixture 2530 (`a[i << 1]`).
        if let ExprKind::BinOp { op: BinOp::Shl, left, right } = &idx.kind
            && let Some(k) = try_const_eval(right)
            && let Some(addr) = self.int_lvalue_addr(left)
        {
            let _ = write!(self.out, "\tmov\tbx,word ptr {addr}\r\n");
            for _ in 0..(k + shifts as u32) {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            return;
        }
        // Int-typed index path: prefer a direct `mov bx, <addr>` over
        // a roundtrip through AX.
        if let Some(addr) = self.int_lvalue_addr(idx) {
            let _ = write!(self.out, "\tmov\tbx,word ptr {addr}\r\n");
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            return;
        }
        // Register-resident int local: `mov bx, <reg>` directly,
        // skipping the AX round-trip.
        if let ExprKind::Ident(name) = &idx.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && !reg.is_byte()
        {
            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            return;
        }
        // Register-resident int local in `arr[i++]` / `arr[i--]`:
        // emit `mov bx, <reg>; <inc|dec> <reg>` directly. The old
        // value lands in BX, the update fires immediately. Skips
        // the canonical AX round-trip (`mov ax, <reg>; <upd>;
        // mov bx, ax`). Char-stride only (no `shl bx` needed) —
        // shifted post-inc forms aren't observed yet. Fixture 3653
        // (`arr[i++]` with `i` in DI).
        if shifts == 0
            && let ExprKind::Update {
                target,
                op,
                position: UpdatePosition::Post,
            } = &idx.kind
            && self.locals.has(target)
            && self.locals.type_of(target).is_int_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(target)
            && !reg.is_byte()
        {
            let mnem = match op {
                UpdateOp::Inc => "inc",
                UpdateOp::Dec => "dec",
            };
            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
            let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
            return;
        }
        // `*<reg-ptr>` index: load through the register-resident
        // pointer directly into BX (`mov bx, [<reg>]`), skipping the
        // AX round-trip. Fixture 3584 (`arr[*p]` for int* p in SI).
        if let ExprKind::Deref(inner) = &idx.kind
            && let ExprKind::Ident(p_name) = &inner.kind
            && self.locals.has(p_name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
            && !reg.is_byte()
            && let Some(pointee) = self.locals.type_of(p_name).pointee()
            && pointee.is_int_like()
        {
            let _ = write!(self.out, "\tmov\tbx,word ptr [{}]\r\n", reg.name());
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            return;
        }
        // `arr[++i]` / `arr[--i]` where i is a register-resident int
        // local: emit the inc/dec on the register, then `mov bx,
        // <reg>` reading the post-update value. Fixture 2837.
        if let ExprKind::Update {
            target,
            op,
            position: crate::ast::UpdatePosition::Pre,
        } = &idx.kind
            && self.locals.has(target)
            && self.locals.type_of(target).is_int_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(target)
        {
            let mnem = match op {
                crate::ast::UpdateOp::Inc => "inc",
                crate::ast::UpdateOp::Dec => "dec",
            };
            let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
            for _ in 0..shifts {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            return;
        }
        // Fallback: evaluate into AX, scale, then move to BX.
        self.emit_expr_to_ax(idx);
        for _ in 0..shifts {
            self.out.extend_from_slice(b"\tshl\tax,1\r\n");
        }
        self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
    }
    /// `a[<index>]` in rvalue position. The `array` side can be:
    /// - An ident referencing a local array (077, 078, 082, 079).
    ///   Constant index → direct `[bp-K]` load; variable index → the
    ///   5-instruction effective-address sequence.
    /// - A string literal (089: `"hi"[0]`). The literal is registered
    ///   in the string pool and the access folds to a direct
    ///   `DGROUP:s@<offset>` reference for constant indices. Variable
    ///   indexing of a string literal isn't observed yet.
    pub(crate) fn emit_array_index_to_ax(&mut self, array: &Expr, index: &Expr) {
        // `<stack-char-arr>[<ident-in-SI-or-DI>]` — fold to the
        // `[BP+SI+disp]` / `[BP+DI+disp]` addressing mode. Each
        // access is `mov al, byte ptr [bp+si+disp]` + widen,
        // saving the LEA / mov-bx / add-bx prelude. Fixture 2488
        // (`char a[4]; ... a[i]` with `i` in SI).
        if let Some((disp, idx_reg)) = self.bp_idx_disp_for_char_array(array, index) {
            let _ = idx_reg;
            let _ = write!(self.out, "\tmov\tal,byte ptr [bp+si{}]\r\n", signed_disp_suffix(disp));
            // Re-resolve the elem type for the widen choice.
            let unsigned = if let ExprKind::Ident(name) = &array.kind
                && self.locals.has(name)
                && let Some(elem) = self.locals.type_of(name).array_elem()
            {
                elem.is_unsigned()
            } else {
                false
            };
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            }
            return;
        }
        if let ExprKind::StringLit(bytes) = &array.kind {
            return self.emit_string_lit_index_to_ax(bytes, index);
        }
        // `(c ? a : b)[K]` — ternary returns the chosen array
        // pointer; index it with a constant K. Emit the ternary
        // into AX (which materializes `lea ax, &arr` in each arm),
        // copy to BX, and read `[bx+K*stride]`. The pointee /
        // element type comes from the then-branch (both branches
        // must produce the same array shape by C semantics).
        // Fixture 2379 (`(c ? a : b)[1]` for two `int[3]` locals).
        if let ExprKind::Ternary { then_value, .. } = &array.kind
            && let Some(k) = try_const_eval(index)
            && let ExprKind::Ident(then_name) = &then_value.kind
            && self.locals.has(then_name)
            && let Some(elem_ty) = self.locals.type_of(then_name).array_elem()
        {
            let elem_ty = elem_ty.clone();
            let stride = i32::from(elem_ty.size_bytes());
            let byte_off = (k as i32).wrapping_mul(stride);
            self.emit_expr_to_ax(array);
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
            let bx_disp = if byte_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{byte_off}]")
            };
            if elem_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&elem_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // `b.data[K]` — read an array element inside a struct field.
        // With a constant index we can fold field offset + K*stride
        // into a single byte displacement. Fixture 497.
        if let ExprKind::Member {
            base,
            field,
            kind: crate::ast::MemberKind::Dot,
        } = &array.kind
        {
            if let ExprKind::Ident(base_name) = &base.kind
                && let Some(k) = try_const_eval(index)
            {
                let base_ty = if self.globals.contains(base_name) {
                    self.globals.type_of(base_name).unwrap().clone()
                } else {
                    self.locals.type_of(base_name).clone()
                };
                if let Some((field_off, field_ty)) = base_ty.field(field) {
                    if let Type::Array { elem, .. } = field_ty {
                        let stride = u32::from(elem.size_bytes());
                        let total_off =
                            u32::from(field_off) + (k as u32).wrapping_mul(stride);
                        let elem_ty = *elem;
                        let width = ptr_width(&elem_ty);
                        let addr = if self.globals.contains(base_name) {
                            if total_off == 0 {
                                format!("DGROUP:_{base_name}")
                            } else {
                                format!("DGROUP:_{base_name}+{total_off}")
                            }
                        } else {
                            let LocalLocation::Stack(struct_off) =
                                self.locals.location_of(base_name)
                            else {
                                panic!("struct local `{base_name}` not stack-resident");
                            };
                            let off = struct_off
                                + i16::try_from(total_off).unwrap_or(i16::MAX);
                            bp_addr(off)
                        };
                        if elem_ty.is_char_like() {
                            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                            self.emit_widen_al(&elem_ty);
                        } else {
                            let _ = write!(
                                self.out,
                                "\tmov\tax,{width} ptr {addr}\r\n"
                            );
                        }
                        return;
                    }
                }
            }
        }
        // `<call>[K]` — function returning a pointer, indexed at a
        // constant subscript. Call the function (result in AX),
        // copy to BX, then read `[bx+K*stride]`. Fixture 1227
        // (`return greet()[0]` where greet returns `char *`).
        if let ExprKind::Call { name: fname, args } = &array.kind
            && let Some(k) = try_const_eval(index)
            && let Some(ret_ty) = self.signatures.ret_ty_of(fname)
            && let Some(pointee) = ret_ty.pointee()
        {
            let pointee = pointee.clone();
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            self.emit_call(fname, args);
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
            let bx_disp = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            if pointee.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&pointee);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // `<ptr>-><field>[K]` where the field is an array type and
        // <ptr> is a register-resident struct pointer: fold the
        // field offset + element offset and emit a single
        // \`mov ax, word ptr [reg + total_off]\`. Fixture 2676.
        // `<global-struct>.<arr-field>[i]` / `[K]` — fold the field
        // offset into the global symbol and index from there.
        // Fixtures 2940, 3422. The chain may nest arbitrarily deep
        // (`g.u.c[K]` where `u` is a union member of struct `g`):
        // `try_lvalue_chain_addr` resolves the whole Dot chain to the
        // root ident, accumulated byte offset, and the array leaf
        // type. Fixture 4195 (array inside a union inside a struct).
        if let ExprKind::Member { kind: crate::ast::MemberKind::Dot, .. } = &array.kind
            && let Some((struct_name, field_off_i32, field_ty)) =
                self.try_lvalue_chain_addr(array)
            && !self.locals.has(&struct_name)
            && self.globals.contains(&struct_name)
            && let Some(elem_ty) = field_ty.array_elem()
            && let Ok(field_off) = u16::try_from(field_off_i32)
        {
            let elem_ty_clone = elem_ty.clone();
            let stride = u32::from(elem_ty_clone.size_bytes());
            if let Some(k) = try_const_eval(index) {
                let off = u32::from(field_off) + k.wrapping_mul(stride);
                let addr = if off == 0 {
                    format!("DGROUP:_{struct_name}")
                } else {
                    format!("DGROUP:_{struct_name}+{off}")
                };
                if elem_ty_clone.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                    self.emit_widen_al(&elem_ty_clone);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
                }
                return;
            }
            // Variable index: scale i into BX, then load through
            // the indexed form. The field offset folds into the
            // symbol reference; BX carries just the i*stride.
            self.emit_index_into_bx(index, &elem_ty_clone);
            let base_sym = if field_off == 0 {
                format!("DGROUP:_{struct_name}")
            } else {
                format!("DGROUP:_{struct_name}+{field_off}")
            };
            if elem_ty_clone.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {base_sym}[bx]\r\n");
                self.emit_widen_al(&elem_ty_clone);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {base_sym}[bx]\r\n");
            }
            return;
        }
        // `<ptr>-><ptr-field>[K]` — load the pointer field into BX,
        // then read `[bx + K*stride]`. Fixture 2703.
        if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Arrow } = &array.kind
            && let ExprKind::Ident(p_name) = &base.kind
            && self.locals.has(p_name)
            && let LocalLocation::Reg(p_reg) = self.locals.location_of(p_name)
            && let Some(pointee) = self.locals.type_of(p_name).pointee()
            && let Some((field_off, field_ty)) = pointee.field(field)
            && let Some(elem_ty) = field_ty.pointee()
            && let Some(k) = try_const_eval(index)
        {
            let elem_ty = elem_ty.clone();
            let stride = i32::from(elem_ty.size_bytes());
            let elem_off = (k as i32).wrapping_mul(stride);
            let p_reg_name = p_reg.name();
            let load_src = if field_off == 0 {
                format!("[{p_reg_name}]")
            } else {
                format!("[{p_reg_name}+{field_off}]")
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr {load_src}\r\n");
            let bx_disp = if elem_off == 0 {
                "[bx]".to_owned()
            } else if elem_off > 0 {
                format!("[bx+{elem_off}]")
            } else {
                format!("[bx-{}]", -elem_off)
            };
            if elem_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&elem_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Arrow } = &array.kind
            && let ExprKind::Ident(p_name) = &base.kind
            && self.locals.has(p_name)
            && let LocalLocation::Reg(p_reg) = self.locals.location_of(p_name)
            && let Some(pointee) = self.locals.type_of(p_name).pointee()
            && let Some((field_off, field_ty)) = pointee.field(field)
            && let Some(elem_ty) = field_ty.array_elem()
            && let Some(k) = try_const_eval(index)
        {
            let stride = u32::from(elem_ty.size_bytes());
            let off = u32::from(field_off) + k.wrapping_mul(stride);
            let p_reg_name = p_reg.name();
            let off_i = off as i32;
            let bx_disp = if off_i == 0 {
                format!("[{p_reg_name}]")
            } else {
                format!("[{p_reg_name}+{off_i}]")
            };
            let elem_ty = elem_ty.clone();
            if elem_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&elem_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // Walk a nested chain `a[i1][i2]...` down to the base ident,
        // collecting indices from innermost to outermost. A bare
        // `a[i]` lands here with `indices = [i]` after the reversal.
        // `(*<ident>)[K]` (pointer-to-array indexed) is treated the
        // same as `<ident>[K]` since the parser already collapses the
        // pointer-to-array type to a flat pointer. Fixture 2329.
        let mut indices: Vec<&Expr> = vec![index];
        let mut cur = array;
        let mut was_dereffed = false;
        let array_name = loop {
            match &cur.kind {
                ExprKind::ArrayIndex { array: inner, index: inner_ix } => {
                    indices.push(inner_ix);
                    cur = inner;
                }
                ExprKind::Ident(name) => break name.as_str(),
                ExprKind::Deref(inner) if matches!(&inner.kind, ExprKind::Ident(_)) => {
                    let ExprKind::Ident(name) = &inner.kind else { unreachable!() };
                    was_dereffed = true;
                    break name.as_str();
                }
                _ => panic!(
                    "array base in `a[i]` must be an ident, nested array-index, or string literal (no fixture for {:?})",
                    cur.kind,
                ),
            }
        };
        indices.reverse();
        // Global array? Route to DGROUP-relative addressing.
        // Fixture 189 (`int a[3] = {1, 2, 3}; return a[0] + a[1] + a[2];`).
        if let Some(gty) = self.globals.type_of(array_name) {
            let gty = gty.clone();
            // Global pointer indexed at depth 1: `p[i]` where `p: T*`.
            // Equivalent to `*(p + i)` — load `p` into `bx` from
            // `DGROUP:_p`, then dereference. Fixture 192
            // (`char *p = "hi"; return p[0];`).
            if let Some(pointee) = gty.pointee() {
                if indices.len() == 1 {
                    return self.emit_global_pointer_index_to_ax(
                        array_name,
                        pointee.clone(),
                        indices[0],
                    );
                }
            }
            // `<global-arr-of-ptr>[K_outer][K_inner]` — array of
            // pointers indexed twice with constants. Load the
            // pointer at index K_outer into BX, then read through
            // `[bx + K_inner*stride]`. Mirrors the stack-resident
            // path at line 12219. Fixtures 2231 (`char *names[3];
            // names[i][0]`), 2345 (extern array variant).
            if indices.len() == 2
                && let Some(outer_elem) = gty.array_elem()
                && let Some(inner_pointee) = outer_elem.pointee()
                && let Some(k_outer) = try_const_eval(indices[0])
                && let Some(k_inner) = try_const_eval(indices[1])
            {
                let inner_pointee = inner_pointee.clone();
                let outer_stride = u32::from(outer_elem.size_bytes());
                let outer_off = k_outer.wrapping_mul(outer_stride);
                let inner_stride = i32::from(inner_pointee.size_bytes());
                let inner_off = (k_inner as i32).wrapping_mul(inner_stride);
                let arr_addr = if outer_off == 0 {
                    format!("DGROUP:_{array_name}")
                } else {
                    format!("DGROUP:_{array_name}+{outer_off}")
                };
                let _ = write!(self.out, "\tmov\tbx,word ptr {arr_addr}\r\n");
                let bx_disp = if inner_off == 0 {
                    "[bx]".to_owned()
                } else if inner_off > 0 {
                    format!("[bx+{inner_off}]")
                } else {
                    format!("[bx-{}]", -inner_off)
                };
                if inner_pointee.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                    self.emit_widen_al(&inner_pointee);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
                }
                return;
            }
            if let Some((const_off, leaf_ty)) =
                try_const_array_offset(&gty, indices.iter().copied())
            {
                let width = ptr_width(&leaf_ty);
                let addr = if const_off == 0 {
                    format!("DGROUP:_{array_name}")
                } else {
                    format!("DGROUP:_{array_name}+{const_off}")
                };
                if leaf_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                    self.emit_widen_al(&leaf_ty);
                } else {
                    let _ = write!(self.out, "\tmov\tax,{width} ptr {addr}\r\n");
                }
                return;
            }
            // Variable-indexed global array `_a[i]` at depth 1. BCC's
            // shape: load the index into BX (going via AX+cbw if the
            // index is char-typed), scale by the element stride
            // (`shl bx, 1` for int, twice for long, skip for char),
            // then read through `[bx+_a]`. Fixture 1284 (int index),
            // 1493 (char-typed index, requires CBW widening).
            //
            // `_a[i ± K]`: fold the constant offset into the FIXUPP
            // disp (`_a + K*stride[bx]`) so the index becomes just
            // `i`. Fixture 3637 (`arr[i+1]`), 3033 (`arr[i-1]`).
            if indices.len() == 1
                && let Some(elem_ty) = gty.array_elem()
            {
                let elem_ty = elem_ty.clone();
                let stride = i32::from(elem_ty.size_bytes());
                let (idx_expr, const_off) = match &indices[0].kind {
                    ExprKind::BinOp { op: BinOp::Add, left, right }
                        if let Some(k) = try_const_eval(right)
                            && try_const_eval(left).is_none() =>
                    {
                        (left.as_ref(), (k as i32).wrapping_mul(stride))
                    }
                    ExprKind::BinOp { op: BinOp::Sub, left, right }
                        if let Some(k) = try_const_eval(right)
                            && try_const_eval(left).is_none() =>
                    {
                        (left.as_ref(), -(k as i32).wrapping_mul(stride))
                    }
                    _ => (indices[0], 0),
                };
                // Char-array read with index in SI: BCC keeps the
                // index in SI and uses SI-indexed addressing
                // directly (`mov al, byte ptr _a[si]`) with no
                // intermediate `mov bx, si`. Fixture 1426 (`for
                // (i=0..) dst[i] = src[i];`).
                if elem_ty.is_char_like()
                    && let ExprKind::Ident(i_name) = &idx_expr.kind
                    && self.locals.has(i_name)
                    && let LocalLocation::Reg(reg) = self.locals.location_of(i_name)
                    && reg.name() == "si"
                {
                    let addr = if const_off == 0 {
                        format!("DGROUP:_{array_name}[si]")
                    } else if const_off > 0 {
                        format!("DGROUP:_{array_name}+{const_off}[si]")
                    } else {
                        format!("DGROUP:_{array_name}-{}[si]", -const_off)
                    };
                    let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                    self.emit_widen_al(&elem_ty);
                    return;
                }
                self.emit_index_into_bx(idx_expr, &elem_ty);
                let width = ptr_width(&elem_ty);
                let addr = if const_off == 0 {
                    format!("DGROUP:_{array_name}[bx]")
                } else if const_off > 0 {
                    format!("DGROUP:_{array_name}+{const_off}[bx]")
                } else {
                    // Negative const offset: tasm syntax is sym-K.
                    // The disp16 in the FIXUPP will be sign-extended;
                    // the underflow wraps in the OBJ. BCC actually
                    // emits this as `_a-K[bx]` (e.g. `_a-2[bx]` for
                    // `arr[i-1]`).
                    format!("DGROUP:_{array_name}-{}[bx]", -const_off)
                };
                if elem_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                    self.emit_widen_al(&elem_ty);
                } else {
                    let _ = write!(self.out, "\tmov\tax,{width} ptr {addr}\r\n");
                }
                return;
            }
            // 2D global array variable-indexed: `g[i][j]` for
            // `T g[M][N]`. BCC computes outer-scaled-then-add into
            // BX, then indexed-load via DGROUP:_g[bx]. Two shapes:
            //
            // **Power-of-2 outer stride** (e.g. `int g[3][2]` →
            // stride 4): start with BX = outer, chained `shl bx,1`
            // for the outer scale, AX = inner, `shl ax,1` if inner
            // stride 2, then `add bx, ax`. Fixture 3194.
            //
            // **Non-power-of-2 outer stride** (e.g. `char d[2][3]`
            // → stride 3): imul requires AX, so AX = outer,
            // `mov dx, <stride>; imul dx`, then add inner directly,
            // `mov bx, ax`. Fixture 2985.
            // Array of pointers indexed twice (`names[0][1]` for
            // `char *names[3]`): inner type is Pointer, not Array.
            // Load the pointer with the outer offset, then deref
            // with the inner offset. Fixture 1394.
            // `<global-arr-of-ptr>[<var>][K_inner]` — outer is a
            // variable-indexed array of pointers, inner is a
            // compile-time constant. Load `<arr>[var]` into BX
            // (via a scaled index), then read at `[bx+K_inner*
            // stride]`. Char-pointee uses byte load + widen.
            // Fixture 2397 (`words[i][0]` for `char *words[]`).
            if indices.len() == 2
                && let Type::Array { elem: arr_elem, .. } = &gty
                && let Type::Pointer(pointee) = &**arr_elem
                && let Some(inner_k) = try_const_eval(indices[1])
                && try_const_eval(indices[0]).is_none()
            {
                let pointee = (**pointee).clone();
                let inner_byte_off =
                    (inner_k as i32).wrapping_mul(i32::from(pointee.size_bytes()));
                // Scale the index by sizeof(ptr) = 2 and load
                // `_<arr>[bx]` into BX (the pointer slot).
                self.emit_index_into_bx(indices[0], &Type::Int);
                let _ = write!(
                    self.out,
                    "\tmov\tbx,word ptr DGROUP:_{array_name}[bx]\r\n",
                );
                let bx_disp = if inner_byte_off == 0 {
                    "[bx]".to_owned()
                } else if inner_byte_off > 0 {
                    format!("[bx+{inner_byte_off}]")
                } else {
                    format!("[bx-{}]", -inner_byte_off)
                };
                if pointee.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                    self.emit_widen_al(&pointee);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
                }
                return;
            }
            if indices.len() == 2
                && let Type::Array { elem: arr_elem, .. } = &gty
                && let Type::Pointer(pointee) = &**arr_elem
                && let Some(outer_k) = try_const_eval(indices[0])
                && let Some(inner_k) = try_const_eval(indices[1])
            {
                let pointee = (**pointee).clone();
                let outer_byte_off = (outer_k as u32).wrapping_mul(2);
                let inner_byte_off = (inner_k as u32)
                    .wrapping_mul(u32::from(pointee.size_bytes()));
                let outer_addr = if outer_byte_off == 0 {
                    format!("DGROUP:_{array_name}")
                } else {
                    format!("DGROUP:_{array_name}+{outer_byte_off}")
                };
                let _ = write!(self.out, "\tmov\tbx,word ptr {outer_addr}\r\n");
                let inner_addr = if inner_byte_off == 0 {
                    "[bx]".to_owned()
                } else {
                    format!("[bx+{inner_byte_off}]")
                };
                if pointee.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {inner_addr}\r\n");
                    self.emit_widen_al(&pointee);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {inner_addr}\r\n");
                }
                return;
            }
            if indices.len() == 2
                && let Type::Array { elem: inner_arr, .. } = &gty
                && let Type::Array { elem: leaf_elem, .. } = &**inner_arr
            {
                let outer_stride = inner_arr.size_bytes();
                let inner_stride = leaf_elem.size_bytes();
                let leaf_ty = (**leaf_elem).clone();
                let outer_addr = self.int_lvalue_src(indices[0]);
                let inner_addr = self.int_lvalue_src(indices[1]);
                if let (Some(outer), Some(inner)) = (outer_addr, inner_addr) {
                    let outer_src = if is_reg16_name(&outer) {
                        outer.clone()
                    } else {
                        format!("word ptr {outer}")
                    };
                    let inner_src = if is_reg16_name(&inner) {
                        inner.clone()
                    } else {
                        format!("word ptr {inner}")
                    };
                    let outer_pow2 = outer_stride > 1 && outer_stride.is_power_of_two();
                    let outer_one = outer_stride == 1;
                    if outer_pow2 || outer_one {
                        let _ = write!(self.out, "\tmov\tbx,{outer_src}\r\n");
                        let outer_shifts = (outer_stride as u32).trailing_zeros();
                        for _ in 0..outer_shifts {
                            self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
                        }
                        let _ = write!(self.out, "\tmov\tax,{inner_src}\r\n");
                        let inner_shifts = (inner_stride as u32).trailing_zeros();
                        for _ in 0..inner_shifts {
                            self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                        }
                        self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
                    } else {
                        // Non-power-of-2 outer stride — imul into AX.
                        let _ = write!(self.out, "\tmov\tax,{outer_src}\r\n");
                        let _ = write!(self.out, "\tmov\tdx,{outer_stride}\r\n");
                        self.out.extend_from_slice(b"\timul\tdx\r\n");
                        if inner_stride == 2 {
                            let _ = write!(self.out, "\tmov\tdx,{inner_src}\r\n");
                            self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                            self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
                        } else {
                            let _ = write!(self.out, "\tadd\tax,{inner_src}\r\n");
                        }
                        self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                    }
                    let width = ptr_width(&leaf_ty);
                    if leaf_ty.is_char_like() {
                        let _ = write!(
                            self.out,
                            "\tmov\tal,byte ptr DGROUP:_{array_name}[bx]\r\n",
                        );
                        self.emit_widen_al(&leaf_ty);
                    } else {
                        let _ = write!(
                            self.out,
                            "\tmov\tax,{width} ptr DGROUP:_{array_name}[bx]\r\n",
                        );
                    }
                    return;
                }
            }
            panic!("variable-indexed global array not yet supported (no fixture)");
        }
        let ty = self.locals.type_of(array_name).clone();
        // `p[i]` where `p` is a pointer (not an array). Equivalent
        // to `*(p + i)`. Fixture 088: `s[0]` with `s: char *` in SI
        // → `mov al, byte ptr [si] / cbw`. Only handled at depth 1.
        if let Some(pointee) = ty.pointee() {
            if indices.len() == 1 {
                // `(*p)[K]` for `T (*p)[N]`: pointee is `Array{N, T}`,
                // but the deref decays the array to a pointer, so the
                // stride should be `sizeof(T)`, not `sizeof(Array)`.
                // Fixtures 2493, 2686.
                let effective_pointee = if was_dereffed
                    && let Some(elem) = pointee.array_elem()
                {
                    elem.clone()
                } else {
                    pointee.clone()
                };
                return self.emit_pointer_index_to_ax(
                    array_name,
                    effective_pointee,
                    indices[0],
                );
            }
            // `pp[i][j]` for char ** / int ** etc. with constant
            // indices: load `*pp[i]` into BX (the first-level ptr),
            // then read through `[bx + j*stride]`. Fixture 2962.
            // Also handles `int (*g)[N]` shape (pointee is an array
            // type rather than another pointer) — fixture 2487.
            if indices.len() == 2
                && let Some(i_k) = try_const_eval(indices[0])
                && let Some(j_k) = try_const_eval(indices[1])
                && let Some(inner_pointee) = pointee
                    .pointee()
                    .or_else(|| pointee.array_elem())
            {
                let inner_pointee = inner_pointee.clone();
                let i_stride = u32::from(pointee.size_bytes());
                let i_off = i_k.wrapping_mul(i_stride) as i32;
                let j_stride = i32::from(inner_pointee.size_bytes());
                let j_off = (j_k as i32).wrapping_mul(j_stride);
                // Pointee-is-Array shape (e.g. `int (*g)[3]` for
                // a `int g[N][M]` parameter): `g[i][j]` is a single
                // memory access at `*g + i*outer + j*inner` because
                // the outer deref is just array-to-pointer decay,
                // not an actual load. Emit `mov ax, [reg + total]`.
                // Fixture 2487.
                if pointee.array_elem().is_some() {
                    let total = i_off + j_off;
                    let total16 = i16::try_from(total).unwrap_or(i16::MAX);
                    if let LocalLocation::Reg(reg) = self.locals.location_of(array_name) {
                        let addr = if total == 0 {
                            format!("[{}]", reg.name())
                        } else if total > 0 {
                            format!("[{}+{total}]", reg.name())
                        } else {
                            format!("[{}-{}]", reg.name(), -total)
                        };
                        let _ = total16;
                        if inner_pointee.is_char_like() {
                            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                            self.emit_widen_al(&inner_pointee);
                        } else {
                            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
                        }
                        return;
                    }
                }
                let p_addr = if let LocalLocation::Reg(reg) = self.locals.location_of(array_name) {
                    if i_off == 0 {
                        format!("[{}]", reg.name())
                    } else {
                        format!("[{}+{}]", reg.name(), i_off)
                    }
                } else if let LocalLocation::Stack(off) = self.locals.location_of(array_name) {
                    // For stack-resident pp, load pp into bx first
                    // then index — but for argv[0][0] BCC has pp in
                    // a register typically. Skip for now.
                    let _ = off;
                    panic!("stack-resident pp in multi-level index not yet supported (no fixture)");
                } else {
                    unreachable!()
                };
                let _ = write!(self.out, "\tmov\tbx,word ptr {p_addr}\r\n");
                let bx_disp = if j_off == 0 {
                    "[bx]".to_owned()
                } else if j_off > 0 {
                    format!("[bx+{j_off}]")
                } else {
                    format!("[bx-{}]", -j_off)
                };
                if inner_pointee.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                    self.emit_widen_al(&inner_pointee);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
                }
                return;
            }
            panic!("multi-level index through a pointer not yet supported (no fixture)");
        }
        let LocalLocation::Stack(base_off) = self.locals.location_of(array_name) else {
            panic!("array `{array_name}` should be stack-resident");
        };
        if let Some((const_off, leaf_ty)) =
            try_const_array_offset(&ty, indices.iter().copied())
        {
            let off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
            let width = ptr_width(&leaf_ty);
            if leaf_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                self.emit_widen_al(&leaf_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,{width} ptr {}\r\n", bp_addr(off));
            }
            return;
        }
        // `<arr-of-pointers>[i][j]` — array of pointers indexed
        // twice. Same shape as `pp[i][j]` but the first level
        // lives in the stack-resident array's bp-relative slot.
        // With constant indices, fold each level's offset.
        // Fixture 1710 (`char *strs[3]; strs[1][0]`).
        if indices.len() == 2
            && let Type::Array { elem, .. } = &ty
            && let Some(inner_pointee) = elem.pointee()
            && let Some(i_k) = try_const_eval(indices[0])
            && let Some(j_k) = try_const_eval(indices[1])
        {
            let inner_pointee = inner_pointee.clone();
            let outer_stride = u32::from(elem.size_bytes());
            let i_off = i_k.wrapping_mul(outer_stride) as i32;
            let elem_off = i32::from(base_off) + i_off;
            let elem_off_i16 = i16::try_from(elem_off).unwrap_or(i16::MAX);
            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(elem_off_i16));
            let j_stride = i32::from(inner_pointee.size_bytes());
            let j_off = (j_k as i32).wrapping_mul(j_stride);
            let bx_disp = if j_off == 0 {
                "[bx]".to_owned()
            } else if j_off > 0 {
                format!("[bx+{j_off}]")
            } else {
                format!("[bx-{}]", -j_off)
            };
            if inner_pointee.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&inner_pointee);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // 2D variable-index read: `a[i][j]` for `int a[M][N]`.
        // Fixture 198. Other multi-dim depths aren't fixtured yet.
        if indices.len() == 2 {
            let (outer_stride, inner_stride, leaf_ty) = match &ty {
                Type::Array { elem, .. } => match &**elem {
                    inner_arr @ Type::Array { elem: inner_elem, .. } => (
                        inner_arr.size_bytes(),
                        inner_elem.size_bytes(),
                        (**inner_elem).clone(),
                    ),
                    _ => panic!("`{array_name}[i][j]`: outer element isn't an array"),
                },
                _ => panic!("`{array_name}[i][j]`: not an array type"),
            };
            self.emit_array_addr_2d_to_bx(
                indices[0],
                indices[1],
                outer_stride,
                inner_stride,
                base_off,
            );
            let width = ptr_width(&leaf_ty);
            if leaf_ty.is_char_like() {
                self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{width} ptr [bx]\r\n");
            }
            return;
        }
        if indices.len() != 1 {
            panic!("multi-dim array read with non-constant indices not yet supported (no fixture)");
        }
        let elem = ty
            .array_elem()
            .unwrap_or_else(|| panic!("`{array_name}[i]`: not an array type"));
        let elem_size = elem.size_bytes();
        let width = ptr_width(elem);
        self.emit_array_addr_to_bx(array_name, indices[0], base_off, elem_size);
        if elem.is_char_like() {
            self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        } else {
            let _ = write!(self.out, "\tmov\tax,{width} ptr [bx]\r\n");
        }
    }
    /// Emit the 4-instruction sequence that lands `&a[index]` in BX
    /// (used as a shared head by `emit_array_index_to_ax` and
    /// Load an integer index into BX and scale by 4 (long stride),
    /// for variable-indexed long-array element access on globals
    /// (the symbol's offset is then folded into the disp16 of the
    /// `[bx+disp]` operand). BCC special-cases the load:
    /// - Int stack local: `mov bx, word ptr [bp-N]` (3 bytes).
    /// - Int register local: `mov bx, <reg>` (2 bytes).
    /// - Anything else: compute into AX, then `mov bx, ax`.
    /// Followed by two `shl bx, 1` (stride 4 = 2^2). Fixtures 303,
    /// 305, 307.
    pub(crate) fn emit_index_into_bx_long_stride(&mut self, index: &Expr) {
        if let ExprKind::Ident(i_name) = &index.kind
            && self.locals.has(i_name)
        {
            match self.locals.location_of(i_name) {
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                }
            }
        } else {
            self.emit_expr_to_ax(index);
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
        }
        self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
        self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
    }
    /// `emit_array_assign` for the variable-index case):
    /// ```text
    ///   mov bx, <index>
    ///   shl bx, 1               ; only when elem stride is 2
    ///   lea ax, word ptr [bp-<base>]
    ///   add bx, ax
    /// ```
    pub(crate) fn emit_array_addr_to_bx(
        &mut self,
        _array: &str,
        index: &Expr,
        base_off: i16,
        elem_size: u16,
    ) {
        // BCC's order depends on whether the index path includes a
        // stride shift:
        //  - Simple ident index + stride 1 (no shl): emit `lea ax,
        //    base` FIRST, then `mov bx, idx`, then add. The two
        //    loads are independent so order is free; BCC's choice
        //    is observable in the byte output. Fixture 1219.
        //  - Otherwise (stride ≥ 2 or BinOp/compound index): emit
        //    the index compute first (it leaves BX hot for the shl),
        //    then `lea ax, base`, then add. Fixtures 1468, 1275.
        let simple_idx = matches!(&index.kind, ExprKind::Ident(_));
        let idx_is_char_ident = if let ExprKind::Ident(n) = &index.kind {
            self.locals.has(n) && self.locals.type_of(n).is_char_like()
        } else {
            false
        };
        if simple_idx && elem_size == 1 && !idx_is_char_ident {
            let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(base_off));
            let ExprKind::Ident(idx_name) = &index.kind else { unreachable!() };
            match self.locals.location_of(idx_name) {
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                }
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                }
            }
            self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
            return;
        }
        // Char-typed index for stride 1: BCC widens via `mov al,
        // byte ptr <i>; cbw` (sign-extend), then computes
        // `lea dx, base; add ax, dx; mov bx, ax`. Mirror that
        // shape rather than the word-read above (which would
        // load garbage from the high byte). Fixture 1428
        // (`a[i]` for char a[5], char i).
        if idx_is_char_ident && elem_size == 1 {
            let ExprKind::Ident(idx_name) = &index.kind else { unreachable!() };
            let unsigned = self.locals.type_of(idx_name).is_unsigned();
            let src = match self.locals.location_of(idx_name) {
                LocalLocation::Reg(reg) if reg.is_byte() => format!("{}", reg.name()),
                LocalLocation::Stack(off) => format!("byte ptr {}", bp_addr(off)),
                _ => panic!("char index in unexpected location"),
            };
            let _ = write!(self.out, "\tmov\tal,{src}\r\n");
            if unsigned {
                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
            } else {
                self.out.extend_from_slice(b"\tcbw\t\r\n");
            }
            let _ = write!(self.out, "\tlea\tdx,word ptr {}\r\n", bp_addr(base_off));
            self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
            return;
        }
        // Compound-index or stride-≥-2 path: compute index into BX
        // first (the shl chains on, leaving BX hot), then lea, add.
        // When the index is a Call (or other expression that
        // naturally leaves the result in AX), BCC keeps AX hot:
        // scale AX, lea base into DX, add ax+dx, then mov bx, ax.
        // Fixture 1372 (`a[idx()]` for stack array).
        if matches!(index.kind, ExprKind::Call { .. }) {
            self.emit_expr_to_ax(index);
            if elem_size == 2 {
                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
            }
            let _ = write!(self.out, "\tlea\tdx,word ptr {}\r\n", bp_addr(base_off));
            self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
            self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
            return;
        }
        match &index.kind {
            ExprKind::Ident(idx_name) => match self.locals.location_of(idx_name) {
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                }
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                }
            },
            ExprKind::BinOp { op: BinOp::Add, left, right } => {
                if let (Some(l_src), Some(r_src)) =
                    (self.int_lvalue_addr(left), self.int_lvalue_addr(right))
                {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {l_src}\r\n");
                    let _ = write!(self.out, "\tadd\tbx,word ptr {r_src}\r\n");
                } else {
                    self.emit_expr_to_ax(index);
                    self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                }
            }
            _ => {
                self.emit_expr_to_ax(index);
                self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
            }
        }
        if elem_size == 2 {
            self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
        } else if elem_size == 4 {
            self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
        }
        let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(base_off));
        self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
    }
    /// Two-dim variable-index address: lands `&a[i][j]` in BX.
    ///
    /// Two shape variants depending on whether outer stride is a
    /// power of two:
    ///
    /// **Power-of-2 outer stride** (e.g. `int m[3][4]`, stride 8) —
    /// accumulator is BX from the start, chained `shl bx, 1` for the
    /// outer scale. Fixture 2346.
    /// ```text
    ///   mov bx, <outer-reg>
    ///   shl bx, 1 ... (log2 of outer stride times)
    ///   mov ax, <inner-reg>
    ///   shl ax, 1                 ; only when inner-stride == 2
    ///   add bx, ax
    ///   lea ax, word ptr [bp-base]
    ///   add bx, ax
    /// ```
    ///
    /// **Non-power-of-2 outer stride** (e.g. `int a[2][3]`, stride 6) —
    /// `imul` requires AX, so the accumulator is AX with a trailing
    /// `mov bx, ax`. Fixture 198.
    /// ```text
    ///   mov ax, <outer-reg>
    ///   mov dx, <outer-stride>
    ///   imul dx
    ///   mov dx, <inner-reg>
    ///   shl dx, 1                 ; only when inner-stride == 2
    ///   add ax, dx
    ///   lea dx, word ptr [bp-base]
    ///   add ax, dx
    ///   mov bx, ax
    /// ```
    pub(crate) fn emit_array_addr_2d_to_bx(
        &mut self,
        outer_idx: &Expr,
        inner_idx: &Expr,
        outer_stride: u16,
        inner_stride: u16,
        base_off: i16,
    ) {
        let outer_reg = self.idx_reg_name(outer_idx);
        let inner_reg = self.idx_reg_name(inner_idx);
        let outer_pow2 = outer_stride > 1 && outer_stride.is_power_of_two();
        if outer_pow2 {
            let _ = write!(self.out, "\tmov\tbx,{outer_reg}\r\n");
            for _ in 0..outer_stride.trailing_zeros() {
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
            }
            let _ = write!(self.out, "\tmov\tax,{inner_reg}\r\n");
            if inner_stride == 2 {
                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
            } else if inner_stride != 1 {
                panic!("2D inner-stride != {{1,2}} not yet supported (no fixture)");
            }
            self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
            let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(base_off));
            self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
            return;
        }
        let _ = write!(self.out, "\tmov\tax,{outer_reg}\r\n");
        let _ = write!(self.out, "\tmov\tdx,{outer_stride}\r\n");
        self.out.extend_from_slice(b"\timul\tdx\r\n");
        let _ = write!(self.out, "\tmov\tdx,{inner_reg}\r\n");
        if inner_stride == 2 {
            self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
        } else if inner_stride != 1 {
            panic!("2D inner-stride != {{1,2}} not yet supported (no fixture)");
        }
        self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
        let _ = write!(self.out, "\tlea\tdx,word ptr {}\r\n", bp_addr(base_off));
        self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
        self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
    }
    /// `a[<i1>][<i2>]... = <value>;` — write into an array slot. With
    /// all-constant indices we fold to a single `mov <width> ptr
    /// [bp-N], K`. Otherwise (single-dim variable index, fixtures
    /// 078/142) we compute `&a[i]` into BX and store through it.
    pub(crate) fn emit_array_assign(&mut self, array: &str, indices: &[Expr], value: &Expr) {
        // `<arr>[K] = <func-name>` — function-pointer array slot
        // assignment with a known function symbol. Emits the direct
        // immediate-to-memory store with the SegRel FIXUPP, no AX
        // round-trip. Fixture 1658 (`ops[0] = add5;`).
        if indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && let ExprKind::Ident(src_name) = &value.kind
            && !self.locals.has(src_name)
            && self.globals.type_of(src_name).is_none()
            && self.signatures.ret_ty_of(src_name).is_some()
        {
            // Local array of pointers — stack-resident.
            if self.locals.has(array)
                && let arr_ty = self.locals.type_of(array).clone()
                && let Some(elem_ty) = arr_ty.array_elem()
                && elem_ty.pointee().is_some()
                && let LocalLocation::Stack(base_off) = self.locals.location_of(array)
            {
                let stride = i32::from(elem_ty.size_bytes());
                let byte_off = (k as i32).wrapping_mul(stride);
                let final_off = base_off + i16::try_from(byte_off).unwrap_or(i16::MAX);
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr {},offset _{src_name}\r\n",
                    bp_addr(final_off),
                );
                return;
            }
            // Global array of pointers.
            if let Some(gty) = self.globals.type_of(array)
                && let Some(elem_ty) = gty.array_elem()
                && elem_ty.pointee().is_some()
            {
                let stride = u32::from(elem_ty.size_bytes());
                let byte_off = k.wrapping_mul(stride);
                let label = if byte_off == 0 {
                    format!("DGROUP:_{array}")
                } else {
                    format!("DGROUP:_{array}+{byte_off}")
                };
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr {label},offset _{src_name}\r\n",
                );
                return;
            }
        }
        // Pointer-base: `p[K] = v` is sugar for `*(p + K) = v`. For a
        // long-pointee constant index of 0, this is identical to
        // `*p = v` — same memory-direct pair through `[reg]`/`[reg+2]`.
        // Fixture 312 (`long *p; p[0] = 42;`).
        if self.locals.has(array)
            && let Some(pointee) = self.locals.type_of(array).pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
        {
            let pointee = pointee.clone();
            // Stack-resident far-pointer parameter / local: each
            // `a[K] = v` is a self-contained `les bx, [bp+a]; mov
            // word es:[bx+K*stride], v`. BCC reloads `les bx` for
            // every write — the prior write through ES:BX is
            // assumed to clobber the pair, so the next access
            // re-establishes it. Fixture 1870 (`a[0] = 10; a[1] =
            // 20;` for `int *a` under -ml).
            if matches!(self.locals.type_of(array), Type::FarPointer { .. })
                && let LocalLocation::Stack(a_off) = self.locals.location_of(array)
            {
                let stride = u32::from(pointee.size_bytes());
                let byte_off = (k * stride) as i32;
                let _ = write!(self.out, "\tles\tbx,word ptr {}\r\n", bp_addr(a_off));
                let addr = if byte_off == 0 {
                    "es:[bx]".to_string()
                } else {
                    format!("es:[bx+{byte_off}]")
                };
                if let Some(v) = try_const_eval(value) {
                    if pointee.is_char_like() {
                        let v8 = v & 0xFF;
                        let _ = write!(self.out, "\tmov\tbyte ptr {addr},{v8}\r\n");
                    } else {
                        let v16 = v & 0xFFFF;
                        let _ = write!(self.out, "\tmov\tword ptr {addr},{v16}\r\n");
                    }
                } else {
                    self.emit_expr_to_ax(value);
                    if pointee.is_char_like() {
                        let _ = write!(self.out, "\tmov\tbyte ptr {addr},al\r\n");
                    } else {
                        let _ = write!(self.out, "\tmov\tword ptr {addr},ax\r\n");
                    }
                }
                return;
            }
            let LocalLocation::Reg(reg) = self.locals.location_of(array) else {
                panic!("stack-resident pointer indexed write not yet supported (no fixture)");
            };
            let r = reg.name();
            let stride = u32::from(pointee.size_bytes());
            let byte_off = (k * stride) as i32;
            if pointee.is_long_like() {
                let Some(v) = try_const_eval(value) else {
                    panic!("non-constant rhs in `p[K] = v` (long pointee) not yet supported (no fixture)");
                };
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let lo_addr = if byte_off == 0 {
                    format!("[{r}]")
                } else {
                    format!("[{r}+{byte_off}]")
                };
                let hi_addr = format!("[{r}+{}]", byte_off + 2);
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},{hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},{lo}\r\n");
                return;
            }
            // `int *p; p[K] = v` — `mov word ptr [<reg>+byte_off],
            // <value>` (or `[<reg>]` when byte_off==0). Fixture 590.
            let width = ptr_width(&pointee);
            let addr = if byte_off == 0 {
                format!("[{r}]")
            } else {
                format!("[{r}+{byte_off}]")
            };
            if let Some(v) = try_const_eval(value) {
                let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(self.out, "\tmov\t{width} ptr {addr},{v_masked}\r\n");
                return;
            }
            panic!("non-constant rhs in `int *p; p[K] = v` not yet supported (no fixture)");
        }
        // Global array? Route to DGROUP-relative addressing.
        if let Some(gty) = self.globals.type_of(array) {
            let gty = gty.clone();
            if let Some((const_off, leaf_ty)) =
                try_const_array_offset(&gty, indices.iter())
            {
                // Long element: store both halves, high then low.
                // Fixture 302 (`long a[3]; a[1] = 42;`).
                if leaf_ty.is_long_like() {
                    let lo_addr = global_offset_addr(array, const_off);
                    let hi_addr = global_offset_addr(array, const_off + 2);
                    if let Some(v) = try_const_eval(value) {
                        let lo = (v & 0xFFFF) as u16;
                        let hi = ((v >> 16) & 0xFFFF) as u16;
                        let _ = write!(self.out, "\tmov\tword ptr {hi_addr},{hi}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {lo_addr},{lo}\r\n");
                        return;
                    }
                    // Non-constant RHS (e.g. `a[1] = g + h`): route
                    // through the long-value-to-dest helper. Fixture
                    // 359.
                    if self.try_emit_long_value_to_dest(value, &hi_addr, &lo_addr) {
                        return;
                    }
                    panic!("non-constant rhs in long-array element assign not yet supported (no fixture)");
                }
                let width = ptr_width(&leaf_ty);
                let addr = global_offset_addr(array, const_off);
                if let Some(v) = try_const_eval(value) {
                    let v_masked =
                        if leaf_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                    let _ = write!(self.out, "\tmov\t{width} ptr {addr},{v_masked}\r\n");
                    return;
                }
                // Non-constant RHS to a fixed-offset global array
                // element: evaluate to AX, then store. Fixture 1458
                // (`int g[3]; g[1] = v;`).
                self.emit_expr_to_ax(value);
                if leaf_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tbyte ptr {addr},al\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tword ptr {addr},ax\r\n");
                }
                return;
            }
            // Global-pointer subscript assignment: `int *p; p[K] = v`.
            // Load the pointer into BX, then `mov word ptr [bx+off],
            // <ax|imm>`. Mirrors the local-pointer path (fixture 590)
            // but the pointer lives in DGROUP, not a register.
            // Fixture 887 (var RHS).
            if let Some(pointee) = gty.pointee()
                && indices.len() == 1
                && let Some(k) = try_const_eval(&indices[0])
                && matches!(pointee, Type::Int | Type::UInt)
            {
                let stride = i32::from(pointee.size_bytes());
                let off = (k as i32).wrapping_mul(stride);
                let bx_disp = if off == 0 {
                    "[bx]".to_owned()
                } else if off > 0 {
                    format!("[bx+{off}]")
                } else {
                    format!("[bx-{}]", -off)
                };
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
                if let Some(v) = try_const_eval(value) {
                    let v_masked = v & 0xFFFF;
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {bx_disp},{v_masked}\r\n",
                    );
                } else {
                    self.emit_expr_to_ax(value);
                    let _ = write!(self.out, "\tmov\tword ptr {bx_disp},ax\r\n");
                }
                return;
            }
            // Long-pointer subscript assignment: `long *p; p[K] = v`.
            // `mov bx, _p; mov word ptr [bx+off+2], <hi>; mov word
            // ptr [bx+off], <lo>`. High-first store convention same
            // as long-global and long-array paths. Fixture 897.
            if let Some(pointee) = gty.pointee()
                && indices.len() == 1
                && let Some(k) = try_const_eval(&indices[0])
                && pointee.is_long_like()
            {
                let Some(v) = try_const_eval(value) else {
                    panic!("non-constant rhs in `long *p; p[K] = v` not yet supported (no fixture)");
                };
                let stride = i32::from(pointee.size_bytes());
                let off = (k as i32).wrapping_mul(stride);
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let lo_addr = if off == 0 {
                    "[bx]".to_owned()
                } else if off > 0 {
                    format!("[bx+{off}]")
                } else {
                    format!("[bx-{}]", -off)
                };
                let hi_off = off + 2;
                let hi_addr = if hi_off > 0 {
                    format!("[bx+{hi_off}]")
                } else if hi_off < 0 {
                    format!("[bx-{}]", -hi_off)
                } else {
                    "[bx]".to_owned()
                };
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},{hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},{lo}\r\n");
                return;
            }
            // Variable-indexed global char-array write. BCC uses SI
            // (the loop var's register) with no stride scaling
            // (stride=1 for char). For const RHS K: `mov byte ptr
            // _arr[si], K`. Fixture 1366 (`for (i=0..) buf[i] =
            // 'X';` for global char buf[]).
            if indices.len() == 1
                && let Some(elem) = gty.array_elem()
                && elem.is_char_like()
                && let ExprKind::Ident(i_name) = &indices[0].kind
                && self.locals.has(i_name)
                && let LocalLocation::Reg(reg) = self.locals.location_of(i_name)
                && reg.name() == "si"
            {
                if let Some(v) = try_const_eval(value) {
                    let v8 = (v & 0xFF) as u8;
                    let _ = write!(
                        self.out,
                        "\tmov\tbyte ptr DGROUP:_{array}[si],{v8}\r\n",
                    );
                    return;
                }
                // Non-const RHS: load value to AL, then store
                // through SI-indexed addressing. Skip the cbw
                // widening — the byte store only needs AL.
                // Fixture 1426 (`dst[i] = src[i]`).
                //
                // Simple char-ident RHS: byte-load directly (no
                // widen). Avoids the wasted cbw / mov ah, 0 that
                // emit_expr_to_ax would emit for a char ident even
                // with skip_widen set (which it doesn't honor in
                // the Ident path — see notes there). Fixture 3450
                // (`put(int i, char v) { arr[i] = v; }`).
                if let ExprKind::Ident(v_name) = &value.kind
                    && self.locals.has(v_name)
                    && self.locals.type_of(v_name).is_char_like()
                {
                    let v_addr = match self.locals.location_of(v_name) {
                        LocalLocation::Stack(off) => format!("byte ptr {}", bp_addr(off)),
                        LocalLocation::Reg(reg) if reg.is_byte() => reg.name().to_owned(),
                        _ => String::new(),
                    };
                    if !v_addr.is_empty() {
                        let _ = write!(self.out, "\tmov\tal,{v_addr}\r\n");
                        let _ = write!(
                            self.out,
                            "\tmov\tbyte ptr DGROUP:_{array}[si],al\r\n",
                        );
                        return;
                    }
                }
                let skip_widen_prev = self.skip_widen;
                self.skip_widen = true;
                self.emit_expr_to_ax(value);
                self.skip_widen = skip_widen_prev;
                let _ = write!(
                    self.out,
                    "\tmov\tbyte ptr DGROUP:_{array}[si],al\r\n",
                );
                return;
            }
            // Variable-indexed global char-array write with i in DX
            // (not SI) — fires when the SI→DX heuristic picks DX for
            // the loop counter. 8086 has no DX-indexed addressing,
            // so copy DX→BX and use bx-indexed `mov byte ptr _arr
            // [bx], <al>`. Fixture 1257 (`char arr[5]; for (i=0..)
            // arr[i] = i` with i in DX).
            if indices.len() == 1
                && let Some(elem) = gty.array_elem()
                && elem.is_char_like()
                && let ExprKind::Ident(i_name) = &indices[0].kind
                && self.locals.has(i_name)
                && let LocalLocation::Reg(reg) = self.locals.location_of(i_name)
                && !reg.is_byte()
            {
                let reg_name = reg.name();
                let low = match reg_name {
                    "dx" => Some("dl"),
                    "bx" => Some("bl"),
                    "cx" => Some("cl"),
                    _ => None,
                };
                // BCC's exact shape for `_arr[i] = i` (i in DX):
                //   mov bx, dx        ; set up index reg first
                //   mov al, dl        ; byte-form load of value
                //   mov byte ptr _arr[bx], al
                if let ExprKind::Ident(v_name) = &value.kind
                    && v_name == i_name
                    && let Some(low_name) = low
                {
                    let _ = write!(self.out, "\tmov\tbx,{reg_name}\r\n");
                    let _ = write!(self.out, "\tmov\tal,{low_name}\r\n");
                    let _ = write!(
                        self.out,
                        "\tmov\tbyte ptr DGROUP:_{array}[bx],al\r\n",
                    );
                    return;
                }
                // Const RHS: just set up BX and store.
                if let Some(v) = try_const_eval(value) {
                    let v8 = (v & 0xFF) as u8;
                    let _ = write!(self.out, "\tmov\tbx,{reg_name}\r\n");
                    let _ = write!(
                        self.out,
                        "\tmov\tbyte ptr DGROUP:_{array}[bx],{v8}\r\n",
                    );
                    return;
                }
                // Generic: evaluate value to AL, copy DX→BX, store.
                self.emit_expr_to_ax(value);
                let _ = write!(self.out, "\tmov\tbx,{reg_name}\r\n");
                let _ = write!(
                    self.out,
                    "\tmov\tbyte ptr DGROUP:_{array}[bx],al\r\n",
                );
                return;
            }
            // Variable-indexed global int-array write. Load `i` into
            // BX, shl once for stride 2, then `mov word ptr
            // _a[bx], <src>`. Fixture 510 (`a[i] = i`).
            //
            // When the index is `i++` (or `i--`), BCC snapshots i to
            // BX directly and defers the post-update to AFTER the
            // store. Fixture 2499 (`a[i++] = 7`).
            if indices.len() == 1
                && let Some(elem) = gty.array_elem()
                && matches!(elem, Type::Int | Type::UInt)
                // Value must be resolve_operand_source-able: a
                // constant, ident lvalue, deref, or member chain.
                // ArrayIndex is OK only when its inner index is a
                // constant (resolve_operand_source's chain-addr
                // walker requires const index). For BinOp / Call /
                // Ternary value, fall through to the more general
                // path below.
                && {
                    try_const_eval(value).is_some()
                        || matches!(value.kind,
                            ExprKind::Ident(_)
                            | ExprKind::IntLit(_)
                            | ExprKind::Deref(_)
                            | ExprKind::Member { .. })
                        || (matches!(value.kind, ExprKind::ArrayIndex { .. })
                            && self.try_lvalue_chain_addr(value).is_some())
                }
            {
                let index = &indices[0];
                // Track whether to emit a post-update after the store.
                let mut deferred_post: Option<(&str, crate::ast::UpdateOp)> = None;
                if let ExprKind::Update {
                    target: upd_name,
                    op: upd_op,
                    position: crate::ast::UpdatePosition::Post,
                } = &index.kind
                    && self.locals.has(upd_name)
                    && let LocalLocation::Reg(upd_reg) = self.locals.location_of(upd_name)
                    && !upd_reg.is_byte()
                {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", upd_reg.name());
                    deferred_post = Some((upd_reg.name(), *upd_op));
                } else if let ExprKind::Ident(i_name) = &index.kind
                    && self.locals.has(i_name)
                {
                    match self.locals.location_of(i_name) {
                        LocalLocation::Stack(off) => {
                            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                        }
                        LocalLocation::Reg(reg) => {
                            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                        }
                    }
                } else {
                    // Call-indexed shape: BCC emits the call (AX =
                    // index), scales AX, loads the value into DX
                    // (to free up AX), then copies AX→BX and
                    // stores DX. Fixture 2914.
                    if matches!(index.kind, ExprKind::Call { .. })
                        && let src = self.resolve_operand_source(value)
                        && !matches!(src, OperandSource::Immediate(_) | OperandSource::Reg(_))
                    {
                        self.emit_expr_to_ax(index);
                        self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                        let _ = write!(
                            self.out,
                            "\tmov\tdx,{}\r\n",
                            src.word(),
                        );
                        self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr DGROUP:_{array}[bx],dx\r\n",
                        );
                        return;
                    }
                    self.emit_expr_to_ax(index);
                    self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                }
                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
                let src = self.resolve_operand_source(value);
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{array}[bx],{}\r\n",
                    src.word(),
                );
                if let Some((upd_reg_name, upd_op)) = deferred_post {
                    let upd_mnem = match upd_op {
                        crate::ast::UpdateOp::Inc => "inc",
                        crate::ast::UpdateOp::Dec => "dec",
                    };
                    let _ = write!(self.out, "\t{upd_mnem}\t{upd_reg_name}\r\n");
                }
                return;
            }
            // Variable-indexed global long-array write. Load `i` into
            // BX (directly if it's a stack/reg local, otherwise via
            // AX), shl twice for stride 4, then write `mov word ptr
            // _a[bx+0], lo` and `mov word ptr _a[bx+2], hi`. Fixture
            // 305.
            if indices.len() == 1
                && let Some(elem) = gty.array_elem()
                && elem.is_long_like()
            {
                let index = &indices[0];
                // Non-const long lvalue RHS: load DX:AX from source
                // (AX=hi, DX=lo — BCC's observed order for this
                // store), then index into BX and store both halves.
                // Fixture 3297 (`arr[i] = v` for long global, long
                // param v).
                if try_const_eval(value).is_none()
                    && let Some((v_hi, v_lo)) = self.long_lvalue_addr_pair(value)
                {
                    self.emit_index_into_bx_long_stride(index);
                    let _ = write!(self.out, "\tmov\tax,word ptr {v_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {v_lo}\r\n");
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr DGROUP:_{array}[bx+2],ax\r\n",
                    );
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr DGROUP:_{array}[bx],dx\r\n",
                    );
                    return;
                }
                let Some(v) = try_const_eval(value) else {
                    panic!("non-constant rhs in variable-indexed global long-array assign not yet supported (no fixture)");
                };
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                self.emit_index_into_bx_long_stride(index);
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{array}[bx+2],{hi}\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr DGROUP:_{array}[bx],{lo}\r\n",
                );
                return;
            }
            // Variable-indexed global int- or char-array assignment.
            // For constant value: compute scaled index into BX, then
            // immediate-store via `<sym>[bx]`. For non-constant value:
            // BCC computes the RHS into AX FIRST, then sets up BX for
            // the indexed store — this order is observed in fixture
            // 1444 (`a[i] = i*10`). The reverse order is
            // functionally equivalent but byte-different.
            if indices.len() == 1
                && let Some(elem) = gty.array_elem()
                && (elem.is_char_like() || elem.is_int_like())
            {
                let elem_ty = elem.clone();
                let elem_is_char = elem_ty.is_char_like();
                if let Some(v) = try_const_eval(value) {
                    self.emit_index_into_bx(&indices[0], &elem_ty);
                    let width = if elem_is_char { "byte" } else { "word" };
                    let v_masked = if elem_is_char { v & 0xFF } else { v & 0xFFFF };
                    let _ = write!(
                        self.out,
                        "\tmov\t{width} ptr DGROUP:_{array}[bx],{v_masked}\r\n",
                    );
                    return;
                }
                // Non-constant value: RHS first into AX (or AL), then
                // BX setup. For char: try a direct byte load if the
                // source is a char-typed lvalue, else go through AL
                // (truncating). For int: full AX.
                if elem_is_char {
                    if let Some((src_name, src_off, src_ty)) =
                        self.try_lvalue_chain_addr(value)
                        && src_ty.is_char_like()
                        && let Some(src_addr) = self.resolve_chain_addr(&src_name, src_off)
                    {
                        let _ = write!(self.out, "\tmov\tal,byte ptr {src_addr}\r\n");
                    } else {
                        self.emit_expr_to_ax(value);
                    }
                    self.emit_index_into_bx(&indices[0], &elem_ty);
                    let _ = write!(
                        self.out,
                        "\tmov\tbyte ptr DGROUP:_{array}[bx],al\r\n",
                    );
                } else {
                    self.emit_expr_to_ax(value);
                    self.emit_index_into_bx(&indices[0], &elem_ty);
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr DGROUP:_{array}[bx],ax\r\n",
                    );
                }
                return;
            }
            // 2D global array variable-indexed write: `a[i][j] = v`
            // for `T a[M][N]`. Symmetric to the 2D read path above
            // (fixtures 198, 1469). Computes the linearized byte
            // offset into BX, then stores via `DGROUP:_a[bx]`.
            if indices.len() == 2
                && let Type::Array { elem: inner_arr, .. } = &gty
                && let Type::Array { elem: leaf_elem, .. } = &**inner_arr
            {
                let outer_stride = inner_arr.size_bytes();
                let inner_stride = leaf_elem.size_bytes();
                let leaf_ty = (**leaf_elem).clone();
                let outer_addr = self.int_lvalue_src(&indices[0]);
                let inner_addr = self.int_lvalue_src(&indices[1]);
                if let (Some(outer), Some(inner)) = (outer_addr, inner_addr) {
                    let outer_is_reg = is_reg16_name(&outer);
                    let inner_is_reg = is_reg16_name(&inner);
                    let outer_src = if outer_is_reg {
                        outer.clone()
                    } else {
                        format!("word ptr {outer}")
                    };
                    let inner_src = if inner_is_reg {
                        inner.clone()
                    } else {
                        format!("word ptr {inner}")
                    };
                    let const_value = try_const_eval(value);
                    let outer_pow2 = outer_stride > 1 && outer_stride.is_power_of_two();
                    let outer_one = outer_stride == 1;
                    if outer_pow2 || outer_one {
                        let _ = write!(self.out, "\tmov\tbx,{outer_src}\r\n");
                        let outer_shifts = (outer_stride as u32).trailing_zeros();
                        for _ in 0..outer_shifts {
                            self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
                        }
                        let _ = write!(self.out, "\tmov\tax,{inner_src}\r\n");
                        let inner_shifts = (inner_stride as u32).trailing_zeros();
                        for _ in 0..inner_shifts {
                            self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                        }
                        self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
                    } else {
                        let _ = write!(self.out, "\tmov\tax,{outer_src}\r\n");
                        let _ = write!(self.out, "\tmov\tdx,{outer_stride}\r\n");
                        self.out.extend_from_slice(b"\timul\tdx\r\n");
                        if inner_stride == 2 {
                            let _ = write!(self.out, "\tmov\tdx,{inner_src}\r\n");
                            self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                            self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
                        } else {
                            let _ = write!(self.out, "\tadd\tax,{inner_src}\r\n");
                        }
                        self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                    }
                    let width = ptr_width(&leaf_ty);
                    if let Some(v) = const_value {
                        let v_masked =
                            if leaf_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                        let _ = write!(
                            self.out,
                            "\tmov\t{width} ptr DGROUP:_{array}[bx],{v_masked}\r\n",
                        );
                    } else {
                        self.emit_expr_to_ax(value);
                        if leaf_ty.is_char_like() {
                            let _ = write!(
                                self.out,
                                "\tmov\tbyte ptr DGROUP:_{array}[bx],al\r\n",
                            );
                        } else {
                            let _ = write!(
                                self.out,
                                "\tmov\tword ptr DGROUP:_{array}[bx],ax\r\n",
                            );
                        }
                    }
                    return;
                }
            }
            panic!("variable-indexed global array assign not yet supported (no fixture)");
        }
        // `p[i] = v` where `p` is a pointer local (not an array).
        // Load the pointer into BX, scale the index, add, store.
        // Fixture 1285 (`p[i] = v` with `char *p` parameter).
        if let Some(pointee) = self.locals.type_of(array).pointee()
            && indices.len() == 1
        {
            let pointee = pointee.clone();
            let stride = i32::from(pointee.size_bytes());
            let elem_is_char = pointee.is_char_like();
            // For a stack-resident pointer with a variable index
            // (no constant), BCC sequences the index scaling FIRST
            // and only then loads the base pointer into BX — saves
            // the spill round-trip a constant-store would force.
            // We mirror that by deferring the BX-load to after the
            // index computation in this var-index path. The const
            // path keeps the original "load BX first" shape since
            // there's no AX dance to interleave. Fixture 1439.
            let is_var_index = try_const_eval(&indices[0]).is_none();
            let stack_ptr_off =
                if let LocalLocation::Stack(off) = self.locals.location_of(array) {
                    Some(off)
                } else {
                    None
                };
            let defer_bx_load = is_var_index && stack_ptr_off.is_some() && stride == 2;
            if !defer_bx_load {
                match self.locals.location_of(array) {
                    LocalLocation::Reg(reg) => {
                        let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                    }
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                    }
                }
            }
            if let Some(k) = try_const_eval(&indices[0]) {
                let off = (k as i32).wrapping_mul(stride);
                let bx_disp = if off == 0 {
                    "[bx]".to_owned()
                } else if off > 0 {
                    format!("[bx+{off}]")
                } else {
                    format!("[bx-{}]", -off)
                };
                if let Some(v) = try_const_eval(value) {
                    let v_masked = if elem_is_char { v & 0xFF } else { v & 0xFFFF };
                    let width = if elem_is_char { "byte" } else { "word" };
                    let _ = write!(self.out, "\tmov\t{width} ptr {bx_disp},{v_masked}\r\n");
                } else {
                    self.emit_expr_to_ax(value);
                    if elem_is_char {
                        let _ = write!(self.out, "\tmov\tbyte ptr {bx_disp},al\r\n");
                    } else {
                        let _ = write!(self.out, "\tmov\tword ptr {bx_disp},ax\r\n");
                    }
                }
                return;
            }
            // Char-stride (1) with index in SI: BCC uses indexed
            // addressing `[bx+si]` directly — no separate add at
            // all. Fixture 3559 (`for (i ... in SI) buf[i] = 0;`).
            if stride == 1
                && let ExprKind::Ident(idx_name) = &indices[0].kind
                && self.locals.has(idx_name)
                && let LocalLocation::Reg(idx_reg) = self.locals.location_of(idx_name)
                && matches!(idx_reg, Reg::Si | Reg::Di)
            {
                let idx_reg_name = idx_reg.name();
                if let Some(v) = try_const_eval(value) {
                    let v_masked = (v & 0xFF) as u8;
                    let _ = write!(
                        self.out,
                        "\tmov\tbyte ptr [bx+{idx_reg_name}],{v_masked}\r\n",
                    );
                } else if let ExprKind::Ident(v_name) = &value.kind
                    && self.locals.has(v_name)
                    && self.locals.type_of(v_name).is_char_like()
                {
                    let v_src = match self.locals.location_of(v_name) {
                        LocalLocation::Stack(off) => format!("byte ptr {}", bp_addr(off)),
                        LocalLocation::Reg(reg) if reg.is_byte() => reg.name().to_owned(),
                        _ => String::new(),
                    };
                    if !v_src.is_empty() {
                        let _ = write!(self.out, "\tmov\tal,{v_src}\r\n");
                        let _ = write!(
                            self.out,
                            "\tmov\tbyte ptr [bx+{idx_reg_name}],al\r\n",
                        );
                    } else {
                        // Fallback through AX.
                        self.emit_expr_to_ax(value);
                        let _ = write!(
                            self.out,
                            "\tmov\tbyte ptr [bx+{idx_reg_name}],al\r\n",
                        );
                    }
                } else {
                    self.emit_expr_to_ax(value);
                    let _ = write!(
                        self.out,
                        "\tmov\tbyte ptr [bx+{idx_reg_name}],al\r\n",
                    );
                }
                return;
            }
            // Char-stride (1) memory-direct add: when stride is 1
            // and the index is a simple int lvalue, skip the AX
            // route and add memory directly to BX. Fixture 1285
            // (`p[i] = v` for char*).
            if stride == 1
                && let Some(idx_addr) = self.int_lvalue_addr(&indices[0])
            {
                let _ = write!(self.out, "\tadd\tbx,word ptr {idx_addr}\r\n");
            } else {
                let idx_src = self.resolve_operand_source(&indices[0]);
                let _ = write!(self.out, "\tmov\tax,{}\r\n", idx_src.word());
                if stride == 2 {
                    self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                }
                if defer_bx_load
                    && let Some(off) = stack_ptr_off
                {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                }
                self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
            }
            if let Some(v) = try_const_eval(value) {
                let v_masked = if elem_is_char { v & 0xFF } else { v & 0xFFFF };
                let width = if elem_is_char { "byte" } else { "word" };
                let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
            } else {
                // For char element store with char-ident value,
                // byte-load directly (skip the wasted widen).
                // Mirrors the global-arr write peephole.
                if elem_is_char
                    && let ExprKind::Ident(v_name) = &value.kind
                    && self.locals.has(v_name)
                    && self.locals.type_of(v_name).is_char_like()
                {
                    let v_src = match self.locals.location_of(v_name) {
                        LocalLocation::Stack(off) => format!("byte ptr {}", bp_addr(off)),
                        LocalLocation::Reg(reg) if reg.is_byte() => reg.name().to_owned(),
                        _ => String::new(),
                    };
                    if !v_src.is_empty() {
                        let _ = write!(self.out, "\tmov\tal,{v_src}\r\n");
                        self.out.extend_from_slice(b"\tmov\tbyte ptr [bx],al\r\n");
                        return;
                    }
                }
                self.emit_expr_to_ax(value);
                if elem_is_char {
                    self.out.extend_from_slice(b"\tmov\tbyte ptr [bx],al\r\n");
                } else {
                    self.out.extend_from_slice(b"\tmov\tword ptr [bx],ax\r\n");
                }
            }
            return;
        }
        let array_ty = self.locals.type_of(array).clone();
        let LocalLocation::Stack(base_off) = self.locals.location_of(array) else {
            panic!("array `{array}` should be stack-resident");
        };
        if let Some((const_off, leaf_ty)) = try_const_array_offset(&array_ty, indices.iter()) {
            let off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
            // Float/double element on stack: walk the value through
            // the FPU pipeline (handles fld1 for 1.0, literal pools,
            // expressions, casts) and `fstp` it to the element slot.
            // Fixture 1679 (`float a[3]; a[0] = 1.0f;`).
            if leaf_ty.is_float_like() {
                if expr_is_float_one(value) {
                    self.out.extend_from_slice(b"\tfld1\t\r\n");
                } else {
                    self.emit_float_load_to_fpu(value);
                }
                let store_width =
                    if matches!(leaf_ty, Type::Float) { "dword" } else { "qword" };
                let _ = write!(
                    self.out,
                    "\tfstp\t{store_width} ptr {}\r\n",
                    bp_addr(off),
                );
                return;
            }
            // Long element on stack: store both halves, high then low.
            // Fixture 304 (`long a[2]; a[0] = 5;`).
            if leaf_ty.is_long_like() {
                let Some(v) = try_const_eval(value) else {
                    panic!("non-constant rhs in long-stack-array element assign not yet supported (no fixture)");
                };
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let _ = write!(self.out, "\tmov\tword ptr {},{hi}\r\n", bp_addr(off + 2));
                let _ = write!(self.out, "\tmov\tword ptr {},{lo}\r\n", bp_addr(off));
                return;
            }
            let width = ptr_width(&leaf_ty);
            if let Some(v) = try_const_eval(value) {
                let v_masked =
                    if leaf_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(
                    self.out,
                    "\tmov\t{width} ptr {},{v_masked}\r\n",
                    bp_addr(off),
                );
                return;
            }
            // Non-constant RHS for an int/uint/pointer element:
            // materialize RHS in AX, then store AX to the element.
            // Fixture 984 (`a[0] = x` with x a stack local).
            if !leaf_ty.is_char_like() {
                // Reg-resident-ident RHS: store the register directly
                // to the array slot. Fixture 2452 (`a[0] = x` with x
                // in SI → `mov [bp-N], si`).
                if let ExprKind::Ident(src_name) = &value.kind
                    && self.locals.has(src_name)
                    && self.locals.type_of(src_name).is_int_like()
                    && let LocalLocation::Reg(reg) = self.locals.location_of(src_name)
                    && !reg.is_byte()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},{}\r\n",
                        bp_addr(off),
                        reg.name(),
                    );
                    return;
                }
                // String-literal RHS: fold to `mov word ptr [bp-N],
                // offset DGROUP:s@+K` (single immediate-to-mem store
                // with relocation). Fixture 1710 (`strs[0] = "AB"`).
                if let ExprKind::StringLit(bytes) = &value.kind {
                    let str_off = self
                        .strings
                        .offset_for_span(value.span.start)
                        .unwrap_or_else(|| self.strings.intern(bytes));
                    let s_suffix = if str_off == 0 {
                        String::new()
                    } else {
                        format!("+{str_off}")
                    };
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset DGROUP:s@{s_suffix}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                self.emit_expr_to_ax(value);
                let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                return;
            }
            panic!("non-constant rhs in constant-indexed array assign not yet supported (no fixture)");
        }
        // 2D variable-index write: `a[i][j] = v` for `int a[M][N]`.
        // Same chain as the read path (fixture 198), with a store
        // through `[bx]` instead of a load.
        if indices.len() == 2 {
            let (outer_stride, inner_stride, leaf_ty) = match &array_ty {
                Type::Array { elem, .. } => match &**elem {
                    inner_arr @ Type::Array { elem: inner_elem, .. } => (
                        inner_arr.size_bytes(),
                        inner_elem.size_bytes(),
                        (**inner_elem).clone(),
                    ),
                    _ => panic!("`{array}[i][j] = v`: outer element isn't an array"),
                },
                _ => panic!("`{array}[i][j] = v`: not an array type"),
            };
            self.emit_array_addr_2d_to_bx(
                &indices[0],
                &indices[1],
                outer_stride,
                inner_stride,
                base_off,
            );
            let width = ptr_width(&leaf_ty);
            let Some(v) = try_const_eval(value) else {
                panic!("non-constant rhs in 2D array assign not yet supported (no fixture)");
            };
            let v_masked =
                if leaf_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
            return;
        }
        // Variable-index fallback: only the single-dim path is wired
        // up today (covers fixtures 078, 142). Deeper multi-dim with
        // any non-const subscript isn't fixtured.
        if indices.len() != 1 {
            panic!("multi-dim (>2) array assign with non-constant indices not yet supported (no fixture)");
        }
        let elem = array_ty
            .array_elem()
            .unwrap_or_else(|| panic!("`{array}[i] = v`: not an array type"));
        let elem_size = elem.size_bytes();
        let width = ptr_width(elem);
        self.emit_array_addr_to_bx(array, &indices[0], base_off, elem_size);
        if let Some(v) = try_const_eval(value) {
            let v_masked = if elem.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
            return;
        }
        // Word-sized store from a register-resident int local —
        // emit `mov word ptr [bx], <reg>` directly (89 mod=00 reg=<r>
        // r/m=111 → tasm parses the `mov [bx], <reg>` form already
        // since 89 with mod=00 r/m=111 is generic). Skip the AX
        // round-trip. Fixture 2244 (`arr[i] = i` with i in SI).
        if !elem.is_char_like()
            && let ExprKind::Ident(name) = &value.kind
            && self.locals.has(name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && !reg.is_byte()
        {
            let _ = write!(self.out, "\tmov\tword ptr [bx],{}\r\n", reg.name());
            return;
        }
        // Char-array store from a register-resident int local: load
        // just the low byte of the source reg into AL (`mov al,
        // <reg-low>` = 2 bytes) instead of the full int via AX
        // (`mov ax, <reg>` = 2 bytes, but then we'd waste the cbw
        // or fall through emit_expr_to_ax that may widen). The
        // byte-form load matches BCC's exact shape. Fixture 1219
        // (`char a[5]; a[i] = i` with i in DX → `mov al, dl`).
        // Also handles `<reg-int> + <const>` shape: the result is
        // truncated to char on store, so byte arithmetic is
        // equivalent and shorter (AL imm8 = 2 bytes vs AX imm16 =
        // 3 bytes). Fixture 1276 (`s[i] = 'a' + i`).
        let (byte_src_reg, byte_addend): (Option<&str>, Option<i32>) = match &value.kind {
            ExprKind::Ident(name) => {
                let n = self.locals.has(name)
                    && self.locals.type_of(name).is_int_like()
                    && matches!(self.locals.location_of(name), LocalLocation::Reg(r) if !r.is_byte());
                if n {
                    let LocalLocation::Reg(reg) = self.locals.location_of(name) else { unreachable!() };
                    let low = match reg.name() {
                        "dx" => Some("dl"),
                        "bx" => Some("bl"),
                        "cx" => Some("cl"),
                        _ => None,
                    };
                    (low, None)
                } else {
                    (None, None)
                }
            }
            ExprKind::BinOp { op: BinOp::Add, left, right } => {
                let ident = match (&left.kind, try_const_eval(right)) {
                    (ExprKind::Ident(n), Some(k)) => Some((n.as_str(), k as i32)),
                    _ => match (&right.kind, try_const_eval(left)) {
                        (ExprKind::Ident(n), Some(k)) => Some((n.as_str(), k as i32)),
                        _ => None,
                    },
                };
                if let Some((name, k)) = ident {
                    let ok = self.locals.has(name)
                        && self.locals.type_of(name).is_int_like()
                        && matches!(self.locals.location_of(name), LocalLocation::Reg(r) if !r.is_byte());
                    if ok {
                        let LocalLocation::Reg(reg) = self.locals.location_of(name) else { unreachable!() };
                        let low = match reg.name() {
                            "dx" => Some("dl"),
                            "bx" => Some("bl"),
                            "cx" => Some("cl"),
                            _ => None,
                        };
                        (low, Some(k))
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                }
            }
            _ => (None, None),
        };
        if elem.is_char_like() && let Some(low_name) = byte_src_reg {
            // BCC's order: emit the byte value into AL FIRST, then
            // compute the address (using DX rather than AX so AL
            // isn't clobbered), then store. The
            // `emit_array_addr_to_bx` call above already wrote the
            // AX-routed address; rewind and re-emit. Fixture 1276
            // (`s[i] = 'a' + i` with i in CX).
            if byte_addend.is_some()
                && let Some(rewound) =
                    try_rewind_array_addr_ax_to_dx(self.out, base_off, low_name)
            {
                self.out.truncate(rewound);
                let _ = write!(self.out, "\tmov\tal,{low_name}\r\n");
                let k = byte_addend.unwrap();
                let k8 = (k & 0xFF) as u8;
                let k_i8 = k8 as i8;
                if k_i8 == 1 {
                    self.out.extend_from_slice(b"\tinc\tal\r\n");
                } else if k_i8 == -1 {
                    self.out.extend_from_slice(b"\tdec\tal\r\n");
                } else {
                    let _ = write!(self.out, "\tadd\tal,{k_i8}\r\n");
                }
                let _ = write!(self.out, "\tlea\tdx,word ptr {}\r\n", bp_addr(base_off));
                let _ = write!(self.out, "\tmov\tbx,{low_name_word}\r\n",
                    low_name_word = match low_name {
                        "dl" => "dx",
                        "bl" => "bx",
                        "cl" => "cx",
                        _ => low_name,
                    });
                self.out.extend_from_slice(b"\tadd\tbx,dx\r\n");
                self.out.extend_from_slice(b"\tmov\tbyte ptr [bx],al\r\n");
                return;
            }
            let _ = write!(self.out, "\tmov\tal,{low_name}\r\n");
            if let Some(k) = byte_addend {
                let k8 = (k & 0xFF) as u8;
                let k_i8 = k8 as i8;
                if k_i8 == 1 {
                    self.out.extend_from_slice(b"\tinc\tal\r\n");
                } else if k_i8 == -1 {
                    self.out.extend_from_slice(b"\tdec\tal\r\n");
                } else {
                    let _ = write!(self.out, "\tadd\tal,{k_i8}\r\n");
                }
            }
            self.out.extend_from_slice(b"\tmov\tbyte ptr [bx],al\r\n");
            return;
        }
        // Non-constant RHS: evaluate to AX (or AL for byte storage),
        // then store through [bx]. For char-element stores, skip
        // the cbw widening that emit_expr_to_ax appends after a
        // byte load — the store only reads AL anyway. Fixture 1426
        // (`dst[i] = src[i]` with char arrays).
        if elem.is_char_like() {
            let skip_widen_prev = self.skip_widen;
            self.skip_widen = true;
            self.emit_expr_to_ax(value);
            self.skip_widen = skip_widen_prev;
            let _ = write!(self.out, "\tmov\tbyte ptr [bx],al\r\n");
        } else {
            self.emit_expr_to_ax(value);
            let _ = write!(self.out, "\tmov\tword ptr [bx],ax\r\n");
        }
    }
}
