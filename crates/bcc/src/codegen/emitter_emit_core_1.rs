use super::*;

impl<'a> super::FunctionEmitter<'a> {
    pub(crate) fn new(
        out: &'a mut Vec<u8>,
        source: &'a str,
        function: &'a Function,
        func_idx: u32,
        signatures: &'a Signatures,
        globals: &'a GlobalTable,
        strings: &'a mut StringPool,
        helpers: &'a mut std::collections::HashSet<String>,
    ) -> Self {
        Self::new_with_opts(
            out, source, function, func_idx, signatures, globals, strings, helpers,
            false,
        )
    }
    pub(crate) fn new_with_opts(
        out: &'a mut Vec<u8>,
        source: &'a str,
        function: &'a Function,
        func_idx: u32,
        signatures: &'a Signatures,
        globals: &'a GlobalTable,
        strings: &'a mut StringPool,
        helpers: &'a mut std::collections::HashSet<String>,
        no_reg_vars: bool,
    ) -> Self {
        let tmp_bytes = compute_struct_call_tmp_bytes(function, signatures);
        Self {
            out,
            source,
            function,
            func_idx,
            lines: LineMap::new(source),
            current_line: 0,
            locals: Locals::analyze_with_opts(function, globals, no_reg_vars),
            label_plan: LabelPlan::build(function),
            signatures,
            globals,
            strings,
            loop_stack: Vec::new(),
            post_function_data: Vec::new(),
            helpers,
            skip_widen: false,
            skip_mod_to_ax: false,
            pending_fpu_store_fwait: false,
            pending_post_update: None,
            in_arg_expr: false,
            target_186: false,
            stack_check: false,
            model_has_far_code: false,
            model_is_huge: false,
            cmp_swapped: false,
            struct_call_tmp_bytes: tmp_bytes,
            pending_hidden_ret_ptr_tmp_off: None,
        }
    }
    /// `[bp+off]` for the top of the struct-call tmp slot (the slot's
    /// lowest address — analogous to a stack local's offset). Pre-
    /// supposes `struct_call_tmp_bytes > 0`.
    pub(crate) fn struct_call_tmp_offset(&self) -> i16 {
        -(i16::try_from(self.locals.stack_bytes() + self.struct_call_tmp_bytes)
            .expect("frame fits in i16"))
    }
    pub(crate) fn exit_label_num(&self) -> u32 {
        LabelPlan::label_number(self.label_plan.exit_slot())
    }
    pub(crate) fn emit_label(&mut self, slot: u32) {
        let n = LabelPlan::label_number(slot);
        let _ = write!(self.out, "@{}@{n}:\r\n", self.func_idx);
    }
    pub(crate) fn label_ref(&self, slot: u32) -> String {
        format!("@{}@{}", self.func_idx, LabelPlan::label_number(slot))
    }
    pub(crate) fn run(&mut self) {
        // Header line: emit `;` comment block for the line where the
        // function definition starts, before the prologue.
        let head_line = self.lines.line_of(self.function.span.start);
        self.advance_to_line(head_line);

        self.out.extend_from_slice(b"\tassume\tcs:_TEXT\r\n");
        let sym = if self.function.is_pascal {
            function_symbol_pascal(&self.function.name)
        } else {
            function_symbol(&self.function.name)
        };
        let _ = write!(self.out, "{sym}\tproc\tnear\r\n");

        // Interrupt prologue: save all GP regs + ES/DS + SI/DI + BP
        // BEFORE the standard frame setup, then load DS = DGROUP so
        // data references inside the body resolve correctly. Fixture
        // 1655.
        if self.function.is_interrupt {
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tbx\r\n");
            self.out.extend_from_slice(b"\tpush\tcx\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tes\r\n");
            self.out.extend_from_slice(b"\tpush\tds\r\n");
            self.out.extend_from_slice(b"\tpush\tsi\r\n");
            self.out.extend_from_slice(b"\tpush\tdi\r\n");
            self.out.extend_from_slice(b"\tpush\tbp\r\n");
            self.out.extend_from_slice(b"\tmov\tbp,DGROUP\r\n");
            self.out.extend_from_slice(b"\tmov\tds,bp\r\n");
            self.out.extend_from_slice(b"\tmov\tbp,sp\r\n");
            // Body.
            for stmt in self.function.body.as_deref().unwrap_or(&[]) {
                self.emit_stmt(stmt);
            }
            let body = self.function.body.as_deref().unwrap_or(&[]);
            if body_has_return(body) {
                self.emit_label(self.label_plan.exit_slot());
            }
            let close_offset = self.function.span.end.saturating_sub(1);
            let close_line = self.lines.line_of(close_offset);
            self.advance_to_line(close_line);
            // Epilogue: reverse the saves and `iret`.
            self.out.extend_from_slice(b"\tpop\tbp\r\n");
            self.out.extend_from_slice(b"\tpop\tdi\r\n");
            self.out.extend_from_slice(b"\tpop\tsi\r\n");
            self.out.extend_from_slice(b"\tpop\tds\r\n");
            self.out.extend_from_slice(b"\tpop\tes\r\n");
            self.out.extend_from_slice(b"\tpop\tdx\r\n");
            self.out.extend_from_slice(b"\tpop\tcx\r\n");
            self.out.extend_from_slice(b"\tpop\tbx\r\n");
            self.out.extend_from_slice(b"\tpop\tax\r\n");
            self.out.extend_from_slice(b"\tiret\t\r\n");
            let _ = write!(self.out, "{sym}\tendp\r\n");
            self.out.extend_from_slice(&self.post_function_data);
            return;
        }

        // Prologue. Order: push bp / mov bp,sp / allocate stack /
        // push callee-saved registers (in order). See
        // specs/bcc/ASM_OUTPUT.md "Prologue and epilogue shape".
        //
        // With -1/-2 (186+ target), the prologue collapses into a
        // single `enter N, 0` instruction (C8 lo hi 00). Fixture
        // 2134/2277.
        let stack_n = self.locals.stack_bytes() + self.struct_call_tmp_bytes;
        if self.target_186 && stack_n > 0 {
            let _ = write!(self.out, "\tenter\t{stack_n},0\r\n");
        } else {
            self.out.extend_from_slice(b"\tpush\tbp\r\n");
            self.out.extend_from_slice(b"\tmov\tbp,sp\r\n");
            match stack_n {
                0 => {}
                n @ 1..=2 => {
                    for _ in 0..n {
                        self.out.extend_from_slice(b"\tdec\tsp\r\n");
                    }
                }
                n => {
                    let _ = write!(self.out, "\tsub\tsp,{n}\r\n");
                }
            }
        }
        // Callee-saved register pushes come *before* the huge-model
        // DS reload — BCC's frame shape is `push bp / mov bp, sp /
        // push si / push di / push ds / mov ax, seg HELLO_DATA /
        // mov ds, ax`. Under huge the pop order at the epilogue is
        // the exact mirror (`pop ds; pop di; pop si; pop bp; retf`),
        // so the SI / DI offsets relative to BP stay consistent
        // with the rest of the frame. Fixtures 1770, 2057, 3711.
        for reg in self.locals.saved_regs() {
            let _ = write!(self.out, "\tpush\t{}\r\n", reg.name());
        }
        // Huge model: each module has its own data segment, so the
        // function reloads DS from `seg HELLO_DATA` on entry (the
        // caller's DS is unknown — under huge, every callsite
        // independently sets its own data frame). Fixtures 1770,
        // 2057.
        if self.model_is_huge {
            self.out.extend_from_slice(b"\tpush\tds\r\n");
            self.out.extend_from_slice(b"\tmov\tax,seg HELLO_DATA\r\n");
            self.out.extend_from_slice(b"\tmov\tds,ax\r\n");
        }
        // `-N` stack-overflow check, emitted AFTER the callee-saved
        // pushes but BEFORE the param register loads. Compares the
        // current `sp` against the global `___brklvl` (the runtime's
        // stack-break sentinel). If brklvl < sp (still have
        // headroom), skip the helper; otherwise call `N_OVERFLOW@`.
        // Fixtures 2129, 2261.
        if self.stack_check {
            let skip_label = format!("@{}@brk", self.func_idx);
            self.out.extend_from_slice(b"\tcmp\tword ptr DGROUP:___brklvl,sp\r\n");
            let _ = write!(self.out, "\tjb\tshort {skip_label}\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_OVERFLOW@\r\n");
            let _ = write!(self.out, "{skip_label}:\r\n");
            self.helpers.insert("N_OVERFLOW@".to_string());
            self.helpers.insert("___brklvl".to_string());
        }
        // Register-promoted incoming parameters: copy each from its
        // caller-built stack slot into its assigned register. Byte
        // registers (char params) load from `byte ptr` — the caller
        // pushes a full word but only the low byte is meaningful for
        // a char arg (fixture 052).
        let param_loads: Vec<ParamLoad> = self.locals.param_loads().to_vec();
        for pl in &param_loads {
            let width = if pl.reg.is_byte() { "byte" } else { "word" };
            let _ = write!(
                self.out,
                "\tmov\t{},{width} ptr [bp+{}]\r\n",
                pl.reg.name(),
                pl.incoming_offset,
            );
        }

        // Body.
        for stmt in self.function.body.as_deref().unwrap_or(&[]) {
            self.emit_stmt(stmt);
        }

        // Single exit label — only emit when the body actually
        // jumps to it (i.e. has at least one explicit return).
        // Void functions that fall off the end have no return
        // statement, so the label would be dead. BCC omits it.
        // Fixture 3575 (`void init() { arr[0] = 0x55; }`).
        let body = self.function.body.as_deref().unwrap_or(&[]);
        if body_has_return(body) {
            self.emit_label(self.label_plan.exit_slot());
        }

        // Closing-brace line gets its own comment block. Span end is the
        // byte just past `}`, so back up by one to get the brace itself.
        let close_offset = self.function.span.end.saturating_sub(1);
        let close_line = self.lines.line_of(close_offset);
        self.advance_to_line(close_line);

        // Epilogue: reverse of the prologue. Under huge model the
        // prologue pushed `si / di / ds`; mirroring gives `pop ds`
        // first, then the callee-saved restores. Fixtures 1770,
        // 2057, 3711.
        if self.model_is_huge {
            self.out.extend_from_slice(b"\tpop\tds\r\n");
        }
        let saved: Vec<Reg> = self.locals.saved_regs().to_vec();
        for reg in saved.iter().rev() {
            let _ = write!(self.out, "\tpop\t{}\r\n", reg.name());
        }
        // 186+ target: `leave` (C9) collapses `mov sp, bp; pop bp`
        // into one byte. Used whenever the prologue used `enter`.
        // Fixture 2134/2277.
        let frame_bytes = self.locals.stack_bytes() + self.struct_call_tmp_bytes;
        if self.target_186 && frame_bytes > 0 {
            self.out.extend_from_slice(b"\tleave\t\r\n");
        } else {
            if frame_bytes > 0 {
                self.out.extend_from_slice(b"\tmov\tsp,bp\r\n");
            }
            self.out.extend_from_slice(b"\tpop\tbp\r\n");
        }
        // Pascal-convention callee cleans up the args off the stack
        // via `ret N` where N = total bytes of parameter storage.
        // Fixture 1653. Far functions use `retf` (`cb`) instead of
        // `ret`; fixture 1654.
        if self.function.is_pascal {
            let n: u32 = self
                .function
                .params
                .iter()
                .map(|p| u32::from(p.ty.size_bytes().max(2)))
                .sum();
            if self.function.is_far {
                let _ = write!(self.out, "\tretf\t{n}\r\n");
            } else {
                let _ = write!(self.out, "\tret\t{n}\r\n");
            }
        } else if self.function.is_far {
            self.out.extend_from_slice(b"\tretf\t\r\n");
        } else {
            self.out.extend_from_slice(b"\tret\t\r\n");
        }

        let _ = write!(self.out, "{sym}\tendp\r\n");
        // Switch jump-tables and linear-search address tables live
        // between `_main endp` and the next `?debug C E9` line. They
        // were staged into `post_function_data` while the body was
        // emitted (see `emit_switch_jump_table` / `_linear_search`).
        self.out.extend_from_slice(&self.post_function_data);
    }
    pub(crate) fn flush_pending_post_update(&mut self) {
        if let Some((reg, stride, mnem)) = self.pending_post_update.take() {
            for _ in 0..stride {
                let _ = write!(self.out, "\t{mnem}\t{reg}\r\n");
            }
        }
    }
    /// Emit just the increment/decrement on the named local — no
    /// load-to-AX. Used by `ExprStmt` and by the "first half" of
    /// pre-form Update in expression position.
    ///
    /// Int register: direct `inc/dec <reg>` (fixture 040).
    /// Char register: round-trip through AL — `mov al, <reg> /
    /// inc/dec al / mov <reg>, al` (fixture 047). BCC does not use
    /// `inc/dec <byte-reg>` directly.
    pub(crate) fn emit_update_in_place(&mut self, name: &str, op: UpdateOp, position: UpdatePosition) {
        // Local-shadowing-global: if there's a local of this name,
        // the global path doesn't fire — a static-local hoisted from
        // a sibling function may share the same identifier with a
        // regular local here. Fixture 2330 (`au()`'s `counter` local
        // shadows `sm()`'s hoisted static `counter`).
        let has_local = self.locals.has(name);
        // Long globals (`g++` / `g--`) use a memory-direct
        // add/adc pair (or sub/sbb for `--`). Acts on memory
        // without loading into registers. Fixture 249 (`g++`).
        if !has_local
            && let Some(ty) = self.globals.type_of(name)
            && ty.is_long_like()
        {
            let (lo_op, hi_op) = match op {
                UpdateOp::Inc => ("add", "adc"),
                UpdateOp::Dec => ("sub", "sbb"),
            };
            let _ = write!(self.out, "\t{lo_op}\tword ptr DGROUP:_{name},1\r\n");
            let _ = write!(self.out, "\t{hi_op}\tword ptr DGROUP:_{name}+2,0\r\n");
            return;
        }
        let mnemonic = match op {
            UpdateOp::Inc => "inc",
            UpdateOp::Dec => "dec",
        };
        // Int/char globals: memory-direct `inc word ptr DGROUP:_g`
        // (or `dec`). Fixture 512 (`g++; g++; return g;`).
        // Global pointers scale by sizeof(pointee) — `++p` on `int
        // *p` adds 2, lowering to `add word ptr [_p], 2` rather
        // than `inc`. Fixture 561.
        if !has_local
            && let Some(gty) = self.globals.type_of(name) {
            if let Some(pointee) = gty.pointee() {
                let stride = u32::from(pointee.size_bytes());
                if stride != 1 {
                    let arith = match op {
                        UpdateOp::Inc => "add",
                        UpdateOp::Dec => "sub",
                    };
                    let _ = write!(
                        self.out,
                        "\t{arith}\tword ptr DGROUP:_{name},{stride}\r\n",
                    );
                    return;
                }
            }
            // Byte globals split on pre vs post:
            //  - Pre (`++g`): AL load-modify-store — `mov al, _g;
            //    inc al; mov _g, al`. BCC keeps the new value in AL
            //    even when the expression is discarded. Fixture
            //    700.
            //  - Post (`g++`) when discarded: memory-direct
            //    `inc byte ptr _g`. BCC notices the old value
            //    isn't materialized. Fixture 702.
            //
            // (The post-not-discarded case lands at the
            // expression-context update path, not here.)
            if gty.is_char_like() {
                if matches!(position, UpdatePosition::Pre) {
                    let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{name}\r\n");
                    let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                    let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{name},al\r\n");
                } else {
                    let _ = write!(self.out, "\t{mnemonic}\tbyte ptr DGROUP:_{name}\r\n");
                }
                return;
            }
            let _ = write!(self.out, "\t{mnemonic}\tword ptr DGROUP:_{name}\r\n");
            return;
        }
        // Pointer increment / decrement uses the pointee's size as
        // stride. For `int *p`, `p++` becomes `inc reg / inc reg`
        // (the +2 peephole — 2 bytes vs. 3 for `add reg, 2`),
        // matching fixture 090. For `char *s`, `s++` is a single
        // `inc reg` (stride 1), fixture 093.
        let stride = self
            .locals
            .type_of(name)
            .pointee()
            .map_or(1, |p| u32::from(p.size_bytes()));
        match self.locals.location_of(name) {
            LocalLocation::Reg(reg) if reg.is_byte() => {
                // Pre vs post matters for byte-register stmt-position
                // updates even when the value is discarded:
                //  - Pre (`++c;`): BCC stages through AL — `mov al,
                //    <reg>; inc al; mov <reg>, al`. Fixture 047,
                //    050–054, etc.
                //  - Post (`c++;`): direct `inc <reg>` / `dec <reg>`.
                //    The byte-register form is preferred without the
                //    AL detour. Fixture 1056.
                if matches!(position, UpdatePosition::Pre) {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                    let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                    let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
                } else {
                    let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                }
            }
            LocalLocation::Reg(reg) => {
                // Pointer stride peephole: K=1 → `inc <reg>` (1 byte);
                // K=2 → two `inc`s (2 bytes); K≥3 → `add <reg>, K`
                // (3 bytes — same as the int compound ±K peephole).
                // Stride 4 (long pointer) crosses the threshold: 4
                // incs cost 4 bytes vs `add reg, 4` at 3. Fixture 313.
                let add_mnem = match op {
                    UpdateOp::Inc => "add",
                    UpdateOp::Dec => "sub",
                };
                if stride <= 2 {
                    for _ in 0..stride {
                        let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                    }
                } else {
                    let _ = write!(self.out, "\t{add_mnem}\t{},{stride}\r\n", reg.name());
                }
            }
            LocalLocation::Stack(off) => {
                let ty = self.locals.type_of(name).clone();
                // Long stack-local ++/-- — memory-direct add/adc 1 (or
                // sub/sbb 1) on the two halves. Identical to the
                // `x += 1` compound shape (fixtures 290, 291). Pre and
                // post are byte-identical when the value is discarded.
                if ty.is_long_like() {
                    let (lo_op, hi_op) = match op {
                        UpdateOp::Inc => ("add", "adc"),
                        UpdateOp::Dec => ("sub", "sbb"),
                    };
                    let _ = write!(self.out, "\t{lo_op}\tword ptr {},1\r\n", bp_addr(off));
                    let _ = write!(self.out, "\t{hi_op}\tword ptr {},0\r\n", bp_addr(off + 2));
                    return;
                }
                // Stack-resident ++/-- on an int: memory-direct
                // inc/dec word. Fixture 2263 (`-r-` keeps every int
                // on the stack, including loop counters that would
                // otherwise enregister).
                if ty.is_int_like() {
                    let _ = write!(
                        self.out,
                        "\t{mnemonic}\tword ptr {}\r\n",
                        bp_addr(off),
                    );
                    return;
                }
                // Far-pointer ++/-- — the offset half (low word at
                // [bp+off]) bumps by the pointee's stride; the
                // segment half is untouched. BCC's plain-`far`
                // arithmetic never normalizes, so even stride-bumps
                // that would cross a segment boundary stay in the
                // offset word. Fixture 1651 (`p++` for `int far *p`
                // after `p = (int far *)a`). The `huge`-qualified
                // sibling fixtures (1771, 1774) need a normalization
                // helper and are handled in a later slice.
                if let Type::FarPointer { pointee, is_huge: false } = &ty {
                    let stride = pointee.size_bytes();
                    if let Ok(stride_i8) = i8::try_from(stride) {
                        let signed = match op {
                            UpdateOp::Inc => stride_i8,
                            UpdateOp::Dec => -stride_i8,
                        };
                        let _ = write!(
                            self.out,
                            "\tadd\tword ptr {},{signed}\r\n",
                            bp_addr(off),
                        );
                    } else {
                        let signed_full = match op {
                            UpdateOp::Inc => i32::from(stride),
                            UpdateOp::Dec => -i32::from(stride),
                        };
                        let _ = write!(
                            self.out,
                            "\tadd\tword ptr {},{signed_full}\r\n",
                            bp_addr(off),
                        );
                    }
                    return;
                }
                // Huge-pointer ++ / -- needs runtime normalization so
                // an offset overflow walks into the next segment.
                // BCC calls one of two helpers in place:
                //   `N_PADA@` for `++`, `N_PSBA@` for `--`. The ABI:
                //     DX:AX = far pointer TO the local being updated
                //             (DX = SS for a stack-resident huge*,
                //              AX = lea of the local slot)
                //     CX:BX = long delta (high:low) of the stride
                //   The helper rewrites the 4-byte slot in place.
                // Fixtures 1771 (`p++` for `int huge *p`) and 1774
                // (`p--`).
                if let Type::FarPointer { pointee, is_huge: true } = &ty {
                    let stride = pointee.size_bytes();
                    let helper = match op {
                        UpdateOp::Inc => "N_PADA@",
                        UpdateOp::Dec => "N_PSBA@",
                    };
                    self.out.extend_from_slice(b"\txor\tcx,cx\r\n");
                    let _ = write!(self.out, "\tmov\tbx,{stride}\r\n");
                    self.out.extend_from_slice(b"\tmov\tdx,ss\r\n");
                    let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(off));
                    let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                    self.helpers.insert(helper.to_string());
                    return;
                }
                // Stack-resident ++/-- on a char uses the AL round-trip
                // (fixture 055).
                assert!(
                    ty.is_char_like(),
                    "++/-- on a stack-resident {:?} not yet supported", ty,
                );
                let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(off));
                let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                let _ = write!(self.out, "\tmov\tbyte ptr {},al\r\n", bp_addr(off));
            }
        }
    }
    pub(crate) fn advance_to_stmt_line(&mut self, stmt: &Stmt) {
        let line = self.lines.line_of(stmt.span.start);
        self.advance_to_line(line);
    }
    /// `for`'s header source-line is the `for` keyword's line. The
    /// init expression doesn't have its own statement span, so we
    /// advance the comment cursor manually using the for's span.
    pub(crate) fn advance_to_for_header_line(&mut self, for_span_start: u32) {
        let line = self.lines.line_of(for_span_start);
        self.advance_to_line(line);
    }
    /// Like `advance_to_stmt_line(stmt)`, but called with just the
    /// span start when the caller doesn't have the full `Stmt`.
    pub(crate) fn advance_to_stmt_line_at(&mut self, span_start: u32) {
        let line = self.lines.line_of(span_start);
        self.advance_to_line(line);
    }
    /// Walk `line` token-by-identifier, replacing each C-variable
    /// reference (stack local → `word ptr [bp-N]`, global →
    /// `word ptr DGROUP:_<name>`, register-resident local → the
    /// register's mnemonic) with its asm-side equivalent.
    /// Identifiers that don't name something in scope (including
    /// asm mnemonics like `mov`, asm registers like `ax`, and the
    /// `_AX` / `_BX` / `_CX` / `_DX` pseudo-registers) pass
    /// through unchanged.
    pub(crate) fn translate_asm_line(&self, line: &str) -> String {
        let bytes = line.as_bytes();
        let mut out = String::with_capacity(line.len());
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            let starts_ident = b.is_ascii_alphabetic() || b == b'_';
            if starts_ident {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                let word = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
                if let Some(repl) = self.substitute_asm_ident(word) {
                    out.push_str(&repl);
                } else {
                    out.push_str(word);
                }
            } else {
                out.push(b as char);
                i += 1;
            }
        }
        out
    }
    /// Resolve one identifier seen inside an asm body. Returns the
    /// substitution string (e.g. `word ptr [bp-2]`), or `None` if
    /// the identifier should pass through unchanged.
    pub(crate) fn substitute_asm_ident(&self, name: &str) -> Option<String> {
        if self.locals.has(name) {
            match self.locals.location_of(name) {
                LocalLocation::Stack(off) => {
                    return Some(format!("word ptr {}", bp_addr(off)));
                }
                LocalLocation::Reg(reg) => {
                    return Some(reg.name().to_owned());
                }
            }
        }
        if self.globals.type_of(name).is_some() {
            return Some(format!("word ptr DGROUP:_{name}"));
        }
        None
    }
    /// Build the asm text that loads a long-typed arm into DX:AX,
    /// in the order BCC emits per-arm of a long-returning ternary.
    /// Returns `None` for arms we don't know how to load. Const-zero
    /// hi prefers `xor dx, dx` (1 byte saved vs `mov dx, 0`).
    pub(crate) fn long_arm_load(&self, e: &Expr) -> Option<String> {
        if let Some(k) = try_const_eval(e) {
            let hi = (k >> 16) & 0xFFFF;
            let lo = k & 0xFFFF;
            let hi_text = if hi == 0 {
                "\txor\tdx,dx\r\n".to_owned()
            } else {
                format!("\tmov\tdx,{hi}\r\n")
            };
            return Some(format!("{hi_text}\tmov\tax,{lo}\r\n"));
        }
        if let Some((hi, lo)) = self.long_lvalue_addr_pair(e) {
            return Some(format!(
                "\tmov\tdx,word ptr {hi}\r\n\tmov\tax,word ptr {lo}\r\n"
            ));
        }
        None
    }
    /// True iff `e` is an unsigned int-typed expression. Used to
    /// pick zero-extend (`xor dx,dx`) vs sign-extend (`cwd`) when
    /// widening to long. Best-effort: bare ulong/uint idents, casts.
    pub(crate) fn expr_int_is_unsigned(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Ident(name) => {
                if let Some(gt) = self.globals.type_of(name) {
                    return gt.is_unsigned();
                }
                self.locals.has(name) && self.locals.type_of(name).is_unsigned()
            }
            ExprKind::Cast { ty, .. } => ty.is_unsigned(),
            _ => false,
        }
    }
    /// True when the given RHS expression can't be reduced to an
    /// OperandSource by `resolve_operand_source` and instead needs to
    /// be evaluated into AX first. Used by the int-reg compound-
    /// assign fallback to decide between the direct `<op> <reg>,
    /// <src>` shape and the AX-route shape.
    pub(crate) fn value_needs_ax_route(&self, e: &Expr) -> bool {
        match &e.kind {
            // Nested binary expressions don't have a single-operand
            // representation.
            ExprKind::BinOp { .. } => true,
            // `<reg-ptr>-><field>` resolves to `[<reg>+disp]` — a
            // single memory operand, no AX route needed. Tasm now
            // supports `<op> <reg16>, [<si|di>+disp]`. Fixture 3343
            // (`s += p->v` with p in SI).
            ExprKind::Member {
                base,
                kind: crate::ast::MemberKind::Arrow,
                ..
            } if matches!(&base.kind, ExprKind::Ident(n)
                if self.locals.has(n)
                    && matches!(
                        self.locals.location_of(n),
                        LocalLocation::Reg(r) if matches!(r, Reg::Si | Reg::Di))) => false,
            // Variable-indexed arrays / chained members: resolvable
            // only when the chain folds to a constant offset. Use
            // try_lvalue_chain_addr's success as the predicate.
            ExprKind::ArrayIndex { .. } | ExprKind::Member { .. } => {
                self.try_lvalue_chain_addr(e).is_none()
            }
            // Casts (e.g. `(char)b`), ternaries, and calls all need
            // evaluation that produces a value in AX. They don't have
            // a single memory/register operand representation.
            // Fixture 1288 (`a += (char)b`).
            ExprKind::Cast { .. }
            | ExprKind::Ternary { .. }
            | ExprKind::Call { .. }
            | ExprKind::CallVia { .. } => true,
            // Comma expressions need their side effects emitted in
            // order, then the final expr's value lands in AX.
            // Fixture 1345 (`a += (b = 3, b + 1)`).
            ExprKind::Comma { .. } => true,
            // Pre/post-update in an rvalue position: the value goes
            // through AX, not through a single operand. Fixture
            // 1347 (`a += b++`), 1348 (`a += ++b`).
            ExprKind::Update { .. } => true,
            // `*p++` / `*--p` — the inner update needs the post/pre
            // sequencing that emit_deref_to_ax materializes through
            // AX, so route the whole thing through the AX path
            // rather than panicking in resolve_operand_source.
            // Fixture 2000.
            ExprKind::Deref(inner)
                if matches!(inner.kind, ExprKind::Update { .. }) => true,
            _ => false,
        }
    }
    /// Find a struct definition with the given tag by scanning both
    /// globals and locals. Globals' types come from `GlobalTable`;
    /// locals' types live in `Locals`. Returns the first complete
    /// (non-placeholder) struct match. Used to resolve fields off a
    /// `Type::Pointer(Struct{name-only, fields:[]})` placeholder that
    /// the AST stores when a recursive struct type would otherwise
    /// require a cycle.
    pub(crate) fn lookup_struct_by_tag(&self, tag: &str) -> Option<Type> {
        fn find<'a>(ty: &'a Type, tag: &str) -> Option<&'a Type> {
            match ty {
                Type::Struct { name: Some(t), fields, .. }
                    if t == tag && !fields.is_empty() =>
                {
                    Some(ty)
                }
                Type::Struct { fields, .. } => fields.iter().find_map(|f| find(&f.ty, tag)),
                Type::Array { elem, .. } => find(elem, tag),
                Type::Pointer(inner) => find(inner, tag),
                _ => None,
            }
        }
        if let Some(g) = self.globals.lookup_struct_by_tag(tag) {
            return Some(g.clone());
        }
        for (_, ty) in self.locals.iter_types() {
            if let Some(found) = find(ty, tag) {
                return Some(found.clone());
            }
        }
        None
    }
    /// True if `e` is a bare ident referring to an int-typed stack
    /// local or global, returning an OperandSource that names the
    /// memory operand. Used by the rhs-clobbers-AX commutative-op
    /// fallback to skip the push/pop dance: evaluate RHS into AX,
    /// then `<op> ax, <mem>` directly on the LHS memory.
    /// Return an asm source for `mov dx, <src>` if `e` can be loaded
    /// into DX in a single instruction without clobbering AX. Covers
    /// int-typed reg locals, int memory lvalues, and int constants.
    /// Used by the RHS-direct-to-DX peephole (fixture 1499).
    pub(crate) fn try_dx_load_source(&self, e: &Expr) -> Option<String> {
        if let Some(k) = try_const_eval(e) {
            return Some(format!("{}", k & 0xFFFF));
        }
        if let ExprKind::Ident(name) = &e.kind {
            if self.locals.has(name)
                && self.locals.type_of(name).is_int_like()
                && let LocalLocation::Reg(reg) = self.locals.location_of(name)
                && !reg.is_byte()
            {
                return Some(reg.name().to_owned());
            }
            if let Some(gty) = self.globals.type_of(name)
                && gty.is_int_like()
            {
                return Some(format!("word ptr DGROUP:_{name}"));
            }
            if self.locals.has(name)
                && self.locals.type_of(name).is_int_like()
                && let LocalLocation::Stack(off) = self.locals.location_of(name)
            {
                return Some(format!("word ptr {}", bp_addr(off)));
            }
        }
        None
    }
    /// Return an asm source for a binary-op second operand
    /// (`<op> dx, <src>`) when `e` can serve as an in-place source
    /// (reg-resident int local, int memory lvalue, or int constant).
    /// Used by the RHS-direct-to-DX peephole (fixture 1499).
    pub(crate) fn try_op_source(&self, e: &Expr) -> Option<String> {
        if let Some(k) = try_const_eval(e) {
            return Some(format!("{}", k & 0xFFFF));
        }
        if let ExprKind::Ident(name) = &e.kind {
            if self.locals.has(name)
                && self.locals.type_of(name).is_int_like()
                && let LocalLocation::Reg(reg) = self.locals.location_of(name)
                && !reg.is_byte()
            {
                return Some(reg.name().to_owned());
            }
            if let Some(gty) = self.globals.type_of(name)
                && gty.is_int_like()
            {
                return Some(format!("word ptr DGROUP:_{name}"));
            }
            if self.locals.has(name)
                && self.locals.type_of(name).is_int_like()
                && let LocalLocation::Stack(off) = self.locals.location_of(name)
            {
                return Some(format!("word ptr {}", bp_addr(off)));
            }
        }
        None
    }
    pub(crate) fn try_memory_source(&self, e: &Expr) -> Option<OperandSource> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        if let Some(gty) = self.globals.type_of(name)
            && gty.is_int_like()
        {
            return Some(OperandSource::Global(name.clone()));
        }
        if self.locals.has(name)
            && self.locals.type_of(name).is_int_like()
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            return Some(OperandSource::Local(off));
        }
        None
    }
    /// Try to lower a non-constant long expression into a load/arith/
    /// store skeleton landing at `dest_hi`/`dest_lo`. Returns true
    /// when the value's shape was recognized and emitted; false if
    /// the caller should fall through to its own panic/path.
    ///
    /// Handles:
    /// - `<long-lvalue>` (plain copy): two loads + two stores.
    /// - `<long-lvalue> <op> <const>` for `+`/`-`: load lvalue,
    ///   add/sub imm to DX, adc/sbb 0/-1 to AX, store.
    /// - `<long-lvalue> <op> <long-lvalue>` for `+`/`-`/`&`/`|`/`^`:
    ///   load operand a, op against operand b's halves, store.
    pub(crate) fn try_emit_long_value_to_dest(
        &mut self,
        value: &Expr,
        dest_hi: &str,
        dest_lo: &str,
    ) -> bool {
        // Algebraic identity simplification — BCC's frontend folds these before
        // codegen, so a long value combining an lvalue with an op's identity
        // element collapses to a plain copy (and `* 0` to a zero store) rather
        // than emitting the full long arithmetic. Confirmed against the oracle:
        // `a+0`, `a-0`, `a*1`, `a/1`, `a|0`, `a^0` and the commutative `0+a` /
        // `1*a` fold to a copy; `a*0` / `0*a` to zero. NOT folded (left to their
        // own paths): shifts `a<<0` / `a>>0` (keep the shift shape) and a
        // const-on-LEFT bitwise `0|a` / `0^a`. Fixture 4186 (`long r = a + 0L`).
        if let ExprKind::BinOp { op, left, right } = &value.kind {
            let identity_copy: Option<&Expr> = if try_const_eval(right) == Some(0)
                && matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitOr | BinOp::BitXor)
            {
                Some(left)
            } else if try_const_eval(right) == Some(1)
                && matches!(op, BinOp::Mul | BinOp::Div)
            {
                Some(left)
            } else if try_const_eval(left) == Some(0) && matches!(op, BinOp::Add) {
                Some(right)
            } else if try_const_eval(left) == Some(1) && matches!(op, BinOp::Mul) {
                Some(right)
            } else {
                None
            };
            if let Some(inner) = identity_copy {
                return self.try_emit_long_value_to_dest(inner, dest_hi, dest_lo);
            }
            if matches!(op, BinOp::Mul)
                && (try_const_eval(right) == Some(0) || try_const_eval(left) == Some(0))
            {
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},0\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},0\r\n");
                return true;
            }
        }
        // `<dest> = -<long-lvalue>` — neg ax / neg dx / sbb ax, 0
        // shape. Mirrors the assign path (fixture 331).
        if let ExprKind::Unary { op: UnaryOp::Neg, operand } = &value.kind
            && let Some((src_hi, src_lo)) = self.long_lvalue_addr_pair(operand)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {src_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {src_lo}\r\n");
            self.out.extend_from_slice(b"\tneg\tax\r\n");
            self.out.extend_from_slice(b"\tneg\tdx\r\n");
            self.out.extend_from_slice(b"\tsbb\tax,0\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            return true;
        }
        // `<dest> = ~<long-lvalue>` — not dx / not ax. BCC's
        // observed order does NOT precede with neg-style propagation.
        // Fixture 2186 (`long r = ~a`).
        if let ExprKind::Unary { op: UnaryOp::BitNot, operand } = &value.kind
            && let Some((src_hi, src_lo)) = self.long_lvalue_addr_pair(operand)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {src_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {src_lo}\r\n");
            self.out.extend_from_slice(b"\tnot\tdx\r\n");
            self.out.extend_from_slice(b"\tnot\tax\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            return true;
        }
        // `<dest> = <int-expr> + <long-lvalue>` (or sibling for
        // sub/and/or/xor): widen the int to DX:AX via `cwd` (or
        // `xor dx,dx` for unsigned), then `<lo_op> ax, [b_lo]; <hi_op>
        // dx, [b_hi]`. AX/DX order is swapped from the value path
        // because cwd places the high half in DX. Fixture 1643
        // (`i + b` for int i + long b).
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && let Some((lo_op, hi_op)) = long_pair_op(*op)
        {
            // long-on-LEFT, int-on-RIGHT, commutative op: BCC loads
            // the long into BX:CX, widens the int into DX:AX, then
            // `<lo_op> cx, ax; <hi_op> bx, dx` (memory dest gets the
            // BX:CX result). Distinct shape from the int-on-left
            // path which adds memory-direct against AX/DX. Fixture
            // 2191 (`l + i` long-left + int-right, stack dest).
            if let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(left)
                && !self.expr_is_long_like(right)
                && try_const_eval(right).is_none()
                && matches!(op, BinOp::Add | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor)
            {
                let unsigned = self.expr_int_is_unsigned(right);
                self.emit_expr_to_ax(right);
                if unsigned {
                    self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                }
                let _ = write!(self.out, "\tmov\tbx,word ptr {b_hi}\r\n");
                let _ = write!(self.out, "\tmov\tcx,word ptr {b_lo}\r\n");
                let _ = write!(self.out, "\t{lo_op}\tcx,ax\r\n");
                let _ = write!(self.out, "\t{hi_op}\tbx,dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},bx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},cx\r\n");
                return true;
            }
            // int-on-LEFT, long-on-RIGHT: widen the int via cwd /
            // xor dx,dx, then `<lo_op> ax, [b_lo]; <hi_op> dx, [b_hi]`
            // memory-direct. AX/DX order swapped from the value path
            // because cwd places the high half in DX. Fixture 1643.
            if let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
                && !self.expr_is_long_like(left)
            {
                let unsigned = self.expr_int_is_unsigned(left);
                self.emit_expr_to_ax(left);
                if unsigned {
                    self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcwd\t\r\n");
                }
                let _ = write!(self.out, "\t{lo_op}\tax,word ptr {b_lo}\r\n");
                let _ = write!(self.out, "\t{hi_op}\tdx,word ptr {b_hi}\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
                return true;
            }
        }
        // Plain copy: `<dest> = <long-lvalue>`.
        if let Some((src_hi, src_lo)) = self.long_lvalue_addr_pair(value) {
            // Only treat as a copy when value itself is the lvalue
            // (not a sub-expression of a binop). We detect that by
            // re-checking — long_lvalue_addr_pair returns Some only
            // for lvalue-shaped exprs, so a top-level match here is
            // the lvalue itself.
            let _ = write!(self.out, "\tmov\tax,word ptr {src_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {src_lo}\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            return true;
        }
        // `<lvalue> <op> <const>` for arith ops.
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && (matches!(op, BinOp::Add) || matches!(op, BinOp::Sub))
            && let Some((src_hi, src_lo)) = self.long_lvalue_addr_pair(left)
            && let Some(k) = try_const_eval(right)
        {
            let signed = k as i32;
            let (delta, carry) = if matches!(op, BinOp::Add) {
                (signed, 0i16)
            } else {
                (-signed, -1i16)
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {src_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {src_lo}\r\n");
            if let Ok(delta_i8) = i8::try_from(delta) {
                let _ = write!(self.out, "\tadd\tdx,{delta_i8}\r\n");
            } else {
                let delta_u16 = (delta as i32) as u16;
                let _ = write!(self.out, "\tadd\tdx,{delta_u16}\r\n");
            }
            let _ = write!(self.out, "\tadc\tax,{carry}\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            return true;
        }
        // `<lvalue_a> <op> <lvalue_b>` for `+`/`-`/`&`/`|`/`^`.
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && let Some((lo_op, hi_op)) = long_pair_op(*op)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\t{lo_op}\tdx,word ptr {b_lo}\r\n");
            let _ = write!(self.out, "\t{hi_op}\tax,word ptr {b_hi}\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            return true;
        }
        // `<dest> = <a> / <K>` / `<a> % <K>` for long lvalue / long
        // const. Same helper as the lvalue-lvalue path but the
        // divisor's words are pushed as composed registers. Fixture
        // 1781 (`r = a % 4L`).
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && matches!(op, BinOp::Div | BinOp::Mod)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some(k) = try_const_eval(right)
        {
            let unsigned = self.expr_is_unsigned(left);
            let helper = match (op, unsigned) {
                (BinOp::Div, false) => "N_LDIV@",
                (BinOp::Mod, false) => "N_LMOD@",
                (BinOp::Div, true)  => "N_LUDIV@",
                (BinOp::Mod, true)  => "N_LUMOD@",
                _ => unreachable!(),
            };
            let lo_k = (k & 0xFFFF) as u16;
            let hi_k = ((k >> 16) & 0xFFFF) as u16;
            if hi_k == 0 {
                self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{hi_k}\r\n");
            }
            let _ = write!(self.out, "\tmov\tdx,{lo_k}\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            return true;
        }
        // `<dest> = <a> * <b>` for two long lvalues. Helper convention
        // is CX:BX (LHS) and DX:AX (RHS) → DX:AX. After the call, store
        // DX→dest_hi and AX→dest_lo. Mirrors the return-path shape (line
        // 3365) with the result captured into memory rather than left
        // as a return value. Fixture 1628.
        if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let _ = write!(self.out, "\tmov\tcx,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tbx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {b_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {b_lo}\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            return true;
        }
        // `<dest> = <a> / <b>` / `<a> % <b>` for two long lvalues. Helpers
        // (N_LDIV@/N_LMOD@/N_LUDIV@/N_LUMOD@) take 4 stack words: divisor
        // pushed first, then dividend — each high-first (= push lo, hi
        // in source order, since stack grows down). Result in DX:AX,
        // store to dest. Fixtures 1629 (signed div), 1633 (unsigned div).
        if let ExprKind::BinOp { op, left, right } = &value.kind
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
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            return true;
        }
        // `<dest> = (long)<int-lvalue>` — widen an int/uint source
        // into DX:AX (cwd for signed, xor dx,dx for unsigned), then
        // store. Same shape as the return-path widen (line 3551).
        // Fixtures 1638 (signed int cast), 1639 (unsigned int cast).
        let widening_src = match &value.kind {
            ExprKind::Cast { ty, operand } if ty.is_long_like() => {
                if let ExprKind::Ident(name) = &operand.kind { Some(name.as_str()) } else { None }
            }
            _ => None,
        };
        if let Some(src_name) = widening_src
            && let Some(addr) = self.int_lvalue_addr(&Expr {
                kind: ExprKind::Ident(src_name.to_owned()),
                span: value.span,
            })
        {
            let src_ty = if let Some(gty) = self.globals.type_of(src_name) {
                gty.clone()
            } else {
                self.locals.type_of(src_name).clone()
            };
            let _ = write!(self.out, "\tmov\tax,word ptr {addr}\r\n");
            if src_ty.is_unsigned() {
                // Destination-driven: write 0 directly to the high-
                // half memory slot instead of going through `xor dx,
                // dx`. Saves the DX clobber and matches BCC's actual
                // shape for fixture 1639.
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},0\r\n");
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
            }
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            return true;
        }
        // `<dest> = <a> << K` / `<a> >> K` for long lvalue and a
        // constant K in [1,255]. K=1 inlines `shl ax,1; rcl dx,1` or
        // the rcr shape for shr; K>1 routes through the N_LX*SH@
        // helpers. Mirrors the return-path shape (line 3599) with
        // DX:AX stored into the dest pair. Fixture 1640.
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some(k) = try_const_eval(right)
            && k >= 1
            && k <= 255
        {
            let unsigned = self.expr_is_unsigned(left);
            // K=1 single-step inline: BCC's shift convention here puts
            // AX=high, DX=low (opposite of the helper-call path) so
            // the carry chain matches: `shl dx, 1; rcl ax, 1` (low
            // first into high). Fixtures 1735, 1736, 1782, 1783.
            // The >1 helper path keeps DX=high, AX=low (helper's
            // calling convention).
            if k == 1 {
                let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
                if matches!(op, BinOp::Shl) {
                    self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                    self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                } else {
                    let hi_op = if unsigned { "shr" } else { "sar" };
                    let _ = write!(self.out, "\t{hi_op}\tax,1\r\n");
                    self.out.extend_from_slice(b"\trcr\tdx,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {a_lo}\r\n");
                let k_u8 = (k & 0xFF) as u8;
                let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                let helper = match (op, unsigned) {
                    (BinOp::Shl, _)     => "N_LXLSH@",
                    (BinOp::Shr, false) => "N_LXRSH@",
                    (BinOp::Shr, true)  => "N_LXURSH@",
                    _ => unreachable!(),
                };
                let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
                self.helpers.insert(helper.to_string());
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            }
            return true;
        }
        // `<dest> = <a> * K_pow2` — strength-reduce to shl. BCC uses
        // inline `shl dx,1; rcl ax,1` (AX=high, DX=low) only for K=2
        // (shift by 1) and for K=1 (just a copy with the same load
        // shape); any larger pow2 routes through N_LXLSH@ with
        // `mov cl, k` and the helper's DX=high, AX=low ABI.
        // Fixtures 1641 (`a * 4L` → helper), 1783 (`a * 1L` → copy).
        if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &value.kind
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some(k) = try_const_eval(right)
            && k > 0
            && k.is_power_of_two()
            && k.trailing_zeros() <= 31
        {
            let shifts = k.trailing_zeros();
            if shifts <= 1 {
                let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
                if shifts == 1 {
                    self.out.extend_from_slice(b"\tshl\tdx,1\r\n");
                    self.out.extend_from_slice(b"\trcl\tax,1\r\n");
                }
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},ax\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},dx\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tdx,word ptr {a_hi}\r\n");
                let _ = write!(self.out, "\tmov\tax,word ptr {a_lo}\r\n");
                let k_u8 = shifts as u8;
                let _ = write!(self.out, "\tmov\tcl,{k_u8}\r\n");
                self.out.extend_from_slice(b"\tcall\tnear ptr N_LXLSH@\r\n");
                self.helpers.insert("N_LXLSH@".to_string());
                let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
                let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            }
            return true;
        }
        // `<dest> = <a> <</>> <n>` for a long lvalue shifted by an
        // int-typed lvalue. BCC loads the operand into DX:AX and the
        // shift count's *low byte* into CL (the value is assumed to
        // fit in a byte, valid for any C shift count ≤ 31), then
        // calls N_LXLSH@ (left) / N_LXRSH@ (signed right) / N_LXURSH@
        // (unsigned right). Result in DX:AX. Fixtures 1630 (signed
        // shr), 1631 (shl), 1634 (unsigned shr).
        if let ExprKind::BinOp { op, left, right } = &value.kind
            && matches!(op, BinOp::Shl | BinOp::Shr)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some(n_addr) = self.int_lvalue_addr(right)
        {
            let unsigned = self.expr_is_unsigned(left);
            let helper = match (op, unsigned) {
                (BinOp::Shl, _)     => "N_LXLSH@",
                (BinOp::Shr, false) => "N_LXRSH@",
                (BinOp::Shr, true)  => "N_LXURSH@",
                _ => unreachable!(),
            };
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tmov\tcl,byte ptr {n_addr}\r\n");
            let _ = write!(self.out, "\tcall\tnear ptr {helper}\r\n");
            self.helpers.insert(helper.to_string());
            let _ = write!(self.out, "\tmov\tword ptr {dest_hi},dx\r\n");
            let _ = write!(self.out, "\tmov\tword ptr {dest_lo},ax\r\n");
            return true;
        }
        false
    }
    /// Whether a comparison between `left` and `right` should use the
    /// unsigned jump mnemonics. Conservative: only inspects bare
    /// `Ident` operands (the common case in our fixtures). An untyped
    /// expression on either side defaults to signed, matching BCC's
    /// "promote literals to int" behavior.
    pub(crate) fn cmp_is_unsigned(&self, left: &Expr, right: &Expr) -> bool {
        self.expr_is_unsigned(left) || self.expr_is_unsigned(right)
    }
    /// Like `expr_is_unsigned`, but applies C's integer-promotion
    /// rule: a `char` / `unsigned char` operand is promoted to
    /// `int` (signed) before a shift, because `int` can hold all
    /// char values. The shift mnemonic (`sar` vs `shr`) follows the
    /// *promoted* type's signedness, not the operand's declared
    /// signedness. Used only by the shift dispatch path; comparison
    /// retains the operand's original signedness because BCC emits
    /// unsigned jumps (`jbe`/`jae`) for uchar compares.
    /// Fixture 1015 (`uchar c >> 2` → `sar` after promotion).
    pub(crate) fn expr_shift_is_unsigned(&self, e: &Expr) -> bool {
        let ExprKind::Ident(name) = &e.kind else { return false };
        let ty = if let Some(gt) = self.globals.type_of(name) {
            gt
        } else {
            self.locals.type_of(name)
        };
        if ty.is_char_like() {
            return false;
        }
        ty.is_unsigned()
    }
    /// Wrapper for `try_lvalue_chain_addr` that takes the base and
    /// field separately, matching what the member-codegen sites
    /// already have on hand (they receive `base, field, kind` rather
    /// than a synthesized `Member` expr).
    pub(crate) fn try_member_dot_chain(
        &self,
        base: &Expr,
        field: &str,
    ) -> Option<(String, i32, Type)> {
        let (n, off, ty) = self.try_lvalue_chain_addr(base)?;
        let (field_off, field_ty) = ty.field(field)?;
        let new_off = off.checked_add(i32::from(field_off))?;
        Some((n, new_off, field_ty))
    }
    /// Emit the post-byte-load widening step needed to promote
    /// AL → AX. Signed char promotes via `cbw` (1 byte, `98`).
    /// Unsigned char promotes via `mov ah, 0` (2 bytes, `B4 00`)
    /// to preserve zero in the upper bits.
    pub(crate) fn emit_widen_al(&mut self, ty: &Type) {
        // Char return ABI: callees only need to populate AL; AH is
        // the caller's job to widen. Skip the widen step entirely
        // when emitting the return-value loader. Fixtures 3019,
        // 3325, 3227, 2881 (all char-return functions).
        if self.skip_widen {
            return;
        }
        if ty.is_unsigned() {
            self.out.extend_from_slice(b"\tmov\tah,0\r\n");
        } else {
            self.out.extend_from_slice(b"\tcbw\t\r\n");
        }
    }
    /// Emit a "test against zero" instruction for a non-comparison
    /// expression — used in boolean contexts (`if (x)`, `x && y`).
    /// Today only `Ident`s are supported; other expressions panic.
    pub(crate) fn emit_zero_test(&mut self, cond: &Expr) {
        // `if (<bf-chain> & <bf>)` — when the cond is a BitAnd
        // chain whose right operand is a bitfield, BCC replaces
        // the chain's final `and ax, dx` with `test ax, dx` (sets
        // ZF without writing back). Saves the `or ax, ax` the
        // generic path would emit. Fixture 3452.
        if let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &cond.kind
            && let Some(right_bf) = self.resolve_bitfield(right)
        {
            // Seed AX: prefer head-bitfield read; otherwise the
            // standard emit_expr_to_ax path.
            if let Some(head_bf) = self.resolve_bitfield(left) {
                self.emit_bitfield_read_to_reg(&head_bf, "ax", "al");
            } else {
                self.emit_expr_to_ax(left);
            }
            self.emit_bitfield_read_to_reg(&right_bf, "dx", "dl");
            self.out.extend_from_slice(b"\ttest\tax,dx\r\n");
            return;
        }
        // `if ((x = expr))` — evaluate the assignment expression
        // into AX (leaving the value behind), then `or ax, ax` to
        // set the flags. Fixture 513.
        if let ExprKind::AssignExpr { .. } = &cond.kind {
            self.emit_expr_to_ax(cond);
            self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            return;
        }
        // `if (f())` — call yields its result in AX, then `or ax, ax`
        // sets ZF for the conditional branch. Fixture 591.
        if let ExprKind::Call { .. } = &cond.kind {
            self.emit_expr_to_ax(cond);
            self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            return;
        }
        // `while (--n)` / `while (++n)` — pre-inc/dec on a register-
        // resident int local. The inc/dec instruction itself sets ZF
        // based on the result, so we don't need a subsequent `or` or
        // load to AX. Just emit `inc/dec <reg>` and let the caller's
        // conditional jump read the flags. Fixtures 1844, 2361, 2749.
        if let ExprKind::Update {
            target,
            op,
            position: crate::ast::UpdatePosition::Pre,
        } = &cond.kind
            && self.locals.has(target)
            && let LocalLocation::Reg(reg) = self.locals.location_of(target)
            && self.locals.type_of(target).is_int_like()
        {
            let mnem = match op {
                crate::ast::UpdateOp::Inc => "inc",
                crate::ast::UpdateOp::Dec => "dec",
            };
            let _ = write!(self.out, "\t{mnem}\t{}\r\n", reg.name());
            return;
        }
        // `while (x--)` — postinc/postdec as a boolean: the
        // current value of `x` is the test, then the side
        // effect happens. BCC's shape: `mov ax, <x>; dec <x>;
        // or ax, ax`. The Update lowering already produces the
        // value-in-AX-and-side-effect sequence; follow with
        // `or ax, ax` to set ZF. Fixture 619.
        if let ExprKind::Update { position: crate::ast::UpdatePosition::Post, .. } = &cond.kind {
            self.emit_expr_to_ax(cond);
            self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            return;
        }
        // `while (*p)` / `if (*p)` — deref of a register-resident
        // pointer local: `cmp <width> ptr [<reg>], 0` directly,
        // avoiding the round-trip through AX. Width follows the
        // pointee type. Fixture 636.
        if let ExprKind::Deref(operand) = &cond.kind
            && let ExprKind::Ident(name) = &operand.kind
            && self.locals.has(name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && let Some(pointee) = self.locals.type_of(name).pointee()
        {
            let width = if pointee.is_char_like() { "byte" } else { "word" };
            let _ = write!(self.out, "\tcmp\t{width} ptr [{}],0\r\n", reg.name());
            return;
        }
        // `while (*p++)` — deref of a postinc/postdec on a register
        // pointer local. Save the pre-update pointer in BX, advance
        // the register-resident pointer, then `cmp <w> ptr [bx], 0`
        // directly. Same shape as `emit_deref_to_ax`'s postinc-
        // through-deref path but with a memory-direct compare
        // instead of a load + widen + or-test. Fixture 2027
        // (`while (*s++)` for `char *s`).
        if let ExprKind::Deref(operand) = &cond.kind
            && let ExprKind::Update {
                target,
                op,
                position: UpdatePosition::Post,
            } = &operand.kind
            && self.locals.has(target)
            && let LocalLocation::Reg(reg) = self.locals.location_of(target)
            && let Some(pointee) = self.locals.type_of(target).pointee()
        {
            let reg_name = reg.name();
            let stride = i32::from(pointee.size_bytes());
            let mnem = match op {
                crate::ast::UpdateOp::Inc => "inc",
                crate::ast::UpdateOp::Dec => "dec",
            };
            let _ = write!(self.out, "\tmov\tbx,{reg_name}\r\n");
            for _ in 0..stride {
                let _ = write!(self.out, "\t{mnem}\t{reg_name}\r\n");
            }
            let width = if pointee.is_char_like() { "byte" } else { "word" };
            let _ = write!(self.out, "\tcmp\t{width} ptr [bx],0\r\n");
            return;
        }
        // `while (*++p)` — deref of a pre-update on a register
        // pointer local. Advance the register, snapshot to BX, then
        // compare `*p` through BX. The BX bounce mirrors the postinc
        // shape (fixture 2027) and is what BCC emits even though
        // `cmp <w> ptr [<reg>], 0` would be shorter — BCC's codegen
        // template always routes the compare through BX. Fixture
        // 1311 (`*++p` for char *p).
        if let ExprKind::Deref(operand) = &cond.kind
            && let ExprKind::Update {
                target,
                op,
                position: UpdatePosition::Pre,
            } = &operand.kind
            && self.locals.has(target)
            && let LocalLocation::Reg(reg) = self.locals.location_of(target)
            && let Some(pointee) = self.locals.type_of(target).pointee()
        {
            let reg_name = reg.name();
            let stride = i32::from(pointee.size_bytes());
            let mnem = match op {
                crate::ast::UpdateOp::Inc => "inc",
                crate::ast::UpdateOp::Dec => "dec",
            };
            for _ in 0..stride {
                let _ = write!(self.out, "\t{mnem}\t{reg_name}\r\n");
            }
            let width = if pointee.is_char_like() { "byte" } else { "word" };
            let _ = write!(self.out, "\tmov\tbx,{reg_name}\r\n");
            let _ = write!(self.out, "\tcmp\t{width} ptr [bx],0\r\n");
            return;
        }
        // `if (p[K])` — global-pointer subscript in boolean context.
        // BCC loads the pointer into BX and emits `cmp word ptr
        // [bx+K*stride], 0` directly. Fixture 889.
        if let ExprKind::ArrayIndex { array, index } = &cond.kind
            && let ExprKind::Ident(name) = &array.kind
            && let Some(gty) = self.globals.type_of(name)
            && let Some(pointee) = gty.pointee()
            && let Some(k) = try_const_eval(index)
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
            let _ = write!(self.out, "\tmov\tbx,word ptr DGROUP:_{name}\r\n");
            let _ = write!(self.out, "\tcmp\tword ptr {bx_disp},0\r\n");
            return;
        }
        // `if (a[K])` — stack-resident array element as a zero
        // test. Same memory-direct shape as the int-local arm
        // below, just with a bp-relative element offset. Width
        // follows the leaf type (byte for char arrays, word for
        // int). Fixture 980.
        if let ExprKind::ArrayIndex { .. } = &cond.kind
            && let Some((name, total_off, leaf_ty)) =
                self.try_lvalue_chain_addr(cond)
            && self.locals.has(&name)
            && let LocalLocation::Stack(base_off) = self.locals.location_of(&name)
        {
            let elem_off = base_off + i16::try_from(total_off).unwrap_or(i16::MAX);
            let width = if leaf_ty.is_char_like() { "byte" } else { "word" };
            let _ = write!(self.out, "\tcmp\t{width} ptr {},0\r\n", bp_addr(elem_off));
            return;
        }
        // `if (<global-arr>[<var-idx>])` — global array, variable
        // index. Scale index into BX, then memory-direct
        // `cmp <width> ptr DGROUP:_<arr>[bx], 0`. Width follows
        // element type. Fixture 1309 (`while (a[i])` for int
        // global array a, var index i).
        if let ExprKind::ArrayIndex { array, index } = &cond.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && let Some(gty) = self.globals.type_of(arr_name)
            && let Some(elem_ty) = gty.array_elem()
        {
            let elem_ty = elem_ty.clone();
            self.emit_index_into_bx(index, &elem_ty);
            let width = if elem_ty.is_char_like() { "byte" } else { "word" };
            let _ = write!(
                self.out,
                "\tcmp\t{width} ptr DGROUP:_{arr_name}[bx],0\r\n",
            );
            return;
        }
        // `if (<stack-char-arr>[<si-resident-int>])` — zero-test on
        // a char-array element accessed via BP+SI. Fold to a single
        // `cmp byte ptr [bp+si+disp], 0`. Fixture 2488
        // (for-loop cond `a[i] != 0`).
        if let ExprKind::ArrayIndex { array, index } = &cond.kind
            && let Some((disp, _)) = self.bp_idx_disp_for_char_array(array, index)
        {
            let _ = write!(
                self.out,
                "\tcmp\tbyte ptr [bp+si{}],0\r\n",
                signed_disp_suffix(disp),
            );
            return;
        }
        // `if (<reg-local> & K)` — bit test against a constant mask
        // when the LHS is a register-resident int local. BCC emits
        // `test <reg>, K` (4 bytes, F7 C6 imm16 for SI; the `&` result
        // is discarded but flags are set). Fixture 1415 (popcount's
        // inner `if (x & 1)` with x in SI).
        if let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &cond.kind
            && let Some(reg) = self.ident_in_register(left)
            && let Some(k) = try_const_eval(right)
            && !reg.is_byte()
        {
            let k16 = k & 0xFFFF;
            let _ = write!(self.out, "\ttest\t{},{k16}\r\n", reg.name());
            return;
        }
        // `if (<int_lvalue> & <expr>)` — collapse the and+test pair
        // to a single memory-form `test`. BCC's lowering: evaluate
        // the RHS expression into AX, then `test ax, word ptr
        // <lvalue_addr>` directly. Saves the push/pop+and+or
        // sequence the generic path would emit. Fixture 2399
        // (`if (mask & (1 << i))` with mask stack-resident).
        if let ExprKind::BinOp { op: BinOp::BitAnd, left, right } = &cond.kind
            && let Some(l_addr) = self.int_lvalue_addr(left)
            && !matches!(&right.kind, ExprKind::IntLit(_))
            && try_const_eval(right).is_none()
        {
            self.emit_expr_to_ax(right);
            let _ = write!(self.out, "\ttest\tax,word ptr {l_addr}\r\n");
            return;
        }
        if let ExprKind::Ident(name) = &cond.kind {
            if let Some(gty) = self.globals.type_of(name) {
                // Global array name decays to its address, which is
                // always non-zero — test the address, not the first
                // element. BCC: `mov ax, offset DGROUP:_arr; or ax,
                // ax`. Fixture 2800.
                if matches!(gty, Type::Array { .. }) {
                    let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{name}\r\n");
                    self.out.extend_from_slice(b"\tor\tax,ax\r\n");
                    return;
                }
                let width = if gty.is_char_like() { "byte" } else { "word" };
                let _ = write!(self.out, "\tcmp\t{width} ptr DGROUP:_{name},0\r\n");
                return;
            }
            match self.locals.location_of(name) {
                LocalLocation::Stack(off) => {
                    let ty = self.locals.type_of(name);
                    let width = if ty.is_char_like() { "byte" } else { "word" };
                    let _ = write!(self.out, "\tcmp\t{width} ptr {},0\r\n", bp_addr(off));
                }
                LocalLocation::Reg(reg) => {
                    let _ = write!(self.out, "\tor\t{0},{0}\r\n", reg.name());
                }
            }
            return;
        }
        // `if (<global-chain>)` — any lvalue chain rooted at a global
        // (e.g. `s.x`, `a[2]`, `s.a[1]`). BCC emits memory-direct
        // `cmp <width> ptr DGROUP:_<sym>[+K], 0`, avoiding the AX
        // round-trip. Fixture 3586 (`if (s.x && s.y)`).
        if let Some((name, off, ty)) = self.try_lvalue_chain_addr(cond)
            && self.globals.contains(&name)
            && !matches!(ty, Type::Array { .. } | Type::Struct { .. })
        {
            let width = if ty.is_char_like() { "byte" } else { "word" };
            let addr = if off == 0 {
                format!("DGROUP:_{name}")
            } else {
                format!("DGROUP:_{name}+{off}")
            };
            let _ = write!(self.out, "\tcmp\t{width} ptr {addr},0\r\n");
            return;
        }
        // `if ((<lvalue> = <value>))` — when the value path ends
        // with `mov byte ptr [...], al` (char store), only AL
        // carries meaningful bits, so a byte-wise `or al, al`
        // matches what BCC emits. Fixture 1808
        // (`while (*d++ = *s++)` strcpy loop).
        if let ExprKind::AssignLvalueExpr { target, .. } = &cond.kind
            && self.target_is_char_lvalue(target)
        {
            self.emit_expr_to_ax(cond);
            if last_emit_ends_with_byte_store_al(self.out) {
                self.out.extend_from_slice(b"\tor\tal,al\r\n");
            } else {
                self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            }
            return;
        }
        // Catch-all: evaluate the condition expression into AX and
        // test with `or ax, ax`. Covers any shape we don't have a
        // dedicated peephole for — `if ("X")` (StringLit address,
        // fixture 1582), `while (*++p)`, `if ((a = f()))`, etc. Not
        // always the tightest byte sequence BCC would pick, but it
        // sets ZF correctly and avoids a crash.
        self.emit_expr_to_ax(cond);
        self.out.extend_from_slice(b"\tor\tax,ax\r\n");
    }
    /// Emit just the `cmp` instruction (no jump). Four shapes,
    /// matching what BCC produces:
    ///
    /// 1. LHS in a register AND RHS is constant 0: `or <reg>, <reg>` —
    ///    a one-byte-shorter alias for `cmp <reg>, 0` (fixture 035).
    ///    Sets ZF/SF/PF the same way and clears OF/CF, which matches
    ///    what a `cmp` against zero produces, so the same signed
    ///    conditional-jump mnemonics work.
    /// 2. LHS in a register: `cmp <reg>, <rhs>`
    /// 3. LHS is a stack local and RHS is a constant: `cmp word ptr [bp-N], K`
    /// 4. Otherwise: `mov ax, <lhs>` then `cmp ax, <rhs>`
    /// Recognize `<stack-char-array>[<ident-in-SI>]` (the index
    /// must specifically be in SI today — DI would also be a valid
    /// base register and we'd extend if a fixture exercises it).
    /// Returns the array base's signed bp-offset together with the
    /// chosen base register. Used by the BP+SI byte-load peephole.
    /// Fixture 2488.
    pub(crate) fn bp_idx_disp_for_char_array(
        &self,
        array: &Expr,
        index: &Expr,
    ) -> Option<(i8, crate::codegen::locals::Reg)> {
        let ExprKind::Ident(arr_name) = &array.kind else { return None };
        if !self.locals.has(arr_name) { return None; }
        let arr_ty = self.locals.type_of(arr_name);
        let Some(elem_ty) = arr_ty.array_elem() else { return None };
        if !elem_ty.is_char_like() { return None; }
        let LocalLocation::Stack(arr_off) = self.locals.location_of(arr_name) else { return None };
        let disp = i8::try_from(arr_off).ok()?;
        let ExprKind::Ident(idx_name) = &index.kind else { return None };
        if !self.locals.has(idx_name) { return None; }
        let idx_ty = self.locals.type_of(idx_name);
        if !idx_ty.is_int_like() { return None; }
        let LocalLocation::Reg(reg) = self.locals.location_of(idx_name) else { return None };
        if !matches!(reg, crate::codegen::locals::Reg::Si) { return None; }
        Some((disp, reg))
    }
    /// Try to match `e` as a `BinOp::Add` chain whose every leaf is
    /// `<arr>[<idx>].<field>` for a stack-local struct array `arr`
    /// with the same `idx` lvalue and a non-power-of-2 element
    /// stride. Returns `(arr_base_bp_off, idx_lvalue_addr,
    /// elem_stride, field_offs_in_chain_order)`. The caller emits
    /// each field's per-iteration address prelude. Fixture 1914.
    pub(crate) fn match_arr_var_field_add_chain(
        &self,
        e: &Expr,
    ) -> Option<(i16, String, u16, Vec<u16>)> {
        let mut arr_name: Option<String> = None;
        let mut idx_addr: Option<String> = None;
        let mut stride: Option<u16> = None;
        let mut field_offs: Vec<u16> = Vec::new();
        fn walk(
            e: &Expr,
            self_ref: &FunctionEmitter<'_>,
            arr_name: &mut Option<String>,
            idx_addr: &mut Option<String>,
            stride: &mut Option<u16>,
            field_offs: &mut Vec<u16>,
        ) -> bool {
            if let ExprKind::BinOp { op: BinOp::Add, left, right } = &e.kind {
                return walk(left, self_ref, arr_name, idx_addr, stride, field_offs)
                    && walk(right, self_ref, arr_name, idx_addr, stride, field_offs);
            }
            let ExprKind::Member {
                base,
                field,
                kind: crate::ast::MemberKind::Dot,
            } = &e.kind else { return false };
            let ExprKind::ArrayIndex { array, index } = &base.kind else { return false };
            let ExprKind::Ident(name) = &array.kind else { return false };
            if !self_ref.locals.has(name) { return false; }
            let arr_ty = self_ref.locals.type_of(name);
            let Some(elem_ty) = arr_ty.array_elem() else { return false };
            let Some((f_off, _)) = elem_ty.field(field) else { return false };
            let elem_stride = elem_ty.size_bytes();
            // Power-of-2 strides are already handled by the
            // shl-based fast path in `resolve_operand_source`. This
            // peephole is for the imul case only.
            if elem_stride < 2
                || elem_stride.is_power_of_two()
            {
                return false;
            }
            let i_addr = match self_ref.named_int_lvalue_addr_or_reg(index) {
                Some(s) => s,
                None => return false,
            };
            match arr_name {
                None => *arr_name = Some(name.clone()),
                Some(a) if a == name => {}
                _ => return false,
            }
            match idx_addr {
                None => *idx_addr = Some(i_addr),
                Some(a) if *a == i_addr => {}
                _ => return false,
            }
            match stride {
                None => *stride = Some(elem_stride),
                Some(s) if *s == elem_stride => {}
                _ => return false,
            }
            field_offs.push(f_off);
            true
        }
        if !walk(e, self, &mut arr_name, &mut idx_addr, &mut stride, &mut field_offs) {
            return None;
        }
        let name = arr_name?;
        let LocalLocation::Stack(arr_base) = self.locals.location_of(&name) else {
            return None;
        };
        Some((arr_base, idx_addr?, stride?, field_offs))
    }
    /// Addressing form for dereferencing a char pointer, staging it through BX
    /// first when the pointer can't be a base register: SI/DI/BX address memory
    /// directly (`[si]`), CX/DX copy to BX (`mov bx, cx`), and a stack-resident
    /// pointer loads from its slot (`mov bx, word ptr [bp+disp]`). Fixtures 4240
    /// (CX) and 4242 (stack spill).
    fn char_deref_addr_via_bx(&mut self, loc: LocalLocation) -> String {
        match loc {
            LocalLocation::Reg(r) if matches!(r, Reg::Si | Reg::Di | Reg::Bx) => {
                format!("[{}]", r.name())
            }
            LocalLocation::Reg(r) => {
                let _ = write!(self.out, "\tmov\tbx,{}\r\n", r.name());
                "[bx]".to_owned()
            }
            LocalLocation::Stack(off) => {
                let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(off));
                "[bx]".to_owned()
            }
        }
    }
    pub(crate) fn emit_compare(&mut self, left: &Expr, right: &Expr) {
        // Huge-pointer comparison: both operands are `int huge *`
        // (or another huge-pointer) lvalues. BCC's normalization
        // helper `N_PCMP@` takes LHS in DX:AX (high=seg, low=off)
        // and RHS in CX:BX, sets flags for the surrounding Jcc.
        // Fixture 1772 (`if (p1 == p2)`).
        if let (Some(l_off), Some(r_off)) =
            (self.huge_ptr_lvalue_addr(left), self.huge_ptr_lvalue_addr(right))
        {
            let _ = write!(self.out, "\tmov\tcx,word ptr {}\r\n", bp_addr(r_off + 2));
            let _ = write!(self.out, "\tmov\tbx,word ptr {}\r\n", bp_addr(r_off));
            let _ = write!(self.out, "\tmov\tdx,word ptr {}\r\n", bp_addr(l_off + 2));
            let _ = write!(self.out, "\tmov\tax,word ptr {}\r\n", bp_addr(l_off));
            self.out.extend_from_slice(b"\tcall\tnear ptr N_PCMP@\r\n");
            self.helpers.insert("N_PCMP@".to_string());
            return;
        }
        // Both sides are comparison BinOps: materialize each into AX
        // as 0/1, push the first, eval second, pop into DX, compare
        // DX with AX. Fixture 1395 (`(a==b) == (b<c)`).
        if let (
            ExprKind::BinOp { op: lop, left: ll, right: lr },
            ExprKind::BinOp { op: rop, left: rl, right: rr },
        ) = (&left.kind, &right.kind)
            && lop.is_comparison()
            && rop.is_comparison()
        {
            self.emit_comparison_as_value(left.span.start, left.span.end, *lop, ll, lr);
            let push_pos = self.out.len();
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.emit_comparison_as_value(right.span.start, right.span.end, *rop, rl, rr);
            hoist_first_setup_above_push(self.out, push_pos);
            self.out.extend_from_slice(b"\tpop\tdx\r\n");
            self.out.extend_from_slice(b"\tcmp\tdx,ax\r\n");
            return;
        }
        // `*<char-ptr-reg> <relop> *<char-ptr-reg>` — both sides
        // are deref of a register-resident char pointer. Emit byte
        // compare `mov al, [reg_l]; cmp al, [reg_r]` directly.
        // Fixture 1352 (`*a == *b` for `char *a, *b` in SI/DI).
        let char_ptr_local = |this: &Self, name: &str| -> Option<LocalLocation> {
            if !this.locals.has(name) { return None; }
            if !this.locals.type_of(name).pointee().is_some_and(|p| p.is_char_like()) { return None; }
            let loc = this.locals.location_of(name);
            match loc {
                LocalLocation::Stack(_) => Some(loc),
                LocalLocation::Reg(r) if !r.is_byte() => Some(loc),
                LocalLocation::Reg(_) => None,
            }
        };
        if let (ExprKind::Deref(l_inner), ExprKind::Deref(r_inner)) =
            (&left.kind, &right.kind)
            && let (ExprKind::Ident(l_name), ExprKind::Ident(r_name)) =
                (&l_inner.kind, &r_inner.kind)
            && let Some(l_loc) = char_ptr_local(self, l_name)
            && let Some(r_loc) = char_ptr_local(self, r_name)
        {
            // SI/DI/BX address memory directly; a pointer in CX/DX (`mov bx, cx`,
            // fixture 4240) or on the STACK (`mov bx, [bp+disp]`, fixture 4242 —
            // the 4th pointer that overflows the SI/DI/CX pool) must be staged
            // through BX first. The L-side load into AL finishes before the
            // R-side reuses BX, so both can route through BX without conflict.
            let l_addr = self.char_deref_addr_via_bx(l_loc);
            let _ = write!(self.out, "\tmov\tal,byte ptr {l_addr}\r\n");
            let r_addr = self.char_deref_addr_via_bx(r_loc);
            let _ = write!(self.out, "\tcmp\tal,byte ptr {r_addr}\r\n");
            return;
        }
        // `<stack-char-arr>[<reg-int>] <relop> <stack-char-arr>[<same-reg-int>]`
        // — both sides indexed by the same SI-resident int local.
        // Fold to `mov al, [bp+si+lhs_disp]; cmp al, [bp+si+rhs_disp]`.
        // Fixture 2488 (`a[i] != b[i]`).
        if let (Some((l_disp, l_reg)), Some((r_disp, r_reg))) = (
            (if let ExprKind::ArrayIndex { array, index } = &left.kind {
                self.bp_idx_disp_for_char_array(array, index)
            } else { None }),
            (if let ExprKind::ArrayIndex { array, index } = &right.kind {
                self.bp_idx_disp_for_char_array(array, index)
            } else { None }),
        ) {
            assert_eq!(l_reg, r_reg, "two char-arr BP+SI accesses must share base reg");
            let _ = write!(
                self.out,
                "\tmov\tal,byte ptr [bp+si{}]\r\n",
                signed_disp_suffix(l_disp),
            );
            let _ = write!(
                self.out,
                "\tcmp\tal,byte ptr [bp+si{}]\r\n",
                signed_disp_suffix(r_disp),
            );
            return;
        }
        // `<char_lvalue> <relop> <char_lvalue>` — both sides are
        // char-typed memory operands. BCC emits a byte compare:
        // `mov al, byte ptr <left>; cmp al, byte ptr <right>`. We
        // were widening left to AX first, which is unnecessary at
        // byte width. Fixture 1457 (`a[0] == a[2]` for char arr).
        if let Some((l_name, l_off, l_ty)) = self.try_lvalue_chain_addr(left)
            && let Some((r_name, r_off, r_ty)) = self.try_lvalue_chain_addr(right)
            && l_ty.is_char_like()
            && r_ty.is_char_like()
        {
            let l_addr = if self.globals.contains(&l_name) {
                if l_off == 0 {
                    format!("DGROUP:_{l_name}")
                } else {
                    format!("DGROUP:_{l_name}+{l_off}")
                }
            } else if let LocalLocation::Stack(base) = self.locals.location_of(&l_name) {
                let off = base + i16::try_from(l_off).unwrap_or(i16::MAX);
                bp_addr(off)
            } else {
                return;
            };
            let r_addr = if self.globals.contains(&r_name) {
                if r_off == 0 {
                    format!("DGROUP:_{r_name}")
                } else {
                    format!("DGROUP:_{r_name}+{r_off}")
                }
            } else if let LocalLocation::Stack(base) = self.locals.location_of(&r_name) {
                let off = base + i16::try_from(r_off).unwrap_or(i16::MAX);
                bp_addr(off)
            } else {
                return;
            };
            let _ = write!(self.out, "\tmov\tal,byte ptr {l_addr}\r\n");
            let _ = write!(self.out, "\tcmp\tal,byte ptr {r_addr}\r\n");
            return;
        }
        // `*p <relop> K` for register-resident pointer p: emit
        // memory-direct `cmp <width> ptr [<reg>], K` instead of
        // loading to AX first. Matches BCC's actual shape for
        // `while (*s != 0)` (fixture 1408) and `if (*r == 0)`
        // (fixture 1566).
        if let ExprKind::Deref(operand) = &left.kind
            && let ExprKind::Ident(name) = &operand.kind
            && self.locals.has(name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(name)
            && let Some(pointee) = self.locals.type_of(name).pointee()
            && let Some(rhs) = try_const_eval(right)
        {
            let width = if pointee.is_char_like() { "byte" } else { "word" };
            let rhs_masked = if pointee.is_char_like() { rhs & 0xFF } else { rhs & 0xFFFF };
            let _ = write!(
                self.out,
                "\tcmp\t{width} ptr [{}],{rhs_masked}\r\n",
                reg.name(),
            );
            return;
        }
        if let Some(reg) = self.ident_in_register(left) {
            // Char in a byte register: 8-bit cmp with byte-truncated
            // immediate (fixture 054). Non-constant RHS is unobserved.
            if reg.is_byte() {
                if let Some(v) = try_const_eval(right) {
                    let v8 = v & 0xFF;
                    let _ = write!(self.out, "\tcmp\t{},{v8}\r\n", reg.name());
                    return;
                }
                panic!("char-register comparison with non-constant rhs not yet supported");
            }
            if let Some(0) = try_const_eval(right) {
                let _ = write!(self.out, "\tor\t{0},{0}\r\n", reg.name());
                return;
            }
            let src = self.resolve_operand_source(right);
            // If the resolved RHS materialized into AX (via a lea
            // for `<local_arr> + <const>`), BCC's pattern is to
            // swap the operand order in the cmp and invert the
            // resulting Jcc mnemonic. Detect via `OperandSource::Ax`
            // and signal the inversion via `self.cmp_swapped`. The
            // outer caller picks the swapped Jcc pair. Fixture 1814.
            if matches!(src, OperandSource::Ax) {
                let _ = write!(self.out, "\tcmp\tax,{}\r\n", reg.name());
                self.cmp_swapped = true;
            } else {
                let _ = write!(self.out, "\tcmp\t{},{}\r\n", reg.name(), src.word());
            }
            return;
        }
        if let (ExprKind::Ident(name), Some(rhs)) = (&left.kind, try_const_eval(right))
            && self.locals.has(name)
            && let LocalLocation::Stack(off) = self.locals.location_of(name)
        {
            // Char-typed stack locals use the byte-form compare
            // (`80 7E disp8 imm8` — fixture 524).
            let ty = self.locals.type_of(name);
            if ty.is_char_like() {
                let rhs8 = rhs & 0xFF;
                let _ = write!(self.out, "\tcmp\tbyte ptr {},{rhs8}\r\n", bp_addr(off));
                return;
            }
            let rhs16 = rhs & 0xFFFF;
            let _ = write!(self.out, "\tcmp\tword ptr {},{rhs16}\r\n", bp_addr(off));
            return;
        }
        // `<int-global> <relop> <const>` — emit a memory-direct
        // compare `cmp word ptr DGROUP:_g, K`. BCC prefers the
        // imm8sx form (`83 3E disp16 ii`) when K fits a signed
        // byte; otherwise the imm16 form. Fixture 429.
        // Pointer globals share the same word-sized cmp path
        // (fixture 504: `if (g == 0)` with `int *g`).
        if let (ExprKind::Ident(name), Some(rhs)) = (&left.kind, try_const_eval(right))
            && let Some(gty) = self.globals.type_of(name)
            && (matches!(gty, Type::Int | Type::UInt) || gty.pointee().is_some())
        {
            let rhs16 = rhs & 0xFFFF;
            let _ = write!(self.out, "\tcmp\tword ptr DGROUP:_{name},{rhs16}\r\n");
            return;
        }
        // `<char-global> <relop> <const>` — byte-form memory
        // compare `cmp byte ptr DGROUP:_c, K` (encoded `80 3E ...`).
        // The char's int value is truncated to 8 bits. Fixture 452.
        if let (ExprKind::Ident(name), Some(rhs)) = (&left.kind, try_const_eval(right))
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_char_like()
        {
            let rhs8 = rhs & 0xFF;
            let _ = write!(self.out, "\tcmp\tbyte ptr DGROUP:_{name},{rhs8}\r\n");
            return;
        }
        // `<reg-ptr>-><field> <relop> <const>` — memory-direct compare
        // through a register-resident struct pointer. BCC emits `cmp
        // word ptr [<reg>+off], K` directly (4 bytes for disp!=0, 3
        // for disp=0). Restricted to SI (tasm only has the SI form
        // today) and word fields with imm8sx constants. Fixture 1007.
        if let (
            ExprKind::Member {
                base,
                field,
                kind: crate::ast::MemberKind::Arrow,
            },
            Some(rhs),
        ) = (&left.kind, try_const_eval(right))
            && let ExprKind::Ident(p_name) = &base.kind
            && self.locals.has(p_name)
            && let LocalLocation::Reg(reg) = self.locals.location_of(p_name)
            && reg.name() == "si"
            && let Some(pty) = self.locals.type_of(p_name).pointee()
            && let Some((field_off, field_ty)) = pty.field(field)
            && !field_ty.is_char_like()
            && i8::try_from(rhs).is_ok()
        {
            let reg_name = reg.name();
            let disp = if field_off == 0 {
                format!("[{reg_name}]")
            } else {
                format!("[{reg_name}+{field_off}]")
            };
            let _ = write!(self.out, "\tcmp\tword ptr {disp},{rhs}\r\n");
            return;
        }
        // `<char-stack> <relop> <char-stack>` — byte-byte compare
        // directly: `mov al, byte ptr <lhs>; cmp al, byte ptr <rhs>`.
        // No `cbw` widening since both sides are already byte values
        // and the compare's signedness is encoded in the *jump*
        // selection (jl/jb), not the operand width. Fixtures 951, 952.
        if let (ExprKind::Ident(ln), ExprKind::Ident(rn)) = (&left.kind, &right.kind)
            && self.locals.has(ln)
            && self.locals.has(rn)
            && self.locals.type_of(ln).is_char_like()
            && self.locals.type_of(rn).is_char_like()
            && let LocalLocation::Stack(loff) = self.locals.location_of(ln)
            && let LocalLocation::Stack(roff) = self.locals.location_of(rn)
        {
            let _ = write!(self.out, "\tmov\tal,byte ptr {}\r\n", bp_addr(loff));
            let _ = write!(self.out, "\tcmp\tal,byte ptr {}\r\n", bp_addr(roff));
            return;
        }
        // `<stack-array-elem> <relop> <const>` — memory-direct
        // compare on a bp-relative array element. `cmp word ptr
        // [bp+(base+K*stride)], imm`. Same shape as the int-global
        // arm above, just with a bp-relative LHS. Sibling for char
        // arrays uses the byte form. Fixtures 978, 979.
        //
        // Also handles global-struct-field and global-array-member
        // chains: `s.x` resolves to `(name="s", total_off=0)` and
        // `s.a[K]` to `(name="s", total_off=field_off + K*stride)`,
        // both routing through the same memory-direct cmp shape but
        // against `DGROUP:_<name>+off`. Fixture 991 (`s.x == 5`).
        if let (ExprKind::ArrayIndex { .. } | ExprKind::Member { kind: crate::ast::MemberKind::Dot, .. }, Some(rhs)) =
            (&left.kind, try_const_eval(right))
            && let Some((name, total_off, leaf_ty)) =
                self.try_lvalue_chain_addr(left)
        {
            if self.locals.has(&name)
                && let LocalLocation::Stack(base_off) = self.locals.location_of(&name)
            {
                let elem_off = base_off + i16::try_from(total_off).unwrap_or(i16::MAX);
                if leaf_ty.is_char_like() {
                    let rhs8 = rhs & 0xFF;
                    let _ = write!(
                        self.out,
                        "\tcmp\tbyte ptr {},{rhs8}\r\n",
                        bp_addr(elem_off),
                    );
                } else {
                    let rhs16 = rhs & 0xFFFF;
                    let _ = write!(
                        self.out,
                        "\tcmp\tword ptr {},{rhs16}\r\n",
                        bp_addr(elem_off),
                    );
                }
                return;
            }
            if self.globals.contains(&name) {
                let addr = if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                };
                if leaf_ty.is_char_like() {
                    let rhs8 = rhs & 0xFF;
                    let _ = write!(self.out, "\tcmp\tbyte ptr {addr},{rhs8}\r\n");
                } else {
                    let rhs16 = rhs & 0xFFFF;
                    let _ = write!(self.out, "\tcmp\tword ptr {addr},{rhs16}\r\n");
                }
                return;
            }
            // Fall through to generic AX-based compare for non-local,
            // non-global roots (shouldn't normally happen).
        }
        // `<stack-int> <relop> <reg-int>` — memory-on-left compare
        // (`cmp word ptr [bp+N], <reg>`) skips loading the LHS into
        // AX. Operand order is preserved so the caller's relop
        // mnemonic doesn't need swapping. Fixture 3588 (`a > b` with
        // a in stack, b in SI → `cmp word ptr [bp+4], si`).
        if let (ExprKind::Ident(lname), ExprKind::Ident(rname)) = (&left.kind, &right.kind)
            && self.locals.has(lname)
            && self.locals.has(rname)
            && let LocalLocation::Stack(loff) = self.locals.location_of(lname)
            && let LocalLocation::Reg(rreg) = self.locals.location_of(rname)
            && self.locals.type_of(lname).is_int_like()
            && self.locals.type_of(rname).is_int_like()
            && !rreg.is_byte()
        {
            let _ = write!(
                self.out,
                "\tcmp\tword ptr {},{}\r\n",
                bp_addr(loff),
                rreg.name(),
            );
            return;
        }
        // `<lhs> <relop> <var-indexed-arr-or-char-load-rhs>` — RHS
        // can't reduce to a single memory operand, so route LHS →
        // AX → push, RHS → AX, pop DX, then `cmp dx, ax`. The Jcc
        // family that follows reads the same flags the unswapped
        // form would, so emit the `cmp_swapped` flag to let the
        // caller pick the mirrored mnemonic. Fixture 2488
        // (`a[i] != b[i]` for char arrays with var index).
        let rhs_needs_emit = matches!(&right.kind, ExprKind::ArrayIndex { index, .. }
            if try_const_eval(index).is_none())
            && self.expr_is_char_load(right);
        if rhs_needs_emit {
            self.emit_expr_to_ax(left);
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.emit_expr_to_ax(right);
            self.out.extend_from_slice(b"\tpop\tdx\r\n");
            self.out.extend_from_slice(b"\tcmp\tdx,ax\r\n");
            self.cmp_swapped = true;
            return;
        }
        self.emit_expr_to_ax(left);
        // `<expr-in-ax> <relop> 0` — use `or ax, ax` (2 bytes) instead
        // of `cmp ax, 0` (3 bytes) since both set ZF/SF the same way.
        // Fixture 555 (`while ((c = g) > 0)` lowers the post-load
        // zero test through this peephole).
        if let Some(0) = try_const_eval(right) {
            // If the last emitted instruction wrote AL into a char
            // slot (`mov byte ptr <X>, al`), then only AL carries
            // meaningful bits — test it byte-wise. Same encoded
            // length (2 bytes) but matches BCC's exact opcode.
            // Fixture 3653 (`while ((c = arr[i++]) != 0)`).
            if last_emit_ends_with_byte_store_al(self.out) {
                self.out.extend_from_slice(b"\tor\tal,al\r\n");
            } else {
                self.out.extend_from_slice(b"\tor\tax,ax\r\n");
            }
            return;
        }
        let src = self.resolve_operand_source(right);
        let _ = write!(self.out, "\tcmp\tax,{}\r\n", src.word());
    }
    /// Emit `a && b` / `a || b` in expression position — the value
    /// (0 or 1) must land in AX. Layout (fixtures 059, 060):
    ///
    /// && (slots: +0 unused, +1 unused, +2 false-mat, +3 end):
    /// ```text
    ///   <cond-branch(a, true=None, false=false-mat)>
    ///   <cond-branch(b, true=None, false=false-mat)>
    ///   mov ax, 1
    ///   jmp short end
    /// false-mat:
    ///   xor ax, ax
    /// end:
    /// ```
    ///
    /// || (slots: +0 unused, +1 true-mat, +2 false-mat, +3 end):
    /// ```text
    ///   <cond-branch(a, true=true-mat, false=None)>
    ///   <cond-branch(b, true=None,     false=false-mat)>
    /// true-mat:
    ///   mov ax, 1
    ///   jmp short end
    /// false-mat:
    ///   xor ax, ax
    /// end:
    /// ```
    pub(crate) fn emit_logical_to_ax(
        &mut self,
        logical_span_start: u32,
        logical_span_end: u32,
        op: LogicalOp,
        left: &Expr,
        right: &Expr,
    ) {
        let base = self.label_plan.base(logical_span_start, logical_span_end);
        let true_mat_slot = base + 1;
        let false_mat_slot = base + 2;
        let end_slot = base + 3;
        match op {
            LogicalOp::And => {
                self.emit_cond_branch(left, None, Some(false_mat_slot));
                self.emit_cond_branch(right, None, Some(false_mat_slot));
            }
            LogicalOp::Or => {
                self.emit_cond_branch(left, Some(true_mat_slot), None);
                self.emit_cond_branch(right, None, Some(false_mat_slot));
                self.emit_label(true_mat_slot);
            }
        }
        self.out.extend_from_slice(b"\tmov\tax,1\r\n");
        let _ = write!(self.out, "\tjmp\tshort {}\r\n", self.label_ref(end_slot));
        self.emit_label(false_mat_slot);
        self.out.extend_from_slice(b"\txor\tax,ax\r\n");
        self.emit_label(end_slot);
    }
    /// Emit `++x` / `--x` / `x++` / `x--` *as an expression* — the
    /// result must land in AX. Shapes (target in a register, fixtures
    /// 043 and 044):
    ///
    /// - Pre  (`++x`): `inc <reg>` / `mov ax, <reg>`
    /// - Post (`x++`): `mov ax, <reg>` / `inc <reg>`
    ///
    /// Equivalents with `dec` for `--`. Stack-resident targets panic
    /// (no fixture yet).
    pub(crate) fn emit_update_to_ax(&mut self, target: &str, op: UpdateOp, position: UpdatePosition) {
        let mnemonic = match op {
            UpdateOp::Inc => "inc",
            UpdateOp::Dec => "dec",
        };
        // Global ++/-- in expression context. Int/uint uses
        // memory-direct `inc word ptr DGROUP:_g` for the side effect
        // plus a separate AX load for the captured value. Pre-update
        // emits the side effect *before* the load; post-update loads
        // first, then mutates. Char/uchar uses the AL detour
        // (`mov al, mem; inc al; mov mem, al; cbw`) for Pre, and
        // load-then-mutate for Post (the captured value is the
        // pre-update one). Fixtures 962/963 (int) and 964 (char).
        if let Some(gty) = self.globals.type_of(target) {
            let gty = gty.clone();
            if gty.is_char_like() {
                let unsigned = gty.is_unsigned();
                match position {
                    UpdatePosition::Pre => {
                        let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                        let _ = write!(self.out, "\tmov\tbyte ptr DGROUP:_{target},al\r\n");
                    }
                    UpdatePosition::Post => {
                        let _ = write!(self.out, "\tmov\tal,byte ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\t{mnemonic}\tbyte ptr DGROUP:_{target}\r\n");
                    }
                }
                if unsigned {
                    self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                } else {
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
                return;
            }
            if matches!(gty, Type::Int | Type::UInt) || gty.pointee().is_some() {
                match position {
                    UpdatePosition::Pre => {
                        let _ = write!(self.out, "\t{mnemonic}\tword ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{target}\r\n");
                    }
                    UpdatePosition::Post => {
                        let _ = write!(self.out, "\tmov\tax,word ptr DGROUP:_{target}\r\n");
                        let _ = write!(self.out, "\t{mnemonic}\tword ptr DGROUP:_{target}\r\n");
                    }
                }
                return;
            }
            panic!("++/-- in expression on non-int/non-char global `{target}` not yet supported (no fixture)");
        }
        // Stack-resident char ++/-- in expression context: BCC uses
        // memory-direct `inc|dec byte ptr [bp-N]` for the side
        // effect, with the captured value loaded via `mov al,
        // byte ptr [bp-N]` before (post) or after (pre) the
        // memory inc/dec, then `cbw` to widen for the caller.
        // Fixture 731 (`f(c++)` for stack-resident char c).
        let reg = match self.locals.location_of(target) {
            LocalLocation::Reg(r) => r,
            LocalLocation::Stack(off) => {
                let ty = self.locals.type_of(target).clone();
                if ty.is_char_like() {
                    let unsigned = ty.is_unsigned();
                    match position {
                        // Pre: AL detour. BCC threads the new value
                        // through AL even for stack-resident char,
                        // mirroring the way `++g` is lowered for
                        // char globals (batch 128). `mov al, mem;
                        // inc al; mov mem, al; cbw`. Fixture 732.
                        UpdatePosition::Pre => {
                            let _ = write!(
                                self.out,
                                "\tmov\tal,byte ptr {}\r\n",
                                bp_addr(off),
                            );
                            let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                            let _ = write!(
                                self.out,
                                "\tmov\tbyte ptr {},al\r\n",
                                bp_addr(off),
                            );
                        }
                        // Post: load value, memory-direct side
                        // effect, widen. The captured value is the
                        // pre-update one. Fixture 731.
                        UpdatePosition::Post => {
                            let _ = write!(
                                self.out,
                                "\tmov\tal,byte ptr {}\r\n",
                                bp_addr(off),
                            );
                            let _ = write!(
                                self.out,
                                "\t{mnemonic}\tbyte ptr {}\r\n",
                                bp_addr(off),
                            );
                        }
                    }
                    if unsigned {
                        self.out.extend_from_slice(b"\tmov\tah,0\r\n");
                    } else {
                        self.out.extend_from_slice(b"\tcbw\t\r\n");
                    }
                    return;
                }
                panic!("++/-- in expression on a stack-resident non-char local not yet supported (no fixture)");
            }
        };
        if reg.is_byte() {
            // Char ++/-- in expression context: load the byte into
            // AL, sign-extend to AX, and apply the side effect to
            // the byte register. For postinc/dec the load goes
            // before the inc/dec so the captured value is the pre-
            // update one. Fixture 649 (`r = c++` with c in DL).
            match position {
                // Pre: BCC routes the inc through AL — `mov al,
                // <reg>; inc al; mov <reg>, al; cbw`. The AL
                // detour mirrors the stack-char and global-char
                // pre paths. Fixture 3273 (`return ++c` with c
                // in DL).
                UpdatePosition::Pre => {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                    let _ = write!(self.out, "\t{mnemonic}\tal\r\n");
                    let _ = write!(self.out, "\tmov\t{},al\r\n", reg.name());
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                }
                UpdatePosition::Post => {
                    let _ = write!(self.out, "\tmov\tal,{}\r\n", reg.name());
                    self.out.extend_from_slice(b"\tcbw\t\r\n");
                    let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                }
            }
            return;
        }
        // Pointer ++/-- in expression context: C scales the step by
        // sizeof(*p). For stride 1 (char*), keep the single-byte
        // `inc/dec reg`. For larger strides, emit `add/sub reg,
        // stride` instead — fixture 3646 (struct Pt* p++ with
        // sizeof(Pt) = 4).
        let stride = self
            .locals
            .type_of(target)
            .pointee()
            .map_or(1u32, |p| u32::from(p.size_bytes()));
        let scaled_op = if stride > 1 {
            let add_or_sub = match op {
                UpdateOp::Inc => "add",
                UpdateOp::Dec => "sub",
            };
            Some((add_or_sub, stride))
        } else {
            None
        };
        match position {
            UpdatePosition::Pre => {
                if let Some((mn, s)) = scaled_op {
                    let _ = write!(self.out, "\t{mn}\t{},{s}\r\n", reg.name());
                } else {
                    let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                }
                let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
            }
            UpdatePosition::Post => {
                let _ = write!(self.out, "\tmov\tax,{}\r\n", reg.name());
                if let Some((mn, s)) = scaled_op {
                    let _ = write!(self.out, "\t{mn}\t{},{s}\r\n", reg.name());
                } else {
                    let _ = write!(self.out, "\t{mnemonic}\t{}\r\n", reg.name());
                }
            }
        }
    }
    /// Emit a function call: push args right-to-left, `call near ptr
    /// _name`, then clean up the pushed args. Each arg is pushed as a
    /// 16-bit word, but **char** parameters use the byte form for the
    /// value-loading instruction (`mov al, K` or `mov al, <src>`)
    /// before the `push ax` — the high byte of the pushed word is
    /// undefined since the callee only reads the low byte (fixture
    /// 052 and 055).
    ///
    /// Cleanup: `pop cx` per arg when there are ≤ 2 args; for ≥ 3
    /// args BCC switches to `add sp, N*2` (one 3-byte instruction
    /// beats three or more `pop cx`s). Fixtures 010 (0), 033 (1),
    /// 034 (2), 049 (3), 046/048 (4).
    /// `<arr>[<idx>](args)` — indirect call via a function-pointer
    /// fetched from a memory location. Pushes args (right-to-left,
    /// same as the direct path), computes the callee address into
    /// BX, then emits `call word ptr [bx]`. The arg-cleanup mirrors
    /// the direct-call path. Fixtures 2308, 2944, 3481, 3696.
    pub(crate) fn emit_call_via(&mut self, addr: &Expr, args: &[Expr]) {
        // Push args right-to-left without a known signature. Use the
        // direct-memory push shape (`push word ptr <src>`) when the
        // arg is a simple lvalue — same optimization the direct-call
        // path uses. Falls back to `mov ax; push ax` otherwise.
        let mut total_bytes: u16 = 0;
        for arg in args.iter().rev() {
            if let Some(push_form) = self.try_direct_arg_push(arg, &Type::Int) {
                let _ = write!(self.out, "\t{push_form}\r\n");
                total_bytes += 2;
                continue;
            }
            self.emit_expr_to_ax(arg);
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            total_bytes += 2;
        }
        // Compute the function pointer into BX from the address
        // expression. For `arr[idx]`, this is the address of the
        // array element. We currently handle the global-fn-ptr-
        // array shape with variable index; other shapes will need
        // additional dispatch.
        // `<stack-arr>[<const>](args)` — local fn-pointer array
        // with constant index. Emits `call word ptr [bp-N+K*2]`.
        // Fixture 1658.
        if let ExprKind::ArrayIndex { array, index } = &addr.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let Some(elem_ty) = self.locals.type_of(arr_name).array_elem()
            && elem_ty.pointee().is_some()
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
            && let Some(k) = try_const_eval(index)
        {
            let stride = i32::from(elem_ty.size_bytes());
            let off = (k as i32).wrapping_mul(stride);
            let final_off = base_off + i16::try_from(off).unwrap_or(i16::MAX);
            let _ = write!(
                self.out,
                "\tcall\tword ptr {}\r\n",
                bp_addr(final_off),
            );
        } else if let ExprKind::ArrayIndex { array, index } = &addr.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && self.locals.has(arr_name)
            && let arr_ty = self.locals.type_of(arr_name).clone()
            && let Some(elem_ty) = arr_ty.array_elem()
            && elem_ty.pointee().is_some()
            && let LocalLocation::Stack(base_off) = self.locals.location_of(arr_name)
        {
            // Variable-index stack fn-pointer array. Scale index
            // into BX, then `call word ptr <bp-base>[bx]`. Fixture
            // 2435 (`for (...) sum += ops[i](10)`).
            let elem_size = elem_ty.size_bytes();
            self.emit_array_addr_to_bx(arr_name, index, base_off, elem_size);
            self.out.extend_from_slice(b"\tcall\tword ptr [bx]\r\n");
        } else if let ExprKind::ArrayIndex { array, index } = &addr.kind
            && let ExprKind::Ident(arr_name) = &array.kind
            && let Some(gty) = self.globals.type_of(arr_name)
            && let Some(elem_ty) = gty.array_elem()
            && elem_ty.pointee().is_some()
        {
            let elem_ty_clone = elem_ty.clone();
            // Scale the index, load the pointer slot into BX.
            if let Some(k) = try_const_eval(index) {
                let stride = u32::from(elem_ty_clone.size_bytes());
                let off = k.wrapping_mul(stride);
                let addr_label = if off == 0 {
                    format!("DGROUP:_{arr_name}")
                } else {
                    format!("DGROUP:_{arr_name}+{off}")
                };
                let _ = write!(
                    self.out,
                    "\tcall\tword ptr {addr_label}\r\n",
                );
            } else {
                self.emit_index_into_bx(index, &elem_ty_clone);
                let _ = write!(
                    self.out,
                    "\tcall\tword ptr DGROUP:_{arr_name}[bx]\r\n",
                );
            }
        } else if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } =
            &addr.kind
            && let Some((name, total_off, _leaf_ty)) =
                self.try_member_dot_chain(base, field)
        {
            // `o.f(args)` — member-field function pointer call.
            // The address is `[bp-N + field_off]` for stack, or
            // `DGROUP:_<sym>+field_off` for globals.
            if self.globals.contains(&name) {
                let label = if total_off == 0 {
                    format!("DGROUP:_{name}")
                } else {
                    format!("DGROUP:_{name}+{total_off}")
                };
                let _ = write!(self.out, "\tcall\tword ptr {label}\r\n");
            } else if let LocalLocation::Stack(base_bp) =
                self.locals.location_of(&name)
            {
                let off = base_bp + i16::try_from(total_off).unwrap_or(i16::MAX);
                let _ = write!(self.out, "\tcall\tword ptr {}\r\n", bp_addr(off));
            } else {
                panic!(
                    "CallVia: member chain rooted at non-stack local `{name}` not yet supported"
                );
            }
        } else {
            panic!("CallVia: unsupported address expression shape (no fixture yet)");
        }
        // Caller-cleanup: pop cx per word ≤ 4 bytes, add sp,N for ≥ 6.
        if total_bytes == 0 {
            // nothing
        } else if total_bytes <= 4 {
            for _ in 0..(total_bytes / 2) {
                self.out.extend_from_slice(b"\tpop\tcx\r\n");
            }
        } else {
            let _ = write!(self.out, "\tadd\tsp,{total_bytes}\r\n");
        }
    }
    pub(crate) fn emit_call(&mut self, name: &str, args: &[Expr]) {
        let param_tys = self.signatures.params_of(name);
        // Pre-intern string literal args in SOURCE (left-to-right)
        // order before the right-to-left push loop. BCC pools
        // strings in source order regardless of push order.
        // Fixture 2196 (`printf("%s\n", "Hello")` — pool layout
        // `"%s\n\0Hello\0"`, not `"Hello\0%s\n\0"`).
        fn intern_strings_in_order(emitter: &mut FunctionEmitter<'_>, e: &Expr) {
            match &e.kind {
                ExprKind::StringLit(bytes) => {
                    emitter.strings.intern_at(e.span.start, bytes);
                }
                ExprKind::BinOp { left, right, .. }
                | ExprKind::Logical { left, right, .. }
                | ExprKind::Comma { left, right } => {
                    intern_strings_in_order(emitter, left);
                    intern_strings_in_order(emitter, right);
                }
                ExprKind::Unary { operand, .. }
                | ExprKind::Cast { operand, .. }
                | ExprKind::Deref(operand) => {
                    intern_strings_in_order(emitter, operand);
                }
                ExprKind::Ternary { cond, then_value, else_value } => {
                    intern_strings_in_order(emitter, cond);
                    intern_strings_in_order(emitter, then_value);
                    intern_strings_in_order(emitter, else_value);
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        intern_strings_in_order(emitter, a);
                    }
                }
                ExprKind::ArrayIndex { array, index } => {
                    intern_strings_in_order(emitter, array);
                    intern_strings_in_order(emitter, index);
                }
                ExprKind::Member { base, .. } => {
                    intern_strings_in_order(emitter, base);
                }
                _ => {}
            }
        }
        for arg in args {
            intern_strings_in_order(self, arg);
        }
        let is_pascal_callee = self.signatures.is_pascal(name);
        let mut total_bytes: u32 = 0;
        // Track whether any FP-typed arg was pushed via fstp so a
        // single trailing `fwait` lands just before the call (BCC's
        // shape). Fixtures 1678, 2195.
        let mut fp_arg_pushed = false;
        // Pascal convention pushes args LEFT-TO-RIGHT; C pushes
        // RIGHT-TO-LEFT. Walk the iteration in the matching order.
        let arg_order: Box<dyn Iterator<Item = (usize, &Expr)>> = if is_pascal_callee {
            Box::new(args.iter().enumerate())
        } else {
            Box::new(args.iter().enumerate().rev())
        };
        for (i, arg) in arg_order {
            // Param type for the i-th arg, defaulting to int when the
            // signature isn't known (extern function — no fixture yet).
            // For variadic args past the named prototype list, infer
            // from the expression's own type so longs/structs get
            // pushed as the full byte count. Fixture 2197
            // (`printf("...%ld...", long_var)`).
            let declared_ty = param_tys
                .and_then(|tys| tys.get(i))
                .cloned();
            let arg_ty = declared_ty.unwrap_or_else(|| {
                if self.expr_is_long_like(arg) {
                    Type::Long
                } else if matches!(arg.kind, ExprKind::DoubleLit(_)) {
                    Type::Double
                } else if matches!(arg.kind, ExprKind::FloatLit(_)) {
                    // C's default argument promotion widens float to
                    // double in variadic calls — pushed as 8 bytes.
                    Type::Double
                } else if self.operand_is_float_like(arg) {
                    // Variadic FP arg via an Ident or BinOp — float
                    // is promoted to double per default arg promotion.
                    // Fixtures 2198, 2201 (printf with `float f` /
                    // `double d` locals).
                    Type::Double
                } else {
                    Type::Int
                }
            });
            if arg_ty.is_long_like() {
                // Long arg: materialize (AX=high, DX=low), push
                // high then low. 4 bytes per arg. Fixture 216.
                self.emit_long_arg_push(arg);
                total_bytes += 4;
            } else if arg_ty.is_float_like() {
                // Float/double arg: the FPU can't `push` directly,
                // so BCC allocates stack space first (`add sp,
                // -size`, encoded with the longer 81 C4 form),
                // then `fstp <width> ptr [bp-<slot>]` writes the
                // FPU top into the slot. A trailing `fwait` (one
                // per FP arg) syncs before the call. Fixture 1678.
                let size = u32::from(arg_ty.size_bytes());
                let store_width =
                    if matches!(arg_ty, Type::Float) { "dword" } else { "qword" };
                self.emit_float_load_to_fpu(arg);
                // BCC emits `add sp, <unsigned-of-negative>` to
                // make the imm16 form (81 C4 lo hi) rather than the
                // sign-extended-i8 form (83 C4 ii). The semantics
                // are identical (SP -= size); the byte encoding
                // matters for byte-exact match.
                let neg = (-(size as i32)) as u16;
                let _ = write!(self.out, "\tadd\tsp,{neg}\r\n");
                let slot_off = -(i32::from(self.locals.stack_bytes())
                    + total_bytes as i32
                    + size as i32);
                let slot_off_i16 =
                    i16::try_from(slot_off).expect("FP arg slot fits in i16");
                let _ = write!(
                    self.out,
                    "\tfstp\t{store_width} ptr {}\r\n",
                    bp_addr(slot_off_i16),
                );
                // Where to emit the fwait sync depends on how many
                // args remain to push after this one:
                //   - 0 or 1 (just the format string in a printf):
                //     defer until immediately before the call so it
                //     lands AFTER the remaining push.
                //   - 2+: emit it now, before the other arg pushes.
                // Pushes happen right-to-left so after processing the
                // arg at source index `i`, args at indices 0..i still
                // remain to push.
                let remaining_after = if is_pascal_callee {
                    args.len().saturating_sub(i + 1)
                } else {
                    i
                };
                if remaining_after >= 2 {
                    self.out.extend_from_slice(b"\tfwait\t\r\n");
                } else {
                    fp_arg_pushed = true;
                }
                total_bytes += size;
            } else if let Type::FarPointer { .. } = &arg_ty {
                // Far-pointer arg — 4 bytes (segment + offset). The
                // common cases:
                //   * Stack array passed to a `T far *` param —
                //     `push ss; lea ax,[bp+arr]; push ax`. BCC's
                //     emission order: ss first, then the LEA, then
                //     push ax. Fixture 1870 (`fill(x)` under -ml,
                //     `void fill(int *a)`).
                //   * A `T far *` local — push both halves of the
                //     stored pair (high then low).
                //   * `&<global>` or `&<stack_local>` — push the
                //     matching segment then the LEA / offset.
                let mut handled = false;
                if let ExprKind::Ident(name) = &arg.kind
                    && self.locals.has(name)
                {
                    let t = self.locals.type_of(name).clone();
                    if let Type::Array { .. } = &t
                        && let LocalLocation::Stack(arr_off) = self.locals.location_of(name)
                    {
                        self.out.extend_from_slice(b"\tpush\tss\r\n");
                        let _ = write!(
                            self.out,
                            "\tlea\tax,word ptr {}\r\n",
                            bp_addr(arr_off),
                        );
                        self.out.extend_from_slice(b"\tpush\tax\r\n");
                        handled = true;
                    } else if matches!(t, Type::FarPointer { .. })
                        && let LocalLocation::Stack(p_off) = self.locals.location_of(name)
                    {
                        // Pass the stored seg:off pair through —
                        // push high (segment) then low (offset).
                        let _ = write!(
                            self.out,
                            "\tpush\tword ptr {}\r\n",
                            bp_addr(p_off + 2),
                        );
                        let _ = write!(
                            self.out,
                            "\tpush\tword ptr {}\r\n",
                            bp_addr(p_off),
                        );
                        handled = true;
                    }
                }
                if !handled
                    && let ExprKind::AddressOf(sym) = &arg.kind
                    && self.globals.type_of(sym).is_some()
                {
                    self.out.extend_from_slice(b"\tpush\tds\r\n");
                    let _ = write!(
                        self.out,
                        "\tmov\tax,offset DGROUP:_{sym}\r\n",
                    );
                    self.out.extend_from_slice(b"\tpush\tax\r\n");
                    handled = true;
                }
                assert!(
                    handled,
                    "far-pointer arg shape not yet supported: {:?}",
                    arg.kind,
                );
                total_bytes += 4;
            } else if let Type::Struct { .. } = &arg_ty {
                // Struct-by-value arg. Two shapes by size:
                //   - 4 bytes: push two words high-first, identical
                //     to a long-arg push (fixture 419 byte-matches
                //     fixture 322's long shape).
                //   - > 4 bytes: route through `N_SPUSH@`. Helper
                //     takes the source far pointer in DX:AX and the
                //     byte count in CX; it pushes the bytes onto the
                //     caller's stack in place. Fixture 420.
                let size = arg_ty.size_bytes() as u32;
                let ExprKind::Ident(src_name) = &arg.kind else {
                    panic!("non-ident struct-by-value arg not yet supported (no fixture)");
                };
                let src_is_global = self.globals.type_of(src_name).is_some();
                if size == 2 {
                    // 2-byte struct (one int field) — just push the
                    // word. Fixture 3100.
                    if src_is_global {
                        let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{src_name}\r\n");
                    } else {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                        else {
                            panic!("struct local `{src_name}` not stack-resident");
                        };
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(src_off));
                    }
                } else if size == 4 {
                    if src_is_global {
                        let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{src_name}+2\r\n");
                        let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{src_name}\r\n");
                    } else {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                        else {
                            panic!("struct local `{src_name}` not stack-resident");
                        };
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(src_off + 2));
                        let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(src_off));
                    }
                } else {
                    if src_is_global {
                        let _ = write!(self.out, "\tmov\tax,offset DGROUP:_{src_name}\r\n");
                        self.out.extend_from_slice(b"\tmov\tdx,ds\r\n");
                    } else {
                        let LocalLocation::Stack(src_off) = self.locals.location_of(src_name)
                        else {
                            panic!("struct local `{src_name}` not stack-resident");
                        };
                        let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(src_off));
                        self.out.extend_from_slice(b"\tmov\tdx,ss\r\n");
                    }
                    let _ = write!(self.out, "\tmov\tcx,{size}\r\n");
                    self.out.extend_from_slice(b"\tcall\tnear ptr N_SPUSH@\r\n");
                    self.helpers.insert("N_SPUSH@".to_string());
                }
                total_bytes += size;
            } else if let ExprKind::ArrayIndex { array, index } = &arg.kind
                && let ExprKind::Ident(pname) = &array.kind
                && let Some(idx) = try_const_eval(index)
                && let Some(pointee) = self.near_ptr_ident_pointee(pname)
                && pointee.is_int_like()
                && !pointee.is_char_like()
                && !arg_ty.is_char_like()
                && !arg_ty.is_long_like()
            {
                // `f(p[K])` for a near pointer p and constant K — load the
                // pointer into BX and push the pointee word directly
                // (`mov bx,<p>; push word ptr [bx+off]`), folding K*stride
                // into the displacement, instead of `mov ax,[bx+off]; push
                // ax`. Fixture 893 (`f(p[1])` for a global `int *p`).
                // Mirrors the memory-direct pointer-subscript compare.
                let stride = i32::from(pointee.size_bytes());
                let off = (idx as i32).wrapping_mul(stride);
                self.emit_load_near_ptr_to_bx(pname);
                let bx_disp = if off == 0 {
                    "[bx]".to_owned()
                } else if off > 0 {
                    format!("[bx+{off}]")
                } else {
                    format!("[bx-{}]", -off)
                };
                let _ = write!(self.out, "\tpush\tword ptr {bx_disp}\r\n");
                total_bytes += 2;
            } else if let Some(push_form) = self.try_direct_arg_push(arg, &arg_ty) {
                // Memory-operand peephole: when the arg is a simple
                // load (stack-local int/ptr, global int/ptr, or a
                // const-index array element resolving to one of those),
                // skip the `mov ax, m / push ax` pair and emit `push
                // word ptr <m>` directly. Fixture 589 (`f(a[1])`).
                let _ = write!(self.out, "\t{push_form}\r\n");
                total_bytes += 2;
            } else if let ExprKind::Comma { left, right } = &arg.kind
                && let Some(push_form) = self.try_direct_arg_push(right, &arg_ty)
            {
                // Comma-arg: emit the LHS for side effects (typically
                // an assignment), then push the simple RHS directly.
                // Fixture 2315 (`sum2((x = 10, x), (x = 20, x))` for
                // x in SI → `mov si, 10; push si; mov si, 20; push
                // si`).
                self.emit_expr_discard(left);
                let _ = write!(self.out, "\t{push_form}\r\n");
                total_bytes += 2;
            } else if !arg_ty.is_char_like()
                && let ExprKind::BinOp { op: BinOp::Mod, .. } = &arg.kind
            {
                // Mod-result arg: the idiv already leaves the
                // remainder in DX. Skip the `mov ax, dx` and push
                // DX directly. Saves 2 bytes per call. Fixture 1391
                // (`gcd(b, a % b)`).
                self.skip_mod_to_ax = true;
                self.emit_arg_into_ax(arg, arg_ty);
                self.skip_mod_to_ax = false;
                self.out.extend_from_slice(b"\tpush\tdx\r\n");
                total_bytes += 2;
            } else if self.target_186
                && let Some(k) = try_const_eval(arg)
                && (k as i32) >= -128
                && (k as i32) <= 127
                && !arg_ty.is_char_like()
            {
                // 186+ `push imm8` (sign-extended to imm16): 2-byte
                // `6a ii` form for small integer constants. Saves
                // 2 bytes per arg vs `mov ax, K; push ax`. Fixture
                // 2277 (`fn(10)`).
                let k_i8 = k as i8;
                let _ = write!(self.out, "\tpush\t{k_i8}\r\n");
                total_bytes += 2;
            } else {
                self.emit_arg_into_ax(arg, arg_ty);
                self.out.extend_from_slice(b"\tpush\tax\r\n");
                total_bytes += 2;
            }
        }
        // Flush a pending FP-arg fwait right before the call. BCC's
        // shape: when any arg was pushed via fstp, the fwait sync
        // sits immediately before the call (not between fstp and
        // subsequent arg pushes). Fixtures 1678, 2195.
        if fp_arg_pushed {
            self.out.extend_from_slice(b"\tfwait\t\r\n");
        }
        // Struct-returning callee (size ∉ {1, 2, 4}): caller passes
        // a hidden far pointer to the tmp buffer as the *last* push
        // before the call, so the callee reads it from [bp+4..7].
        // BCC's emission order is `push ss; lea ax, ...; push ax` —
        // the segment goes down before the offset is computed (vs
        // the regular dest/src far-ptr pushes which compute the
        // offset first, then push segment, then push offset). Adds 4
        // to the post-call cleanup byte count. Fixtures 1685, 1877,
        // 2207, 2352.
        let hidden_ret_ptr_off = self.pending_hidden_ret_ptr_tmp_off.take();
        if let Some(tmp_off) = hidden_ret_ptr_off {
            self.out.extend_from_slice(b"\tpush\tss\r\n");
            let _ = write!(self.out, "\tlea\tax,word ptr {}\r\n", bp_addr(tmp_off));
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            total_bytes += 4;
        }
        // Direct call to a function symbol vs. indirect call through
        // a function-pointer local. The disambiguator is whether
        // `name` names a local in this frame (fixture 110): if so,
        // emit `call word ptr [bp-N]`; otherwise `call near ptr _N`.
        if self.locals.has(name) {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                panic!(
                    "indirect call through register-resident fn-ptr `{name}` not yet supported"
                );
            };
            // Far-code memory models store fn pointers as 4-byte
            // segment:offset pairs; the indirect call uses
            // `call far ptr [bp+off]` (`ff /3`) instead of the
            // near-call `ff /2`. Fixture 2211.
            if matches!(self.locals.type_of(name), Type::FarPointer { .. }) {
                let _ = write!(self.out, "\tcall\tfar ptr {}\r\n", bp_addr(off));
            } else {
                let _ = write!(self.out, "\tcall\tword ptr {}\r\n", bp_addr(off));
            }
        } else if let Some(gty) = self.globals.type_of(name)
            && gty.pointee().is_some()
            && self.signatures.params_of(name).is_none()
        {
            // Global function pointer: `int (*op)(int);` then `op(7)`.
            // BCC emits an indirect-memory call. Fixtures 2607, 3212,
            // 3567, 2913.
            let _ = write!(
                self.out,
                "\tcall\tword ptr DGROUP:_{name}\r\n",
            );
        } else {
            // Three forms:
            //   1. Calls to a function defined in this TU that is
            //      marked far (per-function `is_far`): push CS,
            //      call near — same segment, but the callee's
            //      retf needs CS:IP. Fixture 1654.
            //   2. Calls to externs under medium / large / huge
            //      models: true far call (`9a` opcode + 4-byte
            //      seg:off, fixed up at link time). Fixture 2210
            //      (`printf("hi\n")` from medium-model main).
            //   3. Everything else: plain `call near`.
            let callee_in_tu_far = self.signatures.is_far(name);
            let callee_is_extern = self.signatures.is_extern_function(name);
            let use_call_far = self.model_has_far_code && callee_is_extern;
            if callee_in_tu_far && !use_call_far {
                self.out.extend_from_slice(b"\tpush\tcs\r\n");
            }
            if use_call_far {
                let _ = write!(self.out, "\tcall\tfar ptr _{name}\r\n");
            } else if is_pascal_callee {
                let target = function_symbol_pascal(name);
                let _ = write!(self.out, "\tcall\tnear ptr {target}\r\n");
            } else {
                let _ = write!(self.out, "\tcall\tnear ptr _{name}\r\n");
            }
        }
        // Cleanup: BCC uses `pop cx` per word when total ≤ 4 bytes,
        // `add sp, N` for 6 bytes or more. The threshold is shared
        // across int and long args — fixture 216's single long arg
        // pushes 4 bytes and gets 2 pops, mirroring the 2-int-args
        // shape. Pascal callees clean their own stack via `ret N`,
        // so the caller emits no cleanup.
        if is_pascal_callee || total_bytes == 0 {
            // nothing
        } else if total_bytes <= 4 {
            for _ in 0..(total_bytes / 2) {
                self.out.extend_from_slice(b"\tpop\tcx\r\n");
            }
        } else {
            let _ = write!(self.out, "\tadd\tsp,{total_bytes}\r\n");
        }
    }
    /// Push a long argument onto the call stack as two words, **high
    /// half first, then low half** — so the low half ends up at the
    /// lower bp-offset in the callee. Per BCC's calling convention.
    /// Const args materialize into AX/DX first (fixture 216);
    /// lvalues with known addresses push memory-direct (fixtures
    /// 322–325).
    pub(crate) fn emit_long_arg_push(&mut self, arg: &Expr) {
        if let Some(v) = try_const_eval(arg) {
            let lo = v & 0xFFFF;
            let hi = (v >> 16) & 0xFFFF;
            if hi == 0 {
                self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{hi}\r\n");
            }
            if lo == 0 {
                self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tdx,{lo}\r\n");
            }
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            return;
        }
        // Long global ident — push both halves memory-direct via
        // `push word ptr DGROUP:_g+2 / push word ptr DGROUP:_g`.
        // Fixture 322.
        if let ExprKind::Ident(name) = &arg.kind
            && let Some(gty) = self.globals.type_of(name)
            && gty.is_long_like()
        {
            let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}+2\r\n");
            let _ = write!(self.out, "\tpush\tword ptr DGROUP:_{name}\r\n");
            return;
        }
        // Long stack local — push both halves via `push word ptr
        // [bp+off+2] / push word ptr [bp+off]`. Fixture 323.
        if let ExprKind::Ident(name) = &arg.kind
            && self.locals.has(name)
            && self.locals.type_of(name).is_long_like()
        {
            let LocalLocation::Stack(off) = self.locals.location_of(name) else {
                unreachable!("long is never register-resident");
            };
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(off + 2));
            let _ = write!(self.out, "\tpush\tword ptr {}\r\n", bp_addr(off));
            return;
        }
        // `*p` for `p: long *` register-resident — push both halves
        // through the pointer register. Fixture 325.
        if let ExprKind::Deref(operand) = &arg.kind
            && let ExprKind::Ident(ptr_name) = &operand.kind
            && self.locals.has(ptr_name)
            && let Some(pointee) = self.locals.type_of(ptr_name).pointee()
            && pointee.is_long_like()
            && let LocalLocation::Reg(reg) = self.locals.location_of(ptr_name)
        {
            let r = reg.name();
            let _ = write!(self.out, "\tpush\tword ptr [{r}+2]\r\n");
            let _ = write!(self.out, "\tpush\tword ptr [{r}]\r\n");
            return;
        }
        // Long dot-chain lvalue (`s.x`, `a[K].x`, …) — push both
        // halves memory-direct at the resolved address. Fixture 326.
        if let ExprKind::Member { base, field, kind: crate::ast::MemberKind::Dot } = &arg.kind
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
            let _ = write!(self.out, "\tpush\tword ptr {hi_addr}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lo_addr}\r\n");
            return;
        }
        // Long array element (const index) on a global — push both
        // halves at `_a + K*4`. Fixture 328.
        if let ExprKind::ArrayIndex { array: arr_expr, index } = &arg.kind
            && let ExprKind::Ident(arr_name) = &arr_expr.kind
            && let Some(arr_ty) = self.globals.type_of(arr_name)
            && let Some(elem) = arr_ty.array_elem()
            && elem.is_long_like()
            && let Some(k) = try_const_eval(index)
        {
            let byte_off = (k as i32) * 4;
            let lo_addr = global_offset_addr(arr_name, byte_off);
            let hi_addr = global_offset_addr(arr_name, byte_off + 2);
            let _ = write!(self.out, "\tpush\tword ptr {hi_addr}\r\n");
            let _ = write!(self.out, "\tpush\tword ptr {lo_addr}\r\n");
            return;
        }
        // Long arg from a two-lvalue arith/bitwise expression
        // (`f(a + b)`, `f(a & b)`, …). Compute into AX:DX using the
        // memory-dest register convention (AX=high, DX=low), then
        // push high (AX) first / low (DX) second so the long lands
        // on the stack with low at the lower address. Fixture 386.
        if let ExprKind::BinOp { op, left, right } = &arg.kind
            && let Some((lo_op, hi_op)) = long_pair_op(*op)
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\t{lo_op}\tdx,word ptr {b_lo}\r\n");
            let _ = write!(self.out, "\t{hi_op}\tax,word ptr {b_hi}\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            return;
        }
        // Long arg from a long-returning function call (`f(g())`
        // where `long g();`). The call leaves the result in DX:AX
        // (cdecl long-return ABI: DX=high, AX=low) — so to push
        // high first BCC emits `push dx; push ax`. Note the
        // *order* of pushes is flipped relative to the memory-
        // dest path (fixture 386: `push ax; push dx`), because
        // the producer step left the registers in the opposite
        // convention. The push pair adapts to whatever the producer
        // left in DX:AX. Fixture 387.
        if let ExprKind::Call { name: fname, args } = &arg.kind
            && args.is_empty()
        {
            let _ = write!(self.out, "\tcall\tnear ptr _{fname}\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            return;
        }
        // Long arg from a long mul (`f(g * h)`). Same passthrough
        // pattern as the call case: helper returns DX:AX = high:
        // low, so `push dx; push ax`. First operand → CX:BX,
        // second → DX:AX (helper convention). Fixture 388.
        if let ExprKind::BinOp { op: BinOp::Mul, left, right } = &arg.kind
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(left)
            && let Some((b_hi, b_lo)) = self.long_lvalue_addr_pair(right)
        {
            let _ = write!(self.out, "\tmov\tcx,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tbx,word ptr {a_lo}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {b_hi}\r\n");
            let _ = write!(self.out, "\tmov\tax,word ptr {b_lo}\r\n");
            self.out.extend_from_slice(b"\tcall\tnear ptr N_LXMUL@\r\n");
            self.helpers.insert("N_LXMUL@".to_string());
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            return;
        }
        // `-<long>` — negate a long lvalue and push it. Load the value into
        // AX:DX (high:low), apply the 32-bit negation idiom (negate both halves,
        // propagate the borrow into the high half via `sbb ax,0`), then push
        // high-half first. Fixture 4318 (`sink(-a)`).
        if let ExprKind::Unary { op: UnaryOp::Neg, operand } = &arg.kind
            && let Some((a_hi, a_lo)) = self.long_lvalue_addr_pair(operand)
        {
            let _ = write!(self.out, "\tmov\tax,word ptr {a_hi}\r\n");
            let _ = write!(self.out, "\tmov\tdx,word ptr {a_lo}\r\n");
            self.out.extend_from_slice(b"\tneg\tax\r\n");
            self.out.extend_from_slice(b"\tneg\tdx\r\n");
            self.out.extend_from_slice(b"\tsbb\tax,0\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            return;
        }
        // `(long)<int>` — an int/char value widened to a long argument. Load it
        // into AX, sign-extend (`cwd`) or zero-extend (`xor dx,dx`) into DX:AX,
        // then push the long high-half first (`push dx; push ax`). Fixture 4317
        // (`sink((long)i)`).
        if let ExprKind::Cast { ty, operand } = &arg.kind
            && ty.is_long_like()
        {
            self.emit_expr_to_ax(operand);
            if self.expr_int_is_unsigned(operand) {
                self.out.extend_from_slice(b"\txor\tdx,dx\r\n");
            } else {
                self.out.extend_from_slice(b"\tcwd\t\r\n");
            }
            self.out.extend_from_slice(b"\tpush\tdx\r\n");
            self.out.extend_from_slice(b"\tpush\tax\r\n");
            return;
        }
        panic!("non-constant long argument not yet supported (no fixture)");
    }
}
