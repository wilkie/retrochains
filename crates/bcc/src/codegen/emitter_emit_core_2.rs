use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// Memory-operand `push` peephole: if `arg` is a simple load of a
    /// 2-byte value that resolves to a single addressing mode (stack
    /// local, global, or const-index array element on either), return
    /// the `push word ptr <m>` mnemonic string. The caller substitutes
    /// the `mov ax, <m>; push ax` pair with `push word ptr <m>`,
    /// saving one byte. Fixture 589 (`f(a[1])` over a local int array).
    pub(crate) fn try_direct_arg_push(&self, arg: &Expr, param_ty: &Type) -> Option<String> {
        if param_ty.is_char_like() || param_ty.is_long_like() {
            return None;
        }
        // Bare stack-local int/ptr: `push word ptr [bp+N]` directly.
        // Fixture 3116 (`printf(x)` for x at [bp+4]), 2688 (3-arg int
        // call), 1656.
        if let ExprKind::Ident(name) = &arg.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            return Some(format!("push\tword ptr {}", bp_addr(off)));
        }
        // Bare register-resident int local: `push <reg>` directly
        // (1 byte) instead of `mov ax,<reg>; push ax` (3 bytes).
        // Fixtures 2753, 1506, 1580.
        if let ExprKind::Ident(name) = &arg.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && !reg.is_byte()
        {
            return Some(format!("push\t{}", reg.name()));
        }
        // Bare global int/ptr: `push word ptr DGROUP:_<name>` directly.
        if let ExprKind::Ident(name) = &arg.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_int_like()
        {
            return Some(format!("push\tword ptr DGROUP:_{name}"));
        }
        // `*p` for a register-resident int pointer: `push word ptr
        // [<reg>]` directly. Fixture 1292 (`f(*p)` with p in SI).
        if let ExprKind::Deref(operand) = &arg.kind
            && let ExprKind::Ident(name) = &operand.kind
            && self.locals.has(name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && let Some(pointee) = self.locals.type_of(name).pointee()
            && pointee.is_int_like()
        {
            return Some(format!("push\tword ptr [{}]", reg.name()));
        }
        if let ExprKind::ArrayIndex { array, index } = &arg.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let arr_ty = self.locals.type_of(arr_name).clone()
            && arr_ty.array_elem().is_some_and(|e| e.size_bytes() == 2)
            && let Some((const_off, _leaf)) =
                try_const_array_offset(&arr_ty, std::iter::once(&**index))
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
        {
            let src_off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
            return Some(format!("push\tword ptr {}", bp_addr(src_off)));
        }
        // `<member-dot-chain>` resolving to a 2-byte int/ptr field
        // — emit `push word ptr <bp/dgroup-addr>`. Stack struct
        // chain: `[bp-N+K]`. Global: `DGROUP:_<sym>+K`. Fixture
        // 1812 (`o.f(o.arg)` — push `o.arg` directly).
        if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } =
            &arg.kind
            && let Some((name, total_off, leaf_ty)) =
                self.try_member_dot_chain(base, field)
            && leaf_ty.size_bytes() == 2
        {
            if self.globals.contains(&name) {
                let label = if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                };
                return Some(format!("push\tword ptr {label}"));
            }
            if let LocalLocation::Stack(base_off) = self.locals.location_of(&name) {
                let src_off = base_off + i16::try_from(total_off).unwrap_or(i16::MAX);
                return Some(format!("push\tword ptr {}", bp_addr(src_off)));
            }
        }
        None
    }
    /// Place an argument into AX (the low byte of which is `al`) for
    /// the subsequent `push ax`. For a `char` param the load uses the
    /// 8-bit form so only AL is touched; AH is whatever happened to
    /// be there. For `int`, the standard 16-bit load.
    pub(crate) fn emit_arg_into_ax(&mut self, arg: &Expr, param_ty: Type) {
        if !param_ty.is_char_like() {
            // Array-decay-to-pointer at call sites: passing the bare
            // name of an array global (or array stack local) where a
            // pointer parameter is expected means the array's address,
            // not its value. BCC emits `mov ax, offset DGROUP:_<a>`
            // (or `lea ax, word ptr [bp-N]` for stack arrays) rather
            // than loading. Fixture 923.
            if let ExprKind::Ident(name) = &arg.kind {
                if let Some(gty) = self.globals.type_of(name)
                    && gty.array_elem().is_some()
                {
                    let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{name}\r\n");
                    return;
                }
                if self.locals.has(name)
                    && self.locals.type_of(name).array_elem().is_some()
                    && let LocalLocation::Stack(off) = self.locals.location_of(name)
                {
                    let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
                    return;
                }
            }
            let prev = self.in_arg_expr;
            self.in_arg_expr = true;
            self.emit_expr_to_ax(arg);
            self.in_arg_expr = prev;
            return;
        }
        // Char arg path.
        if let Some(v) = try_const_eval(arg) {
            // 8-bit immediate.
            let v8 = v & 0xFF;
            let _ = write!(self.out, "\tmov\tal,{v8}\r\n");
            return;
        }
        if let ExprKind::Ident(name) = &arg.kind {
            let ty = self.locals.type_of(name);
            assert!(
                ty.is_char_like(),
                "passing non-char `{name}` to a char parameter not yet supported (no fixture)"
            );
            match self.locals.location_of(name) {
                LocalLocation::Stack(off) => {
                    let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                }
            }
            return;
        }
        panic!("complex char-typed arg expression not yet supported (no fixture)");
    }
    /// If `e` is an identifier that refers to a register-resident
    /// local, return that register. Otherwise `None`.
    pub(crate) fn ident_in_register(&self, e: &Expr) -> Option<Reg> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        if !self.locals.has(name) {
            return None;
        }
        match self.locals.location_of(name) {
            LocalLocation::Reg(r) => Some(r),
            LocalLocation::Stack(_) => None,
        }
    }
    /// True iff `e`'s static type is `float` or `double`. Only the
    /// shapes that currently reach the FPU codegen path are checked
    /// (literals, ident locals/params, ident globals); others fall
    /// through to `false` and the cast / arithmetic dispatch keeps
    /// its integer-AX assumption.
    pub(crate) fn operand_is_float_like(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::FloatLit(_) | ExprKind::DoubleLit(_) => true,
            ExprKind::Ident(name) => {
                if self.locals.has(name) {
                    self.locals.type_of(name).is_float_like()
                } else if let Some(ty) = self.globals.type_of(name) {
                    ty.is_float_like()
                } else {
                    false
                }
            }
            // `a[K]` where `a` is a float array: the element type
            // is float-like. Same lookup pattern as Ident but
            // through the array's element type.
            ExprKind::ArrayIndex { array, .. } => {
                if let ExprKind::Ident(name) = &array.kind {
                    let ty = if self.locals.has(name) {
                        Some(self.locals.type_of(name).clone())
                    } else {
                        self.globals.type_of(name).cloned()
                    };
                    ty.as_ref()
                        .and_then(|t| t.array_elem())
                        .is_some_and(|elem| elem.is_float_like())
                } else {
                    false
                }
            }
            // Arithmetic between float-typed operands stays
            // float-typed; classified by either side (C's "usual
            // arithmetic conversions" promote int-with-float to
            // float, so a single float side suffices). Comparison
            // results (`<`, `==`, etc.) are int, so they fall
            // through to `false`.
            ExprKind::BinOp { op, left, right } if matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div
            ) => {
                self.operand_is_float_like(left) || self.operand_is_float_like(right)
            }
            // Function call returning a float / double: the return
            // type is known from signatures. Fixture 2144 (`(int)
            // half(10.0)`).
            ExprKind::Call { name, .. } => {
                self.signatures.ret_ty_of(name).is_some_and(|t| t.is_float_like())
            }
            ExprKind::Unary { op: UnaryOp::Neg, operand } => {
                self.operand_is_float_like(operand)
            }
            ExprKind::Cast { ty, operand } => {
                ty.is_float_like() || self.operand_is_float_like(operand)
            }
            _ => false,
        }
    }
    /// Push a float-typed expression onto the FPU stack. Handles
    /// literals (pooled in `s@`), named locals/globals (loaded via
    /// `fld dword/qword ptr ...`), and left-associative binary
    /// arithmetic (`a + b + c`, `a * b - c`). Each BinOp arm walks
    /// the left subtree onto the stack and then applies the
    /// arithmetic with the right operand as a memory operand —
    /// matches BCC's pattern of using the memory-form FPU ops
    /// rather than pushing both sides and using register-stack
    /// forms.
    pub(crate) fn emit_float_load_to_fpu(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::FloatLit(bits) => {
                // 1.0 / 0.0 use the FPU's built-in constants instead
                // of pooling — `fld1` / `fldz` save 4 bytes of code
                // plus 4 bytes of pooled data each. Fixture 2151
                // (`float f = 1.0f`).
                let f = f32::from_bits(*bits);
                if f == 1.0 {
                    self.out.extend_from_slice(b"\tfld1\t\r\n");
                    return;
                }
                if f == 0.0 && bits >> 31 == 0 {
                    self.out.extend_from_slice(b"\tfldz\t\r\n");
                    return;
                }
                let off = self.strings.intern_float(*bits);
                let src = if off == 0 {
                    "DGROUP:s@".to_owned()
                } else {
                    format!("DGROUP:s@+{off}")
                };
                let _ = write!(self.out, "\tfld\tdword ptr {src}\r\n");
            }
            ExprKind::DoubleLit(bits) => {
                // BCC narrows a double literal to a 32-bit float in
                // the pool whenever the value is exactly representable
                // as float — the FPU's 80-bit internal width fully
                // recovers it on `fld dword`. Same trick used by the
                // double-local initializer path; here it also covers
                // double *arguments* (fixture 1678 passes `3.5` →
                // pool 4 bytes, `fld dword`).
                let d = f64::from_bits(*bits);
                let f = d as f32;
                let (off, width) = if f64::from(f).to_bits() == *bits {
                    (self.strings.intern_float(f.to_bits()), "dword")
                } else {
                    (self.strings.intern_double(*bits), "qword")
                };
                let src = if off == 0 {
                    "DGROUP:s@".to_owned()
                } else {
                    format!("DGROUP:s@+{off}")
                };
                let _ = write!(self.out, "\tfld\t{width} ptr {src}\r\n");
            }
            ExprKind::Ident(name) => {
                let ty = if self.locals.has(name) {
                    self.locals.type_of(name).clone()
                } else if let Some(gty) = self.globals.type_of(name) {
                    gty.clone()
                } else {
                    panic!("unknown name in FPU load: {name}");
                };
                // Int / char operand in an FPU context (e.g. `i + d`
                // where i is int) — widen via fild + scratch slot,
                // mirroring the (float)<int> cast path. BCC stores
                // the int to the reserved scratch even when it
                // already lives on the stack — the scratch keeps
                // fild's source predictable. Fixture 1752.
                if ty.is_int_like() {
                    let scratch = self.locals.fild_int_scratch_offset().expect(
                        "int operand in FPU context without reserved fild scratch slot",
                    );
                    if self.pending_fpu_store_fwait {
                        self.out.extend_from_slice(b"\tfwait\t\r\n");
                        self.pending_fpu_store_fwait = false;
                    }
                    self.emit_expr_to_ax(e);
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},ax\r\n",
                        bp_addr(scratch),
                    );
                    let _ = write!(
                        self.out,
                        "\tfild\tword ptr {}\r\n",
                        bp_addr(scratch),
                    );
                    return;
                }
                let width = if matches!(ty, Type::Float) { "dword" } else { "qword" };
                if self.locals.has(name) {
                    let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                        panic!("float local must live on the stack: {name}")
                    };
                    let _ = write!(
                        self.out,
                        "\tfld\t{width} ptr {}\r\n",
                        bp_addr(off),
                    );
                } else {
                    let _ = write!(
                        self.out,
                        "\tfld\t{width} ptr DGROUP:_{name}\r\n",
                    );
                }
            }
            ExprKind::ArrayIndex { array, index } => {
                if let Some((addr, leaf_ty)) = self.resolve_float_array_addr(e) {
                    let width =
                        if matches!(leaf_ty, Type::Float) { "dword" } else { "qword" };
                    let _ = write!(self.out, "\tfld\t{width} ptr {addr}\r\n");
                } else if let ExprKind::Ident(name) = &array.kind
                    && let Some(gty) = self.globals.type_of(name)
                    && let Some(elem) = gty.array_elem()
                    && elem.is_float_like()
                {
                    // Variable-index float/double global array: load
                    // the index into BX, shift by log2(stride), then
                    // `fld <width> ptr DGROUP:_<name>[bx]`. Fixture
                    // 2150 (`arr[i]` for `static double arr[3]`).
                    let elem = elem.clone();
                    let stride = elem.size_bytes() as u32;
                    let shifts = stride.trailing_zeros();
                    if let Some(idx_addr) = self.int_lvalue_addr(index) {
                        let _ = write!(self.out, "\tmov\tbx,word ptr {idx_addr}\r\n");
                    } else {
                        self.emit_expr_to_ax(index);
                        self.out.extend_from_slice(b"\tmov\tbx,ax\r\n");
                    }
                    for _ in 0..shifts {
                        self.out.extend_from_slice(b"\tshl\tbx,1\r\n");
                    }
                    let width =
                        if matches!(elem, Type::Float) { "dword" } else { "qword" };
                    let _ = write!(
                        self.out,
                        "\tfld\t{width} ptr DGROUP:_{name}[bx]\r\n",
                    );
                    // BCC pulls in `__turboFloat` as an EXTDEF marker
                    // when a float-array element is accessed through
                    // BX-indexed addressing. Empirically observed on
                    // fixture 2150 only; constant-indexed accesses
                    // (1755, 2140) do not produce this symbol.
                    self.helpers.insert("__turboFloat".to_string());
                } else {
                    panic!("float ArrayIndex resolution failed");
                }
            }
            ExprKind::BinOp { op, left, right } => {
                // Left subexpression first onto the FPU stack, then
                // the arithmetic op with the right side as a memory
                // operand. Left-associative chains (`a + b + c`) walk
                // naturally: load a, fadd b (now top = a+b), fadd c.
                self.emit_float_load_to_fpu(left);
                // `<x> - 1.0f` peephole: BCC uses `fld1` + the
                // no-operand register-stack `fsub` (= `fsubp st(1),
                // st0`) instead of pooling 1.0 bytes for a memory
                // operand. Catches both `FloatLit(1.0f)` and
                // `DoubleLit(1.0)` since the FPU representation is
                // identical. Fixture 1673.
                if matches!(op, BinOp::Sub) && expr_is_float_one(right) {
                    self.out.extend_from_slice(b"\tfld1\t\r\n");
                    self.out.extend_from_slice(b"\tfsub\t\r\n");
                    return;
                }
                let mnem = match op {
                    BinOp::Add => "fadd",
                    BinOp::Sub => "fsub",
                    BinOp::Mul => "fmul",
                    BinOp::Div => "fdiv",
                    other => panic!("float BinOp {other:?} not supported yet"),
                };
                self.emit_float_arith_mem(mnem, right);
            }
            ExprKind::Unary { op: UnaryOp::Neg, operand } => {
                // `-<float>` on the FPU: evaluate the operand, then
                // change sign with `fchs`. Fixture 1753.
                self.emit_float_load_to_fpu(operand);
                self.out.extend_from_slice(b"\tfchs\t\r\n");
            }
            ExprKind::Call { name, args } => {
                // Float / double-returning call: the result lands on
                // the FPU stack-top per BCC's ABI. Just emit the
                // call. Fixture 1684.
                self.emit_call(name, args);
            }
            ExprKind::Cast { ty: cast_ty, operand } => {
                // Float↔float casts (`(float)d`, `(double)f`) are
                // no-ops at the FPU-stack level: the register stack
                // carries 80-bit extended precision regardless of
                // the operand-width prefix, so narrowing happens
                // when the value is finally stored back to memory
                // via `fstp dword/qword`. Just evaluate the operand.
                if cast_ty.is_float_like() && self.operand_is_float_like(operand) {
                    self.emit_float_load_to_fpu(operand);
                    return;
                }
                // `(float)<int>` / `(double)<int>` — BCC
                // materializes the int operand into a 2-byte scratch
                // slot at the bottom of the frame and then `fild`s
                // from there. The slot was reserved up-front by
                // `Locals::analyze` when it saw the cast. Fixture
                // 1675.
                if cast_ty.is_float_like() {
                    let scratch = self.locals.fild_int_scratch_offset().expect(
                        "(float)<int> cast without reserved fild scratch slot",
                    );
                    self.emit_expr_to_ax(operand);
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},ax\r\n",
                        bp_addr(scratch),
                    );
                    let _ = write!(
                        self.out,
                        "\tfild\tword ptr {}\r\n",
                        bp_addr(scratch),
                    );
                    return;
                }
                panic!("FPU cast from {:?} to {:?} not supported yet", operand.kind, cast_ty);
            }
            _ => panic!("FPU load not yet supported for {:?}", e.kind),
        }
    }
    /// Emit a float-comparison conditional branch. The sequence:
    /// `fld <left> / fcomp <right> / fstsw [scratch] / fwait /
    /// mov ax,[scratch] / sahf / j<cc> <slot>`. The j<cc>
    /// mnemonic is picked from the UNSIGNED family because the
    /// post-`sahf` flag positions (CF=C0=ST<op, ZF=C3=ST==op,
    /// PF=C2=unordered) align with the unsigned jcc encodings:
    /// `<` → jae (jump if !less), `<=` → ja (jump if !leq), etc.
    /// The fstsw scratch slot reuses the same fpu-scratch offset
    /// that fild allocates. Fixture 1674.
    pub(crate) fn emit_float_compare_branch(
        &mut self,
        op: BinOp,
        left: &Expr,
        right: &Expr,
        true_slot: Option<u32>,
        false_slot: Option<u32>,
    ) {
        let scratch = self.locals.fild_int_scratch_offset().expect(
            "float comparison without reserved fpu scratch slot",
        );
        self.emit_float_load_to_fpu(left);
        // Pick fcomp width from the right operand's static type
        // (matches what BCC emits — the operand width prefix on
        // fcomp reflects the operand we're comparing against).
        let width_str = self.float_operand_width(right);
        match &right.kind {
            ExprKind::Ident(name) => {
                let addr = if self.locals.has(name) {
                    let LocalLocation::Stack(off) = self.locals.location_of(name)
                    else {
                        panic!("float local must live on the stack: {name}")
                    };
                    bp_addr(off)
                } else {
                    format!("DGROUP:_{name}")
                };
                let _ = write!(self.out, "\tfcomp\t{width_str} ptr {addr}\r\n");
            }
            // Float / double literal RHS — pool the bytes (narrowing
            // a double to single when exactly representable, same
            // trick the init path uses) and fcomp against the pool
            // address. Fixture 2139 (`d > 2.0`). The `0.0` literal
            // gets the `fldz` + `fcompp` (no-operand) shape so no
            // pool entry is needed — saves 4 bytes. Fixture 2193
            // (`d == 0.0`).
            ExprKind::FloatLit(bits) => {
                if *bits == 0u32 || *bits == 0x8000_0000u32 {
                    self.out.extend_from_slice(b"\tfldz\t\r\n");
                    self.out.extend_from_slice(b"\tfcompp\t\r\n");
                } else {
                    let off = self.strings.intern_float(*bits);
                    let src = if off == 0 {
                        "DGROUP:s@".to_owned()
                    } else {
                        format!("DGROUP:s@+{off}")
                    };
                    let _ = write!(self.out, "\tfcomp\tdword ptr {src}\r\n");
                }
            }
            ExprKind::DoubleLit(bits) => {
                if *bits == 0u64 || *bits == 0x8000_0000_0000_0000u64 {
                    self.out.extend_from_slice(b"\tfldz\t\r\n");
                    self.out.extend_from_slice(b"\tfcompp\t\r\n");
                } else {
                    let d = f64::from_bits(*bits);
                    let f = d as f32;
                    let (off, width) = if f64::from(f).to_bits() == *bits {
                        (self.strings.intern_float(f.to_bits()), "dword")
                    } else {
                        (self.strings.intern_double(*bits), "qword")
                    };
                    let src = if off == 0 {
                        "DGROUP:s@".to_owned()
                    } else {
                        format!("DGROUP:s@+{off}")
                    };
                    let _ = write!(self.out, "\tfcomp\t{width} ptr {src}\r\n");
                }
            }
            _ => panic!(
                "float comparison right-operand shape not supported: {:?}",
                right.kind
            ),
        }
        let _ = write!(
            self.out,
            "\tfstsw\tword ptr {}\r\n",
            bp_addr(scratch),
        );
        self.out.extend_from_slice(b"\tfwait\t\r\n");
        let _ = write!(
            self.out,
            "\tmov\tax,word ptr {}\r\n",
            bp_addr(scratch),
        );
        self.out.extend_from_slice(b"\tsahf\t\r\n");
        // Post-sahf flags: CF=C0 (set if ST<op), ZF=C3 (set if
        // ST==op), PF=C2 (set if unordered).
        let (true_mnem, false_mnem) = match op {
            BinOp::Lt => ("jb", "jae"),
            BinOp::Le => ("jbe", "ja"),
            BinOp::Gt => ("ja", "jbe"),
            BinOp::Ge => ("jae", "jb"),
            BinOp::Eq => ("je", "jne"),
            BinOp::Ne => ("jne", "je"),
            other => panic!("not a comparison op: {other:?}"),
        };
        if let Some(fslot) = false_slot {
            let _ = write!(
                self.out,
                "\t{false_mnem}\tshort {}\r\n",
                self.label_ref(fslot),
            );
        } else if let Some(tslot) = true_slot {
            let _ = write!(
                self.out,
                "\t{true_mnem}\tshort {}\r\n",
                self.label_ref(tslot),
            );
        }
    }
    /// Width keyword (`dword` or `qword`) for an operand-position
    /// float expression. Used when picking the fcomp/fadd/etc.
    /// memory-operand prefix.
    pub(crate) fn float_operand_width(&self, e: &Expr) -> &'static str {
        match &e.kind {
            ExprKind::FloatLit(_) => "dword",
            ExprKind::DoubleLit(_) => "qword",
            ExprKind::Ident(name) => {
                let ty = if self.locals.has(name) {
                    self.locals.type_of(name)
                } else if let Some(gty) = self.globals.type_of(name) {
                    gty
                } else {
                    panic!("unknown name in float operand width lookup: {name}")
                };
                if matches!(ty, Type::Float) { "dword" } else { "qword" }
            }
            _ => "dword",
        }
    }
    /// Emit `<mnem> <dword|qword> ptr <operand>` where `operand` is a
    /// memory-resident float-typed expression — currently a named
    /// local or global. The width prefix matches the operand's static
    /// type; the family opcode tasm encodes (D8 for dword, DC for
    /// qword) and the ModR/M reg field follow from the mnemonic.
    pub(crate) fn emit_float_arith_mem(&mut self, mnem: &str, operand: &Expr) {
        match &operand.kind {
            ExprKind::Ident(name) => {
                let ty = if self.locals.has(name) {
                    self.locals.type_of(name).clone()
                } else if let Some(gty) = self.globals.type_of(name) {
                    gty.clone()
                } else {
                    panic!("unknown name in FPU arithmetic operand: {name}");
                };
                let width = if matches!(ty, Type::Float) { "dword" } else { "qword" };
                if self.locals.has(name) {
                    let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                        panic!("float local must live on the stack: {name}")
                    };
                    let _ = write!(
                        self.out,
                        "\t{mnem}\t{width} ptr {}\r\n",
                        bp_addr(off),
                    );
                } else {
                    let _ = write!(
                        self.out,
                        "\t{mnem}\t{width} ptr DGROUP:_{name}\r\n",
                    );
                }
            }
            ExprKind::ArrayIndex { .. } => {
                let (addr, leaf_ty) = self
                    .resolve_float_array_addr(operand)
                    .expect("float ArrayIndex resolution failed");
                let width =
                    if matches!(leaf_ty, Type::Float) { "dword" } else { "qword" };
                let _ = write!(self.out, "\t{mnem}\t{width} ptr {addr}\r\n");
            }
            // Float / double literal — pool the bytes in `s@` and
            // arithmetic against the pooled address. Narrows doubles
            // to single precision when exactly representable (same
            // trick the init / compare paths use). Fixture 2144.
            ExprKind::FloatLit(bits) => {
                let off = self.strings.intern_float(*bits);
                let src = if off == 0 {
                    "DGROUP:s@".to_owned()
                } else {
                    format!("DGROUP:s@+{off}")
                };
                let _ = write!(self.out, "\t{mnem}\tdword ptr {src}\r\n");
            }
            ExprKind::DoubleLit(bits) => {
                let d = f64::from_bits(*bits);
                let f = d as f32;
                let (off, width) = if f64::from(f).to_bits() == *bits {
                    (self.strings.intern_float(f.to_bits()), "dword")
                } else {
                    (self.strings.intern_double(*bits), "qword")
                };
                let src = if off == 0 {
                    "DGROUP:s@".to_owned()
                } else {
                    format!("DGROUP:s@+{off}")
                };
                let _ = write!(self.out, "\t{mnem}\t{width} ptr {src}\r\n");
            }
            _ => panic!("FPU memory-arith operand shape not supported: {:?}", operand.kind),
        }
    }
    /// Initialize a freshly-declared local with `init`.
    pub(crate) fn emit_init_local(&mut self, loc: LocalLocation, ty: &Type, init: &Expr) {
        // `float f = <const>;` / `double d = <const>;` — pool the IEEE
        // bytes in `s@`, then `fld <ptr> DGROUP:s@[+off] / fstp <ptr>
        // [bp-N]`. BCC narrows a `double = <float-representable>`
        // initializer to a 32-bit float in the pool, relying on the
        // FPU's 80-bit promotion + truncating `fstp qword` to recover
        // the original double bits (fixture 1672: `double d = 3.0`
        // stores 4 bytes of `3.0f` and reads as `dword`).
        if matches!(ty, Type::Float | Type::Double)
            && let LocalLocation::Stack(stack_off) = loc
        {
            // Literal initializers get a width-narrowing path: BCC
            // pools a `double = <float-representable>` constant as
            // a 32-bit float and relies on the FPU's 80-bit
            // promotion + truncating `fstp qword` to reconstruct
            // the original double bits (fixture 1672).
            // The 1.0 / 0.0 specials use the FPU built-ins (`fld1`
            // / `fldz`) instead of pooling — saves 4 bytes of code +
            // 4 bytes of pool data per use. Fixture 2151.
            let lit_bits = match (&init.kind, ty) {
                (ExprKind::FloatLit(bits), _) => Some(f64::from(f32::from_bits(*bits))),
                (ExprKind::DoubleLit(bits), _) => Some(f64::from_bits(*bits)),
                _ => None,
            };
            let used_builtin = if let Some(v) = lit_bits {
                if v == 1.0 {
                    self.out.extend_from_slice(b"\tfld1\t\r\n");
                    true
                } else if v == 0.0 && v.is_sign_positive() {
                    self.out.extend_from_slice(b"\tfldz\t\r\n");
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !used_builtin {
                let pool_load = match (&init.kind, ty) {
                    (ExprKind::FloatLit(bits), _) => {
                        Some((self.strings.intern_float(*bits), "dword"))
                    }
                    (ExprKind::DoubleLit(bits), Type::Float) => {
                        let f = f64::from_bits(*bits) as f32;
                        Some((self.strings.intern_float(f.to_bits()), "dword"))
                    }
                    (ExprKind::DoubleLit(bits), Type::Double) => {
                        let d = f64::from_bits(*bits);
                        let f = d as f32;
                        if f64::from(f).to_bits() == *bits {
                            Some((self.strings.intern_float(f.to_bits()), "dword"))
                        } else {
                            Some((self.strings.intern_double(*bits), "qword"))
                        }
                    }
                    _ => None,
                };
                if let Some((pool_off, load_width)) = pool_load {
                    let src = if pool_off == 0 {
                        "DGROUP:s@".to_owned()
                    } else {
                        format!("DGROUP:s@+{pool_off}")
                    };
                    let _ = write!(self.out, "\tfld\t{load_width} ptr {src}\r\n");
                } else {
                    // Non-literal initializer (BinOp, Ident, etc.) —
                    // route through the FPU expression walker. The
                    // result lands on the FPU stack top; the trailing
                    // `fstp` writes it back to the local.
                    self.emit_float_load_to_fpu(init);
                }
            }
            let store_width = if matches!(ty, Type::Float) { "dword" } else { "qword" };
            let _ = write!(
                self.out,
                "\tfstp\t{store_width} ptr {}\r\n",
                bp_addr(stack_off),
            );
            self.pending_fpu_store_fwait = true;
            return;
        }
        match loc {
            LocalLocation::Stack(off) => {
                // Stack array (and struct) initializer with a constant
                // image: BCC interns the flattened byte image into
                // `_DATA:s@` and emits the copy. Two shape thresholds
                // mirror the struct-return / struct-copy split:
                //   - size ≤ 4: inline AX/DX moves from `s@[+off]`
                //     into the local — the same path long-init takes
                //     when the source is memory. Fixtures 1612 (2B),
                //     1613 (4B).
                //   - size > 4: `N_SCOPY@` with far-far ptrs. Same
                //     helper used by struct returns and >4B struct
                //     copies. Fixtures 1465, 1475-1476, 1481, 1516,
                //     1616 (3-field struct, 6B), and many more.
                // 2-byte struct returned by a call: just the single
                // word in AX, store to the local. Fixture 1956.
                if let Type::Struct { .. } = ty
                    && ty.size_bytes() == 2
                    && let ExprKind::Call { .. } = &init.kind
                {
                    self.emit_expr_to_ax(init);
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    return;
                }
                // 4-byte struct returned by a call: same shape as the
                // long-call init — DX:AX = high:low, store DX → off+2
                // and AX → off. Fixture 3618.
                if let Type::Struct { .. } = ty
                    && ty.size_bytes() == 4
                    && let ExprKind::Call { .. } = &init.kind
                {
                    self.emit_expr_to_ax(init);
                    let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    return;
                }
                // `struct S x = f();` init from a struct-returning
                // call whose size is ∉ {1, 2, 4} — same hidden-tmp +
                // SCOPY pattern as the assignment form. Fixture 1685.
                if let Type::Struct { .. } = ty
                    && let ExprKind::Call { name: fname, args } = &init.kind
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
                // Struct-copy init: `struct S q = p;` where p is
                // another struct of the same type. Mirrors the
                // assign-side struct copy (size==4 inline AX/DX
                // pair, size>4 N_SCOPY@). Fixture 3198.
                if let Type::Struct { .. } = ty
                    && let ExprKind::Ident(src_name) = &init.kind
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
                if matches!(ty, Type::Array { .. } | Type::Struct { .. })
                    && let Some(bytes) = flatten_init_to_bytes(ty, init)
                {
                    let size = bytes.len() as u32;
                    let pool_off = self.strings.intern_blob(&bytes);
                    // Stack array aggregate init: BCC uses N_SCOPY@
                    // regardless of size — even for size 2 / 4 where
                    // the corresponding struct shape goes inline.
                    // Fixtures 1617, 1618, 1712, 1799-1801, 1883.
                    let is_array = matches!(ty, Type::Array { .. });
                    if !is_array && size == 2 {
                        let src = if pool_off == 0 {
                            "DGROUP:s@".to_owned()
                        } else {
                            format!("DGROUP:s@+{pool_off}")
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {src}\r\n");
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},ax\r\n",
                            bp_addr(off),
                        );
                        return;
                    }
                    if !is_array && size == 4 {
                        let src_hi = if pool_off + 2 == 0 {
                            "DGROUP:s@".to_owned()
                        } else {
                            format!("DGROUP:s@+{}", pool_off + 2)
                        };
                        let src_lo = if pool_off == 0 {
                            "DGROUP:s@".to_owned()
                        } else {
                            format!("DGROUP:s@+{pool_off}")
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr {src_hi}\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr {src_lo}\r\n");
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
                        return;
                    }
                    let src_addr = if pool_off == 0 {
                        "offset DGROUP:s@".to_owned()
                    } else {
                        format!("offset DGROUP:s@+{pool_off}")
                    };
                    // For structs (fixture 1616) BCC computes the
                    // dst address first, then pushes ss before the
                    // addr; same for src (mov ax, src; push ds; push
                    // ax). For arrays (fixture 1475) BCC keeps the
                    // pushes outside: push ss, lea ax, push ax,
                    // push ds, mov ax, push ax.
                    let is_struct = matches!(ty, Type::Struct { .. });
                    if is_struct {
                        let _ = write!(self.out, "\tlea\tax,{}\r\n", bp_addr(off));
                        self.out.extend_from_slice(b"\tpush\tss\r\n");
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        let _ = write!(self.out, "\tmov\tax,{src_addr}\r\n");
                        self.out.extend_from_slice(b"\tpush\tds\r\n");
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tpush\tss\r\n");
                        let _ = write!(self.out, "\tlea\tax,{}\r\n", bp_addr(off));
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        self.out.extend_from_slice(b"\tpush\tds\r\n");
                        let _ = write!(self.out, "\tmov\tax,{src_addr}\r\n");
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_SCOPY@\r\n");
                    self.helpers.insert("N_SCOPY@".to_string());
                    return;
                }
                // `long x = K;` stack local — two word stores, high
                // word at the upper slot offset then low word at the
                // lower slot. Mirrors fixture 205's global-long shape.
                // Fixture 210.
                if ty.is_long_like() {
                    if let Some(v) = try_const_eval(init) {
                        let lo = v & 0xFFFF;
                        let hi = (v >> 16) & 0xFFFF;
                        // `off` points to the LOW word (lower address);
                        // the high word lives at `off + 2`.
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
                    // `long x = g;` long local from long-like global —
                    // load (AX=high, DX=low) then store high (AX → off+2)
                    // and low (DX → off). Fixture 286.
                    if let ExprKind::Ident(src_name) = &init.kind
                        && let Some(src_ty) = self.globals.type_of(src_name)
                        && src_ty.is_long_like()
                    {
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // `long x = f();` long local from a function-call
                    // RHS. The call returns DX:AX (ABI: DX=high, AX=
                    // low); store DX → high (off+2), AX → low (off).
                    // Same pattern as `long g = f();` at global level
                    // (fixture 314). Fixture 315.
                    if let ExprKind::Call { .. } = &init.kind {
                        self.emit_expr_to_ax(init);
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                        return;
                    }
                    // `long x = g + K;` / `long x = g - K;` long local
                    // init from a long-global + constant. Same shape
                    // as the global-global path (slice 207) but
                    // storing into the stack local instead. Load g
                    // into AX:DX (globals convention since dest is
                    // memory), `add/sub dx, K_lo`, `adc/sbb ax,
                    // K_carry`, store. Fixture 350.
                    if let ExprKind::BinOp { op, left, right } = &init.kind
                        && matches!(op, BinOp::Add | BinOp::Sub)
                        && let ExprKind::Ident(src_name) = &left.kind
                        && let Some(src_ty) = self.globals.type_of(src_name)
                        && src_ty.is_long_like()
                        && let Some(k) = try_const_eval(right)
                    {
                        let signed = k as i32;
                        let (delta, carry) = if matches!(op, BinOp::Add) {
                            (signed, 0i16)
                        } else {
                            (-signed, -1i16)
                        };
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{src_name}+2\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr DGROUP:_{src_name}\r\n");
                        if let Ok(delta_i8) = i8::try_from(delta) {
                            let _ = write!(self.out, "\tadd\tdx,{delta_i8}\r\n");
                        } else {
                            let delta_u16 = (delta as i32) as u16;
                            let _ = write!(self.out, "\tadd\tdx,{delta_u16}\r\n");
                        }
                        let _ = write!(self.out, "\tadc\tax,{carry}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off + 2));
                        let _ = write!(self.out, "\tmov\tword ptr {},dx\r\n", bp_addr(off));
                        return;
                    }
                    // General long arith / lvalue-copy → stack local.
                    // Handles `long x = g + h;`, `long x = s.x + 5;`,
                    // `long x = a[1] + b[2];` etc. Fixture 357.
                    let dest_hi = bp_addr(off + 2);
                    let dest_lo = bp_addr(off);
                    if self.try_emit_long_value_to_dest(init, &dest_hi, &dest_lo) {
                        return;
                    }
                    // Fallback: int-typed initializer widened to
                    // long via cwd (or xor for unsigned). Fixture
                    // 1642 (`long r = i + 1` for int i).
                    if !self.expr_is_long_like(init) {
                        let unsigned = self.expr_int_is_unsigned(init);
                        self.emit_expr_to_ax(init);
                        if unsigned {
                            self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                        } else {
                            self.out.extend_from_slice(b"\tcwd\t\r\n");
                        }
                        let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
                        return;
                    }
                    // `long r = (long)<int> * <long> + K;` — promote
                    // the int to long via cwd, save the result on
                    // the stack, load the long into DX:AX, pop the
                    // saved pair back into CX:BX, then run the
                    // standard long-multiply helper. The trailing
                    // `+ K` folds into `add ax, K_lo; adc dx, K_hi`.
                    // Fixture 1777 (`(long)i * l + 7`).
                    if let ExprKind::BinOp { op: BinOp::Add, left: add_l, right: add_r } = &init.kind
                        && let Some(k) = try_const_eval(add_r)
                        && let ExprKind::BinOp { op: BinOp::Mul, left: mul_l, right: mul_r } = &add_l.kind
                        && let Some(int_inner) = strip_cast(mul_l)
                        && let ExprKind::Ident(int_name) = &int_inner.kind
                        && self.named_int_lvalue_addr(int_name).is_some()
                        && let ExprKind::Ident(long_name) = &mul_r.kind
                        && let Some((long_hi_addr, long_lo_addr)) =
                            self.long_lvalue_addr_pair(mul_r).map(|p| p)
                            .or_else(|| {
                                if self.locals.has(long_name)
                                    && self.locals.type_of(long_name).is_long_like()
                                    && let LocalLocation::Stack(lo) = self.locals.location_of(long_name)
                                {
                                    Some((bp_addr(lo + 2), bp_addr(lo)))
                                } else {
                                    None
                                }
                            })
                    {
                        let int_addr = self.named_int_lvalue_addr(int_name).unwrap();
                        let _ = write!(self.out, "\tmov\tax,word ptr {int_addr}\r\n");
                        self.out.extend_from_slice(b"\tcwd\t\r\n");
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        self.out.extend_from_slice(b"\tpush\tdx\r\n");
                        let _ = write!(self.out, "\tmov\tdx,word ptr {long_hi_addr}\r\n");
                        let _ = write!(self.out, "\tmov\tax,word ptr {long_lo_addr}\r\n");
                        self.out.extend_from_slice(b"\tpop\tcx\r\n");
                        self.out.extend_from_slice(b"\tpop\tbx\r\n");
                        self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                        self.helpers.insert("N_LXMUL@".to_string());
                        let k_lo = (k & 0xFFFF) as u16;
                        let k_hi = ((k >> 16) & 0xFFFF) as u16;
                        let _ = write!(self.out, "\tadd\tax,{k_lo}\r\n");
                        let _ = write!(self.out, "\tadc\tdx,{k_hi}\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
                        let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
                        return;
                    }
                    panic!("non-constant long local init not yet supported (no fixture)");
                }
                // Stack init: prefer the immediate-store form when the
                // initializer folds to a constant. For `char` we emit
                // `byte ptr` (fixture 011); for `int`, `word ptr`.
                // Negative constants like `int x = -5;` come through
                // `try_const_eval` as a wide u32; mask to the width
                // tasm expects (fixture 632).
                if let Some(v) = try_const_eval(init) {
                    let width = ptr_width(ty);
                    let v_masked = if ty.is_char_like() { v & 0xFF } else { v & 0xFFFF };
                    let _ = write!(
                        self.out,
                        "\tmov\t{width} ptr {},{v_masked}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // `<ident> * 0` annihilator fold: with a pure ident
                // on the left and 0 on the right, the result is
                // always 0 and BCC emits the same direct mem-imm
                // store. Fixture 2011 (`int r = x * 0`).
                if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &init.kind
                    && matches!(left.kind, ExprKind::Ident(_))
                    && try_const_eval(right) == Some(0)
                {
                    let width = ptr_width(ty);
                    let _ = write!(
                        self.out,
                        "\tmov\t{width} ptr {},0\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // Array-to-pointer decay init: `T *p = arr;` for
                // `arr` a global array. Store the symbol's offset
                // directly. Fixture 2541 (`p = arr` for global arr).
                if ty.pointee().is_some()
                    && let ExprKind::Ident(sym) = &init.kind
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
                // `T *p = &arr[K];` where arr is a global array —
                // direct immediate-to-memory store with the element
                // offset folded into the symbol reference. Saves the
                // AX round-trip from the generic path. Fixture 2269.
                if ty.pointee().is_some()
                    && let ExprKind::AddressOfArrayElem { array, byte_offset } = &init.kind
                    && self.globals.contains(array)
                {
                    // Far pointer (compact / large / huge) — also
                    // store DS into the segment half. Fixture 3958.
                    if matches!(ty, Type::FarPointer { .. }) {
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},ds\r\n",
                            bp_addr(off + 2),
                        );
                    }
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
                // Address-of-global init: `int *p = &x;` for `x` a
                // non-array global. Store the symbol's offset
                // directly. Fixture 1964 (`int *p = &x`).
                if ty.pointee().is_some()
                    && matches!(ty, Type::Pointer(_))
                    && let ExprKind::AddressOf(sym) = &init.kind
                    && self.globals.type_of(sym).is_some()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset DGROUP:_{sym}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // Far-pointer init from `&<global>` (with or without
                // an explicit `(T far *)` cast): the segment half is
                // DS (small-model globals live in DGROUP, which is
                // aliased to DS at runtime), the offset half takes
                // the symbol address. Fixtures 2058
                // (`int far *p = &g;`), 1768 / 1667 (compact / large
                // models, implicit far).
                if matches!(ty, Type::FarPointer { .. })
                    && let ExprKind::AddressOf(sym) = &init.kind
                    && self.globals.type_of(sym).is_some()
                {
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},ds\r\n",
                        bp_addr(off + 2),
                    );
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},offset DGROUP:_{sym}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // Far-pointer init from `(T far *)&<local>` or
                // `(T far *)<stack_array>`: segment half is SS (the
                // local lives on the stack), offset half is the
                // lea-computed bp-relative address. BCC's emission
                // order is `lea ax,[bp+lo]; mov [bp+hi],ss; mov
                // [bp+lo_of_p],ax`. Fixture 1649
                // (`int far *p = (int far *)&x;`), 1650
                // (write through), 2250 (same shape).
                if matches!(ty, Type::FarPointer { .. })
                    && let Some(addr_expr) = strip_cast(init)
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
                    let _ = write!(
                        self.out,
                        "\tlea\tax,word ptr {}\r\n",
                        bp_addr(lea_off_i16),
                    );
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},ss\r\n",
                        bp_addr(off + 2),
                    );
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},ax\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // String-literal init for a pointer local: `char *s =
                // "lit";` lowers to `mov word ptr [bp-N], offset
                // DGROUP:s@+K` directly, no AX roundtrip. Fixture
                // 1931 (`char *s = "ABCD"`). Under huge-mode far-
                // data promotion the slot is 4 bytes — the segment
                // half stores DS (which the prologue just loaded
                // with the module's data segment), and the offset
                // half takes the symbol address. Fixture 3716
                // (`char *s = "hi";` under -mh).
                if let ExprKind::StringLit(bytes) = &init.kind
                    && let Some(pointee) = ty.pointee()
                    && pointee.is_char_like()
                {
                    let pool_off = self.strings.intern(bytes);
                    if matches!(ty, Type::FarPointer { .. }) {
                        let src = if pool_off == 0 {
                            "offset s@".to_owned()
                        } else {
                            format!("offset s@+{pool_off}")
                        };
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},ds\r\n",
                            bp_addr(off + 2),
                        );
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},{src}\r\n",
                            bp_addr(off),
                        );
                    } else {
                        let src = if pool_off == 0 {
                            "offset DGROUP:s@".to_owned()
                        } else {
                            format!("offset DGROUP:s@+{pool_off}")
                        };
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},{src}\r\n",
                            bp_addr(off),
                        );
                    }
                    return;
                }
                // `char c = f();` where f returns char — call returns
                // the value in AL; we only need to store the low byte.
                // Skip the cbw widen the call site normally appends.
                // Fixture 2451.
                if ty.is_char_like()
                    && let ExprKind::Call { name, args } = &init.kind
                    && self.signatures.ret_ty_of(name).map_or(false, |t| t.is_char_like())
                {
                    self.emit_call(name, args);
                    let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
                    return;
                }
                // Function-pointer init: `int (*p)(void) = f;` →
                // `mov word ptr [bp-N],offset _f`. We detect this by
                // the init being a bare ident that names a function
                // defined in this TU (fixture 110). The explicit
                // address-of form `= &f` is equivalent — in C a
                // function designator and `&function` both yield the
                // function's address — so it folds to the same store
                // (fixture 4198). In far-code memory models (medium /
                // large / huge) the slot is 4 bytes (`FarPointer`) and
                // the segment half takes CS — the callee lives in the
                // caller's own code segment, so the runtime CS register
                // supplies the right paragraph. Fixture 2211.
                if let (ExprKind::Ident(name) | ExprKind::AddressOf(name)) = &init.kind
                    && self.signatures.params_of(name).is_some()
                {
                    let sym = function_symbol(name);
                    if matches!(ty, Type::FarPointer { .. }) {
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},cs\r\n",
                            bp_addr(off + 2),
                        );
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},offset {sym}\r\n",
                            bp_addr(off),
                        );
                    } else {
                        let _ = write!(
                            self.out,
                            "\tmov\tword ptr {},offset {sym}\r\n",
                            bp_addr(off),
                        );
                    }
                    return;
                }
                // Int local init from a non-AX register-resident
                // source: `mov word ptr [bp-N], <reg>` directly.
                // Saves the round-trip through AX (`mov ax, <reg>;
                // mov [bp-N], ax`). Also accepts `x | 0` / `x ^ 0`
                // identity wrappers — BCC's frontend folds these at
                // the AST level (but does NOT fold `x << 0` /
                // `x >> 0`, which keep their AX-round-trip shape).
                // Fixture 1711 (`int c = x | 0` with x in SI).
                let init_ident = match &init.kind {
                    ExprKind::Ident(n) => Some(n.as_str()),
                    ExprKind::BinOp { op: BinOp::BitOr | BinOp::BitXor, left, right }
                        if try_const_eval(right) == Some(0) =>
                    {
                        match &left.kind {
                            ExprKind::Ident(n) => Some(n.as_str()),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if ty.is_int_like()
                    && let Some(name) = init_ident
                    && self.locals.has(name)
                    && self.locals.type_of(name).is_int_like()
                    && let LocalLocation::Reg(reg) = self.locals.location_of(name)
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
                // Int local init from a Mod expression: the idiv
                // leaves the remainder in DX. emit_expr_to_ax would
                // normally `mov ax, dx` to materialize the result in
                // AX before our `mov [dest], ax`. Skip the move and
                // store DX directly. Fixture 2089 (`int r = x % 7`),
                // 2088, 1723.
                //
                // Skip this peephole when the RHS is an
                // unsigned-by-pow2 strength reduction (`x % K` with
                // K=pow2 for unsigned x): that path emits `and ax,
                // K-1` and leaves the result in AX, not DX (fixtures
                // 1935, 2087).
                let mod_strength_reduced = matches!(&init.kind,
                    ExprKind::BinOp { op: BinOp::Mod, left, right }
                    if self.expr_is_unsigned(left)
                        && matches!(try_const_eval(right),
                            Some(v) if v > 0 && v.is_power_of_two())
                );
                if ty.is_int_like()
                    && let ExprKind::BinOp { op: BinOp::Mod, .. } = &init.kind
                    && !mod_strength_reduced
                {
                    // Evaluate up to the idiv/div but inhibit the
                    // final mov ax,dx by setting a one-shot flag.
                    self.skip_mod_to_ax = true;
                    self.emit_expr_to_ax(init);
                    self.skip_mod_to_ax = false;
                    let _ = write!(
                        self.out,
                        "\tmov\tword ptr {},dx\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // Non-constant char init. Peephole for `(char)<int-
                // local>` and the bare-ident `char b = a;` (a is
                // either char or int local): load the low byte of
                // the source slot directly with `mov al, byte ptr
                // [bp-Nsrc]`, then store with `mov byte ptr [bp-Nc],
                // al`. Fixture 1039 (`char c = (char)n;`), fixture
                // 1040 (`char b = a;`).
                if ty.is_char_like() {
                    // Unwrap an outer `(char)` cast — the byte-load
                    // sequence is the same whether the source was
                    // already char or was cast from int.
                    let src_expr = if let ExprKind::Cast { ty: cast_ty, operand } = &init.kind
                        && cast_ty.is_char_like()
                    {
                        operand.as_ref()
                    } else {
                        init
                    };
                    if let ExprKind::Ident(src_name) = &src_expr.kind
                        && self.locals.has(src_name)
                        && (self.locals.type_of(src_name).is_char_like()
                            || self.locals.type_of(src_name).is_int_like())
                        && let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                    {
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
                    // Char binop on two char locals stored back to a
                    // char destination: BCC stays at byte width (no
                    // int promotion) because the result is truncated
                    // anyway. Pattern: `mov al, byte ptr <a>; <mnem>
                    // al, byte ptr <b>; mov byte ptr <c>, al`.
                    // Fixture 1046 (`char c = a + b;`).
                    if let ExprKind::BinOp { op, left, right } = &src_expr.kind
                        && let ExprKind::Ident(lname) = &left.kind
                        && let ExprKind::Ident(rname) = &right.kind
                        && self.locals.has(lname)
                        && self.locals.has(rname)
                        && self.locals.type_of(lname).is_char_like()
                        && self.locals.type_of(rname).is_char_like()
                        && let LocalLocation::Stack(loff) = self.locals.location_of(lname)
                        && let LocalLocation::Stack(roff) = self.locals.location_of(rname)
                    {
                        let mnem = match op {
                            BinOp::Add => Some("add"),
                            BinOp::Sub => Some("sub"),
                            BinOp::BitAnd => Some("and"),
                            BinOp::BitOr => Some("or"),
                            BinOp::BitXor => Some("xor"),
                            _ => None,
                        };
                        if let Some(mnem) = mnem {
                            let _ = write!(
                                self.out,
                                "\tmov\tal,byte ptr {}\r\n",
                                bp_addr(loff)
                            );
                            let _ = write!(
                                self.out,
                                "\t{mnem}\tal,byte ptr {}\r\n",
                                bp_addr(roff)
                            );
                            let _ = write!(
                                self.out,
                                "\tmov\tbyte ptr {},al\r\n",
                                bp_addr(off)
                            );
                            return;
                        }
                    }
                    // Char-shift-by-const init. Two distinct shapes:
                    //  - `<<` (left shift): byte arithmetic on AL
                    //    directly — `shl al, 1` repeated K times.
                    //    No widen needed because the high bits fall
                    //    off either way. Fixture 1085.
                    //  - `>>` (right shift): C promotes char → signed
                    //    int before the shift, so BCC widens with
                    //    `cbw` (signed char) or `mov ah, 0` (unsigned
                    //    char), then always `sar` regardless of the
                    //    operand's signedness because the promoted
                    //    type is signed int. Fixtures 1082, 1086,
                    //    1087.
                    if let ExprKind::BinOp { op, left, right } = &src_expr.kind
                        && matches!(op, BinOp::Shr | BinOp::Shl)
                        && let ExprKind::Ident(src_name) = &left.kind
                        && self.locals.has(src_name)
                        && self.locals.type_of(src_name).is_char_like()
                        && let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                        && let Some(k) = try_const_eval(right)
                    {
                        let unsigned = self.locals.type_of(src_name).is_unsigned();
                        let k_u = (k as u32) & 0x1F;
                        let _ = write!(
                            self.out,
                            "\tmov\tal,byte ptr {}\r\n",
                            bp_addr(src_off)
                        );
                        if matches!(op, BinOp::Shl) {
                            // Byte-level left shift on AL only.
                            if k_u <= 3 {
                                for _ in 0..k_u {
                                    self.out.extend_from_slice(b"\tshl\tal,1\r\n");
                                }
                            } else {
                                let _ = write!(self.out, "\tmov\tcl,{k_u}\r\n");
                                self.out.extend_from_slice(b"\tshl\tal,cl\r\n");
                            }
                        } else {
                            // Right shift: widen, then signed `sar`
                            // (promoted-int signedness — always
                            // signed because both `char` and `uchar`
                            // promote to `int`).
                            if !unsigned {
                                self.out.extend_from_slice(b"\tcbw\r\n");
                            } else {
                                self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                            }
                            if k_u <= 3 {
                                for _ in 0..k_u {
                                    self.out.extend_from_slice(b"\tsar\tax,1\r\n");
                                }
                            } else {
                                let _ = write!(self.out, "\tmov\tcl,{k_u}\r\n");
                                self.out.extend_from_slice(b"\tsar\tax,cl\r\n");
                            }
                        }
                        let _ = write!(
                            self.out,
                            "\tmov\tbyte ptr {},al\r\n",
                            bp_addr(off)
                        );
                        return;
                    }
                    // `char b = s.c;` — char init from a `Dot`-kind
                    // Member whose leaf is char-like. Same byte-load
                    // shape as the assign-from-Member peephole
                    // (batch 266): `mov al, byte ptr <field-addr>;
                    // mov byte ptr <dest>, al`. Fixture 1124.
                    if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } =
                        &src_expr.kind
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
                            let _ = write!(
                                self.out,
                                "\tmov\tbyte ptr {},al\r\n",
                                bp_addr(off)
                            );
                            return;
                        }
                        if let LocalLocation::Stack(base_bp) =
                            self.locals.location_of(&src_name)
                        {
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
                    // `char t = *p;` for a register-resident char
                    // pointer p: byte load through [reg], then byte
                    // store to the local. Fixture 3529.
                    if let ExprKind::Deref(inner) = &init.kind
                        && let ExprKind::Ident(p_name) = &inner.kind
                        && self.locals.has(p_name)
                        && let LocalLocation::Reg(p_reg) = self.locals.location_of(p_name)
                        && let Some(pointee) = self.locals.type_of(p_name).pointee()
                        && pointee.is_char_like()
                    {
                        let p_reg_name = p_reg.name();
                        let _ = write!(self.out, "\tmov\tal,byte ptr [{p_reg_name}]\r\n");
                        let _ = write!(
                            self.out,
                            "\tmov\tbyte ptr {},al\r\n",
                            bp_addr(off)
                        );
                        return;
                    }
                    panic!("non-constant char local init shape not yet supported");
                }
                // Pointers and ints share the int-like word-sized
                // path: compute into AX, then store as `word ptr`.
                assert!(
                    ty.is_int_like(),
                    "non-constant init for non-int-like type {:?} not yet supported",
                    ty
                );
                // Pointer init from `<stack-array> + K_const`: fold
                // the element offset into the LEA's displacement so
                // we emit `lea ax, [bp+(base+K*stride)]` directly
                // instead of `lea ax, [bp+base]; add/inc ax, K`.
                // Same shape as the register-init peephole in
                // `emit_store_reg`. Fixture 1066 (`int *p = a + 1;`
                // with p stack-resident).
                if let ExprKind::BinOp { op: BinOp::Add, left, right } = &init.kind
                    && let ExprKind::Ident(arr_name) = &left.kind
                    && self.locals.has(arr_name)
                    && let Some(elem_ty) = self.locals.type_of(arr_name).array_elem()
                    && let Some(k) = try_const_eval(right)
                    && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
                {
                    let stride = i32::from(elem_ty.size_bytes());
                    let adj_off = i32::from(base_off) + (k as i32) * stride;
                    let adj_off_i16 = i16::try_from(adj_off).expect("array+const offset fits in i16");
                    let _ = write!(
                        self.out,
                        "\tlea\tax,word ptr {}\r\n",
                        bp_addr(adj_off_i16)
                    );
                    let _ = write!(self.out, "\tmov\tword ptr {},ax\r\n", bp_addr(off));
                    return;
                }
                self.emit_expr_to_ax(init);
                let src = if self.try_collapse_lhs_clobber_to_dx() { "dx" } else { "ax" };
                let _ = write!(self.out, "\tmov\tword ptr {},{src}\r\n", bp_addr(off));
            }
            LocalLocation::Reg(reg) => self.emit_store_reg(reg, init),
        }
    }
    /// Walk a deref chain and land the address-to-be-deref'd-once-
    /// more in BX. `depth` is the number of *visible* `*`s above the
    /// base ident (so for `**p` called from the outer `*`, depth=1).
    /// Emits the base load and `depth` intermediate `mov bx,[bx]`
    /// chain steps; the caller emits the final read or write through
    /// `[bx]`. Returns the type of the value at `[bx]` (after
    /// `depth + 1` total pointer peels).
    pub(crate) fn emit_chain_to_bx(&mut self, base_name: &str, depth: u32) -> Type {
        let is_global = self.globals.type_of(base_name).is_some();
        let base_ty = if is_global {
            self.globals.type_of(base_name).expect("checked above").clone()
        } else {
            self.locals.type_of(base_name).clone()
        };
        let mut final_ty = base_ty;
        for _ in 0..=depth {
            let next = final_ty
                .pointee()
                .unwrap_or_else(|| panic!("`*{base_name}`: chain too deep for its type"))
                .clone();
            final_ty = next;
        }
        // When `depth > 0` and the root pointer is in a non-BX
        // register, combine the `mov bx,<reg>` + `mov bx,[bx]` pair
        // into a single `mov bx,[<reg>]` (the first peel). Saves 2
        // bytes per chain. Mirrors BCC's actual shape for fixture
        // 1232 (`**pp` with pp in SI → `mov bx,[si]; mov ax,[bx]`).
        let mut remaining_peels = depth;
        if is_global {
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{base_name}\r\n");
        } else {
            match self.locals.location_of(base_name) {
                LocalLocation::Reg(reg) if reg.name() == "bx" => {}
                LocalLocation::Reg(reg) if depth > 0 => {
                    let _ = write!(
                        self.out,
                        "\tmov\tbx,word ptr [{}]\r\n",
                        reg.name(),
                    );
                    remaining_peels -= 1;
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tmov\tbx,{}\r\n", reg.name());
                }
                LocalLocation::Stack(_) => {
                    panic!("stack-resident pointer chain root not yet supported (no fixture)");
                }
            }
        }
        for _ in 0..remaining_peels {
            self.out.extend_from_slice(b"\tmov\tbx,word ptr [bx]\r\n");
        }
        final_ty
    }
    /// `*(<ptr> + <offset>)` for fixtures 091, 092, 094. The pointer
    /// name + pointee type are extracted by the caller; `offset` is
    /// the right side of the `+`.
    /// `*(<seg-selector> + <offset>) = v` write. Loads the segment
    /// into ES, then stores via `es:[<offset>]`. Constant offset
    /// becomes the displacement; constant RHS folds to a single
    /// `mov <width> ptr es:[<off>], imm`. Fixture 4072.
    pub(crate) fn emit_seg_selector_deref_write(
        &mut self,
        name: &str,
        pointee: &Type,
        offset: &Expr,
        value: &Expr,
    ) {
        let p_addr = match self.locals.location_of(name) {
            LocalLocation::Stack(off) => bp_addr(off),
            LocalLocation::Reg(_) => {
                panic!("`_seg` pointer unexpectedly enregistered — should stay on stack");
            }
        };
        let width = ptr_width(pointee);
        let off_operand = if let Some(k) = try_const_eval(offset) {
            format!("{}", k & 0xFFFF)
        } else if let ExprKind::Ident(q_name) = &offset.kind
            && self.locals.has(q_name)
            && let LocalLocation::Reg(q_reg) = self.locals.location_of(q_name)
        {
            q_reg.name().to_owned()
        } else {
            panic!("`_seg` deref-write offset must be a constant or register-resident local (no fixture)");
        };
        let _ = write!(self.out, "\tmov\tes,word ptr {p_addr}\r\n");
        if let Some(v) = try_const_eval(value) {
            let v_masked = if pointee.is_char_like() { v & 0xFF } else { v & 0xFFFF };
            let _ = write!(
                self.out,
                "\tmov\t{width} ptr es:[{off_operand}],{v_masked}\r\n",
            );
            return;
        }
        self.emit_expr_to_ax(value);
        let src = if pointee.is_char_like() { "al" } else { "ax" };
        let _ = write!(
            self.out,
            "\tmov\t{width} ptr es:[{off_operand}],{src}\r\n",
        );
    }
    /// `*(<seg-selector> + <offset>)` value-context read. Loads the
    /// segment into ES, then accesses `es:[<offset>]`. `<offset>` can
    /// be a constant (baked into displacement) or a register-resident
    /// near-pointer / int local (used as `es:[<reg>]`). The pointee's
    /// width picks `byte`/`word` and triggers `cbw` for char.
    /// Fixtures 4070 (offset 0), 4071 (const offset), 4073 (seg + near).
    pub(crate) fn emit_seg_selector_deref_read(
        &mut self,
        name: &str,
        pointee: &Type,
        offset: &Expr,
    ) {
        let p_addr = match self.locals.location_of(name) {
            LocalLocation::Stack(off) => bp_addr(off),
            LocalLocation::Reg(_) => {
                panic!("`_seg` pointer unexpectedly enregistered — should stay on stack");
            }
        };
        let reg_name = if pointee.is_char_like() { "al" } else { "ax" };
        let width = ptr_width(pointee);
        let off_operand = if let Some(k) = try_const_eval(offset) {
            format!("{}", k & 0xFFFF)
        } else if let ExprKind::Ident(q_name) = &offset.kind
            && self.locals.has(q_name)
            && let LocalLocation::Reg(q_reg) = self.locals.location_of(q_name)
        {
            q_reg.name().to_owned()
        } else {
            panic!("`_seg` deref offset must be a constant or register-resident local (no fixture)");
        };
        let _ = write!(self.out, "\tmov\tes,word ptr {p_addr}\r\n");
        let _ = write!(
            self.out,
            "\tmov\t{reg_name},{width} ptr es:[{off_operand}]\r\n",
        );
        if pointee.is_char_like() {
            self.emit_widen_al(pointee);
        }
    }
    /// `p[<index>]` where `p` is a pointer (not an array). Equivalent
    /// to `*(p + index)`. Fixture 088: `s[0]` with `s: char *` in SI
    /// emits `mov al, byte ptr [si] / cbw`. Variable-indexed pointer
    /// access isn't observed yet — would need an add-into-bx step.
    /// `p[K]` where `p` is a global pointer (not array). Load `p`
    /// into BX from `DGROUP:_p`, then deref. Fixture 192
    /// (`char *p = "hi"; return p[0];`).
    pub(crate) fn emit_global_pointer_index_to_ax(&mut self, ptr_name: &str, pointee: Type, index: &Expr) {
        let Some(k) = try_const_eval(index) else {
            panic!("variable-indexed global pointer access not yet supported (no fixture)");
        };
        // Far-pointer global (compact / large pointer-promotion):
        // `les bx, dword ptr DGROUP:_p; mov al, byte ptr es:[bx]`.
        // Fixtures 3760 / 3761.
        if matches!(
            self.globals.type_of(ptr_name),
            Some(Type::FarPointer { .. })
        ) {
            let _ = write!(
                self.out,
                "\tles\tbx,dword ptr DGROUP:_{ptr_name}\r\n"
            );
            let stride = u32::from(pointee.size_bytes());
            let byte_off = k * stride;
            let addr = if byte_off == 0 {
                "es:[bx]".to_owned()
            } else {
                format!("es:[bx+{byte_off}]")
            };
            if pointee.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                if pointee.is_unsigned() {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
            return;
        }
        let _ = write!(
            self.out,
            "\tmov\tbx,word ptr DGROUP:_{ptr_name}\r\n"
        );
        let stride = u32::from(pointee.size_bytes());
        let byte_off = k * stride;
        let addr = if byte_off == 0 {
            "[bx]".to_owned()
        } else {
            format!("[bx+{byte_off}]")
        };
        if pointee.is_char_like() {
            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        } else {
            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
        }
    }
    pub(crate) fn emit_pointer_index_to_ax(&mut self, ptr_name: &str, pointee: Type, index: &Expr) {
        let Some(k) = try_const_eval(index) else {
            // `p[<idx-in-SI-or-DI>]` for a char-pointer (stride=1):
            // BCC uses [bx+si] / [bx+di] indexed addressing directly.
            // Loads the pointer into BX, then byte-load via the
            // indexed mode — no separate `mov ax, idx; add bx, ax`
            // pair. Works only for stride=1 (char-pointers); int+
            // strides need the index scaled first, which would
            // clobber the index reg. Fixture 1420 (`t += s[i]`).
            if pointee.is_char_like()
                && let ExprKind::Ident(idx_name) = &index.kind
                && self.locals.has(idx_name)
                && self.locals.type_of(idx_name).is_int_like()
                && let LocalLocation::Reg(idx_reg) = self.locals.location_of(idx_name)
                && matches!(idx_reg, Reg::Si | Reg::Di)
            {
                match self.locals.location_of(ptr_name) {
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
                }
                let _ = write!(
                    self.out,
                    "\tmov\tal,byte ptr [bx+{}]\r\n",
                    idx_reg.name(),
                );
                self.emit_widen_al(&pointee);
                return;
            }
            // Variable index. BCC's shape (fixture 1339):
            //   mov ax, <index>
            //   shl ax, 1     ; scale by stride (int = 2)
            //   mov bx, <ptr> ; load pointer
            //   add bx, ax    ; pointer arithmetic
            //   mov ax, [bx]  ; deref
            // Char stride is 1 → no shl; long would need shl ax, 2.
            //
            // Char-stride memory-direct add: when stride is 1 (no
            // scaling needed) and the index is a simple int lvalue,
            // BCC skips the AX round-trip and adds the index memory
            // directly to BX. Saves the `mov ax, idx` (`add bx, ax`
            // → `add bx, word ptr <idx>`). Fixture 2851 (`return
            // s[i]` for `char *s, int i`).
            let stride = u32::from(pointee.size_bytes());
            if stride == 1
                && let Some(idx_addr) = self.int_lvalue_addr(index)
            {
                match self.locals.location_of(ptr_name) {
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
                }
                let _ = write!(self.out, "\tadd\tbx,word ptr {idx_addr}\r\n");
                if pointee.is_char_like() {
                    self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                    if pointee.is_unsigned() {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                } else {
                    self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
                }
                return;
            }
            self.emit_expr_to_ax(index);
            for _ in 0..stride.trailing_zeros() {
                self.out.extend_from_slice(b"\tshl\tax,1\r\n");
            }
            match self.locals.location_of(ptr_name) {
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
            }
            self.out.extend_from_slice(b"\tadd\tbx,ax\r\n");
            if pointee.is_char_like() {
                self.out.extend_from_slice(b"\tmov\tal,byte ptr [bx]\r\n");
                if pointee.is_unsigned() {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
            } else {
                self.out.extend_from_slice(b"\tmov\tax,word ptr [bx]\r\n");
            }
            return;
        };
        // Stack-resident far pointer indexed read: `s[K]` for a
        // `T far *` local. BCC's shape: `les bx, [bp+s]; mov ax
        // / al, es:[bx + K*stride]` with the pointee-width widen.
        // Fixtures 3716 (K=0 huge string lit), 3958 (K=2 large
        // int-array decay).
        if matches!(self.locals.type_of(ptr_name), Type::FarPointer { .. })
            && let LocalLocation::Stack(off) = self.locals.location_of(ptr_name)
        {
            let stride = i32::from(pointee.size_bytes());
            let byte_off = (k as i32).wrapping_mul(stride);
            let addr = if byte_off == 0 {
                "es:[bx]".to_owned()
            } else if byte_off > 0 {
                format!("es:[bx+{byte_off}]")
            } else {
                format!("es:[bx{byte_off}]")
            };
            let _ = write!(self.out, "\tles\tbx,dword ptr {}\r\n", bp_addr(off));
            if pointee.is_char_like() {
                let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
                if pointee.is_unsigned() {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
            } else {
                let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            }
            return;
        }
        // A stack-resident pointer (`-r-`, or one BCC couldn't promote) is loaded
        // into `bx` first, then indexed through `[bx±K*stride]`. A register-
        // resident pointer indexes directly. Fixture 4273 (`return p[2];`).
        let addr_reg = match self.locals.location_of(ptr_name) {
            LocalLocation::Reg(reg) => reg.name(),
            LocalLocation::Stack(off) => {
                let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                "bx"
            }
        };
        // The address operand: `[reg]` for k=0, else `[reg±K*stride]`.
        // Handle negative K (e.g. `p[-1]`) via signed wrapping
        // multiplication. Fixture 2377 (`p[-1]`).
        let stride = i32::from(pointee.size_bytes());
        let k_signed = k as i32;
        let byte_off = k_signed.wrapping_mul(stride);
        let addr = if byte_off == 0 {
            format!("[{addr_reg}]")
        } else if byte_off > 0 {
            format!("[{addr_reg}+{byte_off}]")
        } else {
            format!("[{addr_reg}{byte_off}]")
        };
        if pointee.is_char_like() {
            let _ = write!(self.out, "\tmov\tal,byte ptr {addr}\r\n");
            self.emit_widen_al(&pointee);
        } else {
            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
        }
    }
    /// `"<string>"[<index>]` — string literal indexed in place. For
    /// a constant index, BCC folds the access to a direct memory
    /// reference (fixture 089: `"hi"[0]` → `mov al, byte ptr DGROUP:s@`).
    /// Variable indexing of a string literal isn't observed yet.
    pub(crate) fn emit_string_lit_index_to_ax(&mut self, bytes: &[u8], index: &Expr) {
        let pool_offset = self.strings.intern(bytes);
        let Some(k) = try_const_eval(index) else {
            panic!("variable-indexed string literal not yet supported (no fixture)");
        };
        let total_offset = pool_offset + k;
        let label = if total_offset == 0 {
            "DGROUP:s@".to_owned()
        } else {
            format!("DGROUP:s@+{total_offset}")
        };
        // Strings are bytes; load AL then sign-extend, matching the
        // char-array constant-index path.
        let _ = write!(self.out, "\tmov\tal,byte ptr {label}\r\n");
        self.out.extend_from_slice(b"\tcbw\t\r\n");
    }
    /// Look up the register name for an index that's an Ident bound
    /// to a register-resident local. Used by the 2D address helper.
    pub(crate) fn idx_reg_name(&self, index: &Expr) -> &'static str {
        let ExprKind::Ident(name) = &index.kind else {
            panic!("non-ident multi-dim index not yet supported (no fixture)");
        };
        match self.locals.location_of(name) {
            LocalLocation::Reg(reg) => reg.name(),
            LocalLocation::Stack(_) => {
                panic!("stack-resident multi-dim index not yet supported (no fixture)");
            }
        }
    }
    /// `<base>.<field> <op>= <value>;` — compound assignment through a
    /// struct member. Computes the same `<dest>` operand as
    /// `emit_member_assign`, then emits the matching arithmetic
    /// instruction directly to memory (fixture 182's `p->x += 5`
    /// becomes `add word ptr [si], 5`). Only constant RHS values are
    /// fixture-supported today.
    /// Emit `<dest> op= <value>` where `<dest>` is a long memory
    /// location whose halves' assembly addresses are `lo_addr` and
    /// `hi_addr`. The skeleton matches the long-global compound path
    /// (fixtures 251/253/339) and is destination-storage-agnostic —
    /// works for globals, struct fields, and array elements once the
    /// caller has computed the right disp16 expressions. The
    /// `dest_unsigned` flag only matters for `>>=` (chooses `sar` vs
    /// `shr` for the high half / picks the signed-vs-unsigned shift
    /// helper for K>1).
    pub(crate) fn emit_long_compound_to_mem(
        &mut self,
        lo_addr: &str,
        hi_addr: &str,
        op: BinOp,
        value: &Expr,
        dest_unsigned: bool,
    ) {
        // Shift compound: two shapes by K. K=1 inline uses memory-
        // dest register convention (AX=high, DX=low) — the loaded
        // pair matches the trailing store. K>1 routes through the
        // helper and so loads with the helper ABI (DX=high, AX=low);
        // the trailing store adapts. `mov cl, K` lands FIRST in the
        // compound-form reorder. Mirrors the long-global compound-
        // shift path (fixtures 263–266) and the long-stack-local
        // compound-shift path (fixtures 383–385). Fixtures 395
        // (struct field, K=1 `<<=`), 396 (array elem, K=1), 397
        // (struct field, K=2 helper).
        if matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(k) = try_const_eval(value)
            && (1..=255).contains(&k)
        {
            if k == 1 {
                let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                if matches!(op, BinOp::Shl) {
                    self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                    self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                } else {
                    let hi_op = if dest_unsigned { "shr" } else { "sar" };
                    let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                    self.out.extend_from_slice(b"\trcr\tdx,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},dx\r\n");
            } else {
                let helper = match (op, dest_unsigned) {
                    (BinOp::Shl, _)     => "N_LXLSH@",
                    (BinOp::Shr, false) => "N_LXRSH@",
                    (BinOp::Shr, true)  => "N_LXURSH@",
                    _ => unreachable!(),
                };
                let k_u8 = (k & 0xFF) as u8;
                let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},ax\r\n");
            }
            return;
        }
        // Helper-call compound (`*=`, `/=`, `%=`) with a long-lvalue
        // RHS. Mul loads RHS → CX:BX, LHS → DX:AX (compound-form
        // operand-to-slot swap — see batch 23 fingerprint), calls
        // N_LXMUL@, stores DX:AX back. Div/mod push the four words
        // right-to-left in their standard helper order (divisor
        // first in time, dividend at lower addresses on the helper
        // stack), call the unsigned/signed helper, and store the
        // result. Fixtures 407 (struct mul), 408 (array mul), 409
        // (struct div).
        if matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let Some((y_hi, y_lo)) = self.long_lvalue_addr_pair(value)
        {
            match op {
                BinOp::Mul => {
                    let _ = write!(self.out, "\tmov\tcx,word ptr {y_hi}\r\n");
                    let _ = write!(self.out, "\tmov\tbx,word ptr {y_lo}\r\n");
                    let _ = write!(self.out, "\tmov\tdx,word ptr {hi_addr}\r\n");
                    let _ = write!(self.out, "\tmov\tax,word ptr {lo_addr}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
                    self.helpers.insert("N_LXMUL@".to_string());
                }
                BinOp::Div | BinOp::Mod => {
                    let helper = match (op, dest_unsigned) {
                        (BinOp::Div, false) => "N_LDIV@",
                        (BinOp::Mod, false) => "N_LMOD@",
                        (BinOp::Div, true)  => "N_LUDIV@",
                        (BinOp::Mod, true)  => "N_LUMOD@",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\tpush\tword ptr {y_hi}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {y_lo}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {hi_addr}\r\n");
                    let _ = write!(self.out, "\tpush\tword ptr {lo_addr}\r\n");
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                }
                _ => unreachable!(),
            }
            let _ = write!(self.out, "\tmov\tword ptr {hi_addr},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {lo_addr},ax\r\n");
            return;
        }
        // Const RHS: `op [lo], k_lo / op|carry [hi], k_hi_or_0`.
        // Arith uses `83 /n` imm8sx (low half must fit i8sx; high
        // is `adc/sbb 0`). Bitwise uses `81 /n` imm16 (op-family-
        // dependent encoding choice).
        if let Some(k) = try_const_eval(value) {
            let k_lo = (k & 0xFFFF) as u16;
            let k_hi = ((k >> 16) & 0xFFFF) as u16;
            match op {
                BinOp::Add | BinOp::Sub => {
                    let (lo_op, hi_op) = if matches!(op, BinOp::Add) {
                        ("add", "adc")
                    } else {
                        ("sub", "sbb")
                    };
                    let lo_signed = k_lo as i16;
                    if let Ok(lo_i8) = i8::try_from(lo_signed) {
                        let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},{lo_i8}\r\n");
                    } else {
                        let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},{k_lo}\r\n");
                    }
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {hi_addr},0\r\n");
                    return;
                }
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let mnem = match op {
                        BinOp::BitAnd => "and",
                        BinOp::BitOr  => "or",
                        BinOp::BitXor => "xor",
                        _ => unreachable!(),
                    };
                    let _ = write!(self.out, "\t{mnem}\tword ptr {lo_addr},{k_lo}\r\n");
                    let _ = write!(self.out, "\t{mnem}\tword ptr {hi_addr},{k_hi}\r\n");
                    return;
                }
                _ => {}
            }
        }
        // Variable RHS: load y into AX:DX (memory-dest conv), then
        // memory-direct `<op> [lo], dx / <op|carry> [hi], ax`. Mirror
        // of fixture 339 for any memory destination.
        if let Some((y_hi, y_lo)) = self.long_lvalue_addr_pair(value)
            && let Some((lo_op, hi_op)) = long_pair_op(op)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {y_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {y_lo}\r\n");
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},dx\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {hi_addr},ax\r\n");
            return;
        }
        // Int RHS widening into long memory destination. emit_expr_to_ax
        // loads y into AX (with cbw for char), then cwd extends to
        // DX:AX, then memory-direct `add/adc` (or sub/sbb, or paired
        // bitwise). Mirrors fixture 755 (`long_global += int x`) but
        // for an arbitrary memory destination (struct field, array
        // element). Fixture 845.
        if let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::Int | Type::Char)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
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
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},ax\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {hi_addr},dx\r\n");
            return;
        }
        // UInt/UChar RHS widening (zero-extend) into long memory dest.
        // Mirrors fixture 767 (`ulong_global += uint x`).
        if let Some(ty_rhs) = self.rhs_int_compound_type(&value.kind)
            && matches!(ty_rhs, Type::UInt | Type::UChar)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            self.emit_expr_to_ax(value);
            let (lo_op, hi_op) = match op {
                BinOp::Add => ("add", "adc"),
                BinOp::Sub => ("sub", "sbb"),
                BinOp::BitAnd => ("and", "and"),
                BinOp::BitOr => ("or", "or"),
                BinOp::BitXor => ("xor", "xor"),
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{lo_op}\tword ptr {lo_addr},ax\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr {hi_addr},0\r\n");
            return;
        }
        panic!("long compound `{op:?}=` to memory not yet supported for this RHS shape (no fixture)");
    }
    /// Inspect the chain root's type without emitting anything. Used
    /// to decide between constant-immediate vs AX-routed stores before
    /// committing to the chain-to-BX emit sequence.
    pub(crate) fn peek_chain_leaf_ty(&self, base_name: &str, depth: u32) -> Type {
        let mut ty = if self.globals.type_of(base_name).is_some() {
            self.globals.type_of(base_name).cloned().unwrap()
        } else {
            self.locals.type_of(base_name).clone()
        };
        for _ in 0..=depth {
            if let Some(p) = ty.pointee() {
                ty = p.clone();
            } else {
                break;
            }
        }
        ty
    }
    /// Emit the `mov ax, <a>; cwd; idiv <b>` prefix shared by `%`
    /// fast-paths that want the remainder left in DX (rather than
    /// rounding through AX as the generic `emit_arith_op_to_ax`
    /// does). Used by the int-stack `c = a % b;` peephole in
    /// `emit_assign_local`. Fixture 546.
    pub(crate) fn emit_arith_setup_for_mod(&mut self, left: &Expr, right: &Expr) {
        self.emit_expr_to_ax(left);
        let src = self.resolve_operand_source(right);
        self.out.extend_from_slice(b"\tcwd\t\r\n");
        let _ = write!(self.out, "\tidiv\t{}\r\n", src.word());
    }
    /// Post-pass peephole: if `self.out` ends with the 3-instruction
    /// LHS-clobbering binop tail
    /// `mov dx, ax; pop ax; <op> ax, dx` (emitted by the
    /// emit_expr_to_ax path when both operands clobber AX), rewrite
    /// it in place to the 2-instruction form
    /// `pop dx; <op> dx, ax` and return `true`. The result then lives
    /// in DX instead of AX — the caller is responsible for using DX
    /// in the immediately-following store.
    ///
    /// Safe for `add`, `sub`, `and`, `or`, `xor`: the swap of source
    /// and destination preserves the operation (commutative ops
    /// trivially; `sub dx, ax` still reads "dx - ax" which equals the
    /// original "ax - dx" with ax==LHS, dx==RHS after the rewrite).
    /// Saves one byte (`mov dx, ax` is 2 bytes, removed entirely; the
    /// store becomes `mov word ptr [X], dx` same size as `, ax`).
    /// Fixture 1989 (`r = (a==b) + (a==c)`).
    pub(crate) fn try_collapse_lhs_clobber_to_dx(&mut self) -> bool {
        const PATTERNS: &[(&[u8], &[u8])] = &[
            (
                b"\tmov\tdx,ax\r\n\tpop\tax\r\n\tadd\tax,dx\r\n",
                b"\tpop\tdx\r\n\tadd\tdx,ax\r\n",
            ),
            (
                b"\tmov\tdx,ax\r\n\tpop\tax\r\n\tsub\tax,dx\r\n",
                b"\tpop\tdx\r\n\tsub\tdx,ax\r\n",
            ),
            (
                b"\tmov\tdx,ax\r\n\tpop\tax\r\n\tand\tax,dx\r\n",
                b"\tpop\tdx\r\n\tand\tdx,ax\r\n",
            ),
            (
                b"\tmov\tdx,ax\r\n\tpop\tax\r\n\tor\tax,dx\r\n",
                b"\tpop\tdx\r\n\tor\tdx,ax\r\n",
            ),
            (
                b"\tmov\tdx,ax\r\n\tpop\tax\r\n\txor\tax,dx\r\n",
                b"\tpop\tdx\r\n\txor\tdx,ax\r\n",
            ),
        ];
        for (orig, new) in PATTERNS {
            if self.out.ends_with(orig) {
                let new_len = self.out.len() - orig.len();
                self.out.truncate(new_len);
                self.out.extend_from_slice(new);
                return true;
            }
        }
        // Reg-source LHS variant: when the LHS is a register
        // ident, the rhs_clobbers_ax tail looks like
        //   push ax
        //   mov ax, <reg>
        //   pop dx
        //   <op> ax, dx
        // Rewrite as the 3-instruction form
        //   mov dx, <reg>
        //   <op> dx, ax
        // matching BCC's pattern when both LHS and RHS are
        // present and RHS computed AX. Fixture 2397
        // (`sum = sum + words[i][0]` with sum in DI).
        if let Some((truncate_at, reg_start, reg_end, op_mnem)) =
            split_lhs_reg_clobber_tail(&self.out)
        {
            let reg_name: Vec<u8> = self.out[reg_start..reg_end].to_vec();
            self.out.truncate(truncate_at);
            self.out.extend_from_slice(b"\tmov\tdx,");
            self.out.extend_from_slice(&reg_name);
            self.out.extend_from_slice(b"\r\n\t");
            self.out.extend_from_slice(op_mnem);
            self.out.extend_from_slice(b"\tdx,ax\r\n");
            return true;
        }
        // Memory-lvalue LHS variant: when the LHS is a simple
        // memory lvalue (single `mov ax, word ptr <src>`), the
        // rhs_clobbers_ax path leaves us with the 4-instruction
        // tail
        //   push ax
        //   mov ax, word ptr <src>
        //   pop dx
        //   <op> ax, dx
        // BCC's shape skips the push/pop pair and loads the LHS
        // straight into DX:
        //   mov dx, word ptr <src>
        //   <op> dx, ax
        // Saves 2 bytes per occurrence; result lives in DX, so the
        // caller's store uses DX as the source. Fixture 2987 (`x =
        // a + b * c` for stack-int params).
        if let Some((truncate_at, src_start, src_end, op_mnem)) =
            split_lhs_mem_clobber_tail(&self.out)
        {
            let src: Vec<u8> = self.out[src_start..src_end].to_vec();
            self.out.truncate(truncate_at);
            self.out.extend_from_slice(b"\tmov\tdx,word ptr ");
            self.out.extend_from_slice(&src);
            self.out.extend_from_slice(b"\r\n\t");
            self.out.extend_from_slice(op_mnem);
            self.out.extend_from_slice(b"\tdx,ax\r\n");
            return true;
        }
        false
    }
    /// Resolve the right operand to a textual asm source operand. Today
    /// either an immediate (constant-foldable), a register-resident
    /// local, or a `word ptr [bp-N]` stack local.
    /// Best-effort type lookup for the RHS of a long-compound
    /// widening branch. Today only recognizes bare-ident sources
    /// (`g += x`). Returns `None` for compound RHS expressions —
    /// those would need a more general typing pass before they
    /// can pick the right widening shape.
    pub(crate) fn rhs_type_for_long_widening(&self, e: &ExprKind) -> Option<Type> {
        let name = match e {
            ExprKind::Ident(n) => n,
            _ => return None,
        };
        if let Some(t) = self.globals.type_of(name) {
            Some(t.clone())
        } else if self.locals.has(name) {
            Some(self.locals.type_of(name).clone())
        } else {
            None
        }
    }
    /// Resolve a long-typed RHS expression to its (low, high)
    /// address-string halves plus the result type. Supports
    /// `Ident` (any long global or stack local), `ArrayIndex`
    /// with constant index over a long array, and `Member` of
    /// a struct whose field is long. Used by the long+long arm
    /// to accept array elements and members. Fixture 829.
    pub(crate) fn long_rhs_halves(&self, e: &ExprKind) -> Option<(String, String, Type)> {
        match e {
            ExprKind::Ident(n) => {
                let ty = if let Some(t) = self.globals.type_of(n) {
                    t.clone()
                } else if self.locals.has(n) {
                    self.locals.type_of(n).clone()
                } else {
                    return None;
                };
                if !ty.is_long_like() { return None; }
                let (lo, hi) = self.long_halves_of(n);
                Some((lo, hi, ty))
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
                if !elem.is_long_like() { return None; }
                let stride = u32::from(elem.size_bytes());
                let off = (k as u32).wrapping_mul(stride);
                let (lo, hi) = if self.globals.contains(arr_name) {
                    let lo = if off == 0 {
                        format!("DGROUP:_{arr_name}")
                    } else {
                        format!("DGROUP:_{arr_name}+{off}")
                    };
                    let hi = format!("DGROUP:_{arr_name}+{}", off + 2);
                    (lo, hi)
                } else {
                    let LocalLocation::Stack(base) = self.locals.location_of(arr_name) else {
                        return None;
                    };
                    let lo_off = base + i16::try_from(off).ok()?;
                    let hi_off = lo_off + 2;
                    (bp_addr(lo_off), bp_addr(hi_off))
                };
                Some((lo, hi, (*elem).clone()))
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
                let (field_off, field_ty) = base_ty.field(field)?;
                if !field_ty.is_long_like() { return None; }
                let off = u32::from(field_off);
                let (lo, hi) = if self.globals.contains(base_name) {
                    let lo = if off == 0 {
                        format!("DGROUP:_{base_name}")
                    } else {
                        format!("DGROUP:_{base_name}+{off}")
                    };
                    let hi = format!("DGROUP:_{base_name}+{}", off + 2);
                    (lo, hi)
                } else {
                    let LocalLocation::Stack(base_off) = self.locals.location_of(base_name) else {
                        return None;
                    };
                    let lo_off = base_off + i16::try_from(off).ok()?;
                    let hi_off = lo_off + 2;
                    (bp_addr(lo_off), bp_addr(hi_off))
                };
                Some((lo, hi, field_ty))
            }
            _ => None,
        }
    }
    /// Like `rhs_type_for_long_widening` but also resolves
    /// `ArrayIndex` (returning element type), `Deref` (returning
    /// pointee type), `Member` (returning field type), and
    /// `Unary` (returning operand type, since neg/bitnot don't
    /// widen). Used by the int-global compound arm to accept
    /// `g += a[K]`, `g += *p`, `g += s.x`, `g += -y`. Fixtures
    /// 821, 822, 823, 851.
    pub(crate) fn rhs_int_compound_type(&self, e: &ExprKind) -> Option<Type> {
        if let Some(t) = self.rhs_type_for_long_widening(e) {
            return Some(t);
        }
        match e {
            ExprKind::Unary { operand, .. } => self.rhs_int_compound_type(&operand.kind),
            ExprKind::IntLit(_) => Some(Type::Int),
            // Function calls return into AX under BCC's small-
            // model convention. Assume int return; long-returning
            // calls would route through a separate path. Fixture 854.
            ExprKind::Call { .. } => Some(Type::Int),
            // `!y` and `a && b` / `a || b` yield 0/1 in AX, int-
            // typed. Fixture 856 (`g += !y`).
            ExprKind::Logical { .. } => Some(Type::Int),
            // Cast to an int-family type. The cast's target type
            // determines the result. Fixture 857 (`g += (int)c`).
            ExprKind::Cast { ty, .. } => {
                if matches!(ty, Type::Int | Type::UInt | Type::Char | Type::UChar) {
                    Some(ty.clone())
                } else {
                    None
                }
            }
            // Comma operator: type is the last subexpression's
            // type. Fixture 858.
            ExprKind::Comma { right, .. } => self.rhs_int_compound_type(&right.kind),
            // Assignment expression: yields the assigned value
            // in AX. Type comes from the target ident. Fixture 859.
            ExprKind::AssignExpr { target, .. } => {
                if let Some(t) = self.globals.type_of(target) {
                    Some(t.clone())
                } else if self.locals.has(target) {
                    Some(self.locals.type_of(target).clone())
                } else {
                    None
                }
            }
            // Ternary in int-typed branches resolves to int.
            // Fixture 855.
            ExprKind::Ternary { then_value, else_value, .. } => {
                let lt = self.rhs_int_compound_type(&then_value.kind)?;
                let rt = self.rhs_int_compound_type(&else_value.kind)?;
                if lt.is_long_like() || rt.is_long_like() {
                    return None;
                }
                Some(Type::Int)
            }
            ExprKind::BinOp { left, right, .. } => {
                // If both operands resolve to non-long int-family
                // types, the BinOp result is int-typed. Used for
                // sub-expression RHS in int compound (fixture 852).
                let lt = self.rhs_int_compound_type(&left.kind)?;
                let rt = self.rhs_int_compound_type(&right.kind)?;
                if lt.is_long_like() || rt.is_long_like() {
                    return None;
                }
                Some(Type::Int)
            }
            ExprKind::ArrayIndex { array, .. } => {
                let ExprKind::Ident(arr_name) = &array.kind else { return None };
                let ty = if let Some(t) = self.globals.type_of(arr_name) {
                    t.clone()
                } else if self.locals.has(arr_name) {
                    self.locals.type_of(arr_name).clone()
                } else {
                    return None;
                };
                match ty {
                    Type::Array { ref elem, .. } => Some((**elem).clone()),
                    _ => None,
                }
            }
            ExprKind::Deref(inner) => {
                let ExprKind::Ident(p_name) = &inner.kind else { return None };
                let ty = if let Some(t) = self.globals.type_of(p_name) {
                    t.clone()
                } else if self.locals.has(p_name) {
                    self.locals.type_of(p_name).clone()
                } else {
                    return None;
                };
                ty.pointee().cloned()
            }
            ExprKind::Member { base, field, .. } => {
                let ExprKind::Ident(base_name) = &base.kind else { return None };
                let base_ty = if let Some(t) = self.globals.type_of(base_name) {
                    t.clone()
                } else if self.locals.has(base_name) {
                    self.locals.type_of(base_name).clone()
                } else {
                    return None;
                };
                base_ty.field(field).map(|(_, ty)| ty)
            }
            _ => None,
        }
    }
    /// Resolve a long-type lookup by name across globals and
    /// locals. Used by the long-compound-with-long-RHS path
    /// (fixtures 744 / 745) to accept either source kind.
    pub(crate) fn lhs_long_type(&self, name: &str) -> Option<Type> {
        if let Some(t) = self.globals.type_of(name) {
            Some(t.clone())
        } else if self.locals.has(name) {
            Some(self.locals.type_of(name).clone())
        } else {
            None
        }
    }
    pub(crate) fn rhs_long_type_of_ident(&self, name: &str) -> Option<Type> {
        self.lhs_long_type(name)
    }
    /// Format the (low, high) word-pointer address strings for a
    /// long-type identifier (without the `word ptr` prefix —
    /// callers add that themselves so the same helper covers both
    /// load and store).
    pub(crate) fn long_halves_of(&self, name: &str) -> (String, String) {
        if self.globals.contains(name) {
            (
                format!("DGROUP:_{name}"),
                format!("DGROUP:_{name}+2"),
            )
        } else {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long never sits in a register");
            };
            (bp_addr(off), bp_addr(off + 2))
        }
    }
}
