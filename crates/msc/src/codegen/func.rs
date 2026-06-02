use crate::*;

/// Returns true if the function body contains any runtime local array
/// access (read or write with non-constant index). These require the SI
/// register to be saved/restored, upgrading the frame to WithSlideSi.
pub(crate) fn body_needs_si(stmts: &[Stmt], local_inits: &[Option<i32>]) -> bool {
    fn stmt_si(s: &Stmt, inits: &[Option<i32>]) -> bool {
        match s {
            Stmt::Assign { target, value } => {
                matches!(target,
                    AssignTarget::IndexedLocalVar { .. }
                    | AssignTarget::IndexedLocalByteVar { .. })
                    || expr_si(value, inits)
            }
            Stmt::Return(e) => expr_si(e, inits),
            Stmt::ExprStmt(e) => expr_si(e, inits),
            Stmt::Block(ss) => ss.iter().any(|s| stmt_si(s, inits)),
            Stmt::If { cond, then_branch, else_branch } => {
                cond_si(cond, inits)
                    || stmt_si(then_branch, inits)
                    || else_branch.as_ref().is_some_and(|e| stmt_si(e, inits))
            }
            Stmt::While { cond, body } => cond_si(cond, inits) || stmt_si(body, inits),
            Stmt::DoWhile { body, cond } => stmt_si(body, inits) || cond_si(cond, inits),
            Stmt::For { init, cond, step, body } => {
                stmt_si(init, inits)
                    || cond_si(cond, inits)
                    || stmt_si(step, inits)
                    || stmt_si(body, inits)
            }
            _ => false,
        }
    }
    fn expr_si(e: &Expr, inits: &[Option<i32>]) -> bool {
        match e {
            Expr::LocalIndex { index, .. } | Expr::LocalIndexByte { index, .. } => {
                index.fold(inits).is_none()
            }
            Expr::BinOp { left, right, .. } => expr_si(left, inits) || expr_si(right, inits),
            Expr::Call { args, .. } => args.iter().any(|a| expr_si(a, inits)),
            _ => false,
        }
    }
    fn cond_si(c: &Cond, inits: &[Option<i32>]) -> bool {
        match c {
            Cond::Truthy(e) => expr_si(e, inits),
            Cond::Cmp { left, right, .. } => expr_si(left, inits) || expr_si(right, inits),
            Cond::And(a, b) | Cond::Or(a, b) => cond_si(a, inits) || cond_si(b, inits),
        }
    }
    stmts.iter().any(|s| stmt_si(s, local_inits))
}
/// Return the mod=01 or mod=10 modrm byte for `[bp + disp]`.
/// When `disp` fits in i8 (-128..=127) use mod=01 (1-byte disp, modrm as-is).
/// Otherwise use mod=10 (2-byte disp, modrm + 0x40 to flip the mod field).
#[inline]
pub(crate) fn bp_modrm(modrm_mod01: u8, disp: i16) -> u8 {
    if (-128..=127).contains(&disp) { modrm_mod01 } else { modrm_mod01 + 0x40 }
}
/// Push 1 or 2 displacement bytes for a `[bp + disp]` operand.
#[inline]
pub(crate) fn push_bp_disp(buf: &mut Vec<u8>, disp: i16) {
    if (-128..=127).contains(&disp) {
        buf.push(disp as u8);
    } else {
        let b = (disp as u16).to_le_bytes();
        buf.push(b[0]);
        buf.push(b[1]);
    }
}
pub(crate) fn emit_function(
    func: &Function,
    long_globals: &[bool],
    char_globals: &[bool],
    unsigned_globals: &[bool],
    char_returners: &std::collections::HashSet<String>,
    long_param_funcs: &std::collections::HashMap<String, Vec<bool>>,
) -> FunctionEmit {
    let (body, mutated_locals, _mutated_globals) = const_prop_globals(&func.body, &func.locals, long_globals);
    // Extract a `Vec<Option<i32>>` view for the existing fold path —
    // saves rewriting every codegen helper to know about LocalSpec.
    // Strip the init for any local that was mutated during the
    // const-prop walk — the static `spec.init` no longer reflects
    // the runtime value, so fold(locals.inits) must miss for it.
    // Fixture 1154 (`int b = a++; return a + b;`).
    // Only expose init values for locals whose init is a compile-time
    // constant: pure literal, chained literal, or an identity-operation
    // of a literal-init local (e.g. `int a = x << 0` where x=5).
    // MSC does NOT fold type-cast inits (`int i = (int)sc`, fixture 1732)
    // or derived arithmetic (`int sum = x + y`, fixture 2126).
    // These cases leave `init_is_literal = false` via the parser's logic.
    let local_inits: Vec<Option<i32>> = func.locals.iter().enumerate()
        .map(|(i, l)| if mutated_locals.contains(&i) || !l.init_is_literal { None } else { l.init })
        .collect();
    let local_long: Vec<bool> = func.locals.iter().map(|l| l.is_long).collect();
    let local_literals: Vec<bool> = func.locals.iter().map(|l| l.init_is_literal).collect();
    let local_far_ptrs: Vec<bool> = func.locals.iter().map(|l| l.is_far_ptr).collect();
    let local_arrays: Vec<bool> = func.locals.iter().map(|l| l.array_len > 1).collect();
    let local_unsigned: Vec<bool> = func.locals.iter().map(|l| l.is_unsigned).collect();
    let mut bytes = Vec::with_capacity(32);
    let mut fixups: Vec<Fixup> = Vec::new();
    let base_frame = Frame::for_function(func);
    // Upgrade to WithSlideSi when the body uses runtime local array
    // indexing (SI register must be caller-saved).
    let frame = if matches!(base_frame, Frame::WithSlide)
        && body_needs_si(&body, &local_inits)
    {
        Frame::WithSlideSi
    } else {
        base_frame
    };
    // Two-pass frame layout: far/huge pointer locals (4-byte seg:off slots)
    // get the shallowest positions first (in source order), then all other
    // locals in source order.  For functions with no far pointers this
    // reduces to simple source-order allocation.
    let mut local_disps: Vec<i16> = vec![0i16; func.locals.len()];
    // Three-pass frame layout:
    //   Pass 1: far/huge pointer locals (4-byte seg:off slots) — shallowest.
    //   Pass 2: near pointer locals (pointee_size > 0) — intermediate.
    //   Pass 3: non-pointer locals sorted by sum_of_name_chars % 16
    //           (ascending). This mirrors MSC's internal hash-table
    //           traversal order: hash bucket determines slot depth, lower
    //           bucket → closer to BP (smaller absolute displacement).
    let mut cumulative: i32 = 0;
    for (i, spec) in func.locals.iter().enumerate() {
        if spec.is_far_ptr {
            cumulative += spec.storage_bytes() as i32;
            local_disps[i] = -i16::try_from(cumulative).expect("local disp fits");
        }
    }
    for (i, spec) in func.locals.iter().enumerate() {
        if !spec.is_far_ptr && spec.pointee_size > 0 {
            cumulative += spec.storage_bytes() as i32;
            local_disps[i] = -i16::try_from(cumulative).expect("local disp fits");
        }
    }
    {
        let mut np_indices: Vec<usize> = func.locals.iter().enumerate()
            .filter(|(_, s)| !s.is_far_ptr && s.pointee_size == 0)
            .map(|(i, _)| i)
            .collect();
        let name_hash = |i: usize| -> u32 {
            let h: u32 = func.local_names[i].bytes().map(|b| b as u32).sum();
            h % 16
        };
        // Within a hash bucket, MSC's internal hash table uses LIFO
        // (linked-list prepend), so the last-declared variable in a
        // bucket gets the shallowest (smallest absolute) displacement.
        // Secondary key: descending source index to match that ordering.
        np_indices.sort_by(|&a, &b| {
            name_hash(a).cmp(&name_hash(b)).then(b.cmp(&a))
        });
        for i in np_indices {
            let spec = &func.locals[i];
            cumulative += spec.storage_bytes() as i32;
            local_disps[i] = -i16::try_from(cumulative).expect("local disp fits");
        }
    }
    let local_sizes: Vec<usize> = func.locals.iter().map(|l| l.size).collect();
    let frame_bytes: usize = cumulative as usize;

    match frame {
        Frame::None => bytes.extend_from_slice(&[0x33, 0xC0]),
        Frame::BpOnly => bytes.extend_from_slice(&[0x55, 0x8B, 0xEC, 0x33, 0xC0]),
        Frame::WithSlide | Frame::WithSlideSi => {
            bytes.extend_from_slice(&[0x55, 0x8B, 0xEC]);
            bytes.push(0xB8);
            bytes.extend_from_slice(
                &u16::try_from(frame_bytes)
                    .expect("frame fits in u16")
                    .to_le_bytes(),
            );
        }
    }
    // Call to __chkstk — emit a placeholder `e8 00 00` and record
    // a Fixup so the post-pass knows to write the EXTDEF-relative
    // FIXUPP record (chkstk is always external; resolved via the
    // OMF FIXUPP machinery, not in-band).
    let body_offset = bytes.len();
    bytes.extend_from_slice(&[0xE8, 0x00, 0x00]);
    fixups.push(Fixup {
        body_offset,
        kind: FixupKind::ExtCall { target: "__chkstk".to_owned() },
    });
    // For WithSlideSi: save SI after the frame is established.
    if matches!(frame, Frame::WithSlideSi) {
        bytes.push(0x56); // push si
    }

    // Initialized-local writes — `int x = K;` → `c7 46 disp lo hi`;
    // `char x = K;` → `c6 46 disp imm8`. Arrays don't get a constant
    // init here; their element stores live in the prelude body the
    // parser synthesized.
    for (i, spec) in func.locals.iter().enumerate() {
        // Float/double locals: load the literal from the CONST pool and
        // store it to the slot, all x87-wait-prefixed:
        //   9B <D9|DD> 06 <off16>   fld <dword|qword> [$T]   (FloatLoad fixup)
        //   9B <D9|DD> 5E <disp>    fstp <dword|qword> [bp+disp]
        //   [90]                    nop so the next statement is even-aligned
        //   9B                      fwait
        if spec.is_float {
            if let Some(bits) = spec.float_bits {
                let disp = local_disps[i];
                let op = if spec.size == 4 { 0xD9u8 } else { 0xDDu8 };
                // fld: FP-emulator marker on the leading 9B, FloatLoad on off16.
                fixups.push(Fixup { body_offset: bytes.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                bytes.push(0x9B);
                bytes.push(op);
                bytes.push(0x06);
                let body_offset = bytes.len() - 1; // the 06 modrm; off16 at +1
                bytes.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset, kind: FixupKind::FloatLoad { bits, width: spec.size } });
                // fstp: FP-emulator marker on the leading 9B.
                fixups.push(Fixup { body_offset: bytes.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                bytes.push(0x9B);
                bytes.push(op);
                bytes.push(bp_modrm(0x5E, disp));
                push_bp_disp(&mut bytes, disp);
                // fwait: marker on the leading byte (the alignment nop, if any,
                // else the 9B), targeting FIWRQQ.
                fixups.push(Fixup { body_offset: bytes.len(), kind: FixupKind::FloatMarker { target: "FIWRQQ" } });
                if bytes.len() % 2 == 0 {
                    bytes.push(0x90); // nop
                }
                bytes.push(0x9B); // fwait
            }
            continue;
        }
        if let Some(value) = spec.init {
            let disp = local_disps[i];
            if spec.is_long {
                let low = (value as u32 & 0xFFFF) as u16;
                let high = (((value as i32) >> 16) as u32 & 0xFFFF) as u16;
                if low == 0 && high == 0 {
                    // Zero-init peephole: `xor ax, ax; mov [bp-d_hi],
                    // ax; mov [bp-d_lo], ax` — 8 bytes vs 10 for the
                    // two `c7 46` stores. Fixture 1737.
                    bytes.extend_from_slice(&[0x2B, 0xC0]);
                    bytes.push(0x89);
                    bytes.push(bp_modrm(0x46, disp + 2));
                    push_bp_disp(&mut bytes, disp + 2);
                    bytes.push(0x89);
                    bytes.push(bp_modrm(0x46, disp));
                    push_bp_disp(&mut bytes, disp);
                } else {
                    // Long: two word stores. `c7 46 disp_low <lo>`
                    // then `c7 46 disp_high <hi>`.
                    bytes.push(0xC7);
                    bytes.push(bp_modrm(0x46, disp));
                    push_bp_disp(&mut bytes, disp);
                    bytes.extend_from_slice(&low.to_le_bytes());
                    bytes.push(0xC7);
                    bytes.push(bp_modrm(0x46, disp + 2));
                    push_bp_disp(&mut bytes, disp + 2);
                    bytes.extend_from_slice(&high.to_le_bytes());
                }
            } else if spec.size == 1 {
                let imm = (value as u32 & 0xFF) as u8;
                if spec.init_via_cast {
                    // Explicit `(char)` cast init: MSC evaluates through AL.
                    // `b0 imm8; 88 46 disp` — fixture 1039.
                    bytes.push(0xB0);
                    bytes.push(imm);
                    bytes.push(0x88);
                    bytes.push(bp_modrm(0x46, disp));
                    push_bp_disp(&mut bytes, disp);
                } else {
                    // Direct / chained literal init: compact byte-store.
                    // `c6 46 disp imm8` — fixtures 1040, 1045, 1046, etc.
                    bytes.push(0xC6);
                    bytes.push(bp_modrm(0x46, disp));
                    push_bp_disp(&mut bytes, disp);
                    bytes.push(imm);
                }
            } else {
                let imm = (value as u32 & 0xFFFF) as u16;
                bytes.push(0xC7);
                bytes.push(bp_modrm(0x46, disp));
                push_bp_disp(&mut bytes, disp);
                bytes.extend_from_slice(&imm.to_le_bytes());
            }
        }
    }

    let loop_stack: std::cell::RefCell<Vec<LoopCtx>> = std::cell::RefCell::new(Vec::new());
    let param_is_char: Vec<bool> = func.param_is_char.clone();
    let param_is_long: Vec<bool> = func.param_is_long.clone();
    let param_is_unsigned: Vec<bool> = func.param_is_unsigned.clone();
    let locals_view = Locals {
        inits: &local_inits,
        disps: &local_disps,
        sizes: &local_sizes,
        long_globals,
        char_globals,
        unsigned_globals,
        long_locals: &local_long,
        init_literals: &local_literals,
        far_ptr_locals: &local_far_ptrs,
        array_locals: &local_arrays,
        unsigned_locals: &local_unsigned,
        char_params: &param_is_char,
        long_params: &param_is_long,
        unsigned_params: &param_is_unsigned,
        char_returners,
        long_param_funcs,
        loop_stack: &loop_stack,
    };

    let mut reachable = true;
    let mut i = 0;
    while i < body.len() {
        if !reachable { break; }
        let stmt = &body[i];

        // Detect a partial switch (const literal scrutinee with NFC cases).
        // emit_partial_switch_with_continuation handles the switch + all
        // remaining statements in one shot and returns whether the function
        // is still reachable after the continuation.
        if let Stmt::Switch { scrutinee: Expr::IntLit(k), cases } = stmt {
            let has_nfc = cases.iter()
                .any(|a| matches!(a.value, Some(v) if v != 0 && v < *k));
            if has_nfc {
                let cont_reachable = emit_partial_switch_with_continuation(
                    *k,
                    cases,
                    &body[i + 1..],
                    &locals_view,
                    frame,
                    func.return_int,
                    func.return_long,
                    &mut bytes,
                    &mut fixups,
                );
                reachable = cont_reachable;
                break; // all remaining statements consumed inside the call
            }
        }

        emit_stmt(
            stmt,
            &locals_view,
            frame,
            func.return_int,
            func.return_long,
            &mut bytes,
            &mut fixups,
        );
        if stmt_always_returns(stmt, &locals_view) {
            reachable = false;
        }
        i += 1;
    }

    // Implicit return at the end of functions that fall off without an
    // explicit `return`. MSC emits leave+ret for both void and int-returning
    // functions. The epilogue shape follows the function's frame.
    if reachable {
        bytes.extend_from_slice(frame.epilogue_bytes());
    }

    if bytes.len() % 2 != 0 {
        bytes.push(0x90);
    }

    FunctionEmit { bytes, fixups }
}
/// Mangle a C function name into the OBJ symbol it produces.
/// MSC's small-model convention prefixes every function with `_`.
pub(crate) fn symbol_name(c_name: &str) -> String {
    format!("_{c_name}")
}
pub(crate) fn param_disp(idx: usize) -> i16 {
    i16::try_from(4 + (idx * 2)).expect("param disp fits in i16")
}
