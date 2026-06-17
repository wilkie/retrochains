use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// `<base>.<field>` or `<base>-><field>` in rvalue position.
    /// Computes the field's effective address and loads from there
    /// with the appropriate width.
    ///
    /// - **Dot** (`a.x` — fixture 101 etc.): base must be an `Ident`
    ///   referring to a struct stack local. Field at offset `K` lives
    ///   at `[bp - struct_base + K]` which simplifies to a single
    ///   `[bp-N]` load.
    /// - **Arrow** (`p->x` — fixture 105, 106): base must be an
    ///   `Ident` for a pointer in a register. Field at offset `K`
    ///   lives at `[reg + K]`; `K = 0` collapses to `[reg]`.
    pub(crate) fn emit_member_to_ax(
        &mut self,
        base: &Expr,
        field: &str,
        kind: crate::ast::MemberKind,
    ) {
        // Dot: try the lvalue-chain helper so `a.x`, `pts[1].x`, and
        // nested `a.b.c` all fold to a single load. Works for both
        // stack locals (`[bp-N]`) and file-scope globals
        // (`DGROUP:_<name>+K`, fixture 190).
        if matches!(kind, crate::ast::MemberKind::Dot) {
            if let Some((name, total_off, leaf_ty)) =
                self.try_member_dot_chain(base, field)
            {
                // Locals shadow same-named globals — fall through to
                // the local path even if a global of the same name
                // exists (e.g. a static-local in another function).
                // Fixture 2208 (sum_pts's `pts` param vs. main's
                // `static struct P pts[3]`).
                if !self.locals.has(&name) && self.globals.contains(&name) {
                    let load_byte = leaf_ty.is_char_like();
                    let width = if load_byte { "byte" } else { "word" };
                    let addr = if total_off == 0 {
                        format!("DGROUP:_{name}")
                    } else {
                        format!("DGROUP:_{name}+{total_off}")
                    };
                    if load_byte {
                        let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                        self.emit_widen_al(&leaf_ty);
                    } else {
                        let _ = write!(self.out, "\tmov\tax,{width} ptr {addr}\r\n");
                    }
                    return;
                }
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) else {
                    panic!("struct local `{name}` not stack-resident (unexpected)");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                if leaf_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                    self.emit_widen_al(&leaf_ty);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                }
                return;
            }
        }
        // `(<dot-chain>)-><field>` — base is a Dot-chain whose leaf
        // is a pointer-to-struct. Load that pointer into BX, then
        // read through `[bx+field_off]`. Fixture 1419 (`a.next->v`
        // with a global struct having a struct-pointer field).
        //
        // The pointed-to struct's fields aren't carried in the AST
        // type (Pointer holds a name-only placeholder), so we look up
        // the full struct definition via `lookup_struct_by_tag`.
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::Member { base: inner_base, field: inner_field, kind: crate::ast::MemberKind::Dot } = &base.kind
            && let Some((root_name, total_off, leaf_ty)) = self.try_member_dot_chain(inner_base, inner_field)
            && let Some(pointee) = leaf_ty.pointee()
            && let Type::Struct { name: Some(tag), .. } = pointee
            && let Some(full_ty) = self.lookup_struct_by_tag(tag)
            && let Some((field_off, field_ty)) = full_ty.field(field)
        {
            let load_addr = if self.globals.contains(&root_name) {
                if total_off == 0 {
                    format!("DGROUP:_{root_name}")
                } else {
                    format!("DGROUP:_{root_name}+{total_off}")
                }
            } else {
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&root_name) else {
                    panic!("struct local `{root_name}` not stack-resident");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                bp_addr(off)
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr {load_addr}\r\n");
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            if field_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // `make().a` — Member access on a function-call result. For a
        // struct that fits in 4 bytes, the callee returns it in DX:AX
        // (AX = low half = first field, DX = high half = second
        // field). After the call, the requested field is already in
        // AX or DX. Fixtures 2629, 2634.
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::Call { name: fname, args } = &base.kind
            && args.is_empty()
            && let Some(ret_ty) = self.signatures.ret_ty_of(fname)
            && let Type::Struct { fields, size, .. } = ret_ty
            && *size <= 4
            && let Some(field_info) = fields.iter().find(|f| f.name == field)
        {
            let _ = write!(self.out, "\tcall\tnear ptr _{fname}\r\n");
            // 2B struct: field at offset 0 already in AX. 4B struct:
            // offset 0 in AX, offset 2 in DX.
            if field_info.offset == 2 {
                self.out.extend_from_slice(b"\tmov\tax,dx\r\n");
            }
            return;
        }
        // `<reg_ptr>-><field>-><inner>` — chained arrow access. Load
        // the base pointer's field (another pointer) into BX, then
        // read the inner field through BX. Two-step indirection;
        // works whether the base is a stack-local pointer or a
        // function parameter. Fixture 2816 (`o->p->v`).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::Member { base: inner_base, field: inner_field, kind: crate::ast::MemberKind::Arrow } = &base.kind
            && let ExprKind::Ident(root_name) = &inner_base.kind
            && (self.locals.has(root_name) || self.globals.contains(root_name))
        {
            let root_ty = if self.locals.has(root_name) {
                self.locals.type_of(root_name).clone()
            } else {
                self.globals.type_of(root_name).unwrap().clone()
            };
            if let Some(pointee_struct) = root_ty.pointee()
                && let Some((mid_off, mid_ty)) = (if let Type::Struct { name: Some(tag), .. } = pointee_struct {
                    self.lookup_struct_by_tag(tag).and_then(|t| t.field(inner_field))
                } else {
                    pointee_struct.field(inner_field)
                })
                && let Some(final_pointee) = mid_ty.pointee()
                && let Some((field_off, field_ty)) = (if let Type::Struct { name: Some(tag), .. } = final_pointee {
                    self.lookup_struct_by_tag(tag).and_then(|t| t.field(field))
                } else {
                    final_pointee.field(field)
                })
            {
                // First indirection: load the intermediate pointer
                // (the field at mid_off through root). For register-
                // resident root pointers BCC uses the root's reg
                // directly as the base (e.g. `mov bx, [si]` when o is
                // in SI), skipping a `mov bx, si` copy.
                if self.locals.has(root_name)
                    && let LocalLocation::Reg(reg) = self.locals.location_of(root_name)
                {
                    let bx_src = if mid_off == 0 {
                        format!("[{}]", reg.name())
                    } else {
                        format!("[{}+{mid_off}]", reg.name())
                    };
                    let _ = write!(self.out, "\tmov\tbx,word ptr {bx_src}\r\n");
                } else {
                    // Stack-resident root, or global pointer. Load
                    // root into BX first, then perform the +mid_off
                    // indirection.
                    if self.locals.has(root_name)
                        && let LocalLocation::Stack(off) = self.locals.location_of(root_name)
                    {
                        let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                    } else {
                        let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{root_name}\r\n");
                    }
                    let bx1 = if mid_off == 0 { "[bx]".to_owned() } else { format!("[bx+{mid_off}]") };
                    let _ = write!(self.out, "\tmov\tbx,word ptr {bx1}\r\n");
                }
                // Final read at [bx+field_off].
                let bx2 = if field_off == 0 { "[bx]".to_owned() } else { format!("[bx+{field_off}]") };
                if field_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {bx2}\r\n");
                    self.emit_widen_al(&field_ty);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {bx2}\r\n");
                }
                return;
            }
        }
        // `<global_struct_array>[<var>].<field>` — Dot access on a
        // variable-indexed global struct array. Compute the scaled
        // element offset into BX, then load through `[bx +
        // <arr_sym> + field_off]`. Fixture 2841. Skip when the
        // name is also a local in scope (another function's static
        // local shadowed by a parameter — fixture 2208's `pts`).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::ArrayIndex { array: arr_expr, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &arr_expr.kind
            && !self.locals.has(arr_name)
            && let Some(arr_ty) = self.globals.type_of(arr_name)
            && let Some(elem_ty) = arr_ty.array_elem()
            && let Type::Struct { fields, .. } = elem_ty.clone()
            && let Some(field_info) = fields.iter().find(|f| f.name == field)
            && try_const_eval(index).is_none()
        {
            let field_off = field_info.offset;
            let field_ty = field_info.ty.clone();
            self.emit_index_into_bx(index, elem_ty);
            let addr = if field_off == 0 {
                format!("DGROUP:_{arr_name}[bx]")
            } else {
                format!("DGROUP:_{arr_name}+{field_off}[bx]")
            };
            if field_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
            return;
        }
        // `(*<ptr_to_ptr>)-><field>` rvalue: deref the outer ptr
        // through BX, then load the field through that BX. Fixture
        // 2815 (`int extract(struct P **pp) { return (*pp)->x; }`).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::Deref(inner) = &base.kind
            && let ExprKind::Ident(pp_name) = &inner.kind
            && self.locals.has(pp_name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(pp_name)
            && let Some(p_ty) = self.locals.type_of(pp_name).pointee()
            && let Some(struct_ty) = p_ty.pointee()
            && let Some((field_off, field_ty)) = struct_ty.field(field)
        {
            let _ = write!(self.out, "\tmov\tbx,word ptr [{}]\r\n", reg.name());
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            if field_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // `<local-ptr-to-struct>[<var-idx>].<field>` rvalue —
        // `pts[i].x` where `pts` is a stack/reg local pointing at
        // a struct. Scale the index by sizeof(struct), add to the
        // pointer (load via BX), then `[bx+field_off]`. Fixture
        // 2208 (`pts[i].x + pts[i].y` for `struct P *pts` param).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::ArrayIndex { array, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let Some(pointee) = self.locals.type_of(arr_name).pointee().cloned()
            && let Some((field_off, field_ty)) = pointee.field(field)
            && try_const_eval(index).is_none()
        {
            // For a reg-resident pointer (typical when the
            // 1-pointer CX extension fires — fixture 2208's `pts`
            // in CX), BCC's pattern is `mov ax, idx; shl ax, log2;
            // mov bx, ptr; add bx, ax; mov ax, [bx+off]`. The AX
            // scratch then gets overwritten by the load. For a
            // stack-resident pointer we keep the existing
            // BX-direct shape (`mov bx, idx; shl bx; add bx,
            // word ptr [bp+N]`) since the load-from-memory step
            // can't be split into a register-add for free.
            let elem_stride = pointee.size_bytes();
            let log2 = match elem_stride {
                2 => 1, 4 => 2, 8 => 3, 16 => 4, _ => 0,
            };
            let pow2 = log2 > 0 && (1u16 << log2) == elem_stride;
            let used_ax_scratch = if pow2
                && let LocalLocation::Reg(ptr_reg) = self.locals.location_of(arr_name)
                && let ExprKind::Ident(idx_name) = &index.kind
                && self.locals.has(idx_name)
                && let LocalLocation::Reg(idx_reg) = self.locals.location_of(idx_name)
                && !idx_reg.is_byte()
            {
                let _ = write!(self.out, "\tmov\tax,{}\r\n", idx_reg.name());
                for _ in 0..log2 {
                    self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tbx,{}\r\n", ptr_reg.name());
                self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
                true
            } else {
                false
            };
            if !used_ax_scratch {
                self.emit_index_into_bx(index, &pointee);
                match self.locals.location_of(arr_name) {
                    LocalLocation::Reg(reg) => {
                        let _ = write!(self.out, "\tadd\tbx,{}\r\n", reg.name());
                    }
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tadd\tbx,word ptr {}\r\n", bp_addr(off));
                    }
                }
            }
            let _ = elem_stride;
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            let field_ty_clone = field_ty.clone();
            if field_ty_clone.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty_clone);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // `(*<ptr>).<field>` — semantically identical to
        // `<ptr>-><field>`. Rewrite by unwrapping the Deref so the
        // Ident arms below pick it up. Fixture 2960
        // (`int extract(struct P *p) { return (*p).x; }`).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::Deref(inner) = &base.kind
            && matches!(inner.kind, ExprKind::Ident(_))
        {
            return self.emit_member_to_ax(
                inner,
                field,
                crate::ast::MemberKind::Arrow,
            );
        }
        // `(&<ident>)-><field>` — `&ident` is just the address of
        // the ident; combined with `->` it's `<ident>.<field>`.
        // Rewrite as a Dot access. Fixture 3561 (`(&s)->y` for
        // global struct s).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::AddressOf(name) = &base.kind
        {
            let synth_ident = Expr {
                kind: ExprKind::Ident(name.clone()),
                span: base.span,
            };
            return self.emit_member_to_ax(
                &synth_ident,
                field,
                crate::ast::MemberKind::Dot,
            );
        }
        // `<global-struct-arr>[<var-idx>].<field>` read — symmetric
        // to the member-assign path. Compute scaled index into BX,
        // then read through `<sym>+field_off[bx]`. Fixtures 1821,
        // 2438 (`a[i].x` for array of struct).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::ArrayIndex { array, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && let Some(gty) = self.globals.type_of(arr_name)
            && let Some(elem_ty) = gty.array_elem()
            && let Some((field_off, field_ty)) = elem_ty.field(field)
        {
            let elem_ty_clone = elem_ty.clone();
            let field_ty_clone = field_ty.clone();
            self.emit_index_into_bx(index, &elem_ty_clone);
            let addr = if field_off == 0 {
                format!("DGROUP:_{arr_name}[bx]")
            } else {
                format!("DGROUP:_{arr_name}+{field_off}[bx]")
            };
            if field_ty_clone.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                self.emit_widen_al(&field_ty_clone);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
            return;
        }
        // `<stack-struct-arr>[<var-idx>].<field>` read — same shape
        // but the array base is bp-relative. Compute &arr[i] into
        // BX via `emit_array_addr_to_bx`, then read at
        // `[bx+field_off]`. Fixture 2438 (stack array of struct).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::ArrayIndex { array, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let arr_ty = self.locals.type_of(arr_name).clone()
            && let Some(elem_ty) = arr_ty.array_elem()
            && let Some((field_off, field_ty)) = elem_ty.field(field)
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
        {
            let elem_size = elem_ty.size_bytes();
            let field_ty_clone = field_ty.clone();
            self.emit_array_addr_to_bx(arr_name, index, base_off, elem_size);
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            if field_ty_clone.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty_clone);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // `(<ptr> +/- K)-><field>` — fold the constant offset and
        // field offset together into the pointer's reg displacement.
        // Pointer in SI/DI: emit `mov ax, [reg + byte_off]` directly.
        // Pointer on stack: `mov bx, [bp+N]; mov ax, [bx + byte_off]`.
        // Fixture 3251 (`(p - 1)->x`).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::BinOp { op: bop, left, right } = &base.kind
            && (matches!(bop, BinOp::Add) || matches!(bop, BinOp::Sub))
            && let ExprKind::Ident(p_name) = &left.kind
            && let Some(pointee) = self.ident_pointee(p_name)
            && let Some((field_off, field_ty)) = pointee.field(field)
            && let Some(k) = try_const_eval(right)
        {
            let stride = i32::from(pointee.size_bytes());
            let sign = if matches!(bop, BinOp::Add) { 1i32 } else { -1 };
            let byte_off = sign.wrapping_mul(k as i32).wrapping_mul(stride)
                .wrapping_add(i32::from(field_off));
            let field_ty_clone = field_ty.clone();
            let reg_name = match self.locals.location_of(p_name) {
                LocalLocation::Reg(reg) if !reg.is_byte() => reg.name().to_owned(),
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                    "bx".to_owned()
                }
                _ => {
                    panic!("byte-reg pointer in `(p±K)->field` not supported");
                }
            };
            let addr = if byte_off == 0 {
                format!("[{reg_name}]")
            } else if byte_off > 0 {
                format!("[{reg_name}+{byte_off}]")
            } else {
                format!("[{reg_name}{byte_off}]")
            };
            if field_ty_clone.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                self.emit_widen_al(&field_ty_clone);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
            return;
        }
        // `<global-ptr-arr>[<var-idx>]-><field>` read — arr is an
        // array of pointers, arr[i] is a pointer, arrow loads field
        // through that pointer. Compute scaled index → BX, load
        // pointer through `[<sym>+bx]`, then load field through
        // [bx+field_off]. Fixture 3541 (`arr[i]->v`).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::ArrayIndex { array, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && let Some(gty) = self.globals.type_of(arr_name)
            && let Some(elem_ty) = gty.array_elem()
            && let Some(pointee) = elem_ty.pointee()
            && let Some((field_off, field_ty)) = pointee.field(field)
        {
            let elem_ty_clone = elem_ty.clone();
            let field_ty_clone = field_ty.clone();
            self.emit_index_into_bx(index, &elem_ty_clone);
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{arr_name}[bx]\r\n");
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            if field_ty_clone.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty_clone);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // `<ptr>->...-><field>` rvalue — chain of Arrows with optional
        // Dot offsets between. Each Arrow corresponds to a deref step.
        // Walk down to the Ident root, accumulating per-step offsets,
        // then emit `mov bx, <root>; mov bx, [bx+...]; ...; mov ax,
        // [bx+leaf]`. Fixture 3448 (`o->m.p->c`).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let Some((root_name, mut steps, leaf_off, field_ty)) =
                multi_arrow_chain(base, field, |n| self.ident_pointee(n))
        {
            let field_ty_clone = field_ty.clone();
            // The first deref step can read straight through the
            // root's register-resident pointer (e.g. `mov bx,
            // [si+off]`), saving an explicit `mov bx, <reg>`. For a
            // stack-resident pointer we still need to land the
            // pointer in BX first since there's no `[<bp+disp>+off]`
            // double-disp form usable here.
            let mut first_step = true;
            for step_off in steps.drain(..) {
                if first_step {
                    first_step = false;
                    let addr_reg = match self.locals.location_of(&root_name) {
                        LocalLocation::Reg(reg) => reg.name().to_owned(),
                        LocalLocation::Stack(off) => {
                            let _ = write!(
                                self.out,
                                "\tmov\tbx,word ptr {}\r\n",
                                bp_addr(off),
                            );
                            "bx".to_owned()
                        }
                    };
                    let addr = if step_off == 0 {
                        format!("[{addr_reg}]")
                    } else if step_off > 0 {
                        format!("[{addr_reg}+{step_off}]")
                    } else {
                        format!("[{addr_reg}{step_off}]")
                    };
                    let _ = write!(self.out, "\tmov\tbx,word ptr {addr}\r\n");
                    continue;
                }
                let addr = if step_off == 0 {
                    "[bx]".to_owned()
                } else if step_off > 0 {
                    format!("[bx+{step_off}]")
                } else {
                    format!("[bx{step_off}]")
                };
                let _ = write!(self.out, "\tmov\tbx,word ptr {addr}\r\n");
            }
            // If no steps were emitted (root is a direct-deref
            // shape — shouldn't happen for a multi-arrow chain but
            // guard anyway), land the root in BX first.
            if first_step {
                match self.locals.location_of(&root_name) {
                    LocalLocation::Reg(reg) => {
                        let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                    }
                    LocalLocation::Stack(off) => {
                        let _ = write!(
                            self.out,
                            "\tmov\tbx,word ptr {}\r\n",
                            bp_addr(off),
                        );
                    }
                };
            }
            let bx_disp = if leaf_off == 0 {
                "[bx]".to_owned()
            } else if leaf_off > 0 {
                format!("[bx+{leaf_off}]")
            } else {
                format!("[bx{leaf_off}]")
            };
            if field_ty_clone.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty_clone);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        // Ternary base for arrow access: `(c ? &s1 : &s2)->x`.
        // Evaluate the ternary into AX (which yields the pointer),
        // copy to BX, then read through `[bx+field_off]`. Fixture
        // 3558.
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && matches!(base.kind, ExprKind::Ternary { .. })
        {
            // The ternary's branches are address expressions whose
            // pointee type is the struct that holds the field —
            // pull the struct type from the global table by
            // looking at the then-branch.
            if let ExprKind::Ternary { then_value, .. } = &base.kind
                && let ExprKind::AddressOf(then_name) = &then_value.kind
                && let Some(struct_ty) = self.globals.type_of(then_name).cloned()
                && let Some((field_off, field_ty)) = struct_ty.field(field)
            {
                self.emit_expr_to_ax(base);
                self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                let bx_disp = if field_off == 0 {
                    "[bx]".to_owned()
                } else {
                    format!("[bx+{field_off}]")
                };
                let field_ty = field_ty.clone();
                if field_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                    self.emit_widen_al(&field_ty);
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
                }
                return;
            }
        }
        // Arrow path (or Dot whose base isn't a const-chain lvalue):
        // base must be a bare Ident referring to a pointer.
        let ExprKind::Ident(name) = &base.kind else {
            panic!("non-ident base in member access not yet supported (no fixture)");
        };
        // `<global_ptr>-><field>` rvalue: load the pointer into BX,
        // then read through `[bx+field_off]`. Fixture 1429.
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let Some(gty) = self.globals.type_of(name)
            && let Some(pointee) = gty.pointee()
            && let Some((field_off, field_ty)) = pointee.field(field)
        {
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{name}\r\n");
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            if field_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            }
            return;
        }
        let base_ty = self.locals.type_of(name).clone();
        let (field_off, field_ty) = match kind {
            crate::ast::MemberKind::Dot => base_ty.field(field).unwrap_or_else(|| {
                panic!("`{name}.{field}`: no such field in {base_ty:?}")
            }),
            crate::ast::MemberKind::Arrow => {
                let pointee = base_ty
                    .pointee()
                    .unwrap_or_else(|| panic!("`{name}->{field}`: not a pointer type"))
                    .clone();
                pointee.field(field).unwrap_or_else(|| {
                    panic!("`{name}->{field}`: no such field in {pointee:?}")
                })
            }
        };
        let load_byte = field_ty.is_char_like();
        if matches!(kind, crate::ast::MemberKind::Arrow) {
            // `p->x`: p holds the address; field at `[reg + K]`. A stack-resident
            // pointer (`-r-`, or one BCC couldn't promote) is loaded into bx
            // first, then accessed through it. Fixture 4278 (`return p->b;`).
            let reg_name = match self.locals.location_of(name) {
                LocalLocation::Reg(reg) => reg.name(),
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                    "bx"
                }
            };
            let addr = if field_off == 0 {
                format!("[{reg_name}]")
            } else {
                format!("[{reg_name}+{field_off}]")
            };
            if load_byte {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                self.emit_widen_al(&field_ty);
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
        } else {
            // Dot with an unsupported base shape — the chain helper
            // already failed; surface a clear error.
            panic!("non-ident base in `.x` access not yet supported (no fixture for {:?})", base.kind);
        }
    }
    /// `<base>.<field> = <value>;` or `<base>-><field> = <value>;`.
    /// Mirror of `emit_member_to_ax` for the lvalue path.
    pub(crate) fn emit_member_assign(
        &mut self,
        base: &Expr,
        field: &str,
        kind: crate::ast::MemberKind,
        value: &Expr,
    ) {
        // `<member-dot-chain> = <func-name>` — function-pointer
        // field initializer. Same direct-immediate-to-memory form
        // we use for `<fn-ptr-local> = <func>` (line 18074-ish).
        // Saves the AX round-trip. Fixture 1812 (`o.f = dbl`).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::Ident(src_name) = &value.kind
            && !self.locals.has(src_name)
            && self.globals.type_of(src_name).is_none()
            && self.signatures.ret_ty_of(src_name).is_some()
            && let Some((name, total_off, _leaf_ty)) =
                self.try_member_dot_chain(base, field)
        {
            if self.globals.contains(&name) {
                let label = if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                };
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr {label},offset _{src_name}\r\n",
                );
                return;
            }
            if let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) {
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr {},offset _{src_name}\r\n",
                    bp_addr(off),
                );
                return;
            }
        }
        // Bitfield write: detect via the struct's StructField metadata
        // (resolve_bitfield_named handles both within-byte and
        // cross-byte shapes). Currently the `s.<bitfield> = K` shape
        // (Dot, stack-local struct, constant value) is supported.
        // Emits `and <width> ptr <addr>, <preserve>` + (optionally)
        // `or <width> ptr <addr>, <shifted-value>` — width is byte
        // for fields fitting in one byte (fixture 1691), word when
        // the field crosses a byte boundary (fixture 1880).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::Ident(struct_name) = &base.kind
            && let Some(bf) = self.resolve_bitfield_named(struct_name, field)
        {
            if let Some(v) = try_const_eval(value) {
                let field_mask: u32 = (1u32 << bf.bit_width).wrapping_sub(1);
                let preserve_mask: u32 = match bf.access {
                    BitfieldAccess::Byte =>
                        (!((field_mask as u8) << bf.bit_offset)) as u32,
                    BitfieldAccess::Word =>
                        (!((field_mask as u16) << bf.bit_offset)) as u32,
                };
                let v_shifted: u32 = match bf.access {
                    BitfieldAccess::Byte =>
                        (((v as u8) & (field_mask as u8)) << bf.bit_offset) as u32,
                    BitfieldAccess::Word =>
                        (((v as u16) & (field_mask as u16)) << bf.bit_offset) as u32,
                };
                let w = bf.access.ptr();
                // Skip the AND for a 1-bit field assigned to 1 — OR
                // alone sets the bit and there's nothing else to
                // clear (fixture 2105's `fl.f1 = 1`). Wider fields
                // emit both AND + OR even when the value fills the
                // field, matching BCC (fixture 2301's `x.lo = 0x3F`
                // for a 6-bit field still emits the AND).
                let v_masked = match bf.access {
                    BitfieldAccess::Byte => ((v as u8) & (field_mask as u8)) as u32,
                    BitfieldAccess::Word => ((v as u16) & (field_mask as u16)) as u32,
                };
                let one_bit_full = bf.bit_width == 1 && v_masked == 1;
                if !one_bit_full {
                    let _ = write!(
                        self.out,
                        "\tand\t{w} ptr {},{preserve_mask}\r\n",
                        bf.addr,
                    );
                }
                if v_shifted != 0 {
                    let _ = write!(
                        self.out,
                        "\tor\t{w} ptr {},{v_shifted}\r\n",
                        bf.addr,
                    );
                }
                return;
            }
            // Non-constant RHS: materialize into AX, mask to field
            // width, optionally shift to bit position, then AND the
            // storage to clear and OR with AL/AX. Fixture 3322
            // (`s.a = v` where v is a parameter).
            let field_mask: u32 = (1u32 << bf.bit_width).wrapping_sub(1);
            let preserve_mask: u32 = match bf.access {
                BitfieldAccess::Byte =>
                    (!((field_mask as u8) << bf.bit_offset)) as u32,
                BitfieldAccess::Word =>
                    (!((field_mask as u16) << bf.bit_offset)) as u32,
            };
            self.emit_expr_to_ax(value);
            let _ = write!(self.out, "\tand\tax,{field_mask}\r\n");
            if bf.bit_offset >= 4 {
                let _ = write!(self.out, "\tmov\tcl,{}\r\n", bf.bit_offset);
                let _ = write!(self.out, "\tshl\tax,cl\r\n");
            } else {
                for _ in 0..bf.bit_offset {
                    self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                }
            }
            let w = bf.access.ptr();
            let _ = write!(
                self.out,
                "\tand\t{w} ptr {},{preserve_mask}\r\n",
                bf.addr,
            );
            let src_reg = match bf.access {
                BitfieldAccess::Byte => "al",
                BitfieldAccess::Word => "ax",
            };
            let _ = write!(
                self.out,
                "\tor\t{w} ptr {},{src_reg}\r\n",
                bf.addr,
            );
            return;
        }

        // Dot path: try the lvalue-chain helper. Catches `a.x`,
        // `pts[1].x`, nested `a.b.c`, and global `g.x`.
        let (dest, leaf_ty) = if matches!(kind, crate::ast::MemberKind::Dot)
            && let Some((name, total_off, leaf_ty)) = self.try_member_dot_chain(base, field)
        {
            let dest = if self.globals.contains(&name) {
                if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                }
            } else {
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) else {
                    panic!("struct local `{name}` not stack-resident (unexpected)");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                bp_addr(off)
            };
            (dest, leaf_ty)
        } else if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::ArrayIndex { array, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && let Some(gty) = self.globals.type_of(arr_name)
            && let Some(elem_ty) = gty.array_elem()
            && let Some((field_off, field_ty)) = elem_ty.field(field)
        {
            // `<global-struct-arr>[<var-idx>].<field> = <value>` —
            // compute scaled index into BX, then store at
            // `<sym> + field_off [bx]`. Fixture 3240
            // (`arr[i].x = v` for `struct P arr[N]`).
            let elem_ty_clone = elem_ty.clone();
            let field_ty_clone = field_ty.clone();
            // BCC emits the index-into-BX first, then loads value
            // into AX. emit_index_into_bx for an int-typed lvalue
            // index uses `mov bx, <addr>; shl bx, ...` (no AX),
            // so the value load that follows isn't clobbered.
            self.emit_index_into_bx(index, &elem_ty_clone);
            let addr = if field_off == 0 {
                format!("DGROUP:_{arr_name}[bx]")
            } else {
                format!("DGROUP:_{arr_name}+{field_off}[bx]")
            };
            if let Some(v) = try_const_eval(value) {
                let width = if field_ty_clone.is_char_like() { "byte" } else { "word" };
                let v_masked = if field_ty_clone.is_char_like() {
                    v & 0xFF
                } else {
                    v & 0xFFFF
                };
                let _ = write!(self.out, "\tmov\t{width} ptr {addr},{v_masked}\r\n");
                return;
            }
            self.emit_expr_to_ax(value);
            if field_ty_clone.is_char_like() {
                let _ = write!(self.out, "\tmov\tbyte ptr {addr},al\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tword ptr {addr},ax\r\n");
            }
            return;
        } else if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::ArrayIndex { array, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let arr_ty = self.locals.type_of(arr_name).clone()
            && let Some(elem_ty) = arr_ty.array_elem()
            && let Some((field_off, field_ty)) = elem_ty.field(field)
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
            && !elem_ty.size_bytes().is_power_of_two()
            && try_const_eval(index).is_none()
        {
            // `<stack-struct-arr>[<var-idx>].<field> = <value>` for
            // a struct with a non-power-of-2 element stride. BCC's
            // lowering bakes the field offset into the LEA so the
            // store goes through `[bx]` with no displacement:
            //   mov ax, <i>; mov dx, stride; imul dx;
            //   lea dx, [bp + arr_base + field_off]
            //   add ax, dx                              ; AX = address
            //   <compute value into DX or use src reg>
            //   mov bx, ax
            //   mov [bx], <src>
            // When the value is a bare register-resident ident the
            // source IS that register and the prelude can fold the
            // final `mov bx, ax` in early (no DX clobber concern).
            // Fixture 1914 (`struct R arr[3];` writes).
            let idx_addr = match self.named_int_lvalue_addr_or_reg(index) {
                Some(s) => s,
                None => panic!("non-trivial index in struct-arr write not supported"),
            };
            let stride = elem_ty.size_bytes();
            let field_ty_clone = field_ty.clone();
            let is_word = !field_ty_clone.is_char_like();
            // Simple-register-source value: emit the prelude with
            // `mov bx, ax` at the end, then store via that
            // register. `arr[i].a = i;` where `i` is in SI.
            if let ExprKind::Ident(v_name) = &value.kind
                && self.locals.has(v_name)
                && let LocalLocation::Reg(v_reg) = self.locals.location_of(v_name)
                && !v_reg.is_byte()
                && is_word
            {
                self.emit_arr_var_field_addr_to_bx(&idx_addr, stride, base_off, field_off);
                let _ = write!(self.out, "\tmov\tword ptr [bx],{}\r\n", v_reg.name());
                return;
            }
            // `<ident_in_reg> + K_const` value: split the prelude
            // so the trailing `mov bx, ax` lands *after* the value
            // compute. `arr[i].b = i + 10;` style.
            if let ExprKind::BinOp { op: BinOp::Add, left: vl, right: vr } = &value.kind
                && let Some(k) = try_const_eval(vr)
                && let ExprKind::Ident(v_name) = &vl.kind
                && self.locals.has(v_name)
                && let LocalLocation::Reg(v_reg) = self.locals.location_of(v_name)
                && !v_reg.is_byte()
                && is_word
            {
                // Prelude without the final mov bx, ax — AX has
                // the address.
                if is_reg16_name(&idx_addr) {
                    let _ = write!(self.out, "\tmov\tax,{idx_addr}\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tax,word ptr {idx_addr}\r\n");
                }
                let _ = write!(self.out, "\tmov\tdx,{stride}\r\n");
                self.out.extend_from_slice(b"\timul\tdx\r\n");
                let lea_off = base_off + i16::try_from(field_off as i32).unwrap_or(i16::MAX);
                let _ = write!(self.out, "\tlea\tdx,word ptr {}\r\n", bp_addr(lea_off));
                self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
                // Value compute into DX.
                let _ = write!(self.out, "\tmov\tdx,{}\r\n", v_reg.name());
                let k_imm = k & 0xFFFF;
                let _ = write!(self.out, "\tadd\tdx,{k_imm}\r\n");
                self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                self.out.extend_from_slice(b"\tmov\tword ptr [bx],dx\r\n");
                return;
            }
            // Constant value: prelude + `mov word ptr [bx], imm`.
            if let Some(v) = try_const_eval(value) {
                self.emit_arr_var_field_addr_to_bx(&idx_addr, stride, base_off, field_off);
                let v_masked = if is_word { v & 0xFFFF } else { v & 0xFF };
                let width = if is_word { "word" } else { "byte" };
                let _ = write!(self.out, "\tmov\t{width} ptr [bx],{v_masked}\r\n");
                return;
            }
            panic!(
                "non-trivial value in non-pow2 struct-arr write not yet supported (no fixture)"
            );
        } else if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::ArrayIndex { array, index } = &base.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let arr_ty = self.locals.type_of(arr_name).clone()
            && let Some(elem_ty) = arr_ty.array_elem()
            && let Some((field_off, field_ty)) = elem_ty.field(field)
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
        {
            // `<stack-struct-arr>[<var-idx>].<field> = <value>` —
            // compute &arr[i] via emit_array_addr_to_bx, then write
            // at `[bx+field_off]`. Fixture 1914 (struct R arr[3]
            // with 3-int struct, 6-byte stride).
            let elem_size = elem_ty.size_bytes();
            let field_ty_clone = field_ty.clone();
            // Emit value into AX first (the BX-setup may clobber AX).
            if let Some(v) = try_const_eval(value) {
                self.emit_array_addr_to_bx(arr_name, index, base_off, elem_size);
                let bx_disp = if field_off == 0 {
                    "[bx]".to_owned()
                } else {
                    format!("[bx+{field_off}]")
                };
                let width = if field_ty_clone.is_char_like() { "byte" } else { "word" };
                let v_masked = if field_ty_clone.is_char_like() {
                    v & 0xFF
                } else {
                    v & 0xFFFF
                };
                let _ = write!(self.out, "\tmov\t{width} ptr {bx_disp},{v_masked}\r\n");
                return;
            }
            self.emit_expr_to_ax(value);
            self.emit_array_addr_to_bx(arr_name, index, base_off, elem_size);
            let bx_disp = if field_off == 0 {
                "[bx]".to_owned()
            } else {
                format!("[bx+{field_off}]")
            };
            if field_ty_clone.is_char_like() {
                let _ = write!(self.out, "\tmov\tbyte ptr {bx_disp},al\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tword ptr {bx_disp},ax\r\n");
            }
            return;
        } else if matches!(kind, crate::ast::MemberKind::Dot)
            && let Some((ptr_name, total_off, leaf_ty)) =
                self.try_arrow_chain_addr(base, field)
        {
            // `<ptr>-><arrow_field>.<dot_field>[.<more>...] = <value>`
            // — a Dot chain rooted at a Member-Arrow through a named
            // pointer. The arrow's runtime load happens once; the
            // accumulated field offsets become a single ModR/M
            // displacement off the pointer register. Fixture 3693
            // (`o->i.x = v` for `struct Outer { struct Inner i; }`).
            let LocalLocation::Reg(reg) = self.locals.location_of(&ptr_name) else {
                panic!(
                    "stack-resident pointer in `p->i.x = …` not yet supported (no fixture)"
                );
            };
            let r = reg.name();
            let addr = if total_off == 0 {
                format!("[{r}]")
            } else {
                format!("[{r}+{total_off}]")
            };
            if let Some(v) = try_const_eval(value) {
                let width = if leaf_ty.is_char_like() { "byte" } else { "word" };
                let v_masked = if leaf_ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                let _ = write!(self.out, "\tmov\t{width} ptr {addr},{v_masked}\r\n");
                return;
            }
            self.emit_expr_to_ax(value);
            if leaf_ty.is_char_like() {
                let _ = write!(self.out, "\tmov\tbyte ptr {addr},al\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tword ptr {addr},ax\r\n");
            }
            return;
        } else {
            // Arrow (or a Dot whose base isn't a const-chain lvalue).
            let ExprKind::Ident(name) = &base.kind else {
                panic!("non-ident base in member assign not yet supported (no fixture)");
            };
            // `<global_ptr>-><field> = …`: load the global pointer
            // into BX, then write through `[bx+field_off]`. Fixture
            // 1429.
            if matches!(kind, crate::ast::MemberKind::Arrow)
                && let Some(gty) = self.globals.type_of(name)
                && let Some(pointee) = gty.pointee()
                && let Some((field_off, field_ty)) = pointee.field(field)
            {
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{name}\r\n");
                let bx_disp = if field_off == 0 {
                    "[bx]".to_owned()
                } else {
                    format!("[bx+{field_off}]")
                };
                if let Some(v) = try_const_eval(value) {
                    if field_ty.is_char_like() {
                        let v8 = v & 0xFF;
                        let _ = write!(self.out, "\tmov\tbyte ptr {bx_disp},{v8}\r\n");
                    } else {
                        let v16 = v & 0xFFFF;
                        let _ = write!(self.out, "\tmov\tword ptr {bx_disp},{v16}\r\n");
                    }
                    return;
                }
                self.emit_expr_to_ax(value);
                if field_ty.is_char_like() {
                    let _ = write!(self.out, "\tmov\tbyte ptr {bx_disp},al\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tword ptr {bx_disp},ax\r\n");
                }
                return;
            }
            let base_ty = self.locals.type_of(name).clone();
            let (field_off, field_ty) = match kind {
                crate::ast::MemberKind::Dot => base_ty.field(field).unwrap_or_else(|| {
                    panic!("`{name}.{field} = …`: no such field in {base_ty:?}")
                }),
                crate::ast::MemberKind::Arrow => {
                    let pointee = base_ty
                        .pointee()
                        .unwrap_or_else(|| panic!("`{name}->{field} = …`: not a pointer"))
                        .clone();
                    pointee.field(field).unwrap_or_else(|| {
                        panic!("`{name}->{field} = …`: no such field in {pointee:?}")
                    })
                }
            };
            let dest = match kind {
                crate::ast::MemberKind::Dot => {
                    let LocalLocation::Stack(struct_off) = self.locals.location_of(name) else {
                        panic!("struct local `{name}` not stack-resident (unexpected)");
                    };
                    let off = struct_off + i16::try_from(field_off).unwrap_or(i16::MAX);
                    bp_addr(off)
                }
                crate::ast::MemberKind::Arrow => {
                    // A stack-resident struct pointer is loaded into bx first,
                    // then the field is stored through it. Fixture 4279
                    // (`p->b = v;`).
                    let reg_name = match self.locals.location_of(name) {
                        LocalLocation::Reg(reg) => reg.name(),
                        LocalLocation::Stack(off) => {
                            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                            "bx"
                        }
                    };
                    if field_off == 0 {
                        format!("[{reg_name}]")
                    } else {
                        format!("[{reg_name}+{field_off}]")
                    }
                }
            };
            (dest, field_ty)
        };
        // Long-field store: emit two `mov word ptr <addr>, <half>`
        // instructions (high first, then low). Works for both `s.x`
        // (DGROUP-relative or bp-relative dest) and `p->x` (register-
        // indirect dest). Fixtures 316, 317, 318.
        if leaf_ty.is_long_like() {
            if let Some(v) = try_const_eval(value) {
                let lo = (v & 0xFFFF) as u16;
                let hi = ((v >> 16) & 0xFFFF) as u16;
                let hi_dest = shift_dest_by_two(&dest);
                let _ = write!(self.out, "\tmov\tword ptr {hi_dest},{hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest},{lo}\r\n");
                return;
            }
            // Non-constant RHS (e.g. `s.x = g + h`): route through
            // the long-value-to-dest helper. Fixture 358.
            let hi_dest = shift_dest_by_two(&dest);
            if self.try_emit_long_value_to_dest(value, &hi_dest, &dest) {
                return;
            }
            panic!("non-constant rhs in long struct field assign not yet supported (no fixture)");
        }
        let store_byte = leaf_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        if let Some(v) = try_const_eval(value) {
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr {dest},{v_masked}\r\n");
            return;
        }
        // RHS is `&<global>` — emit the direct immediate-store
        // form `mov word ptr <dest>,offset DGROUP:_<src>` (uses
        // the same two-FIXUPP encoding `MovGroupSymOffsetGroupSym`).
        // Fixture 494 (`head.next = &head`).
        if !store_byte
            && let ExprKind::AddressOf(src) = &value.kind
            && self.globals.contains(src)
        {
            let _ = write!(
                self.out,
                "\tmov\t{width} ptr {dest},offset DGROUP:_{src}\r\n",
            );
            return;
        }
        // RHS is a string literal — emit the direct immediate-store
        // form `mov word ptr <dest>, offset DGROUP:s@[+offset]`.
        // Fixture 2420 (`m.name = "hello"` for char* field).
        if !store_byte
            && let ExprKind::StringLit(bytes) = &value.kind
        {
            let offset = self
                .strings
                .offset_for_span(value.span.start)
                .unwrap_or_else(|| self.strings.intern(bytes));
            if offset == 0 {
                let _ = write!(
                    self.out,
                    "\tmov\t{width} ptr {dest},offset DGROUP:s@\r\n",
                );
            } else {
                let _ = write!(
                    self.out,
                    "\tmov\t{width} ptr {dest},offset DGROUP:s@+{offset}\r\n",
                );
            }
            return;
        }
        // Register-resident RHS for an int field: store the
        // register direct, skipping the AX round-trip. Fixture 3560
        // (`r.a = v` with v in SI → `mov [bp-N], si`).
        if !store_byte
            && let ExprKind::Ident(rhs_name) = &value.kind
            && self.locals.has(rhs_name)
            && let LocalLocation::Reg(rhs_reg) = self.locals.location_of(rhs_name)
            && !rhs_reg.is_byte()
        {
            let _ = write!(
                self.out,
                "\tmov\tword ptr {dest},{}\r\n",
                rhs_reg.name(),
            );
            return;
        }
        // Non-constant RHS for an int field: materialize into AX,
        // then store AX to the field. Fixture 990 (`s.x = v;` with
        // v a stack local).
        if !store_byte {
            self.emit_expr_to_ax(value);
            let _ = write!(self.out, "\tmov\tword ptr {dest},ax\r\n");
            return;
        }
        // Non-constant RHS for a char field. Two shapes:
        //  - rhs is a char lvalue: `mov al, byte ptr <rhs>;
        //    mov byte ptr <dest>, al` (no widen).
        //  - rhs is anything else (int expr → AX): take AL low byte.
        // Fixture 3178 (`t.c = b` for char b).
        if let Some(rhs_byte) = self.rhs_byte_addr(&value.kind) {
            let _ = write!(self.out, "\tmov\tal,{rhs_byte}\r\n");
            let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
            return;
        }
        self.emit_expr_to_ax(value);
        let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
    }
    /// `<base>.<field>[<i>] = <value>;` — write to an array element inside a
    /// struct field. With all-constant indices we fold the field offset and
    /// each index into a single byte displacement off the struct base, then
    /// emit one `mov <width> ptr <dest>, <imm>`. Fixture 497.
    pub(crate) fn emit_member_array_assign(
        &mut self,
        lvalue: &Expr,
        indices: &[Expr],
        value: &Expr,
    ) {
        // Fold the `.`-chain (`b.data`, `o.in.vals`, …) to the root
        // ident, the accumulated byte offset of the array field, and
        // the array's type. Then apply the constant subscripts.
        let (base, field_off_i32, field_ty) = self
            .try_lvalue_chain_addr(lvalue)
            .unwrap_or_else(|| panic!("unsupported struct-field array lvalue: {lvalue:?}"));
        // Walk through array dimensions matching the index count.
        let mut elem_ty = field_ty;
        let mut total_off = field_off_i32 as u32;
        for ix in indices {
            let Type::Array { elem, .. } = elem_ty else {
                panic!("`{base}` indexed but not array");
            };
            let stride = u32::from(elem.size_bytes());
            let k = try_const_eval(ix)
                .unwrap_or_else(|| panic!("variable struct-field array index not supported"));
            total_off = total_off.checked_add((k as u32).wrapping_mul(stride)).unwrap();
            elem_ty = *elem;
        }
        let base = base.as_str();
        let dest = if self.globals.contains(base) {
            if total_off == 0 {
                format!("DGROUP:_{base}")
            } else {
                format!("DGROUP:_{base}+{total_off}")
            }
        } else {
            let LocalLocation::Stack(struct_off) = self.locals.location_of(base) else {
                panic!("struct local `{base}` not stack-resident");
            };
            let off = struct_off + i16::try_from(total_off).unwrap_or(i16::MAX);
            bp_addr(off)
        };
        let store_byte = elem_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        if let Some(v) = try_const_eval(value) {
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(self.out, "\tmov\t{width} ptr {dest},{v_masked}\r\n");
            return;
        }
        panic!("non-constant rhs in struct-field array assign not yet supported (no fixture)");
    }
    pub(crate) fn emit_member_compound_assign(
        &mut self,
        base: &Expr,
        field: &str,
        kind: crate::ast::MemberKind,
        op: BinOp,
        value: &Expr,
        from_postfix: bool,
    ) {
        // Bitfield compound-assign: read field into AX (masked),
        // apply the binop with the RHS (typically `inc ax` for ++),
        // re-mask to clear overflow above bit_width, then AND/OR
        // the storage to write it back. Fixture 3445 (`f.a++` on
        // a global struct bitfield).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let ExprKind::Ident(struct_name) = &base.kind
            && let Some(bf) = self.resolve_bitfield_named(struct_name, field)
        {
            let field_mask: u32 = (1u32 << bf.bit_width).wrapping_sub(1);
            // Load + mask into AX. Same shape as a read but the
            // existing helper does the right thing — call it for
            // the masked-but-not-yet-shifted-back value.
            self.emit_bitfield_read_to_reg(&bf, "ax", "al");
            // Apply the op. The ++ / -- postfix path always uses
            // a IntLit(1) RHS; emit a single inc/dec to match
            // BCC's shape. Other ops fall back to emit_expr_to_ax-
            // style materialization through DX — no fixture today.
            let _ = from_postfix; // future: signal pre-vs-post semantics
            if matches!(op, BinOp::Add | BinOp::Sub) && try_const_eval(value) == Some(1) {
                if matches!(op, BinOp::Add) {
                    self.out.extend_from_slice(b"\tinc\tax\r\n");
                } else {
                    self.out.extend_from_slice(b"\tdec\tax\r\n");
                }
            } else {
                panic!("bitfield compound-assign op {op:?} not yet supported");
            }
            // Re-mask to discard overflow past bit_width.
            let _ = write!(self.out, "\tand\tax,{field_mask}\r\n");
            // Shift to bit position.
            if bf.bit_offset >= 4 {
                let _ = write!(self.out, "\tmov\tcl,{}\r\n", bf.bit_offset);
                let _ = write!(self.out, "\tshl\tax,cl\r\n");
            } else {
                for _ in 0..bf.bit_offset {
                    self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                }
            }
            // Clear-and-OR write-back. Same shape as the
            // non-constant write case in emit_member_assign.
            let preserve_mask: u32 = match bf.access {
                BitfieldAccess::Byte =>
                    (!((field_mask as u8) << bf.bit_offset)) as u32,
                BitfieldAccess::Word =>
                    (!((field_mask as u16) << bf.bit_offset)) as u32,
            };
            let w = bf.access.ptr();
            let src_reg = match bf.access {
                BitfieldAccess::Byte => "al",
                BitfieldAccess::Word => "ax",
            };
            let _ = write!(
                self.out,
                "\tand\t{w} ptr {},{preserve_mask}\r\n",
                bf.addr,
            );
            let _ = write!(
                self.out,
                "\tor\t{w} ptr {},{src_reg}\r\n",
                bf.addr,
            );
            return;
        }
        // Long-field path. Resolve the dot/arrow chain to a (lo_addr,
        // hi_addr) pair (struct field at its in-struct offset), then
        // emit the long-compound shape — same skeleton as the long-
        // global compound (fixtures 251/253/339) but with the field's
        // formatted address. Fixtures 389 (`s.x += K`), 390
        // (`s.x &= K` — bitwise uses imm16 even when K fits i8sx),
        // 391 (`s.x += y` — variable RHS).
        if matches!(kind, crate::ast::MemberKind::Dot)
            && let Some((src, total_off, leaf_ty)) = self.try_member_dot_chain(base, field)
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
            self.emit_long_compound_to_mem(
                &lo_addr,
                &hi_addr,
                op,
                value,
                leaf_ty.is_unsigned(),
            );
            return;
        }
        // Arrow access (`p->x op= …`) where `p` is a register-resident
        // pointer to a struct and `x` is a long field. The address
        // pair is `[reg+off]` / `[reg+off+2]` — same skeleton as the
        // other long-compound-to-memory destinations, just with
        // register-base addressing. Fixture 399 (`p->x += K` for the
        // first field, offset 0 → `[si]`/`[si+2]`).
        if matches!(kind, crate::ast::MemberKind::Arrow)
            && let ExprKind::Ident(p_name) = &base.kind
            && self.locals.has(p_name)
            && let Some(pointee) = self.locals.type_of(p_name).pointee()
            && let Some((field_off, field_ty)) = pointee.clone().field(field)
            && field_ty.is_long_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
        {
            let r = reg.name();
            let (lo_addr, hi_addr) = if field_off == 0 {
                (format!("[{r}]"), format!("[{r}+2]"))
            } else {
                (
                    format!("[{r}+{field_off}]"),
                    format!("[{r}+{}]", field_off + 2),
                )
            };
            self.emit_long_compound_to_mem(
                &lo_addr,
                &hi_addr,
                op,
                value,
                field_ty.is_unsigned(),
            );
            return;
        }
        // Int-width Dot path: try the lvalue-chain helper so we
        // handle globals (`s.x <op>= …` for global struct `s`) the
        // same way `emit_member_assign` does. Fixture 444
        // (`s.x &= 0xFF` for global `s`).
        let (dest, field_ty) = if matches!(kind, crate::ast::MemberKind::Dot)
            && let Some((name, total_off, leaf_ty)) = self.try_member_dot_chain(base, field)
        {
            let dest = if self.globals.contains(&name) {
                if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                }
            } else {
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) else {
                    panic!("struct local `{name}` not stack-resident (unexpected)");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                bp_addr(off)
            };
            (dest, leaf_ty)
        } else {
            let ExprKind::Ident(name) = &base.kind else {
                panic!("non-ident base in member compound assign not yet supported (no fixture)");
            };
            let base_ty = self.locals.type_of(name).clone();
            let (field_off, field_ty) = match kind {
                crate::ast::MemberKind::Dot => base_ty.field(field).unwrap_or_else(|| {
                    panic!("`{name}.{field} <op>= …`: no such field in {base_ty:?}")
                }),
                crate::ast::MemberKind::Arrow => {
                    let pointee = base_ty
                        .pointee()
                        .unwrap_or_else(|| panic!("`{name}->{field} <op>= …`: not a pointer"))
                        .clone();
                    pointee.field(field).unwrap_or_else(|| {
                        panic!("`{name}->{field} <op>= …`: no such field in {pointee:?}")
                    })
                }
            };
            let dest = match kind {
                crate::ast::MemberKind::Dot => {
                    let LocalLocation::Stack(struct_off) = self.locals.location_of(name) else {
                        panic!("struct local `{name}` not stack-resident (unexpected)");
                    };
                    let off = struct_off + i16::try_from(field_off).unwrap_or(i16::MAX);
                    bp_addr(off)
                }
                crate::ast::MemberKind::Arrow => {
                    let LocalLocation::Reg(reg) = self.locals.location_of(name) else {
                        panic!(
                            "stack-resident pointer in `p->x <op>= …` not yet supported (no fixture)"
                        );
                    };
                    if field_off == 0 {
                        format!("[{}]", reg.name())
                    } else {
                        format!("[{}+{field_off}]", reg.name())
                    }
                }
            };
            (dest, field_ty)
        };
        let store_byte = field_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        // Char-field compound with char-typed variable RHS —
        // mirrors the char-global var-RHS pattern (batch 121):
        // load RHS into AL, then memory-direct `<op> byte ptr
        // <dest>, al`. The `dest` already includes any non-zero
        // field offset. Fixture 708 (`g.c += d`).
        if store_byte
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && try_const_eval(value).is_none()
            && let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Char | Type::UChar)
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
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest},al\r\n");
            return;
        }
        // Char-field compound with int-typed variable RHS (gets
        // truncated to byte). Same op-family asymmetry as char-
        // array (fixtures 847/850): arith goes through AL,
        // bitwise stays memory-direct. Fixture 848.
        if store_byte
            && matches!(op, BinOp::Add | BinOp::Sub)
            && try_const_eval(value).is_none()
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
            let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
            let _ = write!(self.out, "\t{mnem}\tal,{rhs_byte}\r\n");
            let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
            return;
        }
        if store_byte
            && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && try_const_eval(value).is_none()
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let mnem = match op {
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tal,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest},al\r\n");
            return;
        }
        // Int-field compound with non-constant RHS — load RHS
        // into AX, then memory-direct `<op> word ptr <dest>, ax`.
        // emit_expr_to_ax handles int/char/uchar local/global
        // widening (cbw or `mov ah, 0` as appropriate). Fixture
        // 832 (`s.x += y`).
        if !store_byte
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && try_const_eval(value).is_none()
            && matches!(field_ty, Type::Int | Type::UInt)
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
            let _ = write!(self.out, "\t{mnem}\tword ptr {dest},ax\r\n");
            return;
        }
        // Int-field compound `<<=` / `>>=` with non-constant RHS
        // — `mov cl, byte ptr <rhs>; shl word ptr <dest>, cl`.
        // `dest` already includes any field offset. Fixture 835
        // (`s.x <<= y`).
        if !store_byte
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && try_const_eval(value).is_none()
            && matches!(field_ty, Type::Int | Type::UInt)
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
        {
            let unsigned = field_ty.is_unsigned();
            let mnem = match (op, unsigned) {
                (BinOp::Shl, _) => "shl",
                (BinOp::Shr, false) => "sar",
                (BinOp::Shr, true) => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tcl,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tword ptr {dest},cl\r\n");
            return;
        }
        // Int-field compound `*=` / `/=` / `%=` with non-constant
        // local RHS — load LHS into AX, then `imul`/`idiv` against
        // the RHS in `[bp+N]`. Mirrors the int-global path
        // (fixtures 802, 803). Fixture 834 (`s.x *= y`).
        if !store_byte
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && try_const_eval(value).is_none()
            && matches!(field_ty, Type::Int | Type::UInt)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                panic!("non-stack RHS in member compound Mul/Div not yet supported (no fixture)");
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {dest}\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr {}\r\n", bp_addr(off));
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tidiv\tword ptr {}\r\n", bp_addr(off));
            }
            let result_reg = if matches!(op, BinOp::Mul | BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(self.out, "\tmov\tword ptr {dest},{result_reg}\r\n");
            return;
        }
        let Some(v) = try_const_eval(value) else {
            panic!("non-constant rhs in member compound assign not yet supported (no fixture)");
        };
        let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
        // Char-field arith (`+=` / `-=`) follows the BCC byte-arith-
        // through-AL pattern (same as plain char-global, batch 122):
        // `mov al, byte ptr <dest>; add al, K; mov byte ptr <dest>,
        // al`. BCC canonicalizes `-=` as `add al, (256-K)`. Char-
        // field bitwise (`&=` / `|=` / `^=`) keeps memory-direct.
        // Fixture 704 (`g.c += 5`).
        // Postfix `g.c++` / `g.c--` (discarded): memory-direct
        // `inc|dec byte ptr <dest>` — same pre-vs-post asymmetry as
        // `g++` for char globals. Fixture 716 (`g.c++`).
        if store_byte
            && from_postfix
            && v_masked == 1
            && matches!(op, BinOp::Add | BinOp::Sub)
        {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest}\r\n");
            return;
        }
        if store_byte && matches!(op, BinOp::Add | BinOp::Sub) {
            let imm8 = if matches!(op, BinOp::Add) {
                (v_masked & 0xFF) as u8
            } else {
                ((v_masked & 0xFF) as u8).wrapping_neg()
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
            // K=1 peephole: `inc al` / `dec al` instead of
            // `add al, 1` / `add al, 255`. Same byte count but
            // matches BCC's char-field `++`/`--` lowering
            // (fixture 709 `++g.c` → `inc al`).
            if v_masked == 1 && matches!(op, BinOp::Add) {
                self.out.extend_from_slice(b"\tinc\tal\r\n");
            } else if v_masked == 1 && matches!(op, BinOp::Sub) {
                self.out.extend_from_slice(b"\tdec\tal\r\n");
            } else {
                let _ = write!(self.out, "\tadd\tal,{imm8}\r\n");
            }
            let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
            return;
        }
        // Int-field `++` / `--` (discarded postfix or `+= 1` / `-= 1`):
        // memory-direct `inc`/`dec word ptr <dest>` (2-3 bytes via the
        // FF /0 or /1 form) instead of `add word ptr <dest>, 1` (5
        // bytes for sym+disp or 4 for [si]). Mirrors the char-field
        // K=1 peephole above. Fixture 1290 (`p->x++` with int x at
        // offset 0 in struct, p in SI → `inc word ptr [si]`).
        if !store_byte
            && v_masked == 1
            && matches!(op, BinOp::Add | BinOp::Sub)
        {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\tword ptr {dest}\r\n");
            return;
        }
        // `<field> <<= K` / `>>= K` — emit `shl/shr/sar word ptr
        // <dest>, K` for K==1, or the cl-loaded form for K>1. The
        // signedness of the field's int type picks shr vs sar.
        // Fixture 3521.
        if matches!(op, BinOp::Shl | BinOp::Shr) {
            let unsigned = false; // member type info not threaded here; default to signed shr → sar
            let _ = unsigned;
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr => "sar",
                _ => unreachable!(),
            };
            if v_masked == 1 {
                let _ = write!(self.out, "\t{mnem}\t{width} ptr {dest},1\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tcl,{v_masked}\r\n");
                let _ = write!(self.out, "\t{mnem}\t{width} ptr {dest},cl\r\n");
            }
            return;
        }
        let mnemonic = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::BitAnd => "and",
            BinOp::BitOr => "or",
            BinOp::BitXor => "xor",
            _ => panic!("compound op `{op:?}` on member not yet supported (no fixture)"),
        };
        let _ = write!(self.out, "\t{mnemonic}\t{width} ptr {dest},{v_masked}\r\n");
    }
}
