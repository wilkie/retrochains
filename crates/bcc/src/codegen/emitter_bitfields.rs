use super::*;

impl<'a> super::FunctionEmitter<'a> {
    /// Resolve a `<stack-local-struct>.<bitfield>` expression into
    /// the byte address + bit placement codegen needs to emit a
    /// read or write. Only the within-byte case is supported today
    /// (`bit_offset + bit_width <= 8`); cross-byte bitfields return
    /// `None` and fall through to a panic-or-wrong path. Fixture
    /// 1691.
    pub(crate) fn resolve_bitfield(&self, e: &Expr) -> Option<BitfieldRef> {
        // See through `(int)<bitfield>` / `(unsigned)<bitfield>`
        // casts — the cast is a no-op once the field is extracted
        // into an int register (the post-AND result already lives
        // in 16-bit width). Fixture 2105's `(int)fl.val`.
        let mut inner = e;
        while let ExprKind::Cast { ty, operand } = &inner.kind {
            if !ty.is_int_like() {
                break;
            }
            inner = operand;
        }
        let ExprKind::Member { base, field, kind } = &inner.kind else {
            return None;
        };
        let ExprKind::Ident(name) = &base.kind else {
            return None;
        };
        match kind {
            crate::ast::MemberKind::Dot => self.resolve_bitfield_named(name, field),
            crate::ast::MemberKind::Arrow => self.resolve_bitfield_through_ptr(name, field),
        }
    }
    /// Resolve `<ptr>-><bitfield>` where `ptr` is a stack/register-
    /// resident pointer to a struct with bitfield `field`. The
    /// address becomes `[reg+byte_off]` or `[bp+disp]` depending on
    /// where `ptr` lives. Fixture 3447 (\`unsigned get_a(struct F
    /// *p) { return p->a; }\`).
    pub(crate) fn resolve_bitfield_through_ptr(
        &self,
        ptr_name: &str,
        field: &str,
    ) -> Option<BitfieldRef> {
        if !self.locals.has(ptr_name) {
            return None;
        }
        let ptr_ty = self.locals.type_of(ptr_name).clone();
        let pointee = ptr_ty.pointee()?.clone();
        let field_info = struct_field_info(&pointee, field)?;
        let bf = field_info.bitfield?;
        let addr = match self.locals.location_of(ptr_name) {
            LocalLocation::Reg(reg) => {
                if field_info.offset == 0 {
                    format!("[{}]", reg.name())
                } else {
                    format!("[{}+{}]", reg.name(), field_info.offset)
                }
            }
            LocalLocation::Stack(_) => return None,
        };
        let access = if bf.bit_offset + bf.bit_width <= 8 {
            BitfieldAccess::Byte
        } else {
            BitfieldAccess::Word
        };
        Some(BitfieldRef {
            addr,
            access,
            bit_offset: bf.bit_offset,
            bit_width: bf.bit_width,
            signed: !field_info.ty.is_unsigned(),
        })
    }
    pub(crate) fn resolve_bitfield_named(&self, struct_name: &str, field: &str) -> Option<BitfieldRef> {
        // Stack-resident local: `[bp+disp]`. File-scope global:
        // `DGROUP:_<name>[+off]`. Either way the bitfield info
        // comes from the struct type. Fixture 3209 (global bitfield
        // write to DGROUP:_b).
        let (base_ty, addr) = if self.locals.has(struct_name) {
            let ty = self.locals.type_of(struct_name).clone();
            let field_info = struct_field_info(&ty, field)?;
            let LocalLocation::Stack(struct_off) = self.locals.location_of(struct_name)
            else {
                return None;
            };
            let byte_off =
                struct_off + i16::try_from(field_info.offset).expect("field offset fits");
            (ty, bp_addr(byte_off))
        } else if let Some(gty) = self.globals.type_of(struct_name) {
            let ty = gty.clone();
            let field_info = struct_field_info(&ty, field)?;
            let addr = if field_info.offset == 0 {
                format!("DGROUP:_{struct_name}")
            } else {
                format!("DGROUP:_{struct_name}+{}", field_info.offset)
            };
            (ty, addr)
        } else {
            return None;
        };
        let field_info = struct_field_info(&base_ty, field)?;
        let bf = field_info.bitfield?;
        let access = if bf.bit_offset + bf.bit_width <= 8 {
            BitfieldAccess::Byte
        } else {
            BitfieldAccess::Word
        };
        Some(BitfieldRef {
            addr,
            access,
            bit_offset: bf.bit_offset,
            bit_width: bf.bit_width,
            signed: !field_info.ty.is_unsigned(),
        })
    }
    /// Emit `mov <low>, byte ptr <addr>; [shift]; and <full>, mask`
    /// to materialize a within-byte bitfield value into the
    /// destination register. The shift is omitted when the field
    /// sits at the LSB; uses single-bit `shr <full>, 1` for shifts
    /// of 1–2 (each is 2 bytes), or `mov cl, K; shr <full>, cl`
    /// (5 bytes) for K ≥ 3 — the byte-count crossover.
    pub(crate) fn emit_bitfield_read_to_reg(
        &mut self,
        bf: &BitfieldRef,
        full_reg: &str,
        low_reg: &str,
    ) {
        // Byte access loads the low byte and lets the trailing AND
        // also clear the high half. Word access loads the full
        // register directly — cross-byte bitfields need both bytes
        // present before the shift.
        match bf.access {
            BitfieldAccess::Byte => {
                let _ = write!(
                    self.out,
                    "\tmov\t{low_reg},byte ptr {}\r\n",
                    bf.addr,
                );
            }
            BitfieldAccess::Word => {
                let _ = write!(
                    self.out,
                    "\tmov\t{full_reg},word ptr {}\r\n",
                    bf.addr,
                );
            }
        }
        if bf.signed {
            // Signed bitfield: sign-extend by shifting the field
            // up to the MSB then arithmetically right-shifting back.
            // SHL/SAR both use the CL-loaded form regardless of
            // shift count (fixture 2107 emits `mov cl, 12; shl ax,
            // cl; mov cl, 12; sar ax, cl` for a 4-bit field at
            // bit_offset 0).
            let left_shift: u8 = 16 - bf.bit_offset - bf.bit_width;
            let right_shift: u8 = 16 - bf.bit_width;
            let _ = write!(self.out, "\tmov\tcl,{left_shift}\r\n");
            let _ = write!(self.out, "\tshl\t{full_reg},cl\r\n");
            let _ = write!(self.out, "\tmov\tcl,{right_shift}\r\n");
            let _ = write!(self.out, "\tsar\t{full_reg},cl\r\n");
            return;
        }
        // Unsigned bitfield: shift right then mask.
        // Shift selection: single-bit `shr reg, 1` (2 bytes each)
        // for offsets 1-3, `mov cl, K; shr reg, cl` (5 bytes) for
        // offsets ≥ 4. Empirically matches BCC's choice — fixture
        // 1691 uses CL-loaded at shift 4, fixture 2471 uses three
        // single-bit shifts at shift 3 even though the byte count
        // is 6 (one byte longer than the CL form).
        if bf.bit_offset >= 4 {
            let _ = write!(self.out, "\tmov\tcl,{}\r\n", bf.bit_offset);
            let _ = write!(self.out, "\tshr\t{full_reg},cl\r\n");
        } else {
            for _ in 0..bf.bit_offset {
                let _ = write!(self.out, "\tshr\t{full_reg},1\r\n");
            }
        }
        let mask: u32 = (1u32 << bf.bit_width).wrapping_sub(1);
        let _ = write!(self.out, "\tand\t{full_reg},{mask}\r\n");
    }
    /// When `e` is a left-associative chain of `BinOp(Add|Sub|…)`
    /// over within-byte bitfield reads, emit the canonical BCC
    /// sequence — first operand materialized into AX, each
    /// subsequent operand into DX, with `<op> ax, dx` folding it
    /// into the accumulator. Returns `Some(())` if the chain was
    /// emitted; `None` if any operand fails the bitfield-read
    /// check, in which case the caller falls back to its normal
    /// path. Fixture 1691.
    pub(crate) fn try_emit_bitfield_chain_to_ax(&mut self, e: &Expr) -> Option<()> {
        // Walk the BinOp tree leftward, collecting (op, right-bf)
        // pairs while the right operand is a bitfield and the op
        // is one of the AX-DX-foldable kinds (additive + bitwise).
        // Stop as soon as the chain breaks; the remaining left
        // subexpression seeds AX via either a head-bitfield read
        // or a fallback to the normal emit_expr_to_ax path.
        let mut chain: Vec<(BinOp, BitfieldRef)> = Vec::new();
        let mut cur = e;
        loop {
            match &cur.kind {
                ExprKind::BinOp { op, left, right } => {
                    if !matches!(
                        op,
                        BinOp::Add | BinOp::Sub | BinOp::BitAnd
                        | BinOp::BitOr | BinOp::BitXor
                    ) {
                        break;
                    }
                    let Some(right_bf) = self.resolve_bitfield(right) else { break };
                    chain.push((*op, right_bf));
                    cur = left;
                }
                _ => break,
            }
        }
        if chain.is_empty() {
            return None;
        }
        // Seed AX: prefer a head-bitfield read (matches BCC's
        // shape when the chain's deepest term is also a bitfield —
        // fixture 1691); fall back to the normal AX emitter when
        // the head is any other expression (fixture 2105's
        // `(int)f1 * 100` mul as the seed before the trailing
        // `+ (int)fl.val`).
        let head_bf = self.resolve_bitfield(cur);
        // Order-reversal heuristic: when the head is a SIGNED
        // bitfield and the chain's lone tail term is an UNSIGNED
        // bitfield with a commutative op, BCC computes the
        // UNSIGNED side first into AX, spills it via push, then
        // the SIGNED side into AX, pops the saved value into DX
        // and folds. The result is identical to the natural-order
        // emission, but the byte sequence matches the captured
        // fixture (2300). Today we only fire this for `Add` —
        // other commutative ops haven't been fixture-tested.
        if let (Some(head), 1) = (head_bf.as_ref(), chain.len())
            && head.signed
            && !chain[0].1.signed
            && matches!(chain[0].0, BinOp::Add)
        {
            let (op, bf) = chain.into_iter().next().expect("len==1");
            self.emit_bitfield_read_to_reg(&bf, "ax", "al");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.emit_bitfield_read_to_reg(head, "ax", "al");
            self.out.extend_from_slice(b"\tpop\tdx\r\n");
            let mnem = match op {
                BinOp::Add => "add",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tax,dx\r\n");
            return Some(());
        }
        if let Some(head_bf) = head_bf {
            self.emit_bitfield_read_to_reg(&head_bf, "ax", "al");
        } else {
            self.emit_expr_to_ax(cur);
        }
        for (op, bf) in chain.into_iter().rev() {
            self.emit_bitfield_read_to_reg(&bf, "dx", "dl");
            let mnem = match op {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::BitAnd => "and",
                BinOp::BitOr => "or",
                BinOp::BitXor => "xor",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\t{mnem}\tax,dx\r\n");
        }
        Some(())
    }
}
