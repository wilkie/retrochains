use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// Emit `expr` for its side effects, discarding the value. The
    /// special case is `Update` (`++x;` / `x++;`): BCC emits just the
    /// increment, no `mov ax, ...` afterward (fixture 040). Likewise
    /// for an assignment expression in a `for`-clause: emit the
    /// side-effect store, no value-load afterward.
    pub(crate) fn emit_expr_discard(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Update { target, op, position } => {
                self.emit_update_in_place(target, *op, *position);
            }
            ExprKind::AssignExpr { target, value } => {
                // Global LHS shadowed by a local is handled at
                // statement-level; here in discard context we
                // dispatch to whichever table owns the name. A
                // global without a corresponding local routes to
                // emit_assign_global. Fixture 3509.
                if !self.locals.has(target) && self.globals.contains(target) {
                    self.emit_assign_global(target, value);
                } else {
                    let loc = self.locals.location_of(target);
                    let ty = self.locals.type_of(target).clone();
                    self.emit_assign_local(loc, &ty, value);
                }
            }
            ExprKind::CompoundAssignExpr { target, op, value } => {
                self.emit_compound_assign(target, *op, value);
            }
            ExprKind::UpdateLvalue { target, op, position: _ } => {
                // Discard-position UpdateLvalue: just emit the
                // increment/decrement, no value load. BCC's exact
                // pre-vs-post distinction collapses here because
                // the value is unused. Today only the deref-of-
                // ident target shape is supported (the only one
                // parse_atom produces). Fixtures 714 / 715 / 1344
                // / 2331 / 3376 (`(*p)++;` at stmt level).
                let ExprKind::Deref(inner) = &target.kind else {
                    panic!("UpdateLvalue target shape not supported in discard");
                };
                let ExprKind::Ident(p_name) = &inner.kind else {
                    panic!("UpdateLvalue deref-target must be an ident");
                };
                let LocalLocation::Reg(reg) = self.locals.location_of(p_name) else {
                    panic!("stack-resident pointer in `(*p)++;` not yet supported");
                };
                let r = reg.name();
                let mnem = match op {
                    UpdateOp::Inc => "inc",
                    UpdateOp::Dec => "dec",
                };
                let pointee = self
                    .locals
                    .type_of(p_name)
                    .pointee()
                    .expect("p must be a pointer")
                    .clone();
                let width = if pointee.is_char_like() { "byte" } else { "word" };
                let _ = write!(self.out, "\t{mnem}\t{width} ptr [{r}]\r\n");
            }
            ExprKind::Comma { left, right } => {
                // Both halves of a comma in discard position are
                // themselves discarded — neither contributes a value.
                // Fixture 469's `a = 1, b = 2, ...` chain.
                self.emit_expr_discard(left);
                self.emit_expr_discard(right);
            }
            ExprKind::Ternary { cond, then_value, else_value }
                if matches!(then_value.kind, ExprKind::Update { .. })
                    && matches!(else_value.kind, ExprKind::Update { .. }) =>
            {
                // `cond ? <update> : <update>` as a statement: the
                // ternary's value is discarded, so each arm's Update
                // can fire in-place (no load-then-update for postinc)
                // and BCC still emits the wasted `mov ax, <reg>` that
                // the ternary materialization wants. Inversion of
                // emit order vs the value-producing path: update
                // first, then the dead AX load. Fixture 1202
                // (`a > 0 ? a++ : a--;`).
                let base = self.label_plan.base(expr.span.start, expr.span.end);
                let false_slot = base + 1;
                let merge_slot = base + 2;
                let (t_true, _) = self.emit_cond_test(cond);
                // Invert the true mnemonic to its false form to fall
                // into the else arm via `jcc` to the false slot.
                let inv = match t_true {
                    "je" => "jne",
                    "jne" => "je",
                    "jl" => "jge",
                    "jge" => "jl",
                    "jg" => "jle",
                    "jle" => "jg",
                    "jb" => "jae",
                    "jae" => "jb",
                    "ja" => "jbe",
                    "jbe" => "ja",
                    _ => panic!("unknown jcc {t_true}"),
                };
                let _ = write!(self.out, "\t{inv}\tshort {}\r\n", self.label_ref(false_slot));
                let emit_arm = |this: &mut Self, target: &str, op: UpdateOp, position: UpdatePosition| {
                    this.emit_update_in_place(target, op, position);
                    // After the in-place update, materialize the
                    // (now-stale) value into AX. BCC emits this
                    // load even though the ternary's value is
                    // discarded — the ternary's value-producing
                    // skeleton requires AX. Reg-resident local:
                    // `mov ax, <reg>`. Int global: `mov ax, word
                    // ptr DGROUP:_<g>`. Other shapes haven't been
                    // pinned yet.
                    if this.locals.has(target) {
                        if let LocalLocation::Reg(reg) = this.locals.location_of(target) {
                            let _ = write!(this.out, "\tmov\tax,{}\r\n", reg.name());
                        }
                    } else if let Some(gty) = this.globals.type_of(target)
                        && matches!(gty, Type::Int | Type::UInt)
                    {
                        let _ = write!(
                            this.out,
                            "\tmov\tax,word ptr DGROUP:_{target}\r\n",
                        );
                    }
                };
                if let ExprKind::Update { target, op, position } = &then_value.kind {
                    emit_arm(self, target, *op, *position);
                }
                let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(merge_slot));
                self.emit_label(false_slot);
                if let ExprKind::Update { target, op, position } = &else_value.kind {
                    emit_arm(self, target, *op, *position);
                }
                self.emit_label(merge_slot);
            }
            _ => {
                self.emit_expr_to_ax(expr);
            }
        }
    }
    /// Emit a prefix unary operator. The operand always lands in AX
    /// first, then the per-op tail runs:
    ///
    /// - `-e` → `neg ax`.
    /// - `~e` → `not ax`.
    /// - `!e` → `neg ax / sbb ax,ax / inc ax`. Classic zero-test:
    ///   after `neg`, CF == (operand != 0); `sbb ax,ax` materializes
    ///   `-CF` (0 or 0xFFFF); `inc ax` shifts to 1 or 0. Fixture 038.
    pub(crate) fn emit_unary(&mut self, op: UnaryOp, operand: &Expr) {
        self.emit_expr_to_ax(operand);
        match op {
            UnaryOp::Neg => self.out.extend_from_slice(b"\tneg\tax\r\n"),
            UnaryOp::BitNot => self.out.extend_from_slice(b"\tnot\tax\r\n"),
            UnaryOp::Not => {
                self.out.extend_from_slice(b"\tneg\tax\r\n");
                self.out.extend_from_slice(b"\tsbb\tax,ax\r\n");
                self.out.extend_from_slice(b"\tinc\tax\r\n");
            }
        }
    }
    /// Emit code that leaves the value of `e` in AX.
    pub(crate) fn emit_expr_to_ax(&mut self, e: &Expr) {
        // `arr[i].a + arr[i].b + arr[i].c` (or any +-chain of field
        // accesses against the same stack-array-of-struct base with
        // the same non-const index): BCC's lowering computes each
        // field's address fresh via an imul-by-stride prelude that
        // clobbers AX, so it stashes the accumulating sum on the
        // stack between operands. The chain emit is:
        //
        //   <addr-of arr[i].field_0>; mov ax, [bx]; push ax
        //   <addr-of arr[i].field_1>; pop ax; add ax, [bx]; push ax
        //   ...
        //   <addr-of arr[i].field_n>; pop ax; add ax, [bx]
        //
        // The `<addr-of>` prelude is:
        //   mov ax, <i>; mov dx, <stride>; imul dx;
        //   lea dx, [bp+arr_base+field_off]; add ax, dx; mov bx, ax
        //
        // Fixture 1914 (`struct R { int a,b,c; } arr[3];` chain).
        if let Some((arr_base, idx_addr, stride, field_offs)) =
            self.match_arr_var_field_add_chain(e)
            && field_offs.len() >= 2
        {
            for (i, f_off) in field_offs.iter().enumerate() {
                if i > 0 {
                    self.out.extend_from_slice(b"\tpush\tax\r\n");
                }
                self.emit_arr_var_field_addr_to_bx(&idx_addr, stride, arr_base, *f_off);
                if i == 0 {
                    self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
                } else {
                    self.out.extend_from_slice(b"\tpop\tax\r\n");
                    self.out.extend_from_slice(b"\tadd\tax,word ptr [bx]\r\n");
                }
            }
            return;
        }
        // Huge-pointer subtraction `p2 - p1` — produces a long
        // element-difference value. BCC's emission:
        //   xor ax, ax            ; high half of element-size long
        //   mov dx, <stride>      ; low half
        //   push ax / push dx     ; stride pushed first (becomes the
        //                         ; divisor on the LDIV stack)
        //   mov dx, [p2+2]        ; LHS seg
        //   mov ax, [p2]          ; LHS off
        //   mov cx, [p1+2]        ; RHS seg
        //   mov bx, [p1]          ; RHS off
        //   call N_PSBP@          ; returns byte-diff long in DX:AX
        //   push dx / push ax     ; dividend pushed second
        //   call N_LDIV@          ; result long element-diff in DX:AX
        // The pre-call pushes get cleaned by the Pascal-style ret N
        // tail of each helper, so no caller `add sp,N` is emitted.
        // Fixture 1773 (`(int)(p2 - p1)`).
        if let ExprKind::BinOp { op: BinOp::Sub, left, right } = &e.kind
            && let (Some(l_off), Some(r_off)) =
                (self.huge_ptr_lvalue_addr(left), self.huge_ptr_lvalue_addr(right))
            && let ExprKind::Ident(l_name) = &left.kind
            && let Some(pointee) = self.locals.type_of(l_name).pointee()
        {
            let stride = pointee.size_bytes();
            self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            let _ = write!(self.out, "\tmov\tdx,{stride}\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(l_off + 2));
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(l_off));
            let _ = write!(self.out, "\tmov\tcx,word ptr {}\r\n", bp_addr(r_off + 2));
            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(r_off));
            self.out.extend_from_slice(b"\tcall\tnear ptr N_PSBP@\r\n");
            self.helpers.insert("N_PSBP@".to_string());
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LDIV@\r\n");
            self.helpers.insert("N_LDIV@".to_string());
            return;
        }
        if let Some(v) = try_const_eval(e) {
            // Narrow to 16 bits — BCC writes signed-negative constants
            // as their unsigned-wrapped form (fixture 036: `-5` →
            // `mov ax,65531`).
            let v16 = v & 0xFFFF;
            if v16 == 0 {
                self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{v16}\r\n");
            }
            return;
        }
        // `(++i) * (++i)` (macro-arg side effect form): both
        // operands are Pre-Update on the same reg-resident int
        // local. Emit `inc reg; mov ax, reg; inc reg; mov dx, reg;
        // imul dx`. Fixture 2293.
        if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &e.kind
            && let ExprKind::Update { target: lt, op: lo, position: UpdatePosition::Pre } = &left.kind
            && let ExprKind::Update { target: rt, op: ro, position: UpdatePosition::Pre } = &right.kind
            && lt == rt
            && lo == ro
            && self.locals.has(lt)
            && let LocalLocation::Reg(reg) = self.locals.location_of(lt)
            && self.locals.type_of(lt).is_int_like()
            && !reg.is_byte()
        {
            let reg_name = reg.name();
            let mnem = match lo {
                UpdateOp::Inc => "inc",
                UpdateOp::Dec => "dec",
            };
            let _ = write!(self.out, "\t{mnem}\t{reg_name}\r\n");
            let _ = write!(self.out, "\tmov\tax,{reg_name}\r\n");
            let _ = write!(self.out, "\t{mnem}\t{reg_name}\r\n");
            let _ = write!(self.out, "\tmov\tdx,{reg_name}\r\n");
            self.out.extend_from_slice(b"\timul\tdx\r\n");
            return;
        }
        // Bitfield BinOp chain: when a left-associative chain of
        // BinOp(Add|Sub|...) bottoms out at bitfield reads on both
        // sides, emit the canonical BCC sequence — first operand
        // materialized into AX via `mov al,…; shr;…; and ax,…`,
        // each subsequent operand into DX the same way, with
        // `<op> ax, dx` folding them in. Falls through if any
        // operand isn't a within-byte bitfield read. Fixture 1691.
        if let Some(()) = self.try_emit_bitfield_chain_to_ax(e) {
            return;
        }
        // Single bitfield read into AX. Catches `return s.<bf>`
        // and similar isolated rvalue contexts. Fixture 1691.
        if let Some(bf) = self.resolve_bitfield(e) {
            self.emit_bitfield_read_to_reg(&bf, "ax", "al");
            return;
        }
        // `*(*pp)++` peephole: a Deref of a postfix ++ of a Deref of
        // a named pointer. BCC fuses the load + increment + outer-
        // deref into three instructions: cache `*pp` in BX, advance
        // `*pp` in place by the pointee's stride, then load through
        // BX. Fixture 3662 (`int **pp; return *(*pp)++;`).
        // `(*p)++` / `(*p)--` / `++(*p)` / `--(*p)` in value
        // position. Post: load pre-update value into AX, then
        // increment/decrement in place. Pre: increment first, then
        // load. Fixtures 2857, 3107, 2449 (post), 2762, 3110 (pre).
        if let ExprKind::UpdateLvalue { target, op, position } = &e.kind
            && let ExprKind::Deref(inner) = &target.kind
            && let ExprKind::Ident(p_name) = &inner.kind
            && self.locals.has(p_name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
        {
            let r = reg.name();
            let ptr_ty = self.locals.type_of(p_name).clone();
            let pointee = ptr_ty.pointee().expect("p must be a pointer").clone();
            let mnem = match op {
                UpdateOp::Inc => "inc",
                UpdateOp::Dec => "dec",
            };
            let width = if pointee.is_char_like() { "byte" } else { "word" };
            match position {
                UpdatePosition::Post => {
                    if pointee.is_char_like() {
                        let _ = write!(self.out, "\tmov\tal,byte ptr [{r}]\r\n");
                        self.emit_widen_al(&pointee);
                    } else {
                        let _ = write!(self.out, "\tmov\tax,word ptr [{r}]\r\n");
                    }
                    let _ = write!(self.out, "\t{mnem}\t{width} ptr [{r}]\r\n");
                }
                UpdatePosition::Pre => {
                    let _ = write!(self.out, "\t{mnem}\t{width} ptr [{r}]\r\n");
                    if pointee.is_char_like() {
                        let _ = write!(self.out, "\tmov\tal,byte ptr [{r}]\r\n");
                        self.emit_widen_al(&pointee);
                    } else {
                        let _ = write!(self.out, "\tmov\tax,word ptr [{r}]\r\n");
                    }
                }
            }
            return;
        }
        // `*(*pp)++` peephole: a Deref of a postfix ++ of a Deref of
        // a named pointer. BCC fuses the load + increment + outer-
        // deref into three instructions: cache `*pp` in BX, advance
        // `*pp` in place by the pointee's stride, then load through
        // BX. Fixture 3662 (`int **pp; return *(*pp)++;`).
        if let ExprKind::Deref(outer_inner) = &e.kind
            && let ExprKind::UpdateLvalue { target, op, position } = &outer_inner.kind
            && let ExprKind::Deref(target_inner) = &target.kind
            && let ExprKind::Ident(pp_name) = &target_inner.kind
            && matches!(position, UpdatePosition::Post)
            && self.locals.has(pp_name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(pp_name)
        {
            // `*pp` is an `int *` (or similar) — the pointee of
            // that is what the OUTER deref reads, so the stride is
            // the pointee-of-pointee's size.
            let pp_ty = self.locals.type_of(pp_name).clone();
            let inner_ptr = pp_ty.pointee().expect("pp must be a pointer").clone();
            let leaf = inner_ptr.pointee().expect("*pp must be a pointer").clone();
            let stride = i32::from(inner_ptr.pointee().expect("*pp must be a pointer").size_bytes());
            let r = reg.name();
            let _ = write!(self.out, "\tmov\tbx,word ptr [{r}]\r\n");
            let signed_stride = match op {
                UpdateOp::Inc => stride,
                UpdateOp::Dec => -stride,
            };
            let _ = write!(
                self.out,
                "\tadd\tword ptr [{r}],{signed_stride}\r\n",
            );
            if leaf.is_char_like() {
                self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                self.emit_widen_al(&leaf);
            } else {
                self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
            }
            return;
        }
        match &e.kind {
            ExprKind::IntLit(_) => unreachable!("literals fold via try_const_eval"),
            ExprKind::PseudoReg(name) => {
                // Pseudo-register read in expression position. `_AX`
                // is the live AX (no-op load). Word pseudos copy via
                // `mov ax, <reg>`. Byte pseudos widen as unsigned
                // char: `_AL` is already in AL, just clear AH. Fixture
                // 4052 (`_AL = 0x80; return _AL;` → `mov ah, 0`).
                // `_FLAGS` value-context (`pushf; pop ax`) is handled
                // by its own slice.
                if name == "_AX" {
                    return;
                }
                if name == "_AL" {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    return;
                }
                if name == "_FLAGS" {
                    // `_FLAGS` value-context: BCC materializes the
                    // flags word via `pushf; pop ax`. The surrounding
                    // expression (`_FLAGS & K`) continues normally
                    // after that. Fixture 4062 (`return _FLAGS & 1;`).
                    self.out.extend_from_slice(b"\tpushf\t\r\n");
                    self.out.extend_from_slice(b"\tpop\tax\r\n");
                    return;
                }
                if is_byte_pseudo_register(name) {
                    panic!("byte pseudo-register `{name}` read in int context not yet supported (only `_AL` covered)");
                }
                let reg = pseudo_register_operand(name)
                    .expect("PseudoReg variant carries a valid pseudo name");
                let _ = write!(self.out, "\tmov\tax,{reg}\r\n");
            }
            ExprKind::FloatLit(_) | ExprKind::DoubleLit(_) => {
                // Float/double rvalue at this site means the constant
                // is being consumed as an integer-AX value, which the
                // FPU codegen handles via a different path. Hitting
                // here means we didn't route an FP context correctly.
                panic!("float literal in integer-AX context not supported yet");
            }
            ExprKind::UpdateLvalue { target, op, position } => {
                // `<arr>[K]++` / `--` on a stack int array with a
                // constant index: load the pre-update value into AX,
                // then `inc`/`dec` the memory in place. Post-form
                // returns the pre-update value (already in AX). Pre-
                // form would mutate first then load — not exercised
                // by any fixture today. Fixture 1418.
                if let ExprKind::ArrayIndex { array, index } = &target.kind
                    && let ExprKind::Ident(arr_name) = &array.kind
                    && self.locals.has(arr_name)
                    && let arr_ty = self.locals.type_of(arr_name).clone()
                    && let Some(elem_ty) = arr_ty.array_elem()
                    && elem_ty.is_int_like()
                    && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
                    && let Some(k) = try_const_eval(index)
                    && matches!(position, UpdatePosition::Post)
                {
                    let stride = i32::from(elem_ty.size_bytes());
                    let elem_off = i32::from(base_off) + (k as i32) * stride;
                    let elem_off_i16 = i16::try_from(elem_off).expect("elem offset fits in i16");
                    let mnem = match op {
                        UpdateOp::Inc => "inc",
                        UpdateOp::Dec => "dec",
                    };
                    let _ = write!(
                        self.out,
                        "\tmov\tax,word ptr {}\r\n",
                        bp_addr(elem_off_i16),
                    );
                    let _ = write!(
                        self.out,
                        "\t{mnem}\tword ptr {}\r\n",
                        bp_addr(elem_off_i16),
                    );
                    return;
                }
                // Same shape but the array is a file-scope global.
                // Fixture 2700 (Post: `a[1]++`), 2616 (Pre: `++a[0]`).
                if let ExprKind::ArrayIndex { array, index } = &target.kind
                    && let ExprKind::Ident(arr_name) = &array.kind
                    && !self.locals.has(arr_name)
                    && let Some(arr_ty) = self.globals.type_of(arr_name)
                    && let Some(elem_ty) = arr_ty.array_elem()
                    && elem_ty.is_int_like()
                    && let Some(k) = try_const_eval(index)
                {
                    let stride = u32::from(elem_ty.size_bytes());
                    let off = k.wrapping_mul(stride);
                    let addr = if off == 0 {
                        format!("DGROUP:_{arr_name}")
                    } else {
                        format!("DGROUP:_{arr_name}+{off}")
                    };
                    let mnem = match op {
                        UpdateOp::Inc => "inc",
                        UpdateOp::Dec => "dec",
                    };
                    match position {
                        UpdatePosition::Post => {
                            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
                            let _ = write!(self.out, "\t{mnem}\tword ptr {addr}\r\n");
                        }
                        UpdatePosition::Pre => {
                            let _ = write!(self.out, "\t{mnem}\tword ptr {addr}\r\n");
                            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
                        }
                    }
                    return;
                }
                // Variable index `arr[i]++` / `++arr[i]` on a
                // file-scope int array: scale i into BX, then for
                // Post emit `mov ax, ..[bx]; <mnem> word ptr ..[bx]`
                // (load pre-update value, then mutate); for Pre
                // emit `<mnem> word ptr ..[bx]; mov ax, ..[bx]`
                // (mutate first, load post-update value).
                // Fixtures 3032 (Post), 2937 (Pre).
                if let ExprKind::ArrayIndex { array, index } = &target.kind
                    && let ExprKind::Ident(arr_name) = &array.kind
                    && !self.locals.has(arr_name)
                    && let Some(arr_ty) = self.globals.type_of(arr_name)
                    && let Some(elem_ty) = arr_ty.array_elem()
                    && elem_ty.is_int_like()
                {
                    let elem_ty = elem_ty.clone();
                    let mnem = match op {
                        UpdateOp::Inc => "inc",
                        UpdateOp::Dec => "dec",
                    };
                    self.emit_index_into_bx(index, &elem_ty);
                    match position {
                        UpdatePosition::Post => {
                            let _ = write!(
                                self.out,
                                "\tmov\tax,word ptr DGROUP:_{arr_name}[bx]\r\n",
                            );
                            let _ = write!(
                                self.out,
                                "\t{mnem}\tword ptr DGROUP:_{arr_name}[bx]\r\n",
                            );
                        }
                        UpdatePosition::Pre => {
                            let _ = write!(
                                self.out,
                                "\t{mnem}\tword ptr DGROUP:_{arr_name}[bx]\r\n",
                            );
                            let _ = write!(
                                self.out,
                                "\tmov\tax,word ptr DGROUP:_{arr_name}[bx]\r\n",
                            );
                        }
                    }
                    return;
                }
                // Member access `s.x` or `p->x` resolving to a
                // constant address: inc/dec word/byte ptr at the
                // address, then load it. Post order swaps the
                // two halves. Fixture 3444 (`++s.x`).
                if let Some((name, total_off, leaf_ty)) = self.try_lvalue_chain_addr(target)
                    && let Some(addr) = self.resolve_chain_addr(&name, total_off)
                    && leaf_ty.is_int_like()
                {
                    let mnem = match op {
                        UpdateOp::Inc => "inc",
                        UpdateOp::Dec => "dec",
                    };
                    let width = if leaf_ty.is_char_like() { "byte" } else { "word" };
                    let load_reg = if leaf_ty.is_char_like() { "al" } else { "ax" };
                    match position {
                        UpdatePosition::Post => {
                            let _ = write!(self.out, "\tmov\t{load_reg},{width} ptr {addr}\r\n");
                            let _ = write!(self.out, "\t{mnem}\t{width} ptr {addr}\r\n");
                        }
                        UpdatePosition::Pre => {
                            let _ = write!(self.out, "\t{mnem}\t{width} ptr {addr}\r\n");
                            let _ = write!(self.out, "\tmov\t{load_reg},{width} ptr {addr}\r\n");
                        }
                    }
                    if leaf_ty.is_char_like() {
                        self.emit_widen_al(&leaf_ty);
                    }
                    return;
                }
                panic!(
                    "UpdateLvalue in integer-AX context only supported via the \
                     `*(*pp)++` outer-deref peephole today; the operand was {:?}",
                    e.kind
                );
            }
            ExprKind::Ident(name) => {
                // A local shadows a global of the same name (fixture
                // 532), so only take the global path when no local
                // with this name is in scope.
                // Globals first: if this name is file-scope, lower
                // to a `<width> ptr DGROUP:_<name>` reference rather
                // than a stack/register access (fixtures 083–087).
                // Bare function name in a value context — decay to
                // its offset (a near-pointer to code in the small
                // model). Used when a function is passed as an
                // argument or assigned to a function-pointer variable.
                // Fixtures 2252, 2442.
                if !self.locals.has(name)
                    && self.globals.type_of(name).is_none()
                    && self.signatures.ret_ty_of(name).is_some()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tax,offset _{name}\r\n",
                    );
                    return;
                }
                if !self.locals.has(name)
                    && let Some(gty) = self.globals.type_of(name)
                {
                    if matches!(gty, Type::Array { .. }) {
                        // Global array decay: the value of `arr` is
                        // its address (element 0). Direct
                        // `mov ax, offset DGROUP:_arr` (linker-
                        // resolved). Fixture 3437.
                        let _ = write!(
                            self.out,
                            "\tmov\tax,offset DGROUP:_{name}\r\n",
                        );
                        return;
                    }
                    if gty.is_char_like() {
                        let _ = write!(
                            self.out,
                            "\tmov\tal,byte ptr DGROUP:_{name}\r\n",
                        );
                        if gty.is_unsigned() {
                            // Unsigned char: zero-extend via `mov ah,0`
                            // (B4 00, 2 bytes) — preserves the upper
                            // bits as 0 instead of sign-extending the
                            // 7th bit. Fixture 460.
                            self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                        } else {
                            self.out.extend_from_slice(b"\tcbw\t\r\n");
                        }
                    } else {
                        let _ = write!(
                            self.out,
                            "\tmov\tax,word ptr DGROUP:_{name}\r\n",
                        );
                    }
                    return;
                }
                let ty = self.locals.type_of(name).clone();
                // Array-name decay: when the name refers to a local
                // of array type and we're reading its *value*, the
                // value is the address of element 0. Fixture 090
                // (`int *p = a;`) and fixture 095 (`sum(a)`) both
                // exercise this. Emitted exactly like `&a[0]`.
                if matches!(ty, Type::Array { .. }) {
                    let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                        unreachable!("array `{name}` should be stack-resident");
                    };
                    let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
                    return;
                }
                match self.locals.location_of(name) {
                    LocalLocation::Stack(off) if ty.is_char_like() => {
                        // Char on stack into AX: load AL then widen.
                        // Signed: `cbw` (1 byte). Unsigned:
                        // `mov ah,0` (2 bytes). Fixture 461.
                        let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                        if ty.is_unsigned() {
                            self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                        } else {
                            self.out.extend_from_slice(b"\tcbw\t\r\n");
                        }
                    }
                    LocalLocation::Stack(off) => {
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                    }
                    LocalLocation::Reg(reg) if reg.is_byte() => {
                        // Char in a byte register into AX: copy AL then
                        // widen. Fixture 053 / 461 (register-resident
                        // uchar). Signed picks `cbw`; unsigned picks
                        // `mov ah,0`.
                        let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                        if ty.is_unsigned() {
                            self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                        } else {
                            self.out.extend_from_slice(b"\tcbw\t\r\n");
                        }
                    }
                    LocalLocation::Reg(reg) => {
                        let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                    }
                }
            }
            ExprKind::BinOp { op, left, right } => {
                // Identity / annihilator folds when the LHS is a pure
                // ident (no side effects to preserve):
                //   `<ident> * 0` → `xor ax, ax`
                //   `<ident> * 1` → `<load ident>` (LHS unchanged)
                //   `<ident> / 1` → `<load ident>` (LHS unchanged)
                //   `<ident> % 1` → `xor ax, ax` (result is always 0)
                // BCC folds these to drop the multiply/divide entirely.
                // Fixtures 2011 (`x * 0`), 2014 (`x / 1`), 2391
                // (`x % 1`).
                if (matches!(op, BinOp::Mul) && try_const_eval(right) == Some(0))
                    || (matches!(op, BinOp::Mod) && try_const_eval(right) == Some(1))
                {
                    if matches!(left.kind, ExprKind::Ident(_)) {
                        self.out.extend_from_slice(b"\txor\tax,ax\r\n");
                        return;
                    }
                }
                if matches!(op, BinOp::Mul | BinOp::Div)
                    && matches!(left.kind, ExprKind::Ident(_))
                    && try_const_eval(right) == Some(1)
                {
                    self.emit_expr_to_ax(left);
                    return;
                }
                // `<x> << (<a> + <b>)` / `>>` where the shift count
                // is the byte-level sum of two int memory lvalues.
                // BCC computes the sum directly in CL using two
                // byte-form `mov`/`add` (`mov cl, [a]; add cl, [b]`)
                // and avoids the 16-bit AX-route and `mov cl, dl`
                // shuffle. Fixture 3634 (`return x << (a + b)`).
                let shift_by_byte_sum_src = if matches!(op, BinOp::Shl | BinOp::Shr)
                    && let ExprKind::BinOp { op: BinOp::Add, left: rl, right: rr } = &right.kind
                    && let Some(rl_addr) = self.int_lvalue_addr(rl)
                    && let Some(rr_addr) = self.int_lvalue_addr(rr)
                    && self.try_op_source(left).is_some()
                {
                    Some((rl_addr, rr_addr))
                } else {
                    None
                };
                if op.is_comparison() {
                    self.emit_comparison_as_value(e.span.start, e.span.end, *op, left, right);
                } else if let Some((rl_addr, rr_addr)) = shift_by_byte_sum_src {
                    let signed = !self.expr_is_unsigned(left);
                    let mnem = match (op, signed) {
                        (BinOp::Shl, _) => "shl",
                        (BinOp::Shr, false) => "shr",
                        (BinOp::Shr, true) => "sar",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tmov\tcl,byte ptr {rl_addr}\r\n");
                    let _ = write!(self.out, "\tadd\tcl,byte ptr {rr_addr}\r\n");
                    self.emit_expr_to_ax(left);
                    let _ = write!(self.out, "\t{mnem}\tax,cl\r\n");
                } else if matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Mul)
                    && let ExprKind::ArrayIndex { array, index } = &right.kind
                    && let ExprKind::Ident(arr_name) = &array.kind
                    && try_const_eval(index).is_none()
                    && let Some((elem_ty, addr_emit)) =
                        self.classify_var_idx_array(arr_name)
                    && elem_ty.is_int_like()
                    && self.is_simple_lvalue(left)
                {
                    // Variable-indexed int-array RHS with simple LHS:
                    // BCC computes &arr[i] into BX FIRST, then loads
                    // LHS into AX, then `<op> ax, <addr>`. Avoids
                    // clobbering AX with the index-scale step.
                    // Fixtures 2454 (`total + a[i]`), 2849, 3003.
                    let elem_ty = elem_ty.clone();
                    let addr = match addr_emit {
                        VarIdxKind::StackArr(base_off, elem_sz) => {
                            self.emit_array_addr_to_bx(
                                arr_name, index, base_off, elem_sz,
                            );
                            "word ptr [bx]".to_owned()
                        }
                        VarIdxKind::PtrInt => {
                            let stride = u32::from(elem_ty.size_bytes());
                            self.emit_expr_to_ax(index);
                            for _ in 0..stride.trailing_zeros() {
                                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                            }
                            match self.locals.location_of(arr_name) {
                                LocalLocation::Reg(reg) => {
                                    let _ = write!(self.out, "\tadd\tax,{}\r\n", reg.name());
                                    self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                                }
                                LocalLocation::Stack(off) => {
                                    let _ = write!(
                                        self.out,
                                        "\tmov\tbx,word ptr {}\r\n",
                                        bp_addr(off),
                                    );
                                    self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
                                }
                            }
                            "word ptr [bx]".to_owned()
                        }
                        VarIdxKind::GlobalArr => {
                            self.emit_index_into_bx(index, &elem_ty);
                            format!("word ptr DGROUP:_{arr_name}[bx]")
                        }
                    };
                    self.emit_expr_to_ax(left);
                    let mnem = match op {
                        BinOp::Add => "add",
                        BinOp::Sub => "sub",
                        BinOp::BitAnd => "and",
                        BinOp::BitOr => "or",
                        BinOp::BitXor => "xor",
                        BinOp::Mul => {
                            let _ = write!(self.out, "\timul\t{addr}\r\n");
                            return;
                        }
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\t{mnem}\tax,{addr}\r\n");
                    return;
                } else {
                    // `<char_lvalue> <bitop> <char_lvalue>` — byte op
                    // in AL, single cbw at the end. BCC emits
                    // `mov al, [l]; or al, [r]; cbw` for the
                    // char-or-char case (fixture 1375). Pre-peephole
                    // we widened first and used the word form. Limit
                    // to bitops where C's per-bit semantics are the
                    // same at byte and word width once widened.
                    if matches!(op, BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor)
                        && let Some((l_name, l_off, l_ty)) =
                            self.try_lvalue_chain_addr(left)
                        && let Some((r_name, r_off, r_ty)) =
                            self.try_lvalue_chain_addr(right)
                        && l_ty.is_char_like()
                        && r_ty.is_char_like()
                    {
                        let l_addr = self.resolve_chain_addr(&l_name, l_off);
                        let r_addr = self.resolve_chain_addr(&r_name, r_off);
                        if let (Some(la), Some(ra)) = (l_addr, r_addr) {
                            let mnem = match op {
                                BinOp::BitOr => "or",
                                BinOp::BitAnd => "and",
                                BinOp::BitXor => "xor",
                                _ => unreachable!(),
                            };
                            let _ = write!(self.out, "\tmov\tal,byte ptr {la}\r\n");
                            let _ = write!(self.out, "\t{mnem}\tal,byte ptr {ra}\r\n");
                            self.out.extend_from_slice(b"\tcbw\t\r\n");
                            return;
                        }
                    }
                    // `<ptr-or-array> - <ptr-or-array>` for same-typed
                    // elements — C gives an `int` count of elements
                    // (byte diff / sizeof(elem)). After the byte
                    // subtract, divide AX by stride. Char-typed
                    // elements need no divide; int/long use `idiv bx`
                    // (signed) since ptrdiff is signed. Either side
                    // may be a pointer or array-decay. Fixtures
                    // 1208 (`q - p` for int* pair), 2347 (`p - s`
                    // for `char *p, char s[6]`).
                    let elem_of = |this: &Self, name: &str| -> Option<Type> {
                        this.ident_pointee(name).or_else(|| {
                            let ty = this.globals.type_of(name).cloned()
                                .or_else(|| this.locals.has(name)
                                    .then(|| this.locals.type_of(name).clone()))?;
                            ty.array_elem().cloned()
                        })
                    };
                    if matches!(op, BinOp::Sub)
                        && let ExprKind::Ident(l_name) = &left.kind
                        && let ExprKind::Ident(r_name) = &right.kind
                        && let Some(l_elem) = elem_of(self, l_name)
                        && let Some(r_elem) = elem_of(self, r_name)
                        && l_elem.size_bytes() == r_elem.size_bytes()
                    {
                        let stride = l_elem.size_bytes();
                        let rhs_is_array_lvalue = self.globals
                            .type_of(r_name)
                            .map_or(false, |t| matches!(t, Type::Array { .. }))
                            || (self.locals.has(r_name)
                                && matches!(self.locals.type_of(r_name), Type::Array { .. }));
                        if rhs_is_array_lvalue {
                            // RHS is an array — we need its ADDRESS,
                            // not its content. BCC's shape:
                            //   lea ax, &r ; push ax
                            //   <lhs into ax>
                            //   pop dx ; sub ax, dx
                            // Fixture 2347 (`p - s` for `char *p,
                            // char s[6]`).
                            if let Some(_g) = self.globals.type_of(r_name) {
                                let _ = write!(
                                    self.out,
                                    "\tmov\tax,offset DGROUP:_{r_name}\r\n",
                                );
                            } else if let LocalLocation::Stack(off) =
                                self.locals.location_of(r_name)
                            {
                                let _ = write!(
                                    self.out,
                                    "\tlea\tax,word ptr {}\r\n",
                                    bp_addr(off),
                                );
                            }
                            self.out.extend_from_slice(b"\tpush\tax\r\n");
                            self.emit_expr_to_ax(left);
                            self.out.extend_from_slice(b"\tpop\tdx\r\n");
                            self.out.extend_from_slice(b"\tsub\tax,dx\r\n");
                        } else {
                            self.emit_expr_to_ax(left);
                            self.emit_binary_right(BinOp::Sub, right, false);
                        }
                        if stride > 1 {
                            let _ = write!(self.out, "\tmov\tbx,{stride}\r\n");
                            self.out.extend_from_slice(b"\tcwd\t\r\n");
                            self.out.extend_from_slice(b"\tidiv\tbx\r\n");
                        }
                        return;
                    }
                    // `<global_arr> + <int_expr>` — pointer arithmetic
                    // for an array-decay base. BCC evaluates the int
                    // operand into AX, scales by `sizeof(elem)` via
                    // `shl ax, 1` chains, then adds the symbol offset
                    // as a link-time-resolved immediate. Saves the
                    // push/pop dance the generic rhs_clobbers_ax path
                    // would produce. Fixture 3439 (`arr + (c ? 1 : 2)`
                    // for `int arr[10]`). Stride 1 (char[]) handled
                    // by the generic path — no scale needed.
                    if matches!(op, BinOp::Add)
                        && let ExprKind::Ident(arr_name) = &left.kind
                        && !self.locals.has(arr_name)
                        && let Some(gty) = self.globals.type_of(arr_name)
                        && let Some(elem_ty) = gty.array_elem()
                        && elem_ty.size_bytes() > 1
                        && try_const_eval(right).is_none()
                    {
                        let stride = u16::from(elem_ty.size_bytes());
                        self.emit_expr_to_ax(right);
                        for _ in 0..stride.trailing_zeros() {
                            self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                        }
                        let _ = write!(
                            self.out,
                            "\tadd\tax,offset DGROUP:_{arr_name}\r\n",
                        );
                        return;
                    }
                    // `<ptr-lvalue> + <int-expr>` / `- <int-expr>` —
                    // pointer arithmetic when the LHS is a pointer
                    // ident (not array decay). Scale the int by
                    // sizeof(pointee), push, load pointer, pop, then
                    // add/sub. Stride scaling uses shl for powers of
                    // two; imul for arbitrary strides. Fixtures
                    // 3380 (`p + n` int*), 2771 (struct 5b), 3648
                    // (`p - n` for struct 4b).
                    if matches!(op, BinOp::Add | BinOp::Sub)
                        && let ExprKind::Ident(pname) = &left.kind
                        && let Some(pointee) = self.ident_pointee(pname)
                        && pointee.size_bytes() > 1
                        && try_const_eval(right).is_none()
                    {
                        let stride = u16::from(pointee.size_bytes());
                        self.emit_expr_to_ax(right);
                        if stride.is_power_of_two() {
                            for _ in 0..stride.trailing_zeros() {
                                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                            }
                        } else {
                            let _ = write!(self.out, "\tmov\tdx,{stride}\r\n");
                            self.out.extend_from_slice(b"\timul\tdx\r\n");
                        }
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        self.emit_expr_to_ax(left);
                        self.out.extend_from_slice(b"\tpop\tdx\r\n");
                        let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
                        let _ = write!(self.out, "\t{mnem}\tax,dx\r\n");
                        return;
                    }
                    // `<ptr-typed-expr> + K` / `- K` — C scales the
                    // constant by the pointee size. Walks into nested
                    // adds (e.g. `(p + n) + 1` for int* p) so the
                    // outer add still scales. Always route as Add
                    // with the (possibly negative) scaled byte count
                    // so the ±1/±2 inc/dec peephole fires for small
                    // steps (fixture 2922: `p = p + 1` → `inc ax;
                    // inc ax`) and the AX-accumulator add form fires
                    // for larger ones (fixture 3557). Fixtures 3557,
                    // 3256, 3382, 2922, 3632 (`p + n + 1`).
                    if matches!(op, BinOp::Add | BinOp::Sub)
                        && let Some(pointee) = self.expr_pointee(left)
                        && let Some(k) = try_const_eval(right)
                        && pointee.size_bytes() > 1
                    {
                        let stride = i32::from(pointee.size_bytes());
                        let sign = if matches!(op, BinOp::Add) { 1i32 } else { -1 };
                        let bytes = sign.wrapping_mul(k as i32).wrapping_mul(stride);
                        let scaled = (bytes as u32) & 0xFFFF;
                        self.emit_expr_to_ax(left);
                        // Pointers compare as unsigned but for the
                        // add-or-inc emission we want the Add form
                        // chosen regardless (no `sub` canonicalization).
                        emit_op_with_source(
                            self.out,
                            BinOp::Add,
                            &OperandSource::Immediate(scaled),
                            true,
                        );
                        return;
                    }
                    // Commutative-op operand swap: BCC prefers the
                    // non-constant operand in AX so the immediate or
                    // simpler operand can be the binop's RHS. Fixture
                    // 200 (`3 + *p` → `*p + 3`).
                    let (left, right) = if op.is_commutative()
                        && try_const_eval(left).is_some()
                        && try_const_eval(right).is_none()
                    {
                        (right.as_ref(), left.as_ref())
                    } else {
                        (left.as_ref(), right.as_ref())
                    };
                    // Associative const-fold for Add/Sub chains:
                    // `((X ± K1) ± K2) ± K3 ...` → `X + (K_total)`.
                    // Walks down the left spine collecting constant
                    // additions/subtractions, then emits the variable
                    // base once with a single combined `add ax, K`.
                    // Lets BCC's smaller form fire for arbitrarily
                    // deep chains. Fixtures 2019, 2075 (`x + 1 + 1
                    // + 1`), 2076.
                    if matches!(op, BinOp::Add | BinOp::Sub)
                        && let Some(k_outer) = try_const_eval(right)
                    {
                        let outer_sign = if matches!(op, BinOp::Add) { 1i32 } else { -1 };
                        let mut total: i32 = outer_sign * (k_outer as i32);
                        let mut base: &Expr = left;
                        loop {
                            if let ExprKind::BinOp { op: bop, left: bl, right: br } = &base.kind
                                && matches!(bop, BinOp::Add | BinOp::Sub)
                                && let Some(k) = try_const_eval(br)
                                && try_const_eval(bl).is_none()
                            {
                                let s = if matches!(bop, BinOp::Add) { 1i32 } else { -1 };
                                total += s * (k as i32);
                                base = bl;
                                continue;
                            }
                            break;
                        }
                        // Only fold if we collapsed at least one nested
                        // const (i.e. base != left).
                        if !std::ptr::eq(base, left) {
                            let unsigned = self.expr_is_unsigned(base);
                            self.emit_expr_to_ax(base);
                            let total_masked = (total as u32) & 0xFFFF;
                            if total == 0 {
                                // Net-zero fold: BCC emits `add ax, 0`
                                // explicitly rather than eliding (the
                                // `x + 0` source-level identity fold is
                                // a separate path that fully elides).
                                // Fixture 2077 (`x + 5 - 5`).
                                let _ = write!(self.out, "\tadd\tax,0\r\n");
                            } else {
                                emit_op_with_source(
                                    self.out,
                                    BinOp::Add,
                                    &OperandSource::Immediate(total_masked),
                                    unsigned,
                                );
                            }
                            return;
                        }
                        // base == left: no inner const to fold; fall
                        // through to the normal binop path.
                    }
                    // Shifts encode the left operand's signedness in
                    // the mnemonic (`shr` vs `sar`); everything else
                    // is signedness-agnostic at the instruction level.
                    // Use the promoted-type variant: char/uchar both
                    // become signed `int` before a shift, so they
                    // get `sar`. Fixture 1015.
                    let unsigned = if matches!(op, BinOp::Shr) {
                        self.expr_shift_is_unsigned(left)
                    } else {
                        self.expr_is_unsigned(left)
                    };
                    // RHS-clobbers-AX path: when the right operand is a
                    // call, a char ident (whose load + cbw widen
                    // clobbers AX), or a nested non-constant BinOp
                    // (which produces its result in AX and so
                    // clobbers any LHS already there), BCC evaluates
                    // RHS first, pushes the result, then evaluates
                    // LHS into AX and pops the saved result into DX
                    // before applying the op. Fixture 593 (`n + sum(n
                    // -1)`), 616 (`a + b` with b a char param), 645
                    // (`x + y * 2`).
                    // `(int)<long_lvalue>` is just a low-half memory
                    // load and `(int)<int_lvalue>` is a no-op — neither
                    // clobbers AX in the sense the push/pop dance was
                    // designed to prevent. Let them route through the
                    // normal `add ax, mem` path. Fixtures 1947
                    // (`a + (int)b + c` with long b), 1778
                    // (`ca[1] + (int)ia[1]`).
                    let rhs_is_int_cast_of_int_or_long =
                        if let ExprKind::Cast { ty: cast_ty, operand } = &right.kind {
                            matches!(cast_ty, Type::Int | Type::UInt)
                                && (self.long_lvalue_addr_pair(operand).is_some()
                                    || self.try_lvalue_chain_addr(operand)
                                        .filter(|(_, _, ty)| ty.is_int_like())
                                        .is_some())
                        } else {
                            false
                        };
                    // A right operand that folds to a constant
                    // (literal, or any cast wrapping a literal —
                    // e.g. `(int)sizeof(struct Z)` resolves to a
                    // plain immediate) never clobbers AX. Without
                    // this exemption the Cast-wraps-constant case
                    // takes the push/pop route and misses the
                    // `+1/+2` peephole. Fixture 2302
                    // (`<expr> + (int)sizeof(struct Z)` → `inc ax;
                    // inc ax` rather than push/mov/pop/add).
                    let rhs_is_constant = try_const_eval(right).is_some();
                    // `<call>.<field>` (member access on a call-
                    // returning struct) also clobbers AX — the call
                    // itself writes AX:DX. Fixture 2682
                    // (`make().a + make().b`).
                    let rhs_member_call = matches!(&right.kind,
                        ExprKind::Member { base, .. }
                            if matches!(base.kind, ExprKind::Call { .. }));
                    let rhs_clobbers_ax = !rhs_is_constant
                        && (matches!(right.kind, ExprKind::Call { .. })
                            || matches!(right.kind, ExprKind::CallVia { .. })
                            || self.expr_is_char_load(right)
                            || (matches!(right.kind, ExprKind::Cast { .. })
                                && !rhs_is_int_cast_of_int_or_long)
                            || matches!(right.kind, ExprKind::Ternary { .. })
                            || rhs_member_call
                            || (matches!(right.kind, ExprKind::BinOp { .. })
                                && try_const_eval(right).is_none()));
                    // Callee-preserved register peephole: when the
                    // left operand is a bare ident that lives in
                    // SI or DI (BCC's int register pool sites that
                    // get saved across calls), we can skip the
                    // push/pop dance and apply the op directly with
                    // the register as the source. Fixtures 1697,
                    // 2255 (`n * fact(n-1)` with n in SI → `imul
                    // si` instead of `push ax; mov ax,si; pop dx;
                    // imul dx`).
                    let left_preserved_reg = if let ExprKind::Ident(name) = &left.kind
                        && self.locals.has(name)
                        && let LocalLocation::Reg(reg) = self.locals.location_of(name)
                        && matches!(reg, Reg::Si | Reg::Di)
                    {
                        Some(reg)
                    } else {
                        None
                    };
                    if rhs_clobbers_ax
                        && matches!(op, BinOp::Mul)
                        && let Some(reg) = left_preserved_reg
                    {
                        self.emit_expr_to_ax(right);
                        emit_op_with_source(
                            self.out,
                            *op,
                            &OperandSource::Reg(reg),
                            unsigned,
                        );
                    } else if rhs_clobbers_ax
                        && matches!(op, BinOp::Mul)
                        && let Some(left_src) = self.try_memory_source(left)
                    {
                        // `<int_mem> * <char>` shape: BCC emits
                        // `mov al,<char>; cbw; imul word ptr
                        // <int_mem>` directly, avoiding the push/pop
                        // dance. Other commutative ops (Add/Or/And/
                        // Xor) keep the push/pop — BCC specifically
                        // recognizes the mul-mem-direct shape.
                        // Fixture 1228 (`a * c`).
                        self.emit_expr_to_ax(right);
                        emit_op_with_source(self.out, *op, &left_src, unsigned);
                    } else if rhs_clobbers_ax && {
                        // LHS-first when LHS itself clobbers AX
                        // (nested binop / char-load / call / cast /
                        // ternary). RHS-first when LHS is simple
                        // (it can be loaded last for free).
                        // Div/Mod/Mul: always LHS-first (need AX as
                        // accumulator for the implicit operand).
                        let lhs_member_call = matches!(&left.kind,
                            ExprKind::Member { base, .. }
                                if matches!(base.kind, ExprKind::Call { .. }));
                        let lhs_clobbers_ax =
                            matches!(left.kind, ExprKind::Call { .. })
                            || matches!(left.kind, ExprKind::CallVia { .. })
                            || self.expr_is_char_load(left)
                            || matches!(left.kind,
                                ExprKind::Cast { .. } | ExprKind::Ternary { .. })
                            || lhs_member_call
                            || (matches!(left.kind, ExprKind::BinOp { .. })
                                && try_const_eval(left).is_none());
                        // Exception: `<call> + <call> * <call>` — RHS
                        // ends with `imul <reg>` (no `mov ax, reg`),
                        // so the LHS-first collapse peephole can't
                        // fire and we'd waste a `mov dx, ax`. BCC's
                        // bytes show RHS-first here. Fixture 2050
                        // (`zero() + one() * neg_one()`).
                        let lhs_is_call = matches!(left.kind, ExprKind::Call { .. });
                        let rhs_is_mul_of_calls = matches!(&right.kind,
                            ExprKind::BinOp { op: BinOp::Mul, left: rl, right: rr }
                                if matches!(rl.kind, ExprKind::Call { .. })
                                    && matches!(rr.kind, ExprKind::Call { .. }));
                        // Exception: `(int)<long_lvalue> + (int)
                        // (<long_lvalue> >> K)` — LHS is a single
                        // `mov ax, <low>` and RHS finishes with
                        // `mov ax, <high>; cwd`. BCC's bytes show
                        // RHS-first so the saved AX can be popped
                        // straight into DX without a `mov dx, ax`
                        // bridge. Fixture 1949 (`(int)y + (int)(y
                        // >> 16)` for long y).
                        let lhs_is_int_cast_of_long_ident = matches!(&left.kind,
                            ExprKind::Cast { ty, operand }
                                if matches!(ty, Type::Int | Type::UInt)
                                    && matches!(&operand.kind, ExprKind::Ident(n)
                                        if self.locals.has(n)
                                            && self.locals.type_of(n).is_long_like()));
                        let rhs_is_int_cast_of_long_shift = matches!(&right.kind,
                            ExprKind::Cast { ty, operand }
                                if matches!(ty, Type::Int | Type::UInt)
                                    && matches!(&operand.kind,
                                        ExprKind::BinOp { op: BinOp::Shr | BinOp::Shl, left: shl, .. }
                                            if matches!(&shl.kind, ExprKind::Ident(n)
                                                if self.locals.has(n)
                                                    && self.locals.type_of(n).is_long_like())));
                        let rhs_first_call_exception =
                            matches!(op, BinOp::Add | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
                                && ((lhs_is_call && rhs_is_mul_of_calls)
                                    || (lhs_is_int_cast_of_long_ident
                                        && rhs_is_int_cast_of_long_shift));
                        (matches!(op, BinOp::Div | BinOp::Mod | BinOp::Mul)
                            || lhs_clobbers_ax)
                            && !rhs_first_call_exception
                    } {
                        // `<uchar-lvalue> <op> <uchar-lvalue>` for
                        // Add/Sub/BitAnd/BitOr/BitXor/Mul: BCC widens
                        // both via `mov al, [a]; mov ah, 0` then `mov
                        // dl, [b]; mov dh, 0; <op> ax, dx`. The byte-
                        // local DH=0 zero-extension avoids the
                        // push/pop dance. Mul piggybacks: `imul dx`
                        // produces DX:AX but we only consume AX.
                        // Div/Mod still excluded — divisor must be in
                        // BX to keep DX free for cwd/xor. Fixtures
                        // 1400 (`a + b` uchar+uchar), 1626 (`a * b`).
                        if !matches!(op, BinOp::Div | BinOp::Mod) {
                            let l_chain = self.try_lvalue_chain_addr(left);
                            let r_chain = self.try_lvalue_chain_addr(right);
                            let l_uchar = l_chain.as_ref()
                                .filter(|(_, _, ty)| ty.is_char_like() && ty.is_unsigned())
                                .and_then(|(n, o, _)| self.resolve_chain_addr(n, *o));
                            let r_uchar = r_chain.as_ref()
                                .filter(|(_, _, ty)| ty.is_char_like() && ty.is_unsigned())
                                .and_then(|(n, o, _)| self.resolve_chain_addr(n, *o));
                            // sc + uc — LHS is signed char so it
                            // widens via `mov al; cbw`; RHS still
                            // takes the uchar DX-direct path.
                            // Fixture 2690 (`s + u` for sc + uc).
                            if l_uchar.is_none()
                                && let Some(r_addr) = r_uchar.as_ref()
                                && let Some((l_name, l_off, l_ty)) = l_chain.as_ref()
                                && l_ty.is_char_like()
                                && !l_ty.is_unsigned()
                                && let Some(l_addr) = self.resolve_chain_addr(l_name, *l_off)
                            {
                                let _ = write!(self.out, "\tmov\tal,byte ptr {l_addr}\r\n");
                                self.out.extend_from_slice(b"\tcbw\t\r\n");
                                let _ = write!(self.out, "\tmov\tdl,byte ptr {r_addr}\r\n");
                                self.out.extend_from_slice(b"\tmov\tdh,0\r\n");
                                emit_op_with_source(
                                    self.out,
                                    *op,
                                    &OperandSource::Reg(Reg::Dx),
                                    unsigned,
                                );
                                return;
                            }
                            if let (Some(l_addr), Some(r_addr)) = (l_uchar.as_ref(), r_uchar.as_ref()) {
                                let _ = write!(self.out, "\tmov\tal,byte ptr {l_addr}\r\n");
                                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                                let _ = write!(self.out, "\tmov\tdl,byte ptr {r_addr}\r\n");
                                self.out.extend_from_slice(b"\tmov\tdh,0\r\n");
                                emit_op_with_source(
                                    self.out,
                                    *op,
                                    &OperandSource::Reg(Reg::Dx),
                                    unsigned,
                                );
                                return;
                            }
                            // LHS is an AX-clobbering arithmetic expr
                            // (e.g. a constant shift `(x << 8)`) and RHS
                            // is a plain uchar lvalue. BCC computes LHS
                            // into AX, then loads the RHS byte straight
                            // into DX (`mov dl, ..; mov dh, 0`) and ORs/
                            // adds with `<op> ax, dx`, sidestepping the
                            // push/pop dance. Fixture 4197
                            // (`(u.b[1] << 8) | u.b[0]`).
                            if l_uchar.is_none()
                                && let Some(r_addr) = r_uchar.as_ref()
                                && matches!(&left.kind, ExprKind::BinOp { .. })
                                && try_const_eval(left).is_none()
                            {
                                self.emit_expr_to_ax(left);
                                let _ = write!(self.out, "\tmov\tdl,byte ptr {r_addr}\r\n");
                                self.out.extend_from_slice(b"\tmov\tdh,0\r\n");
                                emit_op_with_source(
                                    self.out,
                                    *op,
                                    &OperandSource::Reg(Reg::Dx),
                                    unsigned,
                                );
                                return;
                            }
                            // Reg-resident uchar held in BL (typical
                            // when the eligible char is a function
                            // param — see locals.rs's "first char
                            // is param → BL first" rule). Both
                            // operands resolve to the same low byte,
                            // and BL is preserved across the AX/DX
                            // widening dance, so no push/pop is
                            // needed: just copy BL into AL and DL
                            // and zero-extend each. Fixture 1991
                            // (`return x + x` for uchar param x).
                            if let ExprKind::Ident(l_name) = &left.kind
                                && let ExprKind::Ident(r_name) = &right.kind
                                && l_name == r_name
                                && self.locals.has(l_name)
                                && let LocalLocation::Reg(reg) = self.locals.location_of(l_name)
                                && matches!(reg, Reg::Bl)
                            {
                                let reg_name = reg.name();
                                let _ = write!(self.out, "\tmov\tal,{reg_name}\r\n");
                                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                                let _ = write!(self.out, "\tmov\tdl,{reg_name}\r\n");
                                self.out.extend_from_slice(b"\tmov\tdh,0\r\n");
                                emit_op_with_source(
                                    self.out,
                                    *op,
                                    &OperandSource::Reg(Reg::Dx),
                                    unsigned,
                                );
                                return;
                            }
                        }
                        // Div/Mod scratch is BX (DX is clobbered by
                        // cwd / xor dx,dx). Mul scratch is DX (imul
                        // writes DX:AX, no other reg-clobbering setup).
                        // Add/Sub/etc with LHS-clobbering RHS-also-
                        // clobbering: DX as scratch is fine. Fixtures
                        // 087 (`a + b + c` w/ char c), 1357, 1625,
                        // 1223, 2006.
                        let scratch = if matches!(op, BinOp::Div | BinOp::Mod) {
                            Reg::Bx
                        } else {
                            Reg::Dx
                        };
                        // RHS is a shift `<src> << K` / `<src> >> K`
                        // by a constant: compute into DX directly via
                        // `mov dx, <src>; mov cl, K; <shift> dx, cl`.
                        // The shift clobbers CL but not AX, so we can
                        // keep the LHS in AX. Fixture 1957
                        // (`(x << 4) | (x >> 12)` rotate emulation).
                        if !matches!(op, BinOp::Div | BinOp::Mod | BinOp::Mul)
                            && let ExprKind::BinOp { op: rop, left: rl, right: rr } = &right.kind
                            && matches!(rop, BinOp::Shl | BinOp::Shr)
                            && let Some(rl_src) = self.try_dx_load_source(rl)
                            && let Some(k) = try_const_eval(rr)
                            && (1..=15).contains(&k)
                        {
                            let signed = !self.expr_is_unsigned(rl);
                            let shift_mnem = match (rop, signed) {
                                (BinOp::Shl, _) => "shl",
                                (BinOp::Shr, false) => "shr",
                                (BinOp::Shr, true) => "sar",
                                _ => unreachable!(),
                            };
                            self.emit_expr_to_ax(left);
                            let _ = write!(self.out, "\tmov\tdx,{rl_src}\r\n");
                            let k8 = (k & 0xFF) as u8;
                            if (1..=3).contains(&k) {
                                for _ in 0..k {
                                    let _ = write!(
                                        self.out,
                                        "\t{shift_mnem}\tdx,1\r\n",
                                    );
                                }
                            } else {
                                let _ = write!(self.out, "\tmov\tcl,{k8}\r\n");
                                let _ = write!(self.out, "\t{shift_mnem}\tdx,cl\r\n");
                            }
                            emit_op_with_source(
                                self.out,
                                *op,
                                &OperandSource::Reg(Reg::Dx),
                                unsigned,
                            );
                            return;
                        }
                        // RHS is a binop of two AX-safe operands
                        // (reg-ident or mem-lvalue): emit it directly
                        // into DX, skipping the push/pop dance.
                        // Outer op is the one that consumes DX as
                        // source; for Mul this becomes `imul dx`
                        // which uses AX as accumulator. Div/Mod
                        // can't reuse this (the divisor must go
                        // into BX, not DX). Fixture 1499 (`(a + b)
                        // + (a - c)`), 1528 (`(a - b) * (a + b)`).
                        if !matches!(op, BinOp::Div | BinOp::Mod)
                            && let ExprKind::BinOp { op: rop, left: rl, right: rr } = &right.kind
                            && matches!(rop, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
                            && let Some(rl_src) = self.try_dx_load_source(rl)
                            && let Some(rr_src) = self.try_op_source(rr)
                        {
                            self.emit_expr_to_ax(left);
                            let _ = write!(self.out, "\tmov\tdx,{rl_src}\r\n");
                            let mnem = match rop {
                                BinOp::Add => "add",
                                BinOp::Sub => "sub",
                                BinOp::BitAnd => "and",
                                BinOp::BitOr => "or",
                                BinOp::BitXor => "xor",
                                _ => unreachable!(),
                            };
                            let _ = write!(self.out, "\t{mnem}\tdx,{rr_src}\r\n");
                            emit_op_with_source(
                                self.out,
                                *op,
                                &OperandSource::Reg(Reg::Dx),
                                unsigned,
                            );
                            return;
                        }
                        self.emit_expr_to_ax(left);
                        let push_pos = self.out.len();
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        self.emit_expr_to_ax(right);
                        // Peephole: hoist a leading non-AX-touching
                        // setup line (typically `mov bx, ...` for a
                        // pointer load) from the start of RHS to
                        // before `push ax`. Mirrors BCC's "delay
                        // the push until just before AL is written"
                        // shape. Fixtures 2231, 2237, 2291, 2345.
                        hoist_first_setup_above_push(self.out, push_pos);
                        // Peephole: collapse `mov ax, <reg>\r\nmov
                        // <reg>, ax\r\n` into nothing — the second
                        // is the inverse of the first and AX/<reg>
                        // both end with their pre-pair values. This
                        // fires when emit_member_to_ax extracts .b
                        // via `mov ax, dx` and the scratch is also
                        // DX. Fixture 2682.
                        let scratch_name = scratch.name();
                        let collapse = format!("\tmov\tax,{scratch_name}\r\n");
                        if self.out.ends_with(collapse.as_bytes()) {
                            let new_len = self.out.len() - collapse.len();
                            self.out.truncate(new_len);
                        } else {
                            let _ = write!(self.out, "\tmov\t{scratch_name},ax\r\n");
                        }
                        self.out.extend_from_slice(b"\tpop\tax\r\n");
                        emit_op_with_source(
                            self.out,
                            *op,
                            &OperandSource::Reg(scratch),
                            unsigned,
                        );
                    } else if rhs_clobbers_ax {
                        self.emit_expr_to_ax(right);
                        let push_pos = self.out.len();
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        self.emit_expr_to_ax(left);
                        hoist_first_setup_above_push(self.out, push_pos);
                        self.out.extend_from_slice(b"\tpop\tdx\r\n");
                        emit_op_with_source(
                            self.out,
                            *op,
                            &OperandSource::Reg(Reg::Dx),
                            unsigned,
                        );
                    } else {
                        let pre_pos = self.out.len();
                        self.emit_expr_to_ax(left);
                        let mid_pos = self.out.len();
                        self.emit_binary_right(*op, right, unsigned);
                        // Peephole: when LHS emitted a single
                        // `mov ax, word ptr <X>\r\n` and RHS begins
                        // with `mov bx, word ptr <Y>\r\n` (a pointer
                        // load) followed by `<op> ax, word ptr [bx]
                        // \r\n`, swap the AX and BX loads so BX is
                        // set up first. BCC's order delays the AX
                        // load until just before the op. Fixture
                        // 2310 (`n1.v + n1.next->v` reads BX before
                        // AX).
                        hoist_bx_load_above_ax_load(self.out, pre_pos, mid_pos);
                    }
                }
            }
            ExprKind::Unary { op, operand } => self.emit_unary(*op, operand),
            ExprKind::Logical { op, left, right } => {
                self.emit_logical_to_ax(e.span.start, e.span.end, *op, left, right);
            }
            ExprKind::Update { target, op, position } => {
                self.emit_update_to_ax(target, *op, *position);
            }
            ExprKind::AssignExpr { target, value } => {
                // Chained assignment `a = b = c = 5;` lands here via
                // the outer statement's RHS. Recursively evaluate the
                // inner value into AX, then store AX into `target`.
                // AX still holds the assigned value so the outer
                // store reuses it. Fixture 500. Char target: store
                // just AL (byte ptr), matching BCC's byte-width
                // store. Fixture 3653 (`(c = arr[i++])` for `char c`).
                self.emit_expr_to_ax(value);
                let target_is_char = if self.globals.contains(target) {
                    self.globals
                        .type_of(target)
                        .map_or(false, |t| t.is_char_like())
                } else if self.locals.has(target) {
                    self.locals.type_of(target).is_char_like()
                } else {
                    false
                };
                // Char-target peephole: the value path commonly ends
                // with `\tcbw\t\r\n` (e.g. char-array load → widen
                // to int). When we're about to store just AL anyway,
                // the cbw is dead — strip it. BCC matches this
                // shape. Fixture 3653.
                if target_is_char && self.out.ends_with(b"\tcbw\t\r\n") {
                    let new_len = self.out.len() - b"\tcbw\t\r\n".len();
                    self.out.truncate(new_len);
                }
                let (width, src) = if target_is_char {
                    ("byte", "al")
                } else {
                    ("word", "ax")
                };
                if self.globals.contains(target) {
                    let _ = write!(
                        self.out,
                        "\tmov\t{width} ptr DGROUP:_{target},{src}\r\n",
                    );
                } else {
                    match self.locals.location_of(target) {
                        LocalLocation::Stack(off) => {
                            let _ = write!(
                                self.out,
                                "\tmov\t{width} ptr {},{src}\r\n",
                                bp_addr(off)
                            );
                        }
                        LocalLocation::Reg(reg) => {
                            let _ = write!(self.out, "\tmov\t{},ax\r\n", reg.name());
                        }
                    }
                }
            }
            ExprKind::AssignLvalueExpr { target, value } => {
                // Value-position assign to an arbitrary lvalue
                // (`*p = v`, `a[i] = v`, `p->x = v`). The surrounding
                // expression (`(*p = 5) + 1`, `v = (a[i] = 42)`)
                // consumes the assigned value from AX after the
                // store. Two emission shapes:
                //
                // 1. Address computation doesn't need AX as scratch
                //    (`*<reg-ptr>`): evaluate value into AX, then
                //    `mov [reg], ax`. Fixture 3333.
                // 2. Address computation needs AX (`<stack-arr>[i]`
                //    via `lea ax, [bp-N]`): compute the address into
                //    BX first, then evaluate value into AX, then
                //    `mov [bx], ax`. Fixture 1986.
                //
                // Stack-resident `*p++` etc. are still gaps and
                // fall through to the helper's panic.
                if self.try_emit_assign_lvalue_addr_first(target, value) {
                    return;
                }
                self.emit_expr_to_ax(value);
                // For a char-lvalue target the store writes AL only.
                // emit_expr_to_ax may have widened the byte value
                // via `cbw` to keep AX consistent — strip that
                // trailing `cbw` since the byte store doesn't need
                // it. Matches BCC's exact emission for fixture 1808
                // (`while (*d++ = *s++)`).
                if self.target_is_char_lvalue(target)
                    && self.out.ends_with(b"\tcbw\t\r\n")
                {
                    let new_len = self.out.len() - b"\tcbw\t\r\n".len();
                    self.out.truncate(new_len);
                }
                self.emit_store_ax_to_lvalue(target);
            }
            ExprKind::CompoundAssignExpr { target, op, value } => {
                // Value-position compound assign: emit the in-place
                // update via emit_compound_assign, then load the
                // (now-current) value into AX so the surrounding
                // expression can consume it. Today only the bare
                // ident-target case is supported.
                self.emit_compound_assign(target, *op, value);
                if self.locals.has(target) {
                    let loc = self.locals.location_of(target);
                    match loc {
                        LocalLocation::Stack(off) => {
                            let _ = write!(
                                self.out,
                                "\tmov\tax,word ptr {}\r\n",
                                bp_addr(off),
                            );
                        }
                        LocalLocation::Reg(reg) => {
                            let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                        }
                    }
                } else {
                    let _ = write!(
                        self.out,
                        "\tmov\tax,word ptr DGROUP:_{target}\r\n",
                    );
                }
            }
            ExprKind::CallVia { addr, args } => {
                self.emit_call_via(addr, args);
            }
            ExprKind::Call { name, args } => {
                self.emit_call(name, args);
                // Char-returning callee leaves only AL meaningful;
                // widen to AX so the caller sees a full int. Signed
                // char uses cbw; uchar uses `mov ah, 0`. Fixture 562.
                if let Some(ret) = self.signatures.ret_ty_of(name)
                    && ret.is_char_like()
                {
                    let ret = ret.clone();
                    self.emit_widen_al(&ret);
                }
            }
            ExprKind::AddressOf(name) => self.emit_address_of(name),
            ExprKind::AddressOfArrayElem { array, byte_offset } => {
                // `&<arr>[K]` at runtime — for a global array, emit
                // the symbol+offset as an immediate. For a stack-
                // resident local array, emit `lea ax, [bp+off+K]`
                // where `off` is the local's bp-offset. Fixture 486.
                if self.globals.contains(array) {
                    if *byte_offset == 0 {
                        let _ = write!(
                            self.out,
                            "\tmov\tax,offset DGROUP:_{array}\r\n",
                        );
                    } else {
                        let _ = write!(
                            self.out,
                            "\tmov\tax,offset DGROUP:_{array}+{byte_offset}\r\n",
                        );
                    }
                } else {
                    let LocalLocation::Stack(base_off) = self.locals.location_of(array) else {
                        panic!("local array `{array}` should be stack-resident");
                    };
                    let total = base_off + i16::try_from(*byte_offset).unwrap_or(i16::MAX);
                    let _ = write!(
                        self.out,
                        "\tlea\tax,word ptr {}\r\n",
                        bp_addr(total),
                    );
                }
            }
            ExprKind::AddressOfArrayElemVar { array, index, elem_size } => {
                // `&<arr>[<var>]` — evaluate the index into AX,
                // scale by elem_size, then add the array's base.
                // Three sub-shapes:
                //   - global array: `add ax, offset DGROUP:_arr`.
                //   - stack array:  `lea bx, [bp-N]; add ax, bx`.
                //   - stack pointer (param): push the scaled
                //     index, load the pointer, pop the index into
                //     DX, then add. Matches BCC's push/pop dance
                //     in fixture 2978.
                // Fixtures 3249, 3645, 2884, 2978.
                self.emit_expr_to_ax(index);
                let stride = u32::from(*elem_size);
                for _ in 0..stride.trailing_zeros() {
                    self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                }
                if self.globals.contains(array) {
                    let _ = write!(
                        self.out,
                        "\tadd\tax,offset DGROUP:_{array}\r\n",
                    );
                } else if let LocalLocation::Stack(off) = self.locals.location_of(array) {
                    let arr_ty = self.locals.type_of(array);
                    if matches!(arr_ty, Type::Pointer(_)) {
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
                        self.out.extend_from_slice(b"\tpop\tdx\r\n");
                        self.out.extend_from_slice(b"\tadd\tax,dx\r\n");
                    } else {
                        let _ = write!(self.out, "\tlea\tbx,word ptr {}\r\n", bp_addr(off));
                        self.out.extend_from_slice(b"\tadd\tax,bx\r\n");
                    }
                } else {
                    unreachable!("array `{array}` should be stack-resident");
                }
            }
            ExprKind::Deref(operand) => self.emit_deref_to_ax(operand),
            ExprKind::ArrayIndex { array, index } => {
                self.emit_array_index_to_ax(array, index);
            }
            ExprKind::StringLit(bytes) => {
                // A bare string literal in value position is its
                // address (the C decay rule). Look up the pool
                // offset via the pre-intern span map when available,
                // else fall back to interning fresh.
                let offset = self
                    .strings
                    .offset_for_span(e.span.start)
                    .unwrap_or_else(|| self.strings.intern(bytes));
                if offset == 0 {
                    let _ = write!(self.out, "\tmov\tax,offset DGROUP:s@\r\n");
                } else {
                    let _ = write!(self.out, "\tmov\tax,offset DGROUP:s@+{offset}\r\n");
                }
            }
            ExprKind::Member { base, field, kind } => {
                self.emit_member_to_ax(base, field, *kind);
            }
            ExprKind::Ternary { cond, then_value, else_value } => {
                self.emit_ternary_to_ax(e.span.start, e.span.end, cond, then_value, else_value);
            }
            ExprKind::Cast { ty, operand } => {
                self.emit_cast_to_ax(ty, operand);
            }
            ExprKind::InitList { .. } => {
                panic!("initializer list not legal in value position");
            }
            ExprKind::Comma { left, right } => {
                // Comma operator: emit left for side effects (as if
                // it were an expression statement) then emit right's
                // value into AX. Fixture 469.
                self.emit_expr_discard(left);
                self.emit_expr_to_ax(right);
            }
        }
    }
    /// Lower `(<ty>) <operand>` into AX. The narrowing int→char case
    /// (the only one with a fixture today, 170) fuses the load with
    /// the truncate: `mov al, byte ptr [bp-N]; cbw` when the operand
    /// is a stack-int local — exactly what BCC emits for reading a
    /// char-typed local from that offset. Widening / no-op casts just
    /// evaluate the operand into AX.
    pub(crate) fn emit_cast_to_ax(&mut self, ty: &Type, operand: &Expr) {
        // `(int)<float-or-double>` — BCC routes through the runtime
        // helper `N_FTOL@`: load the FP operand to the FPU stack,
        // call the helper, which pops the FPU stack and leaves the
        // signed-int result in AX (with DX clobbered for the long
        // form, but for int return the low word in AX is what we
        // want). Fixture 1670 (float local → int).
        if matches!(ty, Type::Int | Type::UInt)
            && self.operand_is_float_like(operand)
        {
            self.emit_float_load_to_fpu(operand);
            self.helpers.insert("N_FTOL@".to_string());
            self.out.extend_from_slice(b"\tcall\tnear ptr N_FTOL@\r\n");
            return;
        }
        // `(int)(<long_lvalue> >> 16)` — fast-path for the long
        // right-shift-by-16 pattern. The high half of the long is
        // exactly what `(int)(x >> 16)` yields, so BCC loads the
        // high half directly and follows with `cwd` (signed) or
        // `xor dx, dx` (unsigned) to widen for the surrounding
        // long-result context. Saves 3 bytes vs the generic
        // `mov ax, lo; mov cl, 16; sar ax, cl` path. Fixtures
        // 2173, 2170, 2324.
        if matches!(ty, Type::Int | Type::UInt)
            && let ExprKind::BinOp { op: BinOp::Shr, left, right } = &operand.kind
            && let Some(k) = try_const_eval(right)
            && k == 16
            && let Some((hi, _lo)) = self.long_lvalue_addr_pair(left)
        {
            let unsigned = self.expr_is_unsigned(left);
            let _ = write!(self.out, "\tmov\tax,word ptr {hi}\r\n");
            // BCC emits `cwd` for the signed case (the long-shr-by-16
            // result lives in DX:AX so the sign-extended high half is
            // written even though the int cast drops it). Unsigned
            // skips the widen entirely. Fixtures 2173 (signed →
            // cwd), 2180 (ulong → no widen).
            if !unsigned {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
            }
            return;
        }
        // `(int)<long_lvalue>` — the cast keeps the low half. Load
        // just `[lo]` into AX, skipping the full long load that
        // emit_expr_to_ax would do. Fixture 1947 (`a + (int)b + c`
        // with long b → BCC chains `add ax, word ptr [b_lo]`).
        if matches!(ty, Type::Int | Type::UInt)
            && let Some((_hi, lo)) = self.long_lvalue_addr_pair(operand)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {lo}\r\n");
            return;
        }
        // `(int)(<long_lvalue> <shift> K)` where K is a constant
        // shift count != 16 (K=16 took the fast path above): call
        // the long-shift helper with DX:AX = operand, CL = K, then
        // AX is the int-cast result. Fixture 1951 (`(int)(y >> 8)`
        // for `long y = (long)x`).
        if matches!(ty, Type::Int | Type::UInt)
            && let ExprKind::BinOp { op: shift_op, left, right } = &operand.kind
            && matches!(shift_op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(right)
            && let Some((hi, lo)) = self.long_lvalue_addr_pair(left)
        {
            let unsigned = self.expr_is_unsigned(left);
            let helper = match (shift_op, unsigned) {
                (BinOp::Shl, _) => "N_LXLSH@",
                (BinOp::Shr, false) => "N_LXRSH@",
                (BinOp::Shr, true) => "N_LXURSH@",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tdx,word ptr {hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {lo}\r\n");
            let k_u8 = (k & 0xFF) as u8;
            let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            return;
        }
        // `(int)(<long_lvalue> * <long_lvalue>)` — BCC doesn't fold
        // the cast through the multiply (`imul lo,lo` would give the
        // same low 16, but BCC emits the full helper call anyway).
        // Load both operands into the four-register helper ABI
        // (CX:BX = a, DX:AX = b), call N_LXMUL@, then AX is the
        // (int) result. Fixture 2580.
        if matches!(ty, Type::Int | Type::UInt)
            && let ExprKind::BinOp { op: BinOp::Mul, left, right } = &operand.kind
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let _ = write!(self.out, "\tmov\tcx,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tbx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {b_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {b_lo}\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            return;
        }
        // `(int)(<long_lvalue> / <long_lvalue>)` / `(int)(... % ...)`
        // — same shape but with the four-word stack-push ABI used by
        // N_LDIV@ / N_LMOD@ / N_LUDIV@ / N_LUMOD@. AX holds the int
        // result on return. Fixture 2585.
        if matches!(ty, Type::Int | Type::UInt)
            && let ExprKind::BinOp { op, left, right } = &operand.kind
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let unsigned = self.expr_is_unsigned(left) || self.expr_is_unsigned(right);
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tpush\tword ptr {b_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {b_lo}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            return;
        }
        if ty.is_char_like() {
            // `(char|uchar) <lvalue>` — byte-load the low byte of
            // the source, then widen per the cast type's signedness.
            // Signed → cbw; unsigned → mov ah, 0. Source can be any
            // int/char lvalue (including char-to-uchar narrowing,
            // fixture 1524). Width-resolving comes from
            // try_lvalue_chain_addr/resolve_chain_addr.
            if let Some((src_name, src_off, _)) = self.try_lvalue_chain_addr(operand)
                && let Some(src_addr) = self.resolve_chain_addr(&src_name, src_off)
            {
                let _ = write!(self.out, "\tmov\tal,byte ptr {src_addr}\r\n");
                if ty.is_unsigned() {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
                return;
            }
        }
        self.emit_expr_to_ax(operand);
    }
    /// Emit a ternary `cond ? then : else` into AX. The shape BCC
    /// produces (fixture 166): test the condition with a reverse
    /// branch to the false-arm label, emit the then-value into AX,
    /// jump to the merge label, emit the false-arm label + else-value,
    /// emit the merge label. Slot layout matches an `if`-`else`:
    /// base+1 is the false arm, base+2 is the merge target.
    pub(crate) fn emit_ternary_to_ax(
        &mut self,
        span_start: u32,
        span_end: u32,
        cond: &Expr,
        then_value: &Expr,
        else_value: &Expr,
    ) {
        // Constant cond: BCC fully folds the ternary, emitting only
        // the surviving arm with no labels or jumps. Fixture 2965
        // (`1 ? a : b` → just `mov ax, [a]`).
        if let Some(v) = try_const_eval(cond) {
            if v != 0 {
                self.emit_expr_to_ax(then_value);
            } else {
                self.emit_expr_to_ax(else_value);
            }
            return;
        }
        let base = self.label_plan.base(span_start, span_end);
        // Some compare shapes need an explicit then-entry label so the
        // 3-jump long-vs-K cmp pattern (and `||` short-circuit-to-true)
        // can land at the start of the then-arm. Mirrors the same
        // pre-allocation logic in `emit_if`. Fixture 433 (`g > 0 ? 1
        // : 0` for long `g`).
        let cond_has_top_or = matches!(
            cond.kind,
            ExprKind::Logical { op: LogicalOp::Or, .. }
        );
        let needs_then_entry = cond_has_top_or
            || self.is_long_signed_globals_cmp(cond)
            || self.is_long_signed_const_cmp(cond)
            || self.is_long_vs_int_cmp(cond)
            || self.is_long_vs_int_ne(cond)
            || self.is_long_ne_const(cond);
        let true_slot = if needs_then_entry { Some(base) } else { None };
        let false_slot = base + 1;
        let merge_slot = base + 2;
        self.emit_cond_branch(cond, true_slot, Some(false_slot));
        if let Some(t) = true_slot {
            self.emit_label(t);
        }
        self.emit_expr_to_ax(then_value);
        let _ = write!(
            self.out,
            "\tjmp\tshort {}\r\n",
            self.label_ref(merge_slot),
        );
        self.emit_label(false_slot);
        self.emit_expr_to_ax(else_value);
        self.emit_label(merge_slot);
    }
    pub(crate) fn emit_comparison_as_value(
        &mut self,
        cmp_span_start: u32,
        cmp_span_end: u32,
        op: BinOp,
        left: &Expr,
        right: &Expr,
    ) {
        let base = self.label_plan.base(cmp_span_start, cmp_span_end);
        let false_slot = base + 1;
        let end_slot = base + 2;
        let unsigned = self.cmp_is_unsigned(left, right);
        let inv = op.jump_if_false(unsigned).expect("comparison op has inverse jump");

        self.emit_compare(left, right);
        let _ = write!(self.out, "\t{inv}\tshort {}\r\n", self.label_ref(false_slot));
        self.out.extend_from_slice(b"\tmov\tax,1\r\n");
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(end_slot));
        self.emit_label(false_slot);
        self.out.extend_from_slice(b"\txor\tax,ax\r\n");
        self.emit_label(end_slot);
    }
    /// Emit the right-hand side of a binary op, applying it to AX.
    pub(crate) fn emit_binary_right(&mut self, op: BinOp, e: &Expr, unsigned: bool) {
        // ±1 / +2 peephole: BCC emits `inc ax` / `dec ax` for ±1 (1
        // byte each vs. 3 for `add ax, 1` / `sub ax, 1`), and a pair
        // of `inc ax` for +2 (2 bytes vs. 3). Notably -2 does NOT
        // collapse to `dec ax; dec ax` — BCC keeps `add ax, -2`
        // (3 bytes, AX-accum imm16). Fixtures 027–031 (±1), 076 case 1
        // (+2 → inc/inc), 2074/1277 (-2 → `add ax, -2`).
        if let Some(v) = try_const_eval(e)
            && ((matches!(op, BinOp::Add) && (v == 1 || v == 2))
                || (matches!(op, BinOp::Sub) && v == 1))
        {
            let mnemonic = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            for _ in 0..v {
                let _ = write!(self.out, "\t{mnemonic}\tax\r\n");
            }
            return;
        }
        // uchar-on-right widening shortcut (no AX disturb): BCC
        // loads DL from the uchar lvalue, zero-extends to DX via
        // `mov dh, 0`, then `<op> ax, dx`. Saves the push/pop pair
        // because uchar zero-extension is local to DX. Fixture
        // 1400 (`a + b` for two uchar stack locals); fixture 3221
        // (uchar struct field via Member chain).
        if let Some((name, off, ty)) = self.try_lvalue_chain_addr(e)
            && ty.is_char_like()
            && ty.is_unsigned()
            && let Some(src_addr) = self.resolve_chain_addr(&name, off)
        {
            let _ = write!(self.out, "\tmov\tdl,byte ptr {src_addr}\r\n");
            self.out.extend_from_slice(b"\tmov\tdh,0\r\n");
            emit_op_with_source_opts(
                self.out,
                op,
                &OperandSource::Reg(Reg::Dx),
                unsigned,
                self.skip_mod_to_ax,
            );
            return;
        }
        // Char-on-right widening dance (fixture 087: `a + b + c` with
        // `c` a char global). Loading a char clobbers AX, so the
        // running sum gets pushed, the char loaded + widened to AX,
        // saved to DX, the sum restored, then combined. The same
        // pattern would apply to a char *stack* local but we have no
        // fixture pinning it yet.
        if let ExprKind::Ident(name) = &e.kind
            && self.ident_is_char(name)
        {
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.emit_expr_to_ax(e);
            self.out.extend_from_slice(b"\tmov\tdx,ax\r\n");
            self.out.extend_from_slice(b"\tpop\tax\r\n");
            emit_op_with_source_opts(
                self.out,
                op,
                &OperandSource::Reg(Reg::Dx),
                unsigned,
                self.skip_mod_to_ax,
            );
            return;
        }
        // Char-array-element on right (`a[K]` where a is a char
        // pointer/array): same push/widen/pop pattern. The byte
        // load goes through `emit_expr_to_ax` which handles all
        // char-load shapes (register-pointer subscript, stack-array
        // subscript, member-chain). Fixture 1239 (`a[0] + a[1]`
        // for `int sum(char a[])`).
        if self.expr_is_char_load(e) {
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.emit_expr_to_ax(e);
            self.out.extend_from_slice(b"\tmov\tdx,ax\r\n");
            self.out.extend_from_slice(b"\tpop\tax\r\n");
            emit_op_with_source_opts(
                self.out,
                op,
                &OperandSource::Reg(Reg::Dx),
                unsigned,
                self.skip_mod_to_ax,
            );
            return;
        }
        let src = self.resolve_operand_source(e);
        emit_op_with_source_opts(self.out, op, &src, unsigned, self.skip_mod_to_ax);
    }
}
