use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// Resolve a struct field on a (possibly self-referential, name-only) pointee
    /// type, looking the tag up in the struct table when its fields are empty.
    fn resolve_pointee_field(&self, pointee: &Type, field: &str) -> Option<(u16, Type)> {
        let resolved = match pointee {
            Type::Struct { name: Some(tag), fields, .. } if fields.is_empty() =>
                self.lookup_struct_by_tag(tag).unwrap_or_else(|| pointee.clone()),
            _ => pointee.clone(),
        };
        resolved.field(field)
    }
    /// Load the POINTER VALUE of an arrow chain `..->f` into BX and return the
    /// struct type it points to. Recurses to any depth: `a.next->next->next`
    /// emits `mov bx,[a.next]; mov bx,[bx+nb]; mov bx,[bx+nc]`. The chain bottoms
    /// out at an lvalue pointer field (loaded with `mov bx,[addr]`). Fixtures
    /// 1928 (2-deep) and the deeper self-ref chains.
    fn emit_arrow_chain_ptr_to_bx(&mut self, e: &Expr) -> Option<Type> {
        let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Arrow } = &e.kind
        else { return None };
        if matches!(&base.kind, ExprKind::Member { kind: crate::ast::MemberKind::Arrow, .. }) {
            // `base` is itself an arrow chain — load it into BX, then step through
            // `base->field` (a pointer field) with one more `mov bx,[bx+off]`.
            let base_pointee = self.emit_arrow_chain_ptr_to_bx(base)?;
            let (off, fty) = self.resolve_pointee_field(&base_pointee, field)?;
            let _ = write!(self.out, "\tmov\tbx,word ptr [bx+{off}]\r\n");
            fty.pointee().cloned()
        } else {
            // `base` is an lvalue pointer field (`a.next`, a static struct ptr) —
            // load its value, then step to `base->field`.
            let (name, total_off, ty) = self.try_lvalue_chain_addr(base)?;
            let base_pointee = ty.pointee()?;
            let (off, fty) = self.resolve_pointee_field(base_pointee, field)?;
            let addr = if self.globals.contains(&name) {
                if total_off == 0 { format!("DGROUP:_{name}") } else { format!("DGROUP:_{name}+{total_off}") }
            } else if let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) {
                bp_addr(i16::try_from(i32::from(base_bp) + total_off as i32).unwrap_or(i16::MAX))
            } else {
                return None;
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr {addr}\r\n");
            let _ = write!(self.out, "\tmov\tbx,word ptr [bx+{off}]\r\n");
            fty.pointee().cloned()
        }
    }
    pub(crate) fn resolve_operand_source(&mut self, e: &Expr) -> OperandSource {
        if let Some(v) = try_const_eval(e) {
            return OperandSource::Immediate(v);
        }
        match &e.kind {
            ExprKind::Ident(name) => {
                if self.globals.contains(name) {
                    return OperandSource::Global(name.clone());
                }
                match self.locals.location_of(name) {
                    LocalLocation::Stack(off) => OperandSource::Local(off),
                    LocalLocation::Reg(reg) => OperandSource::Reg(reg),
                }
            }
            ExprKind::PseudoReg(name) => {
                panic!(
                    "pseudo-register `{name}` as operand of a binary op not yet supported"
                )
            }
            ExprKind::IntLit(_) => unreachable!("literals fold via try_const_eval"),
            ExprKind::UpdateLvalue { .. } => {
                panic!("UpdateLvalue as operand of a binary op not yet supported")
            }
            ExprKind::FloatLit(_) | ExprKind::DoubleLit(_) => {
                panic!("float literal in operand context not yet supported (no FPU path)")
            }
            ExprKind::Call { .. } | ExprKind::CallVia { .. } => {
                panic!("call as right operand not yet supported (need to preserve AX)")
            }
            ExprKind::BinOp { op: BinOp::Add, left, right }
                if let ExprKind::Ident(arr_name) = &left.kind
                    && self.locals.has(arr_name)
                    && let Some(elem) = self.locals.type_of(arr_name).array_elem()
                    && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
                    && let Some(k) = try_const_eval(right) =>
            {
                // `<local_arr_ident> + <const>` — address of element
                // K. Emit `lea ax, [bp+base+K*stride]` and route
                // through AX. Fixture 1814 (`p < a + 5`).
                let stride = i32::from(elem.size_bytes());
                let byte_off = (k as i32).wrapping_mul(stride);
                let total = base_off + i16::try_from(byte_off).unwrap_or(i16::MAX);
                let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(total));
                OperandSource::Ax
            }
            ExprKind::BinOp { op: BinOp::Add, left, right }
                if let ExprKind::Ident(arr_name) = &left.kind
                    && !self.locals.has(arr_name)
                    && let Some(gty) = self.globals.type_of(arr_name)
                    && let Some(elem) = gty.array_elem()
                    && let Some(k) = try_const_eval(right) =>
            {
                // `<global_arr_ident> + <const>` — the link-time
                // ADDRESS of element K, used as a symbolic immediate.
                // The loop guard `p < a + 6` folds the RHS to `offset
                // DGROUP:_a+12` and compares the pointer register
                // directly. Fixture 4226 (`cmp si,offset DGROUP:_a+12`).
                let stride = i32::from(elem.size_bytes());
                let byte_off = (k as i32).wrapping_mul(stride);
                OperandSource::GlobalAddr {
                    name: arr_name.clone(),
                    offset: byte_off,
                }
            }
            ExprKind::BinOp { .. } => {
                panic!("nested non-constant right operand not yet supported")
            }
            ExprKind::Unary { .. } => {
                panic!("non-constant unary expression as right operand not yet supported")
            }
            ExprKind::Update { .. } => {
                panic!("++/-- as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::Logical { .. } => {
                panic!("`&&`/`||` as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::AssignExpr { .. }
            | ExprKind::AssignLvalueExpr { .. }
            | ExprKind::CompoundAssignExpr { .. } => {
                panic!("assignment expression as right operand not yet supported (no fixture)")
            }
            ExprKind::AddressOf(_) | ExprKind::AddressOfArrayElem { .. } | ExprKind::AddressOfArrayElemVar { .. } => {
                panic!("`&x` as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::Deref(inner) => {
                // `*p` as RHS where `p` is a register-resident local
                // pointer — fold to a `<width> ptr [<reg>]` operand
                // (fixture 201). Other deref shapes (chained, global
                // pointer, post-update) still need materialization.
                if let ExprKind::Ident(name) = &inner.kind {
                    if self.globals.type_of(name).is_none() {
                        if let LocalLocation::Reg(reg) = self.locals.location_of(name) {
                            // SI/DI/BX address memory directly; CX/DX cannot be
                            // a base register, so materialize the pointer into BX
                            // first (`mov bx, cx`). Fixture 4240 (3rd pointer in
                            // CX, deref'd as a binop operand).
                            if matches!(reg, Reg::Si | Reg::Di | Reg::Bx) {
                                return OperandSource::DerefReg(reg);
                            }
                            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                            return OperandSource::DerefReg(Reg::Bx);
                        }
                    }
                }
                // `*(p + K)` where p is a register-resident pointer
                // with constant offset K — fold to `[reg + K*stride]`.
                // Fixture 3625 (`*p + *(p + 1)` for `int *p`).
                if let ExprKind::BinOp { op: BinOp::Add, left, right } = &inner.kind
                    && let ExprKind::Ident(name) = &left.kind
                    && self.locals.has(name)
                    && let Some(pointee) = self.locals.type_of(name).pointee()
                    && let LocalLocation::Reg(reg) = self.locals.location_of(name)
                    && let Some(k) = try_const_eval(right)
                {
                    let stride = i32::from(pointee.size_bytes());
                    let off = (k as i32).wrapping_mul(stride);
                    let off16 = i16::try_from(off).unwrap_or(i16::MAX);
                    return OperandSource::DerefRegOffset { reg, offset: off16 };
                }
                // `**(pp + K)` / `**pp` — double indirection through a
                // register-resident pointer-to-pointer. Load the inner
                // pointer value `*(pp + K)` into BX (`mov bx,[si+K*2]`),
                // then the outer deref reads `[bx]`. Fixture 4227
                // (`sum + **(pp + 2)` for `int **pp` in SI).
                if let ExprKind::Deref(inner2) = &inner.kind {
                    // Resolve the inner pointer-value operand. Accept either a
                    // bare register pointer (`*pp`) or `*(pp + K)`.
                    let inner_operand = if let ExprKind::Ident(name) = &inner2.kind {
                        match (self.globals.type_of(name).is_none(), self.locals.location_of(name)) {
                            (true, LocalLocation::Reg(reg)) => {
                                Some(OperandSource::DerefReg(reg))
                            }
                            _ => None,
                        }
                    } else if let ExprKind::BinOp { op: BinOp::Add, left, right } = &inner2.kind
                        && let ExprKind::Ident(name) = &left.kind
                        && self.locals.has(name)
                        && let Some(pointee) = self.locals.type_of(name).pointee()
                        && let LocalLocation::Reg(reg) = self.locals.location_of(name)
                        && let Some(k) = try_const_eval(right)
                    {
                        let stride = i32::from(pointee.size_bytes());
                        let off = (k as i32).wrapping_mul(stride);
                        let off16 = i16::try_from(off).unwrap_or(i16::MAX);
                        Some(OperandSource::DerefRegOffset { reg, offset: off16 })
                    } else {
                        None
                    };
                    if let Some(op) = inner_operand {
                        let _ = write!(self.out, "\tmov\tbx,{}\r\n", op.word());
                        return OperandSource::DerefReg(Reg::Bx);
                    }
                }
                panic!("`*p` as right operand of a binary op only supported for register-resident local pointers (no fixture for {:?})", inner.kind)
            }
            ExprKind::ArrayIndex { array, index } => {
                // `g[K]` where `g` is a file-scope array — fold to
                // `word ptr DGROUP:_g+(K*stride)`. Fixture 189 emits
                // `add ax, word ptr DGROUP:_a+2` for `a[1]`.
                //
                // Also handles member→array chains like `s.a[K]` and
                // global struct field arrays. Fixture 932 (`s.n +
                // s.a[1]` with `struct { int n; int a[3]; } s`).
                //
                // For stack-resident local arrays the same offset
                // arithmetic applies but the operand is a bp-relative
                // `[bp+(base_off+K*stride)]`. Fixture 977.
                //
                // `p[K]` where `p` is a register-resident pointer —
                // fold to `<width> ptr [<reg>+(K*stride)]`. Fixture
                // 1472 (`p[1]` in `sum`: `add ax, [si+2]`).
                if let ExprKind::Ident(pname) = &array.kind
                    && self.locals.has(pname)
                    && let Some(pointee) = self.locals.type_of(pname).pointee()
                    && let LocalLocation::Reg(reg) = self.locals.location_of(pname)
                    && let Some(k) = try_const_eval(index)
                {
                    let stride = i32::from(pointee.size_bytes());
                    let off = (k as i32).wrapping_mul(stride);
                    let off16 = i16::try_from(off).unwrap_or(i16::MAX);
                    return OperandSource::DerefRegOffset { reg, offset: off16 };
                }
                // `(*<reg-ptr>)[K]` — single-level access via an
                // explicit-deref pointer-to-array. The Deref folds
                // to array-to-pointer-decay (no actual memory
                // read), so the result is `[<reg>+K*elem_stride]`.
                // Fixture 2493 (`(*row)[i]` for `int (*row)[3]`).
                if let ExprKind::Deref(inner) = &array.kind
                    && let ExprKind::Ident(ptr_name) = &inner.kind
                    && self.locals.has(ptr_name)
                    && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
                    && let Some(elem) = pointee.array_elem()
                    && let LocalLocation::Reg(reg) =
                        self.locals.location_of(ptr_name)
                    && let Some(k) = try_const_eval(index)
                {
                    let stride = i32::from(elem.size_bytes());
                    let byte_off = (k as i32).wrapping_mul(stride);
                    let off16 = i16::try_from(byte_off).unwrap_or(i16::MAX);
                    return OperandSource::DerefRegOffset { reg, offset: off16 };
                }
                // `<reg-or-stack-ptr-to-arr>[K_outer][K_inner]` —
                // for parameter shape `int g[N][M]` (decays to
                // `int (*g)[M]`), fold both constant indices into a
                // single `[<reg>+offset]` operand. Inner-array elem
                // stride drives the byte offset. Fixture 2487.
                if let ExprKind::ArrayIndex { array: outer_arr, index: outer_idx } =
                    &array.kind
                    && let ExprKind::Ident(ptr_name) = &outer_arr.kind
                    && self.locals.has(ptr_name)
                    && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
                    && let Some(inner_elem) = pointee.array_elem()
                    && let Some(k_outer) = try_const_eval(outer_idx)
                    && let Some(k_inner) = try_const_eval(index)
                {
                    let outer_stride = u32::from(pointee.size_bytes());
                    let inner_stride = i32::from(inner_elem.size_bytes());
                    let outer_off = k_outer.wrapping_mul(outer_stride) as i32;
                    let inner_off = (k_inner as i32).wrapping_mul(inner_stride);
                    let total = outer_off + inner_off;
                    let total16 = i16::try_from(total).unwrap_or(i16::MAX);
                    if let LocalLocation::Reg(reg) = self.locals.location_of(ptr_name) {
                        return OperandSource::DerefRegOffset {
                            reg,
                            offset: total16,
                        };
                    }
                    if let LocalLocation::Stack(base_off) =
                        self.locals.location_of(ptr_name)
                    {
                        let _ = write!(
                            self.out,
                            "\tmov\tbx,word ptr {}\r\n",
                            bp_addr(base_off),
                        );
                        return OperandSource::DerefRegOffset {
                            reg: crate::codegen::locals::Reg::Bx,
                            offset: total16,
                        };
                    }
                }
                // `<global-or-static-int-ptr-arr>[K_outer][K_inner]`
                // as RHS: load the pointer slot into BX, then read
                // a word at [bx + K_inner*stride]. Only int-pointee
                // because the operand-source resolution doesn't
                // track byte vs word width — char pointees need a
                // separate `mov al, [bx]; cbw` path which can't
                // ride through resolve_operand_source.
                if let ExprKind::ArrayIndex { array: outer_arr, index: outer_idx } =
                    &array.kind
                    && let ExprKind::Ident(arr_name) = &outer_arr.kind
                    && let Some(gty) = self.globals.type_of(arr_name)
                    && let Some(elem_ty) = gty.array_elem()
                    && let Some(pointee) = elem_ty.pointee()
                    && pointee.is_int_like()
                    && let Some(k_outer) = try_const_eval(outer_idx)
                    && let Some(k_inner) = try_const_eval(index)
                {
                    let elem_stride = u32::from(elem_ty.size_bytes());
                    let outer_byte_off = k_outer.wrapping_mul(elem_stride);
                    let inner_stride = i32::from(pointee.size_bytes());
                    let inner_byte_off = (k_inner as i32).wrapping_mul(inner_stride);
                    let arr_addr = if outer_byte_off == 0 {
                        format!("DGROUP:_{arr_name}")
                    } else {
                        format!("DGROUP:_{arr_name}+{outer_byte_off}")
                    };
                    let _ = write!(self.out, "\tmov\tbx,word ptr {arr_addr}\r\n");
                    return OperandSource::DerefRegOffset {
                        reg: crate::codegen::locals::Reg::Bx,
                        offset: inner_byte_off as i16,
                    };
                }
                // `(*(<reg-ptr> + K_outer))[K_inner]` — pointer
                // arithmetic on a pointer-to-array, then explicit
                // deref + inner index. Same effective address as
                // `<ptr>[K_outer*<outer-size> + K_inner*<elem-size>]`.
                // Fixture 2329 (`(*(p+1))[2]` for `int (*p)[3]`).
                if let ExprKind::Deref(inner) = &array.kind
                    && let ExprKind::BinOp { op, left, right } = &inner.kind
                    && matches!(op, BinOp::Add | BinOp::Sub)
                    && let ExprKind::Ident(ptr_name) = &left.kind
                    && self.locals.has(ptr_name)
                    && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
                    && let Some(elem) = pointee.array_elem()
                    && let Some(k_outer) = try_const_eval(right)
                    && let Some(k_inner) = try_const_eval(index)
                    && let LocalLocation::Reg(reg) =
                        self.locals.location_of(ptr_name)
                {
                    let outer_stride = i32::from(pointee.size_bytes());
                    let inner_stride = i32::from(elem.size_bytes());
                    let k_outer_signed = if matches!(op, BinOp::Sub) {
                        -(k_outer as i32)
                    } else {
                        k_outer as i32
                    };
                    let outer_off = k_outer_signed.wrapping_mul(outer_stride);
                    let inner_off = (k_inner as i32).wrapping_mul(inner_stride);
                    let total = outer_off + inner_off;
                    let total16 = i16::try_from(total).unwrap_or(i16::MAX);
                    return OperandSource::DerefRegOffset {
                        reg,
                        offset: total16,
                    };
                }
                let (name, total_off, _leaf_ty) = self
                    .try_lvalue_chain_addr(e)
                    .unwrap_or_else(|| {
                        panic!("variable-indexed global array rhs not yet supported")
                    });
                if self.globals.contains(&name) {
                    return OperandSource::GlobalOffset { name, offset: total_off };
                }
                if self.locals.has(&name)
                    && let LocalLocation::Stack(base_off) = self.locals.location_of(&name)
                {
                    let elem_off = base_off + i16::try_from(total_off).unwrap_or(i16::MAX);
                    return OperandSource::Local(elem_off);
                }
                panic!("array-indexed rhs not supported on `{name}`");
            }
            ExprKind::StringLit(_) => {
                panic!("string literal as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } => {
                // `<reg-ptr>[var_idx].<field>` as RHS: scale the
                // index into DX (preserving AX), then `bx = ptr +
                // dx` and return `[bx + field_off]`. BCC's pattern
                // for fixture 2208 (`pts[i].x + pts[i].y` for
                // `struct P *pts`).
                if let ExprKind::ArrayIndex {
                    array: outer_arr, index: outer_idx,
                } = &base.kind
                    && let ExprKind::Ident(arr_name) = &outer_arr.kind
                    && self.locals.has(arr_name)
                    && let Some(pointee) = self.locals.type_of(arr_name).pointee()
                    && let Some((field_off, _ft)) = pointee.field(field)
                    && let LocalLocation::Reg(ptr_reg) =
                        self.locals.location_of(arr_name)
                    && !ptr_reg.is_byte()
                    && try_const_eval(outer_idx).is_none()
                {
                    let elem_stride = pointee.size_bytes();
                    let log2 = match elem_stride {
                        2 => 1,
                        4 => 2,
                        8 => 3,
                        16 => 4,
                        _ => 0,
                    };
                    let pow2_stride = (1u16 << log2) == elem_stride;
                    if pow2_stride && log2 > 0 {
                        let loaded = if let ExprKind::Ident(idx_name) = &outer_idx.kind
                            && self.locals.has(idx_name)
                            && let LocalLocation::Reg(reg) =
                                self.locals.location_of(idx_name)
                            && !reg.is_byte()
                        {
                            let _ = write!(self.out, "\tmov\tdx,{}\r\n", reg.name());
                            true
                        } else if let Some(idx_addr) = self.int_lvalue_addr(outer_idx) {
                            let _ = write!(
                                self.out,
                                "\tmov\tdx,word ptr {idx_addr}\r\n",
                            );
                            true
                        } else {
                            false
                        };
                        if loaded {
                            for _ in 0..log2 {
                                self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                            }
                            let _ = write!(self.out, "\tmov\tbx,{}\r\n", ptr_reg.name());
                            self.out.extend_from_slice(b"\tadd\tbx,dx\r\n");
                            return OperandSource::DerefRegOffset {
                                reg: crate::codegen::locals::Reg::Bx,
                                offset: field_off as i16,
                            };
                        }
                    }
                }
                // `<stack-ptr-to-struct>[<var-idx>].<field>` as RHS:
                // pts is a stack-resident pointer (parameter, etc.)
                // pointing to a struct. Scale the index by sizeof
                // (struct) into BX, then add the pointer's value
                // loaded from the stack slot. Fixture 2208 (when
                // `pts` ends up on the stack because its use count
                // is below the enregister threshold).
                if let ExprKind::ArrayIndex {
                    array: outer_arr, index: outer_idx,
                } = &base.kind
                    && let ExprKind::Ident(arr_name) = &outer_arr.kind
                    && self.locals.has(arr_name)
                    && let Some(pointee) = self.locals.type_of(arr_name).pointee()
                    && let Some((field_off, _field_ty)) = pointee.field(field)
                    && let LocalLocation::Stack(ptr_off) =
                        self.locals.location_of(arr_name)
                    && try_const_eval(outer_idx).is_none()
                {
                    let elem_stride = pointee.size_bytes();
                    let log2 = match elem_stride {
                        2 => 1,
                        4 => 2,
                        8 => 3,
                        16 => 4,
                        _ => 0,
                    };
                    let pow2_stride = (1u16 << log2) == elem_stride;
                    if pow2_stride && log2 > 0 {
                        let loaded = if let ExprKind::Ident(idx_name) = &outer_idx.kind
                            && self.locals.has(idx_name)
                            && let LocalLocation::Reg(reg) =
                                self.locals.location_of(idx_name)
                            && !reg.is_byte()
                        {
                            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                            true
                        } else if let Some(idx_addr) = self.int_lvalue_addr(outer_idx) {
                            let _ = write!(
                                self.out,
                                "\tmov\tbx,word ptr {idx_addr}\r\n",
                            );
                            true
                        } else {
                            false
                        };
                        if loaded {
                            for _ in 0..log2 {
                                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
                            }
                            let _ = write!(
                                self.out,
                                "\tadd\tbx,word ptr {}\r\n",
                                bp_addr(ptr_off),
                            );
                            return OperandSource::DerefRegOffset {
                                reg: crate::codegen::locals::Reg::Bx,
                                offset: field_off as i16,
                            };
                        }
                    }
                }
                // `<stack-arr-of-struct>[var_idx].<field>` as RHS:
                // emit a per-use BX setup (scale index, add base
                // address with field offset, deref BX). The cost is
                // higher than the const-fold path but the operand
                // ends up in a uniform `[bx]` slot. Fixture 2438.
                if let ExprKind::ArrayIndex {
                    array: outer_arr, index: outer_idx,
                } = &base.kind
                    && let ExprKind::Ident(arr_name) = &outer_arr.kind
                    && self.locals.has(arr_name)
                    && let Some(elem_ty) = self.locals.type_of(arr_name).array_elem()
                    && let Some((field_off, _field_ty)) = elem_ty.field(field)
                    && let LocalLocation::Stack(base_bp) =
                        self.locals.location_of(arr_name)
                    && try_const_eval(outer_idx).is_none()
                {
                    let elem_stride = elem_ty.size_bytes();
                    let log2 = match elem_stride {
                        2 => 1,
                        4 => 2,
                        8 => 3,
                        16 => 4,
                        _ => 0,
                    };
                    let pow2_stride = (1u16 << log2) == elem_stride;
                    if pow2_stride && log2 > 0 {
                        // Load index without clobbering AX. Reg-
                        // resident index: `mov bx, <reg>`. Stack
                        // index: `mov bx, word ptr [bp+N]`. Bail
                        // if neither applies (would need to emit
                        // through AX, which we can't do here).
                        let loaded = if let ExprKind::Ident(idx_name) = &outer_idx.kind
                            && self.locals.has(idx_name)
                            && let LocalLocation::Reg(reg) =
                                self.locals.location_of(idx_name)
                            && !reg.is_byte()
                        {
                            let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                            true
                        } else if let Some(idx_addr) = self.int_lvalue_addr(outer_idx) {
                            let _ = write!(
                                self.out,
                                "\tmov\tbx,word ptr {idx_addr}\r\n",
                            );
                            true
                        } else {
                            false
                        };
                        if !loaded {
                            // Fall through to the const-fold panic
                            // path below so the diagnostic is clean.
                        } else {
                            for _ in 0..log2 {
                                self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
                            }
                            let off = base_bp
                                + i16::try_from(field_off as i32).unwrap_or(i16::MAX);
                            // Use DX as the scratch for the lea so
                            // the LHS in AX isn't clobbered. BCC's
                            // exact pattern for fixture 2438's
                            // second operand (`a[i].y` after AX
                            // already holds `a[i].x`).
                            let _ = write!(
                                self.out,
                                "\tlea\tdx,word ptr {}\r\n",
                                bp_addr(off),
                            );
                            self.out.extend_from_slice(b"\tadd\tbx,dx\r\n");
                            return OperandSource::DerefReg(
                                crate::codegen::locals::Reg::Bx,
                            );
                        }
                    }
                }
                // `a.x` / `pts[1].x` / `a.b.c` / global `g.x` as a
                // right operand: walk the lvalue chain. Local chain
                // → `[bp-N]`; global chain → `DGROUP:_<name>+K`.
                // Fixture 103 (`return p.x + p.y;`),
                // fixture 185 (`pts[1].x + pts[1].y`),
                // fixture 190 (global `g.x + g.y`).
                //
                let (name, total_off, _leaf_ty) = self
                    .try_member_dot_chain(base, field)
                    .unwrap_or_else(|| {
                        panic!("non-const-foldable member base in rhs not yet supported")
                    });
                if self.globals.contains(&name) {
                    return OperandSource::GlobalOffset { name, offset: total_off };
                }
                let LocalLocation::Stack(base_bp) = self.locals.location_of(&name) else {
                    panic!("struct local `{name}` not stack-resident");
                };
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                OperandSource::Local(off)
            }
            ExprKind::Member { base, field, kind: crate::ast::MemberKind::Arrow } => {
                // `<reg_ptr>-><field>` as RHS: fold to `<width> ptr
                // [<reg>+field_off]`. Mirrors the `p[K]` case above.
                // Fixture 2313 (`pp->y` for register-resident
                // struct ptr).
                if let ExprKind::Ident(p_name) = &base.kind
                    && self.locals.has(p_name)
                    && let Some(pointee) = self.locals.type_of(p_name).pointee()
                    && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
                    && let Some((field_off, _field_ty)) = pointee.field(field)
                {
                    return OperandSource::DerefRegOffset {
                        reg,
                        offset: field_off as i16,
                    };
                }
                // `<lvalue-chain>-><field>` as RHS where the base
                // chain folds to a const-offset lvalue (stack or
                // global): load the pointer-typed lvalue into BX,
                // then deref with `[bx+field_off]`. Fixtures 1928
                // (stack struct `a.next->v`), 2310 (static struct).
                if let Some((src_name, total_off, src_ty)) =
                    self.try_lvalue_chain_addr(base)
                    && let Some(pointee) = src_ty.pointee()
                    && let Some((field_off, _ft)) = {
                        // Self-referential structs carry a name-only
                        // placeholder for the pointee; resolve via
                        // the globals/locals tag tables when fields
                        // are empty.
                        let resolved = match pointee {
                            Type::Struct { name: Some(tag), fields, .. }
                                if fields.is_empty() =>
                            {
                                self.lookup_struct_by_tag(tag)
                                    .unwrap_or_else(|| pointee.clone())
                            }
                            _ => pointee.clone(),
                        };
                        resolved.field(field).map(|(o, t)| (o, t))
                    }
                {
                    let src_addr = if self.globals.contains(&src_name) {
                        if total_off == 0 {
                            format!("DGROUP:_{src_name}")
                        } else {
                            format!("DGROUP:_{src_name}+{total_off}")
                        }
                    } else if let LocalLocation::Stack(base_bp) =
                        self.locals.location_of(&src_name)
                    {
                        let off = i32::from(base_bp) + total_off as i32;
                        bp_addr(i16::try_from(off).unwrap_or(i16::MAX))
                    } else {
                        panic!("`p->x` base lvalue `{src_name}` not stack/global");
                    };
                    let _ = write!(self.out, "\tmov\tbx,word ptr {src_addr}\r\n");
                    return OperandSource::DerefRegOffset {
                        reg: crate::codegen::locals::Reg::Bx,
                        offset: field_off as i16,
                    };
                }
                // Chained-arrow base: `<inner>-><f>-><field>`.
                // Recursively load the inner arrow into BX, then
                // step through one more `[bx+f_off]` to land BX on
                // the struct whose field we want. Fixture 1928's
                // `a.next->next->v` (and the deeper chain in main).
                if let ExprKind::Member {
                    base: inner_base,
                    field: inner_field,
                    kind: crate::ast::MemberKind::Arrow,
                } = &base.kind
                    && let Some((src_name, total_off, src_ty)) =
                        self.try_lvalue_chain_addr(inner_base)
                    && let Some(inner_ptr_ty) = src_ty.pointee()
                    && let Some((inner_off, inner_field_ty)) = {
                        let resolved = match inner_ptr_ty {
                            Type::Struct { name: Some(tag), fields, .. }
                                if fields.is_empty() =>
                            {
                                self.lookup_struct_by_tag(tag)
                                    .unwrap_or_else(|| inner_ptr_ty.clone())
                            }
                            _ => inner_ptr_ty.clone(),
                        };
                        resolved.field(inner_field).map(|(o, t)| (o, t))
                    }
                    && let Some(next_ptr_ty) = inner_field_ty.pointee()
                    && let Some((field_off, _ft)) = {
                        let resolved = match next_ptr_ty {
                            Type::Struct { name: Some(tag), fields, .. }
                                if fields.is_empty() =>
                            {
                                self.lookup_struct_by_tag(tag)
                                    .unwrap_or_else(|| next_ptr_ty.clone())
                            }
                            _ => next_ptr_ty.clone(),
                        };
                        resolved.field(field).map(|(o, t)| (o, t))
                    }
                {
                    let src_addr = if self.globals.contains(&src_name) {
                        if total_off == 0 {
                            format!("DGROUP:_{src_name}")
                        } else {
                            format!("DGROUP:_{src_name}+{total_off}")
                        }
                    } else if let LocalLocation::Stack(base_bp) =
                        self.locals.location_of(&src_name)
                    {
                        let off = i32::from(base_bp) + total_off as i32;
                        bp_addr(i16::try_from(off).unwrap_or(i16::MAX))
                    } else {
                        panic!("`p->q->x` chain lvalue `{src_name}` not stack/global");
                    };
                    // First arrow: load `inner_base`'s pointer field
                    // into BX (the value of `<inner-base>->`).
                    let _ = write!(self.out, "\tmov\tbx,word ptr {src_addr}\r\n");
                    // Second arrow: dereference once more to land on
                    // the struct that holds the final `field`.
                    let _ = write!(
                        self.out,
                        "\tmov\tbx,word ptr [bx+{}]\r\n",
                        inner_off,
                    );
                    return OperandSource::DerefRegOffset {
                        reg: crate::codegen::locals::Reg::Bx,
                        offset: field_off as i16,
                    };
                }
                // General N-arrow chain (3+ deep): recursively load the chain
                // pointer into BX, then deref the final field. The 1- and 2-deep
                // cases above return early; this catches deeper self-ref chains.
                if matches!(&base.kind, ExprKind::Member { kind: crate::ast::MemberKind::Arrow, .. })
                    && let Some(pointee) = self.emit_arrow_chain_ptr_to_bx(base)
                    && let Some((field_off, _)) = self.resolve_pointee_field(&pointee, field)
                {
                    return OperandSource::DerefRegOffset {
                        reg: crate::codegen::locals::Reg::Bx,
                        offset: field_off as i16,
                    };
                }
                panic!("`p->x` as right operand not yet supported for non-register pointers (no fixture for {:?})", base.kind)
            }
            ExprKind::Ternary { .. } => {
                panic!("ternary as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::Cast { ty: cast_ty, operand } => {
                // `(int)<int_lvalue>` — no-op cast; recurse into the
                // operand. Fixture 1778 (`ca[1] + (int)ia[1]`).
                if matches!(cast_ty, Type::Int | Type::UInt)
                    && let Some((_name, _off, ty)) = self.try_lvalue_chain_addr(operand)
                    && ty.is_int_like()
                {
                    return self.resolve_operand_source(operand);
                }
                // `(int)<long_lvalue>` — fold to the low-half memory
                // address as a word-sized source operand. The cast is
                // a no-op at the byte level (low half of a long IS
                // an int). Fixture 1947 (`a + (int)b + c`).
                if matches!(cast_ty, Type::Int | Type::UInt)
                    && let Some((_hi, lo)) = self.long_lvalue_addr_pair(operand)
                {
                    // `[bp-N]` / `DGROUP:_<sym>` / `DGROUP:_<sym>+K`.
                    // Use Local for bp-relative, Global for DGROUP.
                    if let Some(off_str) = lo.strip_prefix("[bp+").and_then(|s| s.strip_suffix(']')) {
                        let off: i16 = off_str.parse().unwrap_or(0);
                        return OperandSource::Local(off);
                    }
                    if let Some(off_str) = lo.strip_prefix("[bp-").and_then(|s| s.strip_suffix(']')) {
                        let off: i16 = off_str.parse().unwrap_or(0);
                        return OperandSource::Local(-off);
                    }
                    if let Some(sym_off) = lo.strip_prefix("DGROUP:_") {
                        if let Some((sym, off)) = sym_off.split_once('+') {
                            let offset: i32 = off.parse().unwrap_or(0);
                            return OperandSource::GlobalOffset {
                                name: sym.to_string(),
                                offset,
                            };
                        }
                        return OperandSource::Global(sym_off.to_string());
                    }
                }
                panic!("cast as right operand of a binary op not yet supported (no fixture)")
            }
            ExprKind::InitList { .. } => {
                panic!("initializer list not legal as a binary-op operand")
            }
            ExprKind::Comma { .. } => {
                panic!("comma expression as right operand of a binary op not yet supported (no fixture)")
            }
        }
    }
    /// Emit `;` source-comment block(s). Emits ALL source lines from
    /// `current_line + 1` through `line` (inclusive) as one combined
    /// block — leading blank `;\t`, one `;\t<content>` per line, then
    /// trailing blank `;\t`. This matches what BCC does when multiple
    /// source lines have no asm between them (e.g. a `while` header
    /// followed by its first body statement; the close-brace of a
    /// `while` body followed by a statement after the loop).
    ///
    /// The very first comment block in a function — when
    /// `current_line == 0` — emits only the *target* line, not the
    /// preceding source. Otherwise functions defined later in the file
    /// would carry along all prior content as part of their opening
    /// comment block (fixture 009).
    pub(crate) fn advance_to_line(&mut self, line: u32) {
        if line <= self.current_line {
            return;
        }
        let from = if self.current_line == 0 { line } else { self.current_line + 1 };
        self.out.extend_from_slice(b"   ;\t\r\n");
        for ln in from..=line {
            let content = self.lines.line_content(self.source, ln);
            let _ = write!(self.out, "   ;\t{content}\r\n");
        }
        self.out.extend_from_slice(b"   ;\t\r\n");
        self.current_line = line;
    }
}
