use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// Emit a conditional branch: control flows to `true_slot` when
    /// `cond` is true, to `false_slot` when false. Exactly one of the
    /// two should be `None` — that direction falls through to the
    /// next instruction emitted.
    ///
    /// `Logical` operators (`&&`, `||`) recurse into this function on
    /// both operands, short-circuiting via fall-through:
    /// - `a && b`: a's false → false_slot; a's true → fall through to
    ///   b's test (a's true target becomes `None`). Then b carries
    ///   the original true/false targets.
    /// - `a || b`: a's true → true_slot; a's false → fall through to
    ///   b's test (a's false target becomes `None`). Then b same.
    /// Whether `cond` is a long-vs-long compare (signed or unsigned)
    /// between two long-family idents — either or both may be a long
    /// global or a long stack local. Triggers the 3-jump pattern.
    /// Used by `emit_if` to decide whether to allocate a
    /// `then_entry_slot` for the test's true-target jump. Fixtures
    /// 234–237 (globals signed), 242 (globals unsigned), 297 (stack).
    pub(crate) fn is_long_signed_globals_cmp(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op, left, right } = &cond.kind else { return false };
        if !matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge) {
            return false;
        }
        let (ExprKind::Ident(a), ExprKind::Ident(b)) = (&left.kind, &right.kind) else {
            return false;
        };
        self.ident_is_long_like(a) && self.ident_is_long_like(b)
    }
    /// Whether `cond` is `<long_var> <op> K` for a relational
    /// comparison op (`<,>,<=,>=`) on a long global or stack local.
    /// BCC inlines K into the `cmp <mem>, imm` instruction (per
    /// half), choosing the shorter imm8sx form when each half fits
    /// and the wider imm16 otherwise. Fixtures 240 (i8sx global),
    /// 282 (imm16 global), 293 (i8sx stack local).
    pub(crate) fn is_long_signed_const_cmp(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op, left, right } = &cond.kind else { return false };
        if !matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge) {
            return false;
        }
        let ExprKind::Ident(name) = &left.kind else { return false };
        let is_long_global = self.globals.type_of(name).map_or(false, |t| t.is_long_like());
        let is_long_local = self.locals.has(name) && self.locals.type_of(name).is_long_like();
        if !is_long_global && !is_long_local {
            return false;
        }
        try_const_eval(right).is_some()
    }
    /// Whether `cond` is a long-vs-int relational compare between
    /// a long global and an int global. BCC widens the int with
    /// `cwd` (DX:AX = widened i), then compares against g. The
    /// 3-jump pattern uses operand-swapped mnemonics (since the
    /// operand order is widened-int-LHS / long-RHS, but the
    /// source semantics is long-LHS / int-RHS). Fixtures 273
    /// (`<`), and 280 (`!=`) which uses a different shape.
    pub(crate) fn is_long_vs_int_cmp(&self, cond: &Expr) -> bool {
        let ExprKind::BinOp { op, left, right } = &cond.kind else { return false };
        if !matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge) {
            return false;
        }
        let (ExprKind::Ident(a), ExprKind::Ident(b)) = (&left.kind, &right.kind) else {
            return false;
        };
        let a_ty = self.globals.type_of(a);
        let b_ty = self.globals.type_of(b);
        a_ty.map_or(false, |t| t.is_long_like())
            && b_ty.map_or(false, |t| matches!(t, Type::Int))
    }
    pub(crate) fn emit_cond_branch(
        &mut self,
        cond: &Expr,
        true_slot: Option<u32>,
        false_slot: Option<u32>,
    ) {
        // `if (_FLAGS & <flag-bit>)` (and the negated form `!(... & K)`)
        // — BCC special-cases this to a single conditional skip-then
        // jump keyed to the bit. No `pushf`/`test`/`and` emitted. The
        // recognized bits and their skip-then mnemonics:
        //   0x1   (CF)  → jnc      0x4   (PF)  → jnp
        //   0x40  (ZF)  → jne      0x80  (SF)  → jns
        //   0x800 (OF)  → jno
        // The negated form (one `!`) flips skip-then to take-then
        // (jc / jp / je / js / jo). Fixtures 4055, 4057–4061.
        if let Some((skip_mnemonic, take_mnemonic)) = flags_bit_test_mnemonics(cond) {
            if let Some(fslot) = false_slot {
                let _ = write!(self.out, "\t{skip_mnemonic}\tshort {}\r\n", self.label_ref(fslot));
            }
            if let Some(tslot) = true_slot {
                let _ = write!(self.out, "\t{take_mnemonic}\tshort {}\r\n", self.label_ref(tslot));
            }
            return;
        }
        // `if (<byte-pseudo> == imm8)` / `!=` — direct `cmp <reg>, imm8`
        // with no widening. Fixture 4054 (`if (_AL == 0x80)` →
        // `cmp al, 128; jne short ...`).
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && matches!(op, BinOp::Eq | BinOp::Ne)
            && let ExprKind::PseudoReg(name) = &left.kind
            && is_byte_pseudo_register(name)
            && let Some(k) = try_const_eval(right)
        {
            let reg = pseudo_register_operand(name).expect("byte pseudo has operand");
            let _ = write!(self.out, "\tcmp\t{reg},{}\r\n", k & 0xFF);
            let (jmp_true, jmp_false) = match op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
            if let Some(tslot) = true_slot {
                let _ = write!(self.out, "\t{jmp_true}\tshort {}\r\n", self.label_ref(tslot));
            }
            if let Some(fslot) = false_slot {
                let _ = write!(self.out, "\t{jmp_false}\tshort {}\r\n", self.label_ref(fslot));
            }
            return;
        }
        // `<stack-char-arr>[<si-int>] != 0` / `== 0` — direct
        // memory compare via the BP+SI addressing mode. Saves the
        // `mov al; cbw; or ax, ax` chain the generic path would
        // emit. Fixture 2488 (for-loop cond `a[i] != 0`).
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && matches!(op, BinOp::Eq | BinOp::Ne)
            && matches!(&right.kind, ExprKind::IntLit(0))
            && let ExprKind::ArrayIndex { array, index } = &left.kind
            && let Some((disp, _)) = self.bp_idx_disp_for_char_array(array, index)
        {
            let _ = write!(
                self.out,
                "\tcmp\tbyte ptr [bp+si{}],0\r\n",
                signed_disp_suffix(disp),
            );
            let (jmp_true, jmp_false) = match op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
            if let Some(tslot) = true_slot {
                let _ = write!(self.out, "\t{jmp_true}\tshort {}\r\n", self.label_ref(tslot));
            }
            if let Some(fslot) = false_slot {
                let _ = write!(self.out, "\t{jmp_false}\tshort {}\r\n", self.label_ref(fslot));
            }
            return;
        }
        // Float comparison: `<float-expr> <relop> <float-expr>`
        // routes through the 8087 fcomp / fstsw / sahf dance. The
        // condition codes (C0/C2/C3) map onto CF/PF/ZF after
        // sahf, so we use the UNSIGNED conditional-jump family
        // even for signed-looking C operators. Fixture 1674.
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && op.is_comparison()
            && (self.operand_is_float_like(left)
                || self.operand_is_float_like(right))
        {
            self.emit_float_compare_branch(*op, left, right, true_slot, false_slot);
            return;
        }
        // Constant-false cond: emit an unconditional `jmp short
        // <false_slot>` (no cmp/test/jcc). BCC's shape for
        // `if (0) ...`. Fixture 1585.
        if let Some(v) = try_const_eval(cond)
            && v == 0
            && let Some(fslot) = false_slot
        {
            let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // Constant-true cond: no test, no jump — fall through into
        // the then-branch. Used by `if (1) {...} else {...}` shape
        // where the caller still wants the else branch emitted (as
        // dead code with the trailing `jmp` over it). Fixture 2022.
        if let Some(v) = try_const_eval(cond)
            && v != 0
        {
            // If there's a true_slot, fall through reaches the then
            // entry naturally — but the slot label still needs to
            // be emitted later. Caller handles that.
            if let Some(tslot) = true_slot {
                let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(tslot));
            }
            return;
        }
        // `!<array_name>` — array address is link-time non-zero,
        // so `!arr` is constant-falsy. Emit unconditional jmp to
        // false_slot. The unwrapped `arr` truthy case is NOT folded
        // (BCC still emits the `mov ax, offset _arr; or ax, ax`
        // test — matches the const-cond asymmetry where `if (1)`
        // elides only when there's no else). Fixture 2986
        // (`if (!data)` — falsy → jump); fixture 2800 (`if (data)`
        // keeps the cmp).
        if let ExprKind::Unary { op: crate::ast::UnaryOp::Not, operand } = &cond.kind {
            let mut polarity = false;
            let mut cur = operand.as_ref();
            loop {
                if let ExprKind::Unary { op: crate::ast::UnaryOp::Not, operand } = &cur.kind {
                    polarity = !polarity;
                    cur = operand;
                    continue;
                }
                break;
            }
            let is_array_name = if let ExprKind::Ident(name) = &cur.kind {
                self.globals.type_of(name)
                    .map_or(false, |t| matches!(t, Type::Array { .. }))
                    || (self.locals.has(name)
                        && matches!(self.locals.type_of(name), Type::Array { .. }))
            } else {
                false
            };
            if is_array_name && !polarity {
                // The outermost expr is `!arr` (odd number of `!`s
                // wrapping an array name) — always falsy.
                if let Some(fslot) = false_slot {
                    let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(fslot));
                    return;
                }
            }
        }
        // `<long_global> <relop> <int_global>` mixed compare. BCC
        // widens the int (mov ax, _i / cwd to DX:AX), then compares
        // against g. The operand-order in the cmp is widened-int-LHS
        // / long-RHS, but the source semantics is long-LHS /
        // int-RHS — so the mnemonic flips (e.g. `g < i` lowers to
        // `i > g`). Fixture 273.
        if self.is_long_vs_int_cmp(cond)
            && let ExprKind::BinOp { op, left, right } = &cond.kind
            && let ExprKind::Ident(g) = &left.kind
            && let ExprKind::Ident(i) = &right.kind
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            // Flip the op: g <op> i ⇔ i <flipped> g (operands swapped).
            // Then look up mnemonics for the flipped op.
            let flipped = match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Gt => BinOp::Lt,
                BinOp::Le => BinOp::Ge,
                BinOp::Ge => BinOp::Le,
                _ => unreachable!(),
            };
            // Reuse the same mnemonic table as the globals-vs-globals
            // path. Signedness here is "either operand unsigned" →
            // unsigned. Both long_like for unsigned check covers
            // signed long + signed int = signed, etc.
            let unsigned = self.globals.type_of(g).map_or(false, |t| t.is_unsigned())
                || self.globals.type_of(i).map_or(false, |t| t.is_unsigned());
            let (hi_to_false, hi_to_true, lo_to_false) = match (flipped, unsigned) {
                (BinOp::Lt, false) => ("jg", "jl",  "jae"),
                (BinOp::Gt, false) => ("jl", "jg",  "jbe"),
                (BinOp::Le, false) => ("jg", "jne", "ja"),
                (BinOp::Ge, false) => ("jl", "jne", "jb"),
                (BinOp::Lt, true)  => ("ja", "jb",  "jae"),
                (BinOp::Gt, true)  => ("jb", "ja",  "jbe"),
                (BinOp::Le, true)  => ("ja", "jne", "ja"),
                (BinOp::Ge, true)  => ("jb", "jne", "jb"),
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{i}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            let _ = write!(self.out, "\tcmp\tdx,word ptr DGROUP:_{g}+2\r\n");
            let _ = write!(self.out, "\t{hi_to_false}\tshort {}\r\n", self.label_ref(fslot));
            let _ = write!(self.out, "\t{hi_to_true}\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tax,word ptr DGROUP:_{g}\r\n");
            let _ = write!(self.out, "\t{lo_to_false}\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // `<long_global> != <int_global>` mixed inequality. Same
        // widen-via-cwd as `<` but with the chained-cmp shape:
        // jne→true on the high half (definitive), je→false on the
        // low half (both equal → ==). Fixture 280.
        if self.is_long_vs_int_ne(cond)
            && let ExprKind::BinOp { left, right, .. } = &cond.kind
            && let ExprKind::Ident(g) = &left.kind
            && let ExprKind::Ident(i) = &right.kind
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{i}\r\n");
            self.out.extend_from_slice(b"\tcwd\t\r\n");
            let _ = write!(self.out, "\tcmp\tdx,word ptr DGROUP:_{g}+2\r\n");
            let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tax,word ptr DGROUP:_{g}\r\n");
            let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // Signed long-vs-long compare between two long globals. BCC
        // emits a 3-jump pattern: high-half signed cmp with `jg/jl`
        // for definitive answers, low-half unsigned cmp for the
        // tie-breaker. Caller must supply BOTH slots so the
        // intermediate signed-direction jump can land at the body
        // (true target). Fixture 234.
        // `<far-ptr> <eq/ne> <far-ptr>` for non-huge FarPointer stack
        // locals — BCC emits an inline two-half compare rather than
        // calling N_PCMP@ (which is reserved for huge pointers).
        // Shape: load LHS seg→AX and off→DX; cmp AX,RHS-seg; cmp
        // DX,RHS-off; jumps wired so segs-differ-OR-offs-differ → !=
        // and both-match → ==. Mirrors the long-eq/ne arm below.
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && matches!(op, BinOp::Eq | BinOp::Ne)
            && let Some(l_off) = self.far_ptr_lvalue_addr(left)
            && let Some(r_off) = self.far_ptr_lvalue_addr(right)
            && let Some(fslot) = false_slot
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(l_off + 2));
            let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(l_off));
            let _ = write!(self.out, "\tcmp\tax,word ptr {}\r\n", bp_addr(r_off + 2));
            match op {
                BinOp::Eq => {
                    let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(fslot));
                    let _ = write!(self.out, "\tcmp\tdx,word ptr {}\r\n", bp_addr(r_off));
                    let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(fslot));
                }
                BinOp::Ne => {
                    let Some(tslot) = true_slot else {
                        let local_true =
                            self.label_plan.base(cond.span.start, cond.span.end);
                        let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(local_true));
                        let _ = write!(self.out, "\tcmp\tdx,word ptr {}\r\n", bp_addr(r_off));
                        let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(fslot));
                        self.emit_label(local_true);
                        return;
                    };
                    let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(tslot));
                    let _ = write!(self.out, "\tcmp\tdx,word ptr {}\r\n", bp_addr(r_off));
                    let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(fslot));
                }
                _ => unreachable!(),
            }
            return;
        }
        // `<long_lvalue> <eq/ne> <long_lvalue>` — both stack or
        // global, equality only (no strict-cmp signedness issue).
        // Load a's halves into AX/DX, cmp against b's halves with
        // jne short-circuits. Fixture 1644
        // (`if (a == b)` for two long stack locals).
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && matches!(op, BinOp::Eq | BinOp::Ne)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
            && let Some(fslot) = false_slot
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tcmp\tax,word ptr {b_hi}\r\n");
            match op {
                BinOp::Eq => {
                    let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(fslot));
                    let _ = write!(self.out, "\tcmp\tdx,word ptr {b_lo}\r\n");
                    let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(fslot));
                }
                BinOp::Ne => {
                    let Some(tslot) = true_slot else {
                        // Ne without true_slot: caller wants
                        // fall-through-on-true. Need both jumps to
                        // false_slot when EQUAL.
                        let local_true = self.label_plan.base(cond.span.start, cond.span.end);
                        let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(local_true));
                        let _ = write!(self.out, "\tcmp\tdx,word ptr {b_lo}\r\n");
                        let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(fslot));
                        self.emit_label(local_true);
                        return;
                    };
                    let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(tslot));
                    let _ = write!(self.out, "\tcmp\tdx,word ptr {b_lo}\r\n");
                    let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(fslot));
                }
                _ => unreachable!(),
            }
            return;
        }
        if self.is_long_signed_globals_cmp(cond)
            && let ExprKind::BinOp { op, left, right } = &cond.kind
            && let ExprKind::Ident(a) = &left.kind
            && let ExprKind::Ident(b) = &right.kind
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            // Mnemonic table. Signed (fixtures 234–237) vs unsigned
            // (fixture 242) differs only in the high-half jumps:
            // signed uses jl/jg, unsigned uses jb/ja. The non-strict
            // high-half true jump is `jne` in both cases. Low-half
            // is always unsigned (jae/jbe strict; ja/jb non-strict).
            let unsigned = self.cmp_is_unsigned(left, right);
            let (hi_to_false, hi_to_true, lo_to_false) = match (op, unsigned) {
                (BinOp::Lt, false) => ("jg", "jl",  "jae"),
                (BinOp::Gt, false) => ("jl", "jg",  "jbe"),
                (BinOp::Le, false) => ("jg", "jne", "ja"),
                (BinOp::Ge, false) => ("jl", "jne", "jb"),
                (BinOp::Lt, true)  => ("ja", "jb",  "jae"),
                (BinOp::Gt, true)  => ("jb", "ja",  "jbe"),
                (BinOp::Le, true)  => ("ja", "jne", "ja"),
                (BinOp::Ge, true)  => ("jb", "jne", "jb"),
                _ => unreachable!(),
            };
            let (a_hi, a_lo) = self.long_addr_pair(a);
            let (b_hi, b_lo) = self.long_addr_pair(b);
            let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tcmp\tax,word ptr {b_hi}\r\n");
            let _ = write!(self.out, "\t{hi_to_false}\tshort {}\r\n", self.label_ref(fslot));
            let _ = write!(self.out, "\t{hi_to_true}\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tdx,word ptr {b_lo}\r\n");
            let _ = write!(self.out, "\t{lo_to_false}\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // `<long_global> <op> K` for K with both halves fitting
        // i8sx — same 3-jump shape as fixture 234 but using
        // `cmp <mem>, imm` directly (no AX/DX load). Fixture 240.
        if self.is_long_signed_const_cmp(cond)
            && let ExprKind::BinOp { op, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(k) = try_const_eval(right)
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            // Each half is formatted as i8sx-decimal when it fits,
            // u16-decimal otherwise — letting the assembler pick
            // the `83 3E` (5 bytes) vs `81 3E` (6 bytes) opcode
            // automatically. Fixtures 240 (i8sx), 282 (imm16).
            let hi = (k >> 16) as i32;
            let lo = (k & 0xFFFF) as i32;
            let fmt = |v: i32| -> String {
                if (-128..=127).contains(&v) {
                    format!("{v}")
                } else {
                    format!("{}", v as u16)
                }
            };
            let unsigned = if let Some(gt) = self.globals.type_of(name) {
                gt.is_unsigned()
            } else {
                self.locals.type_of(name).is_unsigned()
            };
            let (hi_to_false, hi_to_true, lo_to_false) = match (op, unsigned) {
                (BinOp::Lt, false) => ("jg", "jl",  "jae"),
                (BinOp::Gt, false) => ("jl", "jg",  "jbe"),
                (BinOp::Le, false) => ("jg", "jne", "ja"),
                (BinOp::Ge, false) => ("jl", "jne", "jb"),
                (BinOp::Lt, true)  => ("ja", "jb",  "jae"),
                (BinOp::Gt, true)  => ("jb", "ja",  "jbe"),
                (BinOp::Le, true)  => ("ja", "jne", "ja"),
                (BinOp::Ge, true)  => ("jb", "jne", "jb"),
                _ => unreachable!(),
            };
            // Choose between DGROUP-relative (global) and bp-relative
            // (stack-local) operand text. Fixtures 240 (global), 293
            // (stack local).
            let (hi_addr, lo_addr) = if self.globals.contains(name) {
                (format!("DGROUP:_{name}+2"), format!("DGROUP:_{name}"))
            } else {
                let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                    unreachable!("long is never register-resident");
                };
                (bp_addr(off + 2), bp_addr(off))
            };
            let _ = write!(self.out, "\tcmp\tword ptr {},{}\r\n", hi_addr, fmt(hi));
            let _ = write!(self.out, "\t{hi_to_false}\tshort {}\r\n", self.label_ref(fslot));
            let _ = write!(self.out, "\t{hi_to_true}\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tword ptr {},{}\r\n", lo_addr, fmt(lo));
            let _ = write!(self.out, "\t{lo_to_false}\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // `<long_global> != K` for non-zero K — chained cmp with
        // both slots: jne→true (high differs is definitive), then
        // je→false (high equal AND low equal). Fall-through (low
        // differs, high equal) lands at true. Fixture 239.
        if self.is_long_ne_const(cond)
            && let ExprKind::BinOp { op: BinOp::Ne, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(k) = try_const_eval(right)
            && let (Some(tslot), Some(fslot)) = (true_slot, false_slot)
        {
            let hi = (k >> 16) as i32;
            let lo = (k & 0xFFFF) as i32;
            let _ = write!(self.out, "\tcmp\tword ptr DGROUP:_{name}+2,{hi}\r\n");
            let _ = write!(self.out, "\tjne\tshort {}\r\n", self.label_ref(tslot));
            let _ = write!(self.out, "\tcmp\tword ptr DGROUP:_{name},{lo}\r\n");
            let _ = write!(self.out, "\tje\tshort {}\r\n", self.label_ref(fslot));
            return;
        }
        // `<long_global> == K` for non-zero K — BCC emits a chained
        // cmp+jne pair: high half against (K>>16), low half against
        // (K&0xFFFF). Both halves use Grp1 imm8sx form, so each half
        // must fit a sign-extended i8. Only the false-slot-only shape
        // shows up in fixture 223 (`if (g == K) ...`); a true-slot
        // form would invert to `je` and pick up later.
        if let ExprKind::BinOp { op: BinOp::Eq, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_long_like()
            && let Some(k) = try_const_eval(right)
            && k != 0
            && true_slot.is_none()
            && let Some(fslot) = false_slot
        {
            let hi = (k >> 16) as i32;
            let lo = (k & 0xFFFF) as i32;
            // Each half must sign-extend cleanly from imm8. BCC has
            // wider forms for out-of-range K (not yet observed); fall
            // through to the generic path when this guard fails.
            if (-128..=127).contains(&hi) && (-128..=127).contains(&lo) {
                let _ = write!(
                    self.out,
                    "\tcmp\tword ptr DGROUP:_{name}+2,{hi}\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tjne\tshort {}\r\n",
                    self.label_ref(fslot),
                );
                let _ = write!(
                    self.out,
                    "\tcmp\tword ptr DGROUP:_{name},{lo}\r\n",
                );
                let _ = write!(
                    self.out,
                    "\tjne\tshort {}\r\n",
                    self.label_ref(fslot),
                );
                return;
            }
        }
        if let ExprKind::Logical { op, left, right } = &cond.kind {
            // The recursive structure handles chained `&&`/`||`
            // (fixtures 620/621): each non-final operand short-
            // circuits to the outer false_slot (for `&&`) or
            // true_slot (for `||`), and the last operand inherits
            // the outer (true, false) target pair. The recursion is
            // safe as long as the AST nests left-associatively
            // (parser default for `&&` and `||` in C).
            match op {
                LogicalOp::And => {
                    // a false → false_slot; a true → fall through to b.
                    // b carries the outer true/false targets.
                    // When `&&` is the LHS of an outer `||`
                    // (false_slot is None but true_slot is set), `a`
                    // false needs a local "after-b" label to fall
                    // into the outer `||`'s right operand. Use this
                    // `&&`'s own plan slot (base+0) for that.
                    // Fixture 1358 (`a && b || c`).
                    let local_false = if false_slot.is_none() && true_slot.is_some() {
                        Some(self.label_plan.base(cond.span.start, cond.span.end))
                    } else {
                        false_slot
                    };
                    self.emit_cond_branch(left, None, local_false);
                    self.emit_cond_branch(right, true_slot, false_slot);
                    if false_slot.is_none()
                        && true_slot.is_some()
                        && let Some(slot) = local_false
                    {
                        self.emit_label(slot);
                    }
                }
                LogicalOp::Or => {
                    // a true → true_slot (jump); a false → fall through to b.
                    // For the rightmost (final) operand of an Or chain
                    // the caller will emit `true_slot`'s label right
                    // after, so b can fall through on true; that's the
                    // case when `false_slot.is_some()` (we're at the
                    // top of an if-cond Or chain). For non-final Ors
                    // (this Or is itself the LHS of an outer Or — the
                    // chained case from fixture 621) b's true must
                    // jump explicitly, since the caller emits more
                    // code (the outer Or's right operand) before the
                    // true label.
                    //
                    // `(a || b) && c` — the Or is on the LHS of an
                    // outer And, so true_slot is None (And's left
                    // call doesn't pass a true target). `a` true
                    // needs a local label to jump past the Or to the
                    // next operand. Allocate one from the Or's plan
                    // slot. Fixture 3510.
                    let local_true = if true_slot.is_none() && false_slot.is_some() {
                        Some(self.label_plan.base(cond.span.start, cond.span.end))
                    } else {
                        true_slot
                    };
                    self.emit_cond_branch(left, local_true, None);
                    let (right_true, right_false) = if false_slot.is_some() {
                        (None, false_slot)
                    } else {
                        (true_slot, None)
                    };
                    self.emit_cond_branch(right, right_true, right_false);
                    if true_slot.is_none()
                        && false_slot.is_some()
                        && let Some(slot) = local_true
                    {
                        self.emit_label(slot);
                    }
                }
            }
            return;
        }
        // Base case: single test (comparison or treat-as-bool).
        let (true_mnem, false_mnem) = self.emit_cond_test(cond);
        match (true_slot, false_slot) {
            (Some(slot), None) => {
                let _ = write!(
                    self.out,
                    "\t{true_mnem}\tshort {}\r\n",
                    self.label_ref(slot),
                );
            }
            (None, Some(slot)) => {
                let _ = write!(
                    self.out,
                    "\t{false_mnem}\tshort {}\r\n",
                    self.label_ref(slot),
                );
            }
            (Some(_), Some(false_slot)) => {
                // Both targets specified. This fires for the right-
                // most operand of an `&&` chain inside an `if` cond
                // that has a `then_entry` label allocated (see
                // `needs_then_entry` in emit_if). The convention is
                // that `then_entry` (= true_slot) is the *next*
                // emitted label, so fall-through-on-true is correct.
                // Emit `<false_mnem> false_slot` and let the true
                // path fall through. Fixture 3510 (`(a||b) && c`).
                let _ = write!(
                    self.out,
                    "\t{false_mnem}\tshort {}\r\n",
                    self.label_ref(false_slot),
                );
            }
            (None, None) => panic!(
                "emit_cond_branch with both targets fall-through: no jump would be emitted"
            ),
        }
    }
    /// Emit the actual test instruction for a simple (non-Logical)
    /// condition and return the (jump-if-true, jump-if-false)
    /// mnemonic pair the caller should use.
    ///
    /// - Comparison `a <op> b`: emit `emit_compare`, return the op's
    ///   `(jump_if_true, jump_if_false)` mnemonics.
    /// - Anything else: treat as boolean. Emit `cmp <expr>, 0` (or
    ///   `or <reg>, <reg>` peephole for register locals); the cond is
    ///   non-zero ⇔ true, so the mnemonic pair is `("jne", "je")`.
    pub(crate) fn emit_cond_test(&mut self, cond: &Expr) -> (&'static str, &'static str) {
        // `if (!<expr>)` — generate the same flag-setting test as
        // `<expr>` but swap the true/false jump mnemonics so the
        // conditional jump takes the inverted path. Fixture 536
        // (`if (!g)` on an int global lowers to `cmp [g], 0 / jne
        // <skip-then>`). Nested `!!x` falls back into this case so
        // the swap composes correctly.
        if let ExprKind::Unary { op: crate::ast::UnaryOp::Not, operand } = &cond.kind {
            // `!<char-ident>` — BCC widens the char to int before
            // testing the value (integer-promotion of the bare char
            // operand of `!`). The bare-ident path emits the shorter
            // `cmp byte ptr [bp+off], 0` only when the char appears
            // *unwrapped* in conditional position (fixture 999); when
            // wrapped in `!`, the widen-then-or shape is canonical.
            // Fixture 3204 (`if (!c)` for a char param).
            if let ExprKind::Ident(name) = &operand.kind
                && self.locals.has(name)
                && self.locals.type_of(name).is_char_like()
                && let LocalLocation::Stack(off) = self.locals.location_of(name)
            {
                let unsigned = self.locals.type_of(name).is_unsigned();
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                if unsigned {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
                self.out.extend_from_slice(b"\tor\tax,ax\r\n");
                return ("je", "jne");
            }
            let (t, f) = self.emit_cond_test(operand);
            return (f, t);
        }
        // `if (<int-global> & K)` — bit-test against a constant
        // mask. BCC emits `test word ptr [_g], K` (F7 06 lo hi
        // imm16, 6 bytes) which sets ZF based on the AND result
        // without storing it. Fixture 569.
        if let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(gty) = self.globals.type_of(name)
            && matches!(gty, Type::Int | Type::UInt)
            && let Some(k) = try_const_eval(right)
        {
            let k16 = k & 0xFFFF;
            let _ = write!(
                self.out,
                "\ttest\tword ptr DGROUP:_{name},{k16}\r\n",
            );
            return ("jne", "je");
        }
        // `if (<int-local> & K)` — stack-local sibling. `test word
        // ptr [bp+N], K` (5 bytes) vs the load + and + or sequence.
        // Fixture 1853.
        if let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &cond.kind
            && let ExprKind::Ident(name) = &left.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
            && let Some(k) = try_const_eval(right)
        {
            let k16 = k & 0xFFFF;
            let _ = write!(
                self.out,
                "\ttest\tword ptr {},{k16}\r\n",
                bp_addr(off),
            );
            return ("jne", "je");
        }
        // `(<int-mem> & <int-mem>) == 0` / `!= 0` — both operands
        // are int lvalues. BCC loads one into AX, then `test [other],
        // ax` (sets ZF without storing). Fixture 3539.
        if let ExprKind::BinOp { op: outer_op, left: outer_l, right: outer_r } = &cond.kind
            && matches!(outer_op, BinOp::Eq | BinOp::Ne)
            && try_const_eval(outer_r) == Some(0)
            && let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &outer_l.kind
            && let ExprKind::Ident(lname) = &left.kind
            && let ExprKind::Ident(rname) = &right.kind
            && self.locals.has(lname)
            && self.locals.has(rname)
            && self.locals.type_of(lname).is_int_like()
            && self.locals.type_of(rname).is_int_like()
            && let LocalLocation::Stack(l_off) = self.locals.location_of(lname)
            && let LocalLocation::Stack(r_off) = self.locals.location_of(rname)
        {
            // BCC loads the RHS-ident into AX, then `test [l], ax`.
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(r_off));
            let _ = write!(self.out, "\ttest\tword ptr {},ax\r\n", bp_addr(l_off));
            let mnem_pair = match outer_op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
            return mnem_pair;
        }
        // `(<int-mem> & K) == 0` or `!= 0` — the `& K` already sets
        // ZF via TEST, so the outer compare against 0 is implicit.
        // Routes through the same TestBpRelImm16 / TestGroupSymImm16
        // shape but inverts the true/false mnemonic based on Eq vs
        // Ne. Fixtures 3540 (`(x & 0x10) == 0`), 3264 (`(x & 0xff)
        // != 0`).
        if let ExprKind::BinOp { op: outer_op, left: outer_l, right: outer_r } = &cond.kind
            && matches!(outer_op, BinOp::Eq | BinOp::Ne)
            && try_const_eval(outer_r) == Some(0)
            && let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &outer_l.kind
            && let ExprKind::Ident(name) = &left.kind
            && let Some(k) = try_const_eval(right)
        {
            let k16 = k & 0xFFFF;
            let mnem_pair = match outer_op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
            if let Some(gty) = self.globals.type_of(name)
                && gty.is_int_like()
            {
                let _ = write!(
                    self.out,
                    "\ttest\tword ptr DGROUP:_{name},{k16}\r\n",
                );
                return mnem_pair;
            }
            if self.locals.has(name)
                && self.locals.type_of(name).is_int_like()
                && let LocalLocation::Stack(off) = self.locals.location_of(name)
            {
                let _ = write!(
                    self.out,
                    "\ttest\tword ptr {},{k16}\r\n",
                    bp_addr(off),
                );
                return mnem_pair;
            }
        }
        // `<long_global> == 0` / `<long_global> != 0` — BCC folds the
        // 32-bit comparison into `mov ax,low / or ax,high`, which
        // sets ZF iff both halves are zero. Fixture 215.
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && matches!(op, BinOp::Eq | BinOp::Ne)
            && let ExprKind::Ident(name) = &left.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_long_like()
            && try_const_eval(right) == Some(0)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\tor\tax,word ptr DGROUP:_{name}+2\r\n");
            return match op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
        }
        // Same shape for a stack-resident long local vs 0 (fixture 292).
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && matches!(op, BinOp::Eq | BinOp::Ne)
            && let ExprKind::Ident(name) = &left.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
            && try_const_eval(right) == Some(0)
        {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long is never register-resident");
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
            let _ = write!(self.out, "\tor\tax,word ptr {}\r\n", bp_addr(off + 2));
            return match op {
                BinOp::Eq => ("je", "jne"),
                BinOp::Ne => ("jne", "je"),
                _ => unreachable!(),
            };
        }
        if let ExprKind::BinOp { op, left, right } = &cond.kind
            && op.is_comparison()
        {
            // `<arith>(X, Y) <relop> 0` — the arith op already set
            // the flags we want. Just evaluate the arith expression
            // into AX and use the relop's mnemonic directly. Saves
            // the `or ax,ax` (or `cmp ax,0`) instruction. Fixtures
            // 3254 (`a + b > 0`), 3257 (`a - b == 0`).
            let unsigned = self.cmp_is_unsigned(left, right);
            if try_const_eval(right) == Some(0)
                && let ExprKind::BinOp { op: arith, .. } = &left.kind
                && matches!(
                    arith,
                    BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
                )
            {
                self.emit_expr_to_ax(left);
                return (
                    op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                    op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
                );
            }
            // `++<reg>` / `--<reg>` <relop> 0 — the inc/dec
            // instruction sets ZF/SF, which `<relop>` can read
            // directly. Emit just the inc/dec on the register.
            // Fixture 3644 (`while (--n > 0)`).
            if try_const_eval(right) == Some(0)
                && let ExprKind::Update {
                    target,
                    op: upd_op,
                    position: crate::ast::UpdatePosition::Pre,
                } = &left.kind
                && self.locals.has(target)
                && let LocalLocation::Reg(reg) = self.locals.location_of(target)
                && self.locals.type_of(target).is_int_like()
            {
                let mnem = match upd_op {
                    crate::ast::UpdateOp::Inc => "inc",
                    crate::ast::UpdateOp::Dec => "dec",
                };
                let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
                return (
                    op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                    op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
                );
            }
            if try_const_eval(left).is_some() && try_const_eval(right).is_none() {
                let flipped_op = match op {
                    BinOp::Eq | BinOp::Ne => *op,
                    BinOp::Lt => BinOp::Gt,
                    BinOp::Gt => BinOp::Lt,
                    BinOp::Le => BinOp::Ge,
                    BinOp::Ge => BinOp::Le,
                    _ => unreachable!(),
                };
                self.emit_compare(right, left);
                return (
                    flipped_op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                    flipped_op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
                );
            }
            // `<int-lvalue> <relop> <call>` — the call returns in
            // AX, so emit it first, then `cmp ax, <lvalue>` with
            // the comparison's operands implicitly swapped. The
            // jump mnemonic uses the flipped op (cmp ax, x for
            // `x > result` reads flags from result-x, so the
            // original Gt becomes Lt for the jump mnemonic).
            // Fixture 2044 (`if (x > get_threshold())`).
            if let ExprKind::Call { name: fname, args } = &right.kind
                && let Some(ret_ty) = self.signatures.ret_ty_of(fname)
                && ret_ty.is_int_like()
                && let Some(lhs_addr) = self.int_lvalue_addr(left)
            {
                let flipped_op = match op {
                    BinOp::Eq | BinOp::Ne => *op,
                    BinOp::Lt => BinOp::Gt,
                    BinOp::Gt => BinOp::Lt,
                    BinOp::Le => BinOp::Ge,
                    BinOp::Ge => BinOp::Le,
                    _ => unreachable!(),
                };
                self.emit_call(fname, args);
                let _ = write!(self.out, "\tcmp\tax,word ptr {lhs_addr}\r\n");
                return (
                    flipped_op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                    flipped_op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
                );
            }
            // For Eq/Ne, char-vs-int lvalue compare loads the char
            // operand first (widened to int) then `cmp ax, word ptr
            // <int>`. Safe for commutative ops only — emit_compare
            // doesn't see the op and can't flip jump mnemonics.
            // Fixture 3435 (`if (x == gc)` for int param x, char
            // global gc).
            if matches!(op, BinOp::Eq | BinOp::Ne)
                && self.try_emit_int_vs_char_cmp(left, right)
            {
                return (
                    op.jump_if_true(unsigned).expect("comparison op has true mnemonic"),
                    op.jump_if_false(unsigned).expect("comparison op has false mnemonic"),
                );
            }
            // `p[idx] <eq/ne> 0` where p is a stack-resident pointer
            // local. After the address calc lands in BX, BCC emits
            // `cmp <w> ptr [bx], 0` directly instead of `mov ax, [bx]
            // / or ax, ax`. Fixture 3331 (`for (...; p[i] != 0;...)`).
            if matches!(op, BinOp::Eq | BinOp::Ne)
                && try_const_eval(right) == Some(0)
                && let ExprKind::ArrayIndex { array, index } = &left.kind
                && let ExprKind::Ident(name) = &array.kind
                && self.locals.has(name)
                && let Some(pointee) = self.locals.type_of(name).pointee()
            {
                let pointee = pointee.clone();
                let stride = u32::from(pointee.size_bytes());
                self.emit_expr_to_ax(index);
                for _ in 0..stride.trailing_zeros() {
                    self.out.extend_from_slice(b"\tshl\tax,1\r\n");
                }
                match self.locals.location_of(name) {
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
                let width = if pointee.is_char_like() { "byte" } else { "word" };
                let _ = write!(self.out, "\tcmp\t{width} ptr [bx],0\r\n");
                return match op {
                    BinOp::Eq => ("je", "jne"),
                    BinOp::Ne => ("jne", "je"),
                    _ => unreachable!(),
                };
            }
            // `<global_arr>[<var_idx>] <eq/ne> 0` — scale index into
            // BX, then `cmp <w> ptr DGROUP:_<arr>[bx], 0` directly.
            // Fixture 3232 (`while (... && data[i] != 0)`). Constant
            // index folds to a direct `cmp DGROUP:_<arr>+<off>, 0`
            // (no [bx]). Fixture 3233 (`data[0] != 0`).
            if matches!(op, BinOp::Eq | BinOp::Ne)
                && try_const_eval(right) == Some(0)
                && let ExprKind::ArrayIndex { array, index } = &left.kind
                && let ExprKind::Ident(arr_name) = &array.kind
                && let Some(gty) = self.globals.type_of(arr_name)
                && let Some(elem_ty) = gty.array_elem()
            {
                let elem_ty = elem_ty.clone();
                let width = if elem_ty.is_char_like() { "byte" } else { "word" };
                if let Some(k) = try_const_eval(index) {
                    let stride = i32::from(elem_ty.size_bytes());
                    let off = (k as i32).wrapping_mul(stride);
                    let addr = if off == 0 {
                        format!("DGROUP:_{arr_name}")
                    } else if off > 0 {
                        format!("DGROUP:_{arr_name}+{off}")
                    } else {
                        format!("DGROUP:_{arr_name}{off}")
                    };
                    let _ = write!(self.out, "\tcmp\t{width} ptr {addr},0\r\n");
                } else {
                    self.emit_index_into_bx(index, &elem_ty);
                    let _ = write!(
                        self.out,
                        "\tcmp\t{width} ptr DGROUP:_{arr_name}[bx],0\r\n",
                    );
                }
                return match op {
                    BinOp::Eq => ("je", "jne"),
                    BinOp::Ne => ("jne", "je"),
                    _ => unreachable!(),
                };
            }
            self.cmp_swapped = false;
            self.emit_compare(left, right);
            let true_mnem = op.jump_if_true(unsigned).expect("comparison op has true mnemonic");
            let false_mnem = op.jump_if_false(unsigned).expect("comparison op has false mnemonic");
            if self.cmp_swapped {
                self.cmp_swapped = false;
                return (swap_jcc(true_mnem), swap_jcc(false_mnem));
            }
            return (true_mnem, false_mnem);
        }
        // Bare long-global ident in condition position — equivalent
        // to `<long> != 0`. Use the OR-then-test idiom (fixture 284:
        // `if (a || b)` for two longs lowers to two of these tests
        // chained by short-circuit).
        if let ExprKind::Ident(name) = &cond.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_long_like()
        {
            let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\tor\tax,word ptr DGROUP:_{name}+2\r\n");
            return ("jne", "je");
        }
        // Bare long-stack-local ident in condition position — sibling
        // to the long-global case. Both halves must be zero for the
        // long to be falsy: `mov ax, [lo]; or ax, [hi]` sets ZF iff
        // both are zero. Fixture 2188 (`long a = 5L; if (a)`).
        if let ExprKind::Ident(name) = &cond.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
        {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long is never register-resident");
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(off));
            let _ = write!(self.out, "\tor\tax,word ptr {}\r\n", bp_addr(off + 2));
            return ("jne", "je");
        }
        self.emit_zero_test(cond);
        ("jne", "je")
    }
    /// Try to emit a char-vs-int memory compare for `Eq` / `Ne`
    /// only: when exactly one of `left` / `right` is a char-typed
    /// lvalue and the other an int-typed lvalue, BCC loads the
    /// char operand first (widened via `cbw` or `mov ah, 0`) and
    /// then compares AX against the int memory. Returns `true` if
    /// the compare was emitted (caller skips its own
    /// `emit_compare`). Restricted to commutative ops — the
    /// implicit operand swap (loading char first regardless of
    /// which side it was on) would invalidate the relop semantics
    /// for `<`, `<=`, `>`, `>=`.
    pub(crate) fn try_emit_int_vs_char_cmp(&mut self, left: &Expr, right: &Expr) -> bool {
        let Some((l_name, l_off, l_ty)) = self.try_lvalue_chain_addr(left) else {
            return false;
        };
        let Some((r_name, r_off, r_ty)) = self.try_lvalue_chain_addr(right) else {
            return false;
        };
        if l_ty.is_char_like() == r_ty.is_char_like() {
            return false;
        }
        if !matches!(l_ty, Type::Int | Type::UInt | Type::Char | Type::UChar)
            || !matches!(r_ty, Type::Int | Type::UInt | Type::Char | Type::UChar)
        {
            return false;
        }
        let (char_name, char_off, char_ty, int_name, int_off) =
            if l_ty.is_char_like() {
                (l_name, l_off, l_ty, r_name, r_off)
            } else {
                (r_name, r_off, r_ty, l_name, l_off)
            };
        let Some(char_addr) = self.resolve_chain_addr(&char_name, char_off) else {
            return false;
        };
        let Some(int_addr) = self.resolve_chain_addr(&int_name, int_off) else {
            return false;
        };
        let unsigned = char_ty.is_unsigned();
        let _ = write!(self.out, "\tmov\tal,byte ptr {char_addr}\r\n");
        if unsigned {
            self.out.extend_from_slice(b"\tmov\tah,0\r\n");
        } else {
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        }
        let _ = write!(self.out, "\tcmp\tax,word ptr {int_addr}\r\n");
        true
    }
}
