use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// Decompose a long-typed expression into a left-associative
    /// chain of long-pair-op operands: first the deepest LHS (which
    /// must be a long lvalue — it loads DX:AX), then a sequence of
    /// (lo_op, hi_op, hi_addr, lo_addr) steps. Returns None if the
    /// chain bottoms out at something that isn't a long lvalue or
    /// uses ops other than long-pair ops. Used by chained-long-add
    /// return shapes. Fixture 3301.
    pub(crate) fn collect_long_lvalue_chain(&self, e: &Expr) -> Option<Vec<LongChainStep>> {
        // Walk down `(((a op b) op c) op d)` collecting RHS operands
        // into `steps` (innermost first), then reverse.
        let mut steps: Vec<LongChainStep> = Vec::new();
        let mut cur = e;
        loop {
            if let ExprKind::BinOp { op, left, right } = &cur.kind
                && let Some((lo_op, hi_op)) = long_pair_op(*op)
                && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
            {
                steps.push(LongChainStep { lo_op, hi_op, hi: b_hi, lo: b_lo });
                cur = left;
            } else {
                break;
            }
        }
        let (root_hi, root_lo) = self.long_lvalue_addr_pair(cur)?;
        steps.reverse();
        let mut chain = vec![LongChainStep {
            lo_op: "",
            hi_op: "",
            hi: root_hi,
            lo: root_lo,
        }];
        chain.extend(steps);
        if chain.len() < 2 {
            return None;
        }
        Some(chain)
    }
    /// `(high-addr, low-addr)` text for a long-like ident, either as
    /// `DGROUP:_g+2` / `DGROUP:_g` (global) or `[bp+N+2]` / `[bp+N]`
    /// (stack). Panics on a register-resident or non-existent ident
    /// — callers should gate with `ident_is_long_like` first.
    pub(crate) fn long_addr_pair(&self, name: &str) -> (String, String) {
        if self.globals.contains(name) {
            (format!("DGROUP:_{name}+2"), format!("DGROUP:_{name}"))
        } else {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long is never register-resident");
            };
            (bp_addr(off + 2), bp_addr(off))
        }
    }
    /// True iff `e` is loadable into AX with at most a single
    /// (mov ax, <src>) instruction that doesn't disturb BX. Used by
    /// the var-idx-array RHS peephole to decide whether the LHS can
    /// be loaded AFTER the BX-address setup without itself clobbering
    /// the newly-prepared BX.
    pub(crate) fn is_simple_lvalue(&self, e: &Expr) -> bool {
        if try_const_eval(e).is_some() {
            return true;
        }
        let ExprKind::Ident(name) = &e.kind else { return false };
        if self.globals.contains(name) {
            return true;
        }
        if self.locals.has(name) {
            return matches!(
                self.locals.location_of(name),
                LocalLocation::Stack(_) | LocalLocation::Reg(_),
            );
        }
        false
    }
    /// Resolve an int-like lvalue (global or stack-resident local) to
    /// its asm memory operand. Returns `None` for register-resident
    /// locals (caller can fall back to a register-source path) and
    /// for non-lvalue expressions.
    pub(crate) fn int_lvalue_addr(&self, e: &Expr) -> Option<String> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        self.named_int_lvalue_addr(name)
    }
    /// Address of a named int lvalue (`<global>` → `DGROUP:_<name>`,
    /// stack local → `[bp+off]`). Identical body to
    /// [`int_lvalue_addr`] for the Ident case, but takes the name
    /// directly so callers that already destructured the
    /// `Ident(...)` don't have to re-wrap the operand in a synthetic
    /// `Expr`. Returns `None` if `name` doesn't refer to an int-like
    /// stack-local or global.
    pub(crate) fn named_int_lvalue_addr(&self, name: &str) -> Option<String> {
        if let Some(gty) = self.globals.type_of(name)
            && gty.is_int_like()
        {
            return Some(format!("DGROUP:_{name}"));
        }
        if self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            return Some(bp_addr(off));
        }
        None
    }
    /// Variant of [`int_lvalue_addr`] that returns either a memory
    /// address string (for stack/global lvalues) or a bare register
    /// name (for register-resident locals). Callers that need to
    /// handle both shapes can disambiguate via [`is_reg16_name`].
    pub(crate) fn int_lvalue_src(&self, e: &Expr) -> Option<String> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        if let Some(gty) = self.globals.type_of(name)
            && gty.is_int_like()
        {
            return Some(format!("DGROUP:_{name}"));
        }
        if self.locals.has(name) && self.locals.type_of(name).is_int_like() {
            match self.locals.location_of(name) {
                LocalLocation::Stack(off) => return Some(bp_addr(off)),
                LocalLocation::Reg(reg) if !reg.is_byte() => {
                    return Some(reg.name().to_owned());
                }
                _ => {}
            }
        }
        None
    }
    pub(crate) fn long_lvalue_addr_pair(&self, e: &Expr) -> Option<(String, String)> {
        // Bare ident.
        if let ExprKind::Ident(name) = &e.kind
            && self.ident_is_long_like(name)
        {
            return Some(self.long_addr_pair(name));
        }
        // Dot/arrow member chain folding to a constant address.
        if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } = &e.kind
            && let Some((src, total_off, leaf_ty)) = self.try_member_dot_chain(base, field)
            && leaf_ty.is_long_like()
        {
            if self.globals.contains(&src) {
                return Some((
                    global_offset_addr(&src, total_off + 2),
                    global_offset_addr(&src, total_off),
                ));
            }
            if let LocalLocation::Stack(base_bp) = self.locals.location_of(&src) {
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                return Some((bp_addr(off + 2), bp_addr(off)));
            }
        }
        // Array index with constant subscript (global or stack array).
        if let ExprKind::ArrayIndex { array: arr_expr, index } = &e.kind
            && let ExprKind::Ident(arr_name) = &arr_expr.kind
            && let Some(k) = try_const_eval(index)
        {
            let byte_off = (k as i32) * 4;
            if let Some(arr_ty) = self.globals.type_of(arr_name)
                && let Some(elem) = arr_ty.array_elem()
                && elem.is_long_like()
            {
                return Some((
                    global_offset_addr(arr_name, byte_off + 2),
                    global_offset_addr(arr_name, byte_off),
                ));
            }
            if self.locals.has(arr_name)
                && let Some(elem) = self.locals.type_of(arr_name).array_elem()
                && elem.is_long_like()
            {
                let LocalLocation::Stack(base_off) =
                    self.locals.location_of(arr_name)
                else {
                    unreachable!("array is stack-resident");
                };
                let off = base_off + i16::try_from(byte_off).unwrap_or(i16::MAX);
                return Some((bp_addr(off + 2), bp_addr(off)));
            }
        }
        // `*p` for a register-resident long pointer.
        if let ExprKind::Deref(operand) = &e.kind
            && let ExprKind::Ident(ptr_name) = &operand.kind
            && self.locals.has(ptr_name)
            && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
            && pointee.is_long_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(ptr_name)
        {
            let r = reg.name();
            return Some((format!("[{r}+2]"), format!("[{r}]")));
        }
        None
    }
    /// Resolve a stack-resident lvalue chain (`Ident`, `ArrayIndex`
    /// with constant subscripts, `Member` via `Dot`, or any
    /// composition of those) to `(base_name, total_byte_offset,
    /// leaf_type)`. Returns `None` if the chain includes a
    /// non-constant subscript, a pointer dereference, or anything
    /// outside this lvalue shape. Used by the member/array codegen
    /// to fold `pts[1].x` and friends into a single `[bp-N]` operand
    /// (fixture 185).
    /// Build the textual ModR/M address for a name + byte offset
    /// returned by [`Self::try_lvalue_chain_addr`]. Returns `None`
    /// when the name resolves to a non-stack local (register-resident
    /// or non-existent), since those can't be addressed by memory
    /// operand directly.
    pub(crate) fn resolve_chain_addr(&self, name: &str, off: i32) -> Option<String> {
        if self.globals.contains(name) {
            return Some(if off == 0 {
                format!("DGROUP:_{name}")
            } else {
                format!("DGROUP:_{name}+{off}")
            });
        }
        if let LocalLocation::Stack(base) = self.locals.location_of(name) {
            let final_off = base + i16::try_from(off).unwrap_or(i16::MAX);
            return Some(bp_addr(final_off));
        }
        None
    }
    pub(crate) fn try_lvalue_chain_addr(&self, e: &Expr) -> Option<(String, i32, Type)> {
        match &e.kind {
            ExprKind::Ident(name) => {
                // Look up in globals first, then locals. Caller decides
                // whether to address via DGROUP-relative or bp-relative.
                let ty = if let Some(gt) = self.globals.type_of(name) {
                    gt.clone()
                } else {
                    self.locals.type_of(name).clone()
                };
                Some((name.clone(), 0, ty))
            }
            ExprKind::ArrayIndex { array, index } => {
                let (n, off, ty) = self.try_lvalue_chain_addr(array)?;
                let k = i32::try_from(try_const_eval(index)?).ok()?;
                let Type::Array { elem, .. } = &ty else { return None };
                let stride = i32::from(elem.size_bytes());
                let new_off = off.checked_add(k.checked_mul(stride)?)?;
                Some((n, new_off, (**elem).clone()))
            }
            ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } => {
                self.try_member_dot_chain(base, field)
            }
            _ => None,
        }
    }
    /// Resolve a `<ptr>-><f1>.<f2>[.<f3>...]` chain into the
    /// (pointer-ident, accumulated-byte-offset, leaf-field-type)
    /// triple. The outermost member is the trailing `Dot` (the
    /// `field` argument); the chain underneath must eventually
    /// bottom out at a `Member { kind: Arrow, base: Ident(...) }`
    /// against a named pointer in scope (locals or globals). Any
    /// intermediate `Dot` members add their field offsets to the
    /// running total. Returns `None` if the shape doesn't match.
    /// Used by `emit_member_assign` to handle nested struct writes
    /// through a pointer (fixture 3693).
    pub(crate) fn try_arrow_chain_addr(
        &self,
        base: &Expr,
        field: &str,
    ) -> Option<(String, i32, Type)> {
        // Walk down: each Dot wrapper records its field, then we
        // recurse into its own base. Stop when we hit an Arrow on
        // an Ident — that's the runtime pointer load site.
        let mut dot_fields: Vec<&str> = vec![field];
        let mut cur = base;
        loop {
            match &cur.kind {
                ExprKind::Member { base: inner, field: f, kind } => match kind {
                    crate::ast::MemberKind::Dot => {
                        dot_fields.push(f.as_str());
                        cur = inner;
                    }
                    crate::ast::MemberKind::Arrow => {
                        let ExprKind::Ident(ptr_name) = &inner.kind else {
                            return None;
                        };
                        let ptr_ty = if self.locals.has(ptr_name) {
                            self.locals.type_of(ptr_name).clone()
                        } else if let Some(gty) = self.globals.type_of(ptr_name) {
                            gty.clone()
                        } else {
                            return None;
                        };
                        let pointee = ptr_ty.pointee()?.clone();
                        let (arrow_off, arrow_ty) = pointee.field(f)?;
                        let mut total: i32 = i32::from(arrow_off);
                        let mut ty = arrow_ty;
                        // Apply each accumulated Dot (innermost first
                        // — the chain we built has the outer-most
                        // `field` at index 0 and the innermost Dot at
                        // the end, so iterate in reverse).
                        for df in dot_fields.iter().rev() {
                            let (off, next_ty) = ty.field(df)?;
                            total = total.checked_add(i32::from(off))?;
                            ty = next_ty;
                        }
                        return Some((ptr_name.clone(), total, ty));
                    }
                },
                _ => return None,
            }
        }
    }
    /// True when the target lvalue ultimately writes a byte
    /// (char/uchar pointee or char field). Used by the cond emitter
    /// to decide between `or ax, ax` and `or al, al`.
    pub(crate) fn target_is_char_lvalue(&self, target: &Expr) -> bool {
        match &target.kind {
            ExprKind::Deref(inner) => {
                if let ExprKind::Ident(name) = &inner.kind
                    && self.locals.has(name)
                    && let Some(pointee) = self.locals.type_of(name).pointee()
                {
                    return pointee.is_char_like();
                }
                if let ExprKind::Update { target: name, .. } = &inner.kind
                    && self.locals.has(name)
                    && let Some(pointee) = self.locals.type_of(name).pointee()
                {
                    return pointee.is_char_like();
                }
                false
            }
            _ => false,
        }
    }
    /// Emit BCC's per-iteration address-into-BX prelude for a
    /// stack-array-of-struct field access with a non-power-of-2
    /// element stride:
    ///   mov ax, <i>
    ///   mov dx, <stride>
    ///   imul dx                            (DX:AX = i*stride)
    ///   lea dx, [bp+arr_base+field_off]    (DX = &arr[0].field)
    ///   add ax, dx                         (AX = &arr[i].field)
    ///   mov bx, ax
    /// AX and DX are clobbered; BX ends up pointing at the
    /// requested field of element `i`. Fixture 1914.
    pub(crate) fn emit_arr_var_field_addr_to_bx(
        &mut self,
        idx_src: &str,
        stride: u16,
        arr_base: i16,
        field_off: u16,
    ) {
        // `mov ax, <reg>` (no `word ptr`) for a register-resident
        // index, `mov ax, word ptr <mem>` otherwise.
        if is_reg16_name(idx_src) {
            let _ = write!(self.out, "\tmov\tax,{idx_src}\r\n");
        } else {
            let _ = write!(self.out, "\tmov\tax,word ptr {idx_src}\r\n");
        }
        let _ = write!(self.out, "\tmov\tdx,{stride}\r\n");
        self.out.extend_from_slice(b"\timul\tdx\r\n");
        let lea_off = arr_base + i16::try_from(field_off as i32).unwrap_or(i16::MAX);
        let _ = write!(self.out, "\tlea\tdx,word ptr {}\r\n", bp_addr(lea_off));
        self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
        self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
    }
    /// Like `int_lvalue_addr` but also accepts a register-resident
    /// int as a bare register-name source (e.g. `si`). Used by the
    /// arr[i] address prelude where the index might live in a
    /// register or on the stack — either way it has to be a
    /// memory- or register-direct word source.
    pub(crate) fn named_int_lvalue_addr_or_reg(&self, e: &Expr) -> Option<String> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        if let Some(gty) = self.globals.type_of(name)
            && gty.is_int_like()
        {
            return Some(format!("DGROUP:_{name}"));
        }
        if self.locals.has(name) && self.locals.type_of(name).is_int_like() {
            match self.locals.location_of(name) {
                LocalLocation::Stack(off) => return Some(bp_addr(off)),
                LocalLocation::Reg(reg) if !reg.is_byte() => {
                    return Some(reg.name().to_owned());
                }
                _ => {}
            }
        }
        None
    }
    /// Type of a far-pointer lvalue, or `None` if `e` doesn't name a
    /// stack-resident FarPointer local. Used to decide whether to
    /// route a comparison through the huge-pointer runtime helper.
    pub(crate) fn huge_ptr_lvalue_addr(&self, e: &Expr) -> Option<i16> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        if !self.locals.has(name) { return None; }
        let ty = self.locals.type_of(name);
        if !matches!(ty, Type::FarPointer { is_huge: true, .. }) { return None; }
        if let LocalLocation::Stack(off) = self.locals.location_of(name) {
            Some(off)
        } else {
            None
        }
    }
    /// Stack offset of a non-huge `FarPointer` lvalue, or `None`. Used
    /// to route inline two-half equality / inequality comparisons.
    /// The low half (offset) lives at `[bp+off]`; the high half
    /// (segment) lives at `[bp+off+2]`.
    pub(crate) fn far_ptr_lvalue_addr(&self, e: &Expr) -> Option<i16> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        if !self.locals.has(name) { return None; }
        let ty = self.locals.type_of(name);
        if !matches!(ty, Type::FarPointer { is_huge: false, .. }) { return None; }
        if let LocalLocation::Stack(off) = self.locals.location_of(name) {
            Some(off)
        } else {
            None
        }
    }
    /// Resolve an `ArrayIndex` expression with a constant index to
    /// the (address-string, element-type) pair codegen needs to
    /// emit an FPU memory operand. Returns `None` if the index is
    /// non-constant, the base isn't an Ident, or the element type
    /// isn't float-like. Local arrays produce `[bp+disp]`; globals
    /// produce `DGROUP:_<sym>[+const]`.
    pub(crate) fn resolve_float_array_addr(&self, e: &Expr) -> Option<(String, Type)> {
        let ExprKind::ArrayIndex { array, index } = &e.kind else {
            return None;
        };
        let ExprKind::Ident(name) = &array.kind else {
            return None;
        };
        let k = try_const_eval(index)?;
        if self.locals.has(name) {
            let ty = self.locals.type_of(name).clone();
            let elem = ty.array_elem()?.clone();
            if !elem.is_float_like() {
                return None;
            }
            let LocalLocation::Stack(base_off) = self.locals.location_of(name) else {
                return None;
            };
            let stride = i32::from(elem.size_bytes());
            let off = base_off + i16::try_from(k as i32 * stride).ok()?;
            Some((bp_addr(off), elem))
        } else if let Some(gty) = self.globals.type_of(name) {
            let elem = gty.array_elem()?.clone();
            if !elem.is_float_like() {
                return None;
            }
            let stride = i32::from(elem.size_bytes());
            let byte_off = (k as i32) * stride;
            let addr = if byte_off == 0 {
                format!("DGROUP:_{name}")
            } else {
                format!("DGROUP:_{name}+{byte_off}")
            };
            Some((addr, elem))
        } else {
            None
        }
    }
    /// `&<name>` — load the effective address of `name`'s stack slot
    /// into AX. Pattern (fixture 080):
    /// ```text
    ///   lea ax, word ptr [bp-N]
    /// ```
    /// `name` must be stack-resident — its address was taken at parse
    /// time, which the locals analyzer uses to force it off the
    /// register pool.
    pub(crate) fn emit_address_of(&mut self, name: &str) {
        // `&<global>` — emit the symbol's offset as an immediate.
        // Pattern from `p = &g;` at runtime, fixture 480 (the
        // file-scope init form is handled separately via the static
        // init path).
        if self.globals.contains(name) {
            let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{name}\r\n");
            return;
        }
        let LocalLocation::Stack(off) = self.locals.location_of(name) else {
            panic!(
                "`&{name}`: register-resident local cannot have its address taken \
                 (locals analyzer should have forced it to the stack)"
            );
        };
        let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
    }
    /// Pointee type of `name` if it's a pointer-typed identifier;
    /// `None` for non-pointers and unknown names. Used by the
    /// pointer-arithmetic stride scaling (fixture 3557).
    /// If `e` evaluates to a pointer (or decayed array), return the
    /// pointee type. Walks through BinOp(Add/Sub) and Cast(ptr_ty)
    /// to find a pointer-typed Ident at the root. Used to scale
    /// nested `<ptr-expr> + K` properly when the outer add follows
    /// another pointer add. Fixture 3632 (`p + n + 1`).
    pub(crate) fn expr_pointee(&self, e: &Expr) -> Option<Type> {
        match &e.kind {
            ExprKind::Ident(name) => {
                if let Some(p) = self.ident_pointee(name) {
                    return Some(p);
                }
                if let Some(ty) = self.globals.type_of(name).cloned()
                    .or_else(|| self.locals.has(name).then(|| self.locals.type_of(name).clone()))
                {
                    if let Some(elem) = ty.array_elem() {
                        return Some(elem.clone());
                    }
                }
                None
            }
            ExprKind::BinOp { op: BinOp::Add | BinOp::Sub, left, .. } => {
                self.expr_pointee(left)
            }
            ExprKind::Cast { ty, .. } => ty.pointee().cloned(),
            _ => None,
        }
    }
    pub(crate) fn ident_pointee(&self, name: &str) -> Option<Type> {
        if let Some(ty) = self.globals.type_of(name) {
            return ty.pointee().cloned();
        }
        if self.locals.has(name) {
            return self.locals.type_of(name).pointee().cloned();
        }
        None
    }
    /// Resolve an RHS expression to a `byte ptr <addr>` form
    /// pointing at its low byte. Supports `Ident` (global or
    /// stack local), `ArrayIndex` with constant index, and
    /// `Member` of a stack or global struct. Used by the shift
    /// arm to load CL with the shift count. Fixture 826.
    pub(crate) fn rhs_byte_addr(&self, e: &ExprKind) -> Option<String> {
        match e {
            ExprKind::Ident(n) => {
                if self.globals.contains(n) {
                    Some(format!("byte ptr DGROUP:_{n}"))
                } else if self.locals.has(n) {
                    let LocalLocation::Stack(off) = self.locals.location_of(n) else {
                        return None;
                    };
                    Some(format!("byte ptr {}", bp_addr(off)))
                } else {
                    None
                }
            }
            ExprKind::ArrayIndex { array, index } => {
                let ExprKind::Ident(arr_name) = &array.kind else { return None };
                let k = try_const_eval(index)?;
                let arr_ty = if self.globals.contains(arr_name) {
                    self.globals.type_of(arr_name)?.clone()
                } else if self.locals.has(arr_name) {
                    self.locals.type_of(arr_name).clone()
                } else {
                    return None;
                };
                let Type::Array { elem, .. } = arr_ty else { return None };
                let stride = u32::from(elem.size_bytes());
                let off = (k as u32).wrapping_mul(stride);
                if self.globals.contains(arr_name) {
                    let addr = if off == 0 {
                        format!("DGROUP:_{arr_name}")
                    } else {
                        format!("DGROUP:_{arr_name}+{off}")
                    };
                    Some(format!("byte ptr {addr}"))
                } else {
                    let LocalLocation::Stack(base) = self.locals.location_of(arr_name) else {
                        return None;
                    };
                    let total = base + i16::try_from(off).ok()?;
                    Some(format!("byte ptr {}", bp_addr(total)))
                }
            }
            ExprKind::Member { base, field, .. } => {
                let ExprKind::Ident(base_name) = &base.kind else { return None };
                let base_ty = if self.globals.contains(base_name) {
                    self.globals.type_of(base_name)?.clone()
                } else if self.locals.has(base_name) {
                    self.locals.type_of(base_name).clone()
                } else {
                    return None;
                };
                let (field_off, _) = base_ty.field(field)?;
                if self.globals.contains(base_name) {
                    let addr = if field_off == 0 {
                        format!("DGROUP:_{base_name}")
                    } else {
                        format!("DGROUP:_{base_name}+{field_off}")
                    };
                    Some(format!("byte ptr {addr}"))
                } else {
                    let LocalLocation::Stack(base_off) = self.locals.location_of(base_name) else {
                        return None;
                    };
                    let total = base_off + i16::try_from(field_off).ok()?;
                    Some(format!("byte ptr {}", bp_addr(total)))
                }
            }
            _ => None,
        }
    }
    /// Resolve an RHS expression to a DGROUP-relative address
    /// string (`DGROUP:_<name>[+<offset>]`) plus the resulting
    /// type, if it lives entirely in one DGROUP slot. Supports
    /// `Ident` (whole global), `ArrayIndex` with constant index
    /// (`a[K]`), and `Member` with `.` (`s.field`). Returns
    /// `None` for stack-resident RHS or non-foldable expressions.
    /// Used by the int-global Mul/Div arm to pick an `imul/idiv
    /// word ptr <addr>` mem operand.
    pub(crate) fn global_int_rhs_addr(&self, e: &ExprKind) -> Option<(String, Type)> {
        match e {
            ExprKind::Ident(n) => {
                let ty = self.globals.type_of(n)?.clone();
                Some((format!("DGROUP:_{n}"), ty))
            }
            ExprKind::ArrayIndex { array, index } => {
                let ExprKind::Ident(arr_name) = &array.kind else { return None };
                if !self.globals.contains(arr_name) { return None; }
                let k = try_const_eval(index)?;
                let arr_ty = self.globals.type_of(arr_name)?.clone();
                let Type::Array { elem, .. } = arr_ty else { return None };
                let stride = u32::from(elem.size_bytes());
                let off = (k as u32).wrapping_mul(stride);
                let addr = if off == 0 {
                    format!("DGROUP:_{arr_name}")
                } else {
                    format!("DGROUP:_{arr_name}+{off}")
                };
                Some((addr, (*elem).clone()))
            }
            ExprKind::Member { base, field, .. } => {
                let ExprKind::Ident(base_name) = &base.kind else { return None };
                if !self.globals.contains(base_name) { return None; }
                let base_ty = self.globals.type_of(base_name)?.clone();
                let (field_off, field_ty) = base_ty.field(field)?;
                let off = u32::from(field_off);
                let addr = if off == 0 {
                    format!("DGROUP:_{base_name}")
                } else {
                    format!("DGROUP:_{base_name}+{off}")
                };
                Some((addr, field_ty))
            }
            _ => None,
        }
    }
}
