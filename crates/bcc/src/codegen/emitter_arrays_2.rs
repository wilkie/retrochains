use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// `a[<i1>][<i2>]... <op>= <value>;` — read-modify-write on an
    /// array element. Mirrors `emit_array_assign` for the all-const
    /// index path; emits `<op> <width> ptr [bp-N],<imm>` instead of
    /// `mov` (fixture 184).
    pub(crate) fn emit_array_compound_assign(
        &mut self,
        array: &str,
        indices: &[Expr],
        op: BinOp,
        value: &Expr,
        from_postfix: bool,
    ) {
        // Long-element path. For both global (`long a[];`) and stack
        // (`long a[N];` as a local) array bases with a constant index,
        // a long array element behaves byte-identically to a long
        // struct field at the same effective address — same compound
        // skeletons, just a different disp16. Fixtures 392
        // (`a[1] += K`), 393 (`a[1] &= K`), 394 (`a[1] += y`).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some((const_off, leaf_ty)) =
                try_const_array_offset(g_ty, indices.iter())
            && leaf_ty.is_long_like()
        {
            let lo_addr = global_offset_addr(array, const_off as i32);
            let hi_addr = global_offset_addr(array, const_off as i32 + 2);
            self.emit_long_compound_to_mem(
                &lo_addr,
                &hi_addr,
                op,
                value,
                leaf_ty.is_unsigned(),
            );
            return;
        }
        // Long-pointer subscript compound: `long *p; p[K] += v`.
        // Load the pointer into BX once, then route through the
        // long-compound-to-mem helper with `[bx+off]` / `[bx+off+2]`
        // addresses. Same skeleton as the long-array path, just
        // BX-based instead of DGROUP-direct. Fixture 901.
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && pointee.is_long_like()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
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
            // Shift K=1 special-case: BCC reloads BX between the
            // register-arith and the store-back (BCC doesn't keep
            // BX alive across `shl/rcl`). The shared helper doesn't
            // know about the BX reload, so we inline this shape
            // here rather than routing through it. Fixture 904
            // (`p[1] <<= 1`).
            if matches!(op, BinOp::Shl | BinOp::Shr)
                && let Some(n) = try_const_eval(value)
                && n == 1
            {
                let unsigned = pointee.is_unsigned();
                let _ = write!(self.out, "\tmov\tax,word ptr {hi_addr}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {lo_addr}\r\n");
                if matches!(op, BinOp::Shl) {
                    self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                    self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                } else {
                    let hi_op = if unsigned { "shr" } else { "sar" };
                    let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                    self.out.extend_from_slice(b"\trcr\tdx,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {hi_addr},ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {lo_addr},dx\r\n");
                return;
            }
            self.emit_long_compound_to_mem(&lo_addr, &hi_addr, op, value, pointee.is_unsigned());
            return;
        }
        // Global int/char array with VARIABLE index, postfix ±1 form
        // (\`arr[i]++\` rewritten to compound \`+= 1\`): emit a single
        // \`inc/dec <width> ptr DGROUP:_<arr>[bx]\` — saves the
        // load+store+restore pair the const path uses. Fixture 3516.
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(elem_ty) = g_ty.array_elem()
            && (elem_ty.is_int_like() || elem_ty.is_char_like())
            && indices.len() == 1
            && try_const_eval(&indices[0]).is_none()
            && from_postfix
            && matches!(op, BinOp::Add | BinOp::Sub)
            && try_const_eval(value) == Some(1)
        {
            let elem_ty_clone = elem_ty.clone();
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let width = if elem_ty_clone.is_char_like() { "byte" } else { "word" };
            // Index is a register-resident int local AND the stride
            // is 1 (char element): use the direct `[<reg>+<sym>]`
            // addressing mode — no BX bounce needed. Fixture 3516
            // (`char arr[5]; arr[i]++` with i in SI).
            if elem_ty_clone.is_char_like()
                && let ExprKind::Ident(idx_name) = &indices[0].kind
                && self.locals.has(idx_name)
                && self.locals.type_of(idx_name).is_int_like()
                && let LocalLocation::Reg(idx_reg) = self.locals.location_of(idx_name)
                && !idx_reg.is_byte()
                && matches!(idx_reg, crate::codegen::locals::Reg::Si | crate::codegen::locals::Reg::Di)
            {
                let _ = write!(
                    self.out,
                    "\t{mnem}\t{width} ptr DGROUP:_{array}[{}]\r\n",
                    idx_reg.name(),
                );
                return;
            }
            self.emit_index_into_bx(&indices[0], &elem_ty_clone);
            let _ = write!(
                self.out,
                "\t{mnem}\t{width} ptr DGROUP:_{array}[bx]\r\n",
            );
            return;
        }
        // Global char array with VARIABLE index + constant value
        // (non-postfix): char compound canonicalizes through AL.
        // \`mov al, byte ptr DGROUP:_<arr>[bx]; <mnem> al, <k>;
        // mov byte ptr DGROUP:_<arr>[bx], al\`. Fixture 3515.
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(elem_ty) = g_ty.array_elem()
            && elem_ty.is_char_like()
            && indices.len() == 1
            && try_const_eval(&indices[0]).is_none()
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && let Some(v) = try_const_eval(value)
        {
            let elem_ty_clone = elem_ty.clone();
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let v_masked = v & 0xFF;
            // Index lives in SI/DI: addr through `[<reg>+sym]` and
            // skip the `mov bx, <reg>` bounce. Fixture 3515.
            if let ExprKind::Ident(idx_name) = &indices[0].kind
                && self.locals.has(idx_name)
                && self.locals.type_of(idx_name).is_int_like()
                && let LocalLocation::Reg(idx_reg) = self.locals.location_of(idx_name)
                && matches!(
                    idx_reg,
                    crate::codegen::locals::Reg::Si | crate::codegen::locals::Reg::Di
                )
            {
                let idx_name_str = idx_reg.name();
                let _ = write!(
                    self.out,
                    "\tmov\tal,byte ptr DGROUP:_{array}[{idx_name_str}]\r\n",
                );
                // `±1` peephole: BCC uses `inc al` / `dec al`
                // (FE C0 / FE C8) instead of `add al, 1` / `sub al,
                // 1` (04 01 / 2C 01) — same byte length but a
                // different opcode. Fixture 3515.
                if v_masked == 1 && matches!(op, BinOp::Add) {
                    self.out.extend_from_slice(b"\tinc\tal\r\n");
                } else if v_masked == 1 && matches!(op, BinOp::Sub) {
                    self.out.extend_from_slice(b"\tdec\tal\r\n");
                } else {
                    let _ = write!(self.out, "\t{mnem}\tal,{v_masked}\r\n");
                }
                let _ = write!(
                    self.out,
                    "\tmov\tbyte ptr DGROUP:_{array}[{idx_name_str}],al\r\n",
                );
                return;
            }
            self.emit_index_into_bx(&indices[0], &elem_ty_clone);
            let _ = write!(
                self.out,
                "\tmov\tal,byte ptr DGROUP:_{array}[bx]\r\n",
            );
            let _ = write!(self.out, "\t{mnem}\tal,{v_masked}\r\n");
            let _ = write!(
                self.out,
                "\tmov\tbyte ptr DGROUP:_{array}[bx],al\r\n",
            );
            return;
        }
        // Char/int global-array element with a constant index — same
        // shapes as the corresponding char-global / int-global compound
        // patterns, just with a `DGROUP:_<a>+<K>` address. Fixture 706
        // (`a[2] += 5` for `char a[4]` global → `mov al, _a+2; add al,
        // 5; mov _a+2, al`).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some((const_off, leaf_ty)) =
                try_const_array_offset(g_ty, indices.iter())
        {
            let dest = global_offset_addr(array, const_off as i32);
            let store_byte = leaf_ty.is_char_like();
            // Int-element compound with non-constant RHS — mirrors
            // the int-global compound add path (fixture 794): load
            // RHS into AX via emit_expr_to_ax (handles char/uchar
            // widening too), then memory-direct `<op> word ptr
            // <dest>, ax`. Fixture 833 (`a[1] += y`).
            if !store_byte
                && matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                )
                && try_const_eval(value).is_none()
                && matches!(leaf_ty, Type::Int | Type::UInt)
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
            // Int-element compound `*=` / `/=` / `%=` with
            // non-constant local int RHS — `mov ax, <dest>;
            // imul/idiv word ptr [bp+N]; mov <dest>, ax|dx`.
            // Mirrors fixture 802. Fixture 836 (`a[1] *= y`).
            if !store_byte
                && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
                && try_const_eval(value).is_none()
                && matches!(leaf_ty, Type::Int | Type::UInt)
                && let ExprKind::Ident(b) = &value.kind
                && !self.globals.contains(b)
            {
                let LocalLocation::Stack(off) = self.locals.location_of(b) else {
                    panic!("non-stack RHS in array compound Mul/Div not yet supported (no fixture)");
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
            // Int-element compound `<<=` / `>>=` with non-constant
            // RHS — `mov cl, byte ptr <rhs>; shl/sar/shr word ptr
            // <dest>, cl`. Fixture 837 (`a[1] <<= y`).
            if !store_byte
                && matches!(op, BinOp::Shl | BinOp::Shr)
                && try_const_eval(value).is_none()
                && matches!(leaf_ty, Type::Int | Type::UInt)
                && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
            {
                let unsigned = leaf_ty.is_unsigned();
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
            // Char-element compound with non-constant RHS. BCC
            // splits two ways by op family (same asymmetry as
            // char-global compound, batch 121/122):
            //  - `+=`/`-=`: AL-through (`mov al, <dest>; add al,
            //    <rhs>; mov <dest>, al`) — arith canonicalizes
            //    through the accumulator.
            //  - `&=`/`|=`/`^=`: memory-direct (`mov al, <rhs>;
            //    and byte ptr <dest>, al`).
            // Fixture 847 (arith), 850 (bitwise).
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
            let Some(v) = try_const_eval(value) else {
                panic!("non-constant rhs in global-array compound assign not yet supported (no fixture)");
            };
            let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
            // Postfix `a[K]++` / `a[K]--` (discarded): memory-direct
            // `inc|dec byte ptr <dest>` (fixture 717).
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
            let mnemonic = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => panic!("compound op `{op:?}` on global-array element not yet supported (no fixture)"),
            };
            let width = if store_byte { "byte" } else { "word" };
            let _ = write!(self.out, "\t{mnemonic}\t{width} ptr {dest},{v_masked}\r\n");
            return;
        }
        // Global-pointer subscript compound: `p[K] op= …` for `int *p`
        // at file scope. BCC's shape: load the pointer into BX
        // (`mov bx, word ptr DGROUP:_<p>`), then memory-direct
        // `<op> word ptr [bx+offset], <rhs>`. Offset = K *
        // pointee_stride. Fixture 862 (`p[1] += y` — non-const RHS),
        // 864 (`p[1] += K` — const RHS, imm8sx form).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
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
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            if let Some(v) = try_const_eval(value) {
                let v_masked = v & 0xFFFF;
                // Same `++a[K]` / `--a[K]` memory-direct peephole
                // the array path uses (fixture 547): const RHS of
                // 1 for Add/Sub becomes `inc|dec word ptr [bx+K]`
                // (2-3 bytes vs. 4 bytes for the imm8sx form).
                // Fixture 880 (`p[1]++` discarded).
                if v_masked == 1 && matches!(op, BinOp::Add | BinOp::Sub) {
                    let m = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                    let _ = write!(self.out, "\t{m}\tword ptr {bx_disp}\r\n");
                } else {
                    let _ = write!(
                        self.out,
                        "\t{mnem}\tword ptr {bx_disp},{v_masked}\r\n",
                    );
                }
            } else {
                self.emit_expr_to_ax(value);
                let _ = write!(self.out, "\t{mnem}\tword ptr {bx_disp},ax\r\n");
            }
            return;
        }
        // Int-pointer subscript shift compound with const RHS:
        // `int *p; p[K] <<= N`. BCC unrolls into N repetitions of
        // `<shift> word ptr [bx+K*2], 1` — same shape as the flat
        // int-global shift path (fixture 539), just with BX-based
        // addressing. Fixture 878 (`p[1] <<= 3`).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some(n) = try_const_eval(value)
            && n >= 1
            && n <= 8
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
            let signed = !pointee.is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            for _ in 0..n {
                let _ = write!(self.out, "\t{mnem}\tword ptr {bx_disp},1\r\n");
            }
            return;
        }
        // Int-pointer subscript shift compound with variable RHS:
        // `int *p; p[K] <<= y`. BCC loads the shift count into CL
        // via the byte-RHS path, then `<shift> word ptr [bx+K*2],
        // cl`. Mirrors the int-global variable shift path (batch
        // 175 / fixture 802 family). Fixture 882.
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && try_const_eval(value).is_none()
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
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
            let signed = !pointee.is_unsigned();
            let mnem = match op {
                BinOp::Shl => "shl",
                BinOp::Shr if signed => "sar",
                BinOp::Shr => "shr",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tcl,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tword ptr {bx_disp},cl\r\n");
            return;
        }
        // Int-pointer subscript Mul/Div/Mod compound:
        // `int *p; p[K] *= y` (or `/=`, `%=`). BCC loads the LHS
        // through BX into AX, then `imul`/`idiv` against the
        // variable RHS, then stores back. Fixture 883
        // (`p[1] *= y`).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(op, BinOp::Mul | BinOp::Div | BinOp::Mod)
            && let ExprKind::Ident(b) = &value.kind
            && !self.globals.contains(b)
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
            let LocalLocation::Stack(boff) = self.locals.location_of(b) else {
                panic!("non-stack RHS in pointer-subscript Mul/Div not yet supported (no fixture)");
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {bx_disp}\r\n");
            if matches!(op, BinOp::Mul) {
                let _ = write!(self.out, "\timul\tword ptr {}\r\n", bp_addr(boff));
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tidiv\tword ptr {}\r\n", bp_addr(boff));
                // BCC reloads BX after `idiv` (Div and Mod) before
                // the store-back — `idiv` clobbers more state than
                // `imul`, so BCC doesn't keep BX alive across it.
                // Fixture 885 (div), 884 (mod).
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            }
            let result_reg = if matches!(op, BinOp::Mul | BinOp::Div) { "ax" } else { "dx" };
            let _ = write!(self.out, "\tmov\tword ptr {bx_disp},{result_reg}\r\n");
            return;
        }
        // Char-pointee global-pointer subscript compound: `char *p;
        // p[K] += y`. BCC uses the AL-arith-through pattern plus a
        // second `mov bx, _p` reload before the store (BCC doesn't
        // keep BX alive across the byte arith). Fixtures 865, 869
        // (var RHS), 877 (const RHS via imm8), 886 (K=1 peephole).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && pointee.is_char_like()
            && matches!(op, BinOp::Add | BinOp::Sub)
            && let Some(rhs_text) = try_const_eval(value)
                .map(|v| (v & 0xFF).to_string())
                .or_else(|| self.rhs_byte_addr(&value.kind))
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
            // K=1 memory-direct peephole: `inc|dec byte ptr [bx+
            // K]` (3 bytes vs. 11 for the AL-through pattern).
            // BCC applies the same shape as char-global / char-
            // array postinc (fixtures 717, 721). Fixture 886.
            if let Some(v) = try_const_eval(value)
                && (v & 0xFF) == 1
            {
                let m = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
                let _ = write!(self.out, "\t{m}\tbyte ptr {bx_disp}\r\n");
                return;
            }
            let mnem = if matches!(op, BinOp::Add) { "add" } else { "sub" };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tal,byte ptr {bx_disp}\r\n");
            let _ = write!(self.out, "\t{mnem}\tal,{rhs_text}\r\n");
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tbyte ptr {bx_disp},al\r\n");
            return;
        }
        // Char-pointee global-pointer subscript bitwise compound:
        // `char *p; p[K] &= y` (and `|=`/`^=`). BCC uses the same
        // mem-direct shape as char-global / char-array bitwise:
        // `mov al, <rhs>; <op> byte ptr [bx+K], al` — no BX reload,
        // no AL pre-load. Fixtures 870, 871 (and pending XOR).
        if let Some(g_ty) = self.globals.type_of(array)
            && let Some(pointee) = g_ty.pointee()
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && pointee.is_char_like()
            && matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            && try_const_eval(value).is_none()
            && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
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
            let mnem = match op {
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{array}\r\n");
            let _ = write!(self.out, "\tmov\tal,{rhs_byte}\r\n");
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {bx_disp},al\r\n");
            return;
        }
        // Register-resident local-pointer subscript compound:
        // `int *p; p[K] op= …` for a stack-local pointer held in a
        // register (BCC's typical SI/DI placement). BCC's shape:
        // `<op> word ptr [<reg>+K*stride], ax` after the RHS lands
        // in AX. Same offset computation as the global-pointer path,
        // just with register addressing. Fixture 863.
        if self.locals.has(array)
            && let Some(pointee) = self.locals.type_of(array).pointee()
            && let LocalLocation::Reg(reg) = self.locals.location_of(array)
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && try_const_eval(value).is_none()
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let reg_name = reg.name();
            let disp = if off == 0 {
                format!("[{reg_name}]")
            } else if off > 0 {
                format!("[{reg_name}+{off}]")
            } else {
                format!("[{reg_name}-{}]", -off)
            };
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tword ptr {disp},ax\r\n");
            return;
        }
        // Stack-resident local/param pointer subscript compound:
        // `int *p` held on the stack (a parameter, or a spilled local),
        // `p[K] op= <var>`. BCC loads the pointer into BX *first*, then
        // evaluates the RHS into AX, then `<op> word ptr [bx+K*stride],
        // ax`. Distinct from the register-resident sibling above (which
        // skips the BX load). Fixture 4281 (`p->b += v` through a
        // struct-pointer parameter; the field access decompiles to this
        // `int *` subscript form).
        if self.locals.has(array)
            && let Some(pointee) = self.locals.type_of(array).pointee()
            && let LocalLocation::Stack(ptr_off) = self.locals.location_of(array)
            && indices.len() == 1
            && let Some(k) = try_const_eval(&indices[0])
            && matches!(pointee, Type::Int | Type::UInt)
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
            && try_const_eval(value).is_none()
        {
            let stride = i32::from(pointee.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let disp = if off == 0 {
                "[bx]".to_owned()
            } else if off > 0 {
                format!("[bx+{off}]")
            } else {
                format!("[bx-{}]", -off)
            };
            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(ptr_off));
            self.emit_expr_to_ax(value);
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tword ptr {disp},ax\r\n");
            return;
        }
        // `<global-int-arr>[<var-idx>] <op>= <rhs>` — compute index
        // into BX (with scale), then memory-direct compound op on
        // `<sym>[bx]`. Fixture 2949 (`arr[i] += 1` → `inc word ptr
        // _arr[bx]`), 3593 (`arr[i] += arr[j]`).
        if let Some(gty) = self.globals.type_of(array)
            && let Some(elem_ty) = gty.array_elem()
            && elem_ty.is_int_like()
            && indices.len() == 1
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let elem_ty = elem_ty.clone();
            // ±1 peephole: emit `inc|dec word ptr _arr[bx]` directly.
            if let Some(v) = try_const_eval(value)
                && (v & 0xFFFF) == 1
                && matches!(op, BinOp::Add | BinOp::Sub)
            {
                let m = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
                self.emit_index_into_bx(&indices[0], &elem_ty);
                let _ = write!(
                    self.out,
                    "\t{m}\tword ptr DGROUP:_{array}[bx]\r\n",
                );
                return;
            }
            // Const RHS (non-±1): `<op> word ptr _arr[bx], K`.
            if let Some(v) = try_const_eval(value) {
                let mnem = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                self.emit_index_into_bx(&indices[0], &elem_ty);
                let v16 = (v & 0xFFFF) as u16;
                let _ = write!(
                    self.out,
                    "\t{mnem}\tword ptr DGROUP:_{array}[bx],{v16}\r\n",
                );
                return;
            }
            // Non-const RHS: evaluate to AX, then `<op> word ptr
            // _arr[bx], ax`. Fixture 3593 (`arr[i] += arr[j]`).
            self.emit_expr_to_ax(value);
            self.emit_index_into_bx(&indices[0], &elem_ty);
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
                "\t{mnem}\tword ptr DGROUP:_{array}[bx],ax\r\n",
            );
            return;
        }
        // Global char array variable-index compound add/sub/bitwise:
        // route through AL then `<mnem> byte ptr [arr+bx], al`.
        // Fixture 3522 (`arr[i] += v` for char arr, char v).
        if let Some(gty) = self.globals.type_of(array)
            && let Some(elem_ty) = gty.array_elem()
            && elem_ty.is_char_like()
            && indices.len() == 1
            && matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
            )
        {
            let elem_ty = elem_ty.clone();
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            if try_const_eval(value).is_none()
                && let Some(rhs_byte) = self.rhs_byte_addr(&value.kind)
            {
                let _ = write!(self.out, "\tmov\tal,{rhs_byte}\r\n");
                // Index in SI/DI → `<op> byte ptr [<reg>+sym], al`
                // directly. Fixture 3522 (`arr[i] += v`).
                if let ExprKind::Ident(idx_name) = &indices[0].kind
                    && self.locals.has(idx_name)
                    && self.locals.type_of(idx_name).is_int_like()
                    && let LocalLocation::Reg(idx_reg) = self.locals.location_of(idx_name)
                    && matches!(
                        idx_reg,
                        crate::codegen::locals::Reg::Si | crate::codegen::locals::Reg::Di
                    )
                {
                    let _ = write!(
                        self.out,
                        "\t{mnem}\tbyte ptr DGROUP:_{array}[{}],al\r\n",
                        idx_reg.name(),
                    );
                    return;
                }
                self.emit_index_into_bx(&indices[0], &elem_ty);
                let _ = write!(
                    self.out,
                    "\t{mnem}\tbyte ptr DGROUP:_{array}[bx],al\r\n",
                );
                return;
            }
        }
        let array_ty = self.locals.type_of(array).clone();
        let LocalLocation::Stack(base_off) = self.locals.location_of(array) else {
            panic!("array `{array}` should be stack-resident");
        };
        let Some((const_off, leaf_ty)) =
            try_const_array_offset(&array_ty, indices.iter())
        else {
            panic!("variable-indexed array compound assign not yet supported (no fixture)");
        };
        let off = base_off + i16::try_from(const_off).unwrap_or(i16::MAX);
        let store_byte = leaf_ty.is_char_like();
        let width = if store_byte { "byte" } else { "word" };
        let Some(v) = try_const_eval(value) else {
            // Non-constant RHS for an int element compound. Load RHS
            // into AX, then `<op> word ptr [bp+elem_off], ax`. Same
            // shape as the global-pointer-subscript compound (sibling
            // path above). Fixture 988 (`a[1] -= x`).
            if !store_byte
                && matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                )
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
                    "\t{mnem}\tword ptr {},ax\r\n",
                    bp_addr(off),
                );
                return;
            }
            // Char-typed dest + char-typed lvalue RHS: load RHS to
            // AL then `<op> byte ptr [bp+dst], al`. Mirrors the int
            // path above. Fixture 1447 (`a[0] ^= a[1];`).
            if store_byte
                && matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                )
                && let Some((r_name, r_off, r_ty)) = self.try_lvalue_chain_addr(value)
                && r_ty.is_char_like()
                && let Some(r_addr) = self.resolve_chain_addr(&r_name, r_off)
            {
                let _ = write!(self.out, "\tmov\tal,byte ptr {r_addr}\r\n");
                let mnem = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\t{mnem}\tbyte ptr {},al\r\n", bp_addr(off));
                return;
            }
            panic!("non-constant rhs in array compound assign not yet supported (no fixture)");
        };
        let v_masked = if store_byte { v & 0xFF } else { v & 0xFFFF };
        let dest = bp_addr(off);
        // Postfix `a[K]++` / `a[K]--` (discarded) on char-array: BCC
        // uses memory-direct `inc|dec byte ptr [bp-N]`. Sibling of
        // the global-array case (fixture 717). Int arrays use the
        // same shape (fixture 547) since `inc word ptr` already
        // matches BCC's prefix behavior.
        if store_byte
            && from_postfix
            && v_masked == 1
            && matches!(op, BinOp::Add | BinOp::Sub)
        {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\tbyte ptr {dest}\r\n");
            return;
        }
        // Int-element `++a[K]` / prefix-K=1 reuse the memory-direct
        // `inc word ptr` shape (fixture 547: `++a[1]` on int array).
        if !store_byte && v_masked == 1 && matches!(op, BinOp::Add | BinOp::Sub) {
            let mnem = if matches!(op, BinOp::Add) { "inc" } else { "dec" };
            let _ = write!(self.out, "\t{mnem}\t{width} ptr {dest}\r\n");
            return;
        }
        // Char-element arith (`a[K] += C`) — AL load-modify-store
        // through `bp_addr(off)`, same shape as char-global compound
        // (fixture 719). The K=1 peephole picks `inc al` over
        // `add al, 1`.
        if store_byte && matches!(op, BinOp::Add | BinOp::Sub) {
            let imm8 = if matches!(op, BinOp::Add) {
                (v_masked & 0xFF) as u8
            } else {
                ((v_masked & 0xFF) as u8).wrapping_neg()
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
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
        // Int-element `<word-arr>[K] *= C` — `imul` route through AX
        // with the constant materialized in DX. Mirrors the global
        // compound `*=` shape (line 6491). Fixture 1210
        // (`a[0] *= 5`).
        if !store_byte && matches!(op, BinOp::Mul) {
            let _ = write!(self.out, "\tmov\tdx,{v_masked}\r\n");
            let _ = write!(self.out, "\tmov\tax,{width} ptr {dest}\r\n");
            self.out.extend_from_slice(b"\timul\tdx\r\n");
            let _ = write!(self.out, "\tmov\t{width} ptr {dest},ax\r\n");
            return;
        }
        // Char-element `<byte-arr>[K] *= C` — widen byte to int,
        // multiply, narrow back. Same pattern as int-element Mul but
        // with the AL load + cbw widening up front and a byte store
        // at the end. Fixture 1211 (`a[0] *= 5` for char a[3]).
        if store_byte && matches!(op, BinOp::Mul) {
            let _ = write!(self.out, "\tmov\tal,byte ptr {dest}\r\n");
            self.out.extend_from_slice(b"\tcbw\t\r\n");
            let _ = write!(self.out, "\tmov\tdx,{v_masked}\r\n");
            self.out.extend_from_slice(b"\timul\tdx\r\n");
            let _ = write!(self.out, "\tmov\tbyte ptr {dest},al\r\n");
            return;
        }
        let mnemonic = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::BitAnd => "and",
            BinOp::BitOr => "or",
            BinOp::BitXor => "xor",
            _ => panic!("compound op `{op:?}` on array element not yet supported (no fixture)"),
        };
        let _ = write!(
            self.out,
            "\t{mnemonic}\t{width} ptr {dest},{v_masked}\r\n",
        );
    }
}
