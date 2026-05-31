use super::*;

impl<'a> super::FunctionEmitter<'a> {
    pub(crate) fn ident_is_long_like(&self, name: &str) -> bool {
        if let Some(gt) = self.globals.type_of(name) {
            return gt.is_long_like();
        }
        self.locals.has(name) && self.locals.type_of(name).is_long_like()
    }
    /// True iff `e` is a long-typed expression (long/ulong lvalue,
    /// long binop, or long cast). Best-effort — covers the shapes
    /// the long-widening paths need to distinguish.
    pub(crate) fn expr_is_long_like(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Ident(name) => self.ident_is_long_like(name),
            ExprKind::Cast { ty, .. } => ty.is_long_like(),
            ExprKind::BinOp { left, right, .. } => {
                self.expr_is_long_like(left) || self.expr_is_long_like(right)
            }
            ExprKind::Unary { operand, .. } => self.expr_is_long_like(operand),
            ExprKind::Ternary { then_value, else_value, .. } => {
                self.expr_is_long_like(then_value) || self.expr_is_long_like(else_value)
            }
            ExprKind::Member { base, field, .. } => {
                if let Some((_, _, ty)) = self.try_lvalue_chain_addr(e) {
                    return ty.is_long_like();
                }
                let _ = (base, field);
                false
            }
            ExprKind::ArrayIndex { array, .. } => {
                if let Some((_, _, ty)) = self.try_lvalue_chain_addr(e) {
                    return ty.is_long_like();
                }
                // Variable index — try_lvalue_chain_addr only handles
                // constant indices. Look at the array's element type
                // directly. Fixture 3288 (`arr[i]` with arr a long
                // global array, i variable).
                if let ExprKind::Ident(name) = &array.kind {
                    let elem = self.globals.type_of(name)
                        .and_then(|t| t.array_elem())
                        .or_else(|| {
                            if self.locals.has(name) {
                                self.locals.type_of(name).array_elem()
                            } else {
                                None
                            }
                        });
                    if let Some(e_ty) = elem {
                        return e_ty.is_long_like();
                    }
                    // Pointer to long: `p[i]` where p is `long *`.
                    if let Some(pointee) = self.ident_pointee(name) {
                        return pointee.is_long_like();
                    }
                }
                false
            }
            ExprKind::Call { name, .. } => {
                self.signatures.ret_ty_of(name).is_some_and(|t| t.is_long_like())
            }
            _ => false,
        }
    }
    /// Classify a name for the var-idx-array RHS peephole. Returns
    /// `(elem_ty, kind)` if `name` is one of the supported shapes
    /// (stack int array, int* local, global int array). The caller
    /// uses `kind` to dispatch the appropriate address-into-BX
    /// computation. Fixtures 2454, 2849, 3003.
    pub(crate) fn classify_var_idx_array(&self, name: &str) -> Option<(Type, VarIdxKind)> {
        if self.locals.has(name) {
            let ty = self.locals.type_of(name).clone();
            if let Some(elem_ty) = ty.array_elem() {
                if let LocalLocation::Stack(base_off) = self.locals.location_of(name) {
                    let elem_sz = elem_ty.size_bytes();
                    return Some((
                        elem_ty.clone(),
                        VarIdxKind::StackArr(base_off, elem_sz),
                    ));
                }
            }
            if let Some(pointee) = ty.pointee() {
                return Some((pointee.clone(), VarIdxKind::PtrInt));
            }
        }
        if let Some(gty) = self.globals.type_of(name)
            && let Some(elem_ty) = gty.array_elem()
        {
            return Some((elem_ty.clone(), VarIdxKind::GlobalArr));
        }
        None
    }
    /// Whether `cond` is `<long_global> != <int_global>`. Same
    /// widen-via-cwd shape as `is_long_vs_int_cmp` but uses the
    /// chained-cmp pattern with both slots (jne→true, je→false).
    /// Fixture 280.
    pub(crate) fn is_long_vs_int_ne(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op: BinOp::Ne, left, right } = &cond.kind else {
            return false;
        };
        let (ExprKind::Ident(a), ExprKind::Ident(b)) = (&left.kind, &right.kind) else {
            return false;
        };
        let a_ty = self.globals.type_of(a);
        let b_ty = self.globals.type_of(b);
        a_ty.map_or(false, |t| t.is_long_like())
            && b_ty.map_or(false, |t| matches!(t, Type::Int))
    }
    /// Whether `cond` is `<long_global> != K` for a small const K —
    /// uses the chained-cmp pattern with both slots (jne→true,
    /// je→false). Fixture 239.
    pub(crate) fn is_long_ne_const(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op: BinOp::Ne, left, right } = &cond.kind else {
            return false;
        };
        let ExprKind::Ident(name) = &left.kind else { return false };
        if !self.globals.type_of(name).map_or(false, |t| t.is_long_like()) {
            return false;
        }
        let Some(k) = try_const_eval(right) else { return false };
        if k == 0 {
            return false; // long != 0 uses the OR-then-test idiom (fixture 238)
        }
        let hi = (k >> 16) as i32;
        let lo = (k & 0xFFFF) as i32;
        (-128..=127).contains(&hi) && (-128..=127).contains(&lo)
    }
    pub(crate) fn expr_is_unsigned(&self, e: &Expr) -> bool {
        let ExprKind::Ident(name) = &e.kind else { return false };
        let ty = if let Some(gt) = self.globals.type_of(name) {
            gt
        } else {
            self.locals.type_of(name)
        };
        // Pointer compares are unsigned (addresses can exceed 0x7FFF
        // in real mode). Fixtures 2929, 2934 (`a < b` / `a >= b` for
        // pointer args).
        ty.is_unsigned() || ty.pointee().is_some()
    }
    /// True iff `name` refers to an identifier (global or local)
    /// whose static type is `unsigned char`. Used to decide
    /// between the zero-extend (mov dh, 0) and sign-extend (cbw)
    /// widening shapes for the char-on-right peephole.
    pub(crate) fn ident_is_uchar(&self, name: &str) -> bool {
        if let Some(ty) = self.globals.type_of(name) {
            return matches!(ty, Type::UChar);
        }
        if self.locals.has(name) {
            return matches!(self.locals.type_of(name), Type::UChar);
        }
        false
    }
    /// True iff `name` refers to an identifier (global or local)
    /// whose static type is `char`. Used by `emit_binary_right` to
    /// detect when the right operand needs the widening dance.
    pub(crate) fn ident_is_char(&self, name: &str) -> bool {
        if let Some(ty) = self.globals.type_of(name) {
            return ty.is_char_like();
        }
        // The locals analyzer panics on unknown names, so only ask
        // if there's no global match.
        self.locals.type_of(name).is_char_like()
    }
    /// True when `e` evaluates to a char-typed value via a memory
    /// load that would clobber AX (`mov al, ...; cbw`). Covers bare
    /// char idents, char array elements, and char struct fields.
    /// Used by the binop RHS path to decide whether to push the LHS
    /// before evaluating the RHS — otherwise the load would
    /// overwrite AX. Fixture 2006 (`a[0] + a[126]` for char a[]).
    pub(crate) fn expr_is_char_load(&self, e: &Expr) -> bool {
        if let ExprKind::Ident(name) = &e.kind {
            return self.ident_is_char(name);
        }
        if let Some((_, _, ty)) = self.try_lvalue_chain_addr(e) {
            return ty.is_char_like();
        }
        // `p[K]` for `p` a char-pointer ident — chain-addr fails
        // because chain walking expects Type::Array, but the subscript
        // through a char pointer still produces a byte load. Fixture
        // 1239 (`a[0] + a[1]` for `int sum(char a[])` — `a` decays
        // to char*).
        if let ExprKind::ArrayIndex { array, .. } = &e.kind
            && let ExprKind::Ident(pname) = &array.kind
            && let Some(pointee) = self.ident_pointee(pname)
            && pointee.is_char_like()
        {
            return true;
        }
        // `<char-array>[<idx>]` — var-indexed char array (local or
        // global) is still a byte load. try_lvalue_chain_addr
        // declines variable indices, so detect this shape directly.
        if let ExprKind::ArrayIndex { array, .. } = &e.kind
            && let ExprKind::Ident(arr_name) = &array.kind
        {
            let arr_ty = self
                .globals
                .type_of(arr_name)
                .cloned()
                .or_else(|| {
                    self.locals
                        .has(arr_name)
                        .then(|| self.locals.type_of(arr_name).clone())
                });
            if let Some(ty) = arr_ty
                && let Some(elem) = ty.array_elem()
                && elem.is_char_like()
            {
                return true;
            }
        }
        // `*p` for `p` a char-pointer ident — same reasoning.
        if let ExprKind::Deref(inner) = &e.kind
            && let ExprKind::Ident(pname) = &inner.kind
            && let Some(pointee) = self.ident_pointee(pname)
            && pointee.is_char_like()
        {
            return true;
        }
        // `arr[i][j]` for `arr` an array of char-pointers (or a
        // pointer-to-char-pointer). The outer subscript yields a
        // `char *`, the inner one a byte. try_lvalue_chain_addr
        // stops at the outer pointer level so doesn't see the
        // final byte; we recognize the shape directly. Fixtures
        // 2231, 2345.
        if let ExprKind::ArrayIndex { array: outer, .. } = &e.kind
            && let ExprKind::ArrayIndex { array: inner, .. } = &outer.kind
            && let ExprKind::Ident(base) = &inner.kind
        {
            let outer_ty = self
                .globals
                .type_of(base)
                .cloned()
                .or_else(|| self.locals.has(base).then(|| self.locals.type_of(base).clone()));
            if let Some(ty) = outer_ty
                && let Some(level1) = ty.array_elem().or_else(|| ty.pointee())
                && let Some(level2) = level1.pointee()
                && level2.is_char_like()
            {
                return true;
            }
        }
        false
    }
}
