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
                    | AssignTarget::IndexedLocalByteVar { .. }
                    | AssignTarget::Index2D { .. })
                    || matches!(target, AssignTarget::ParamIndexStore { index, .. } if index.fold(inits).is_none())
                    // `arr[i] = arr[j]` / `arr[i] ± arr[j]` (runtime indices): the
                    // RHS array read uses SI so the LHS index in BX survives.
                    || crate::codegen::assign::global_index_rhs_uses_si(target, value, inits)
                    // `*dst = *src [± K]` copies through a pointer param from a
                    // deref of another param — the source deref uses SI.
                    || matches!(target, AssignTarget::DerefParam(d)
                        if deref_param_src_is_param(value, *d))
                    // `*d++ = *s++` reads the source through SI so dst (BX) survives.
                    || matches!(target, AssignTarget::DerefPostMutateParam { .. }
                        if matches!(value, Expr::PostIncDeref { .. }))
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
            // `a[i]` on a pointer/array PARAM with a runtime index loads the
            // pointer into SI (`mov si,[bp+p]; mov ax,[bx+si]`).
            Expr::ParamIndex { index, .. } => index.fold(inits).is_none(),
            Expr::Index2D { .. } => true,
            // `*(ptr + i)` (and the nested `*(p + i + j)`) with a runtime index on
            // a pointer param/local uses SI (`mov si,[p]; mov ax,[bx+si]`); a
            // decayed global array uses BX only. add_deref_uses_si peels the
            // add-tree the same way emit_offset_deref does. Fixture 3468.
            Expr::DerefWord { ptr } | Expr::DerefByte { ptr } => {
                if let Expr::BinOp { op: BinOp::Add, left, right } = ptr.as_ref() {
                    add_deref_uses_si(ptr, inits)
                        || expr_si(left, inits) || expr_si(right, inits)
                } else {
                    expr_si(ptr, inits)
                }
            }
            Expr::BinOp { left, right, .. } => expr_si(left, inits) || expr_si(right, inits),
            Expr::Call { args, .. } => args.iter().any(|a| expr_si(a, inits)),
            // `*d++ = *s++` as an expression/condition reads the source via SI.
            Expr::AssignExpr { target, value } => {
                (matches!(target,
                    AssignTarget::DerefPostMutateParam { .. }
                    | AssignTarget::DerefPostMutateLocal { .. })
                    && matches!(value.as_ref(), Expr::PostIncDeref { .. }))
                    || expr_si(value, inits)
            }
            _ => false,
        }
    }
    // Does an offset-deref of `base` use SI? Mirrors emit_offset_deref's peel:
    // a nested `(p + i) + j` flattens to a Param/Local pointer base with a
    // runtime index sum (→ SI); a constant index uses a disp instead, and a
    // decayed global array (AddrOfGlobal base) uses BX only.
    fn add_deref_uses_si(base: &Expr, inits: &[Option<i32>]) -> bool {
        fn peel(base: &Expr, idx_runtime: bool, inits: &[Option<i32>]) -> bool {
            match base {
                Expr::BinOp { op: BinOp::Add, left, right } => {
                    let r_runtime = right.fold(inits).is_none();
                    peel(left, idx_runtime || r_runtime, inits)
                }
                Expr::Param(_) | Expr::Local(_) => idx_runtime,
                _ => false,
            }
        }
        peel(base, false, inits)
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
/// Structural check matching `deref_param_source`: is `value` a `*Param(s)` or
/// `*Param(s) ± K` with `s != dst`? Used by body_needs_si (no pointee-width
/// info available here; the corpus never mixes widths so the structural test
/// agrees with the codegen path).
fn deref_param_src_is_param(value: &Expr, dst: usize) -> bool {
    fn src_of(e: &Expr) -> Option<usize> {
        match e {
            Expr::DerefWord { ptr } | Expr::DerefByte { ptr } => match ptr.as_ref() {
                Expr::Param(s) => Some(*s),
                _ => None,
            },
            _ => None,
        }
    }
    let s = match value {
        Expr::BinOp { op: BinOp::Add | BinOp::Sub, left, .. } => src_of(left),
        _ => src_of(value),
    };
    matches!(s, Some(s) if s != dst)
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
/// The float local whose value a `return` consumes from x87 st(0) for the
/// FpuStack coupling. Recognizes a bare local (`(int)f`), `0 - local` (unary
/// negation → `fchs`), and `local <op> <floatlit>` (→ `fadd`/`fsub`/…).
pub(crate) fn coupled_return_local(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Local(i) => Some(*i),
        Expr::BinOp { op: BinOp::Sub, left, right }
            if matches!(left.as_ref(), Expr::IntLit(0)) =>
        {
            if let Expr::Local(i) = right.as_ref() { Some(*i) } else { None }
        }
        Expr::BinOp { left, right, .. } if matches!(right.as_ref(), Expr::FloatLit(..)) => {
            if let Expr::Local(i) = left.as_ref() { Some(*i) } else { None }
        }
        _ => None,
    }
}
pub(crate) fn emit_function(
    func: &Function,
    struct_temp_offset: Option<u16>,
    long_globals: &[bool],
    char_globals: &[bool],
    unsigned_globals: &[bool],
    float_globals: &[usize],
    global_elem_sizes: &[usize],
    char_returners: &std::collections::HashSet<String>,
    long_returners: &std::collections::HashSet<String>,
    pascal_fns: &std::collections::HashSet<String>,
    float_returners_arg: &std::collections::HashMap<String, usize>,
    long_param_funcs: &std::collections::HashMap<String, Vec<bool>>,
    struct_param_funcs: &std::collections::HashMap<String, Vec<usize>>,
    struct_return_funcs: &std::collections::HashMap<String, usize>,
    struct_is_union: &[bool],
    union_globals: &std::collections::HashSet<usize>,
) -> FunctionEmit {
    let (body, mutated_locals, _mutated_globals) = const_prop_globals(&func.body, &func.locals, long_globals, global_elem_sizes, struct_is_union, union_globals);
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
    let local_pointee: Vec<usize> = func.locals.iter().map(|l| if l.array_len > 1 { 0 } else { l.pointee_size as usize }).collect();
    let local_float: Vec<usize> = func.locals.iter().map(|l| if l.is_float { l.size } else { 0 }).collect();
    let fpu_live: std::cell::Cell<Option<usize>> = std::cell::Cell::new(None);
    let fpu_pending_fwait: std::cell::Cell<bool> = std::cell::Cell::new(false);
    // A `pascal` function pops its arguments in the epilogue (`ret N`); N is the
    // total stack footprint of the params (long=4, struct=even-padded, else 2).
    let pascal_cleanup: u16 = if func.is_pascal {
        (0..func.params.len()).map(|i| {
            if func.param_is_long.get(i).copied().unwrap_or(false) { 4u16 }
            else if func.param_struct_bytes.get(i).copied().unwrap_or(0) > 0 {
                ((func.param_struct_bytes[i] + 1) & !1) as u16
            } else { 2 }
        }).sum()
    } else { 0 };
    let mut bytes = Vec::with_capacity(32);
    let mut fixups: Vec<Fixup> = Vec::new();
    // A call that passes a float/double literal pushes it via `sub sp,N;
    // mov bx,sp; fstp [bx]`, sliding SP — so the function needs a WithSlide
    // frame (cleanup via `mov sp,bp`) plus a 2-byte temp for the call result.
    let has_float_arg_call = func_has_float_arg_call(func);
    // A `make().field` call spills its result to a frame temp sized by chkstk, so
    // the function needs a WithSlide frame even with no declared locals (2629).
    let base_frame = if has_float_arg_call || func.struct_field_temp_count > 0 {
        Frame::WithSlide
    } else {
        Frame::for_function(func)
    };
    // Receiving a float/double return (`double d = f();`) copies 8 bytes from
    // __fac via `movsw`, needing DI and SI saved → WithSlideDiSi.
    let receives_float_return = body.iter().any(|s| matches!(s,
        Stmt::Assign { target: AssignTarget::Local(_), value: Expr::Call { name, .. } }
            if float_returners_arg.contains_key(&symbol_name(name))));
    // `return (int)<float-returning call>` receives the result into a hidden
    // 8-byte temp, then `fld; call __ftol`. Same di+si copy frame.
    let returns_float_call = body.iter().any(|s| matches!(s,
        Stmt::Return(Expr::Call { name, .. })
            if float_returners_arg.contains_key(&symbol_name(name))));
    // Upgrade to WithSlideSi when the body uses runtime local array
    // indexing (SI register must be caller-saved).
    // Returning a struct > 4 bytes copies it to the `_BSS` temp via `movsw`,
    // needing DI+SI saved → WithSlideDiSi. Same for a caller RECEIVING a >4-byte
    // struct return (`dest = f()` → movsw from the temp).
    let receives_big_struct = body.iter().any(|s| matches!(s,
        Stmt::Assign { target: AssignTarget::Local(_) | AssignTarget::Global(_), value: Expr::Call { name, .. } }
            if struct_return_funcs.get(&symbol_name(name)).copied().unwrap_or(0) > 4));
    // A whole-struct copy `g1 = g2;` of >4 bytes lowers to `rep movsw`, needing
    // DI+SI saved. Fixture 3612.
    let copies_big_struct = body.iter().any(|s| matches!(s,
        Stmt::Assign { target: AssignTarget::StructGlobalCopy { bytes, .. } | AssignTarget::StructLocalCopy { bytes, .. }, .. }
            if *bytes > 4));
    let returns_big_struct = func.return_struct_bytes > 4 || receives_big_struct || copies_big_struct;
    // `register int` locals are accessed via SI (1st) / DI (2nd), saved across
    // the call. The locals still reserve their stack slots (so base_frame is
    // already WithSlide), but the frame upgrades to push/pop the register(s).
    // Fixtures 1550 (1 reg → SI), 2069 (2 regs → DI+SI).
    let reg_locals: Vec<usize> = func.locals.iter().enumerate()
        .filter(|(_, l)| l.is_register).map(|(i, _)| i).collect();
    let si_local = reg_locals.first().copied();
    let di_local = reg_locals.get(1).copied();
    let frame = if reg_locals.len() >= 2 {
        Frame::WithSlideDiSi
    } else if reg_locals.len() == 1 {
        Frame::WithSlideSi
    } else if returns_big_struct && matches!(base_frame, Frame::None) {
        // No locals (e.g. `return <global>`) — keep the bp-less prologue but add
        // di/si for the movsw copy.
        Frame::NoneDiSi
    } else if receives_float_return || returns_float_call || returns_big_struct {
        Frame::WithSlideDiSi
    } else if matches!(base_frame, Frame::WithSlide)
        && body_needs_si(&body, &local_inits)
    {
        Frame::WithSlideSi
    } else if matches!(base_frame, Frame::BpOnly)
        && body_needs_si(&body, &local_inits)
    {
        Frame::BpOnlySi
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
    // Block-level locals (declared inside a nested `{ ... }`) are laid out
    // separately, DEEPER than the function frame, by a dedicated pass below.
    // The three hash-bucket passes skip them.
    let mut cumulative: i32 = 0;
    for (i, spec) in func.locals.iter().enumerate() {
        if spec.is_far_ptr && spec.block_offset.is_none() {
            cumulative += spec.storage_bytes() as i32;
            local_disps[i] = -i16::try_from(cumulative).expect("local disp fits");
        }
    }
    let name_hash = |i: usize| -> u32 {
        let h: u32 = func.local_names[i].bytes().map(|b| b as u32).sum();
        h % 16
    };
    // Near pointers share the same hash-bucket / LIFO ordering as other locals
    // (so `int *p; int **pp;` puts the later `pp` shallower — fixtures 1232,
    // 1964), but as a separate pass they still precede the non-pointer locals.
    {
        let mut np_ptrs: Vec<usize> = func.locals.iter().enumerate()
            .filter(|(_, s)| !s.is_far_ptr && s.pointee_size > 0 && s.block_offset.is_none())
            .map(|(i, _)| i)
            .collect();
        np_ptrs.sort_by(|&a, &b| name_hash(a).cmp(&name_hash(b)).then(b.cmp(&a)));
        for i in np_ptrs {
            cumulative += func.locals[i].storage_bytes() as i32;
            local_disps[i] = -i16::try_from(cumulative).expect("local disp fits");
        }
    }
    {
        let mut np_indices: Vec<usize> = func.locals.iter().enumerate()
            .filter(|(_, s)| !s.is_far_ptr && s.pointee_size == 0 && s.block_offset.is_none())
            .map(|(i, _)| i)
            .collect();
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
    // Block-local pass: each block local sits at `-(F + block_offset)`, where
    // `F = cumulative` is the function frame size computed above. `block_offset`
    // is the parser-assigned cumulative depth (with slot reuse across sibling
    // blocks); the high-water mark extends the frame below `F`.
    let f = cumulative;
    let mut block_frame_max: i32 = 0;
    for (i, spec) in func.locals.iter().enumerate() {
        if let Some(off) = spec.block_offset {
            local_disps[i] = -i16::try_from(f + off as i32).expect("block local disp fits");
            block_frame_max = block_frame_max.max(off as i32);
        }
    }
    cumulative = f + block_frame_max;
    let local_sizes: Vec<usize> = func.locals.iter().map(|l| l.size).collect();
    // The float-arg call result is spilled to a 2-byte temp at [bp-2]; reserve
    // it at the deepest frame slot so chkstk sizes the frame to include it.
    // `make().field` temps occupy 4 bytes each, just below the locals, allocated
    // in source order (temp #0 shallowest). temp #0's disp = -(cumulative + 4).
    let struct_field_temp_base: i16 = if func.struct_field_temp_count > 0 {
        -i16::try_from(cumulative + 4).expect("struct-field temp disp fits")
    } else {
        0
    };
    let frame_bytes: usize = cumulative as usize
        + 4 * func.struct_field_temp_count as usize
        + if has_float_arg_call { 2 } else { 0 }
        + if returns_float_call { 8 } else { 0 };
    // The float-call-return temp is the deepest 8-byte slot.
    let float_call_temp_disp: i16 = if returns_float_call {
        -i16::try_from(frame_bytes).expect("frame fits")
    } else {
        0
    };

    match frame {
        Frame::None | Frame::NoneDiSi => bytes.extend_from_slice(&[0x33, 0xC0]),
        Frame::BpOnly | Frame::BpOnlySi => bytes.extend_from_slice(&[0x55, 0x8B, 0xEC, 0x33, 0xC0]),
        Frame::WithSlide | Frame::WithSlideSi | Frame::WithSlideDiSi => {
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
    // WithSlideDiSi / NoneDiSi: save DI then SI for the `movsw` copy.
    if matches!(frame, Frame::WithSlideDiSi | Frame::NoneDiSi) {
        bytes.push(0x57); // push di
        bytes.push(0x56); // push si
    }
    // For WithSlideSi / BpOnlySi: save SI after the frame is established.
    if matches!(frame, Frame::WithSlideSi | Frame::BpOnlySi) {
        bytes.push(0x56); // push si
    }

    // FpuStack coupling: the float local returned via `(int)<local>` is stored
    // with `fst` (st(0) kept live) so the cast is a bare `call __ftol` — but
    // only when it is the LAST float local initialized (otherwise a later float
    // init would have clobbered st(0)). Literal float locals fold to `mov ax,K`
    // via const-prop, so they never reach here as `Return(Local)`.
    // A float local is coupled (st(0) kept live via `fst`) when its `(int)` read
    // would NOT const-fold: either its init wasn't a direct literal, or it was
    // mutated (compound FP assign) so the fold view dropped its init.
    let returned_float_local = body.iter().rev().find_map(|s| match s {
        Stmt::Return(e) => coupled_return_local(e),
        _ => None,
    }).filter(|&i| func.locals.get(i).map(|l| {
        l.is_float && (!l.init_is_literal || mutated_locals.contains(&i))
    }).unwrap_or(false));
    let last_float_init = func.locals.iter().enumerate()
        .filter(|(_, l)| l.is_float && l.float_bits.is_some())
        .map(|(i, _)| i)
        .next_back();
    let coupled_float_local = match (returned_float_local, last_float_init) {
        (Some(r), Some(l)) if r == l => Some(r),
        _ => None,
    };
    // Whether the first op after the prologue float inits is itself an FP
    // instruction — then no `fwait` is needed after the last float init. True
    // for a coupled `(int)<local>` (→ `call __ftol`) and for a float `return`
    // (→ `fld …; fstp __fac`).
    let body_starts_fp = coupled_float_local.is_some()
        || (func.return_float_width != 0
            && matches!(body.first(), Some(Stmt::Return(Expr::FloatLit(..)))));

    // Initialized-local writes — `int x = K;` → `c7 46 disp lo hi`;
    // `char x = K;` → `c6 46 disp imm8`. Arrays don't get a constant
    // init here; their element stores live in the prelude body the
    // parser synthesized.
    for (i, spec) in func.locals.iter().enumerate() {
        // Float/double locals: load the const-pool value, store it to the slot:
        //   9B <D9|DD> 06 <off16>   fld  <dword|qword> [$T]   (FIDRQQ + FloatLoad)
        //   9B <D9|DD> 5E/56 <disp> fstp/fst <dword|qword> [bp+disp]   (FIDRQQ)
        // The coupled local (consumed by `(int)<local>`) uses `fst` (5E→56) to
        // keep st(0) live. A standalone `fwait` (`90 9B`, FIWRQQ — the 8087
        // emulator's 2-byte patch slot) follows only when the NEXT emitted op
        // is non-FP: i.e. this is the last float init and it is NOT coupled (a
        // coupled value is consumed by the body's `call __ftol`). Consecutive
        // float inits need no fwait between them.
        if spec.is_float {
            if let Some(bits) = spec.float_bits {
                let disp = local_disps[i];
                let op = if spec.size == 4 { 0xD9u8 } else { 0xDDu8 };
                let coupled = coupled_float_local == Some(i);
                // Shared-load peephole: an adjacent preceding float local with
                // the same (bits, width) already loaded the value onto st(0)
                // (it used `fst`), so this store reuses it without a new `fld`.
                let same = |j: usize| func.locals.get(j)
                    .map(|l| l.is_float && l.float_bits == Some(bits) && l.size == spec.size)
                    .unwrap_or(false);
                let prev_same = i > 0 && same(i - 1);
                let next_same = same(i + 1);
                if !prev_same {
                    // fld <width> [$T]
                    fixups.push(Fixup { body_offset: bytes.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                    bytes.push(0x9B);
                    bytes.push(op);
                    bytes.push(0x06);
                    let body_offset = bytes.len() - 1; // the 06 modrm; off16 at +1
                    bytes.extend_from_slice(&[0x00, 0x00]);
                    fixups.push(Fixup { body_offset, kind: FixupKind::FloatLoad { bits, width: spec.size } });
                }
                // `fst` keeps st(0) live — used when coupled (the cast consumes
                // it) or when the next adjacent local shares this value.
                let keep = coupled || next_same;
                fixups.push(Fixup { body_offset: bytes.len(), kind: FixupKind::FloatMarker { target: "FIDRQQ" } });
                bytes.push(0x9B);
                bytes.push(op);
                bytes.push(bp_modrm(if keep { 0x56 } else { 0x5E }, disp));
                push_bp_disp(&mut bytes, disp);
                if coupled {
                    fpu_live.set(Some(i));
                }
                // fwait only when the next op is non-FP.
                let more_floats_after = func.locals[i + 1..].iter()
                    .any(|l| l.is_float && l.float_bits.is_some());
                if !more_floats_after && !body_starts_fp {
                    fixups.push(Fixup { body_offset: bytes.len(), kind: FixupKind::FloatMarker { target: "FIWRQQ" } });
                    bytes.push(0x90); // nop — emulator patch slot
                    bytes.push(0x9B); // fwait
                }
            }
            continue;
        }
        if let Some(value) = spec.init {
            // A `register` local lives in SI/DI, not its stack slot — never emit a
            // slot init store. The value is still exposed via local_inits, so reads
            // that fold (e.g. `if(x)`, `return x+y`) need no runtime materialization;
            // a non-folding read materializes `mov si,K` at the use site. 1560/2069.
            if spec.is_register {
                continue;
            }
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
    let labels: std::cell::RefCell<std::collections::HashMap<String, usize>> = std::cell::RefCell::new(std::collections::HashMap::new());
    let label_fixups: std::cell::RefCell<Vec<(String, usize)>> = std::cell::RefCell::new(Vec::new());
    let param_is_char: Vec<bool> = func.param_is_char.clone();
    let param_is_long: Vec<bool> = func.param_is_long.clone();
    let param_is_unsigned: Vec<bool> = func.param_is_unsigned.clone();
    let param_float_widths: Vec<usize> = func.param_float_width.clone();
    let param_pointee_sizes: Vec<usize> = func.param_pointee_size.clone();
    let locals_view = Locals {
        inits: &local_inits,
        disps: &local_disps,
        sizes: &local_sizes,
        long_globals,
        char_globals,
        global_elem_sizes,
        unsigned_globals,
        float_globals,
        long_locals: &local_long,
        init_literals: &local_literals,
        far_ptr_locals: &local_far_ptrs,
        array_locals: &local_arrays,
        unsigned_locals: &local_unsigned,
        local_pointee_sizes: &local_pointee,
        float_locals: &local_float,
        char_params: &param_is_char,
        param_struct_bytes: &func.param_struct_bytes,
        long_params: &param_is_long,
        unsigned_params: &param_is_unsigned,
        param_float_widths: &param_float_widths,
        param_pointee_sizes: &param_pointee_sizes,
        char_returners,
        long_returners,
        pascal_fns,
        pascal_cleanup,
        si_local,
        di_local,
        long_param_funcs,
        struct_param_funcs,
        struct_return_funcs,
        float_returners: float_returners_arg,
        loop_stack: &loop_stack,
        labels: &labels,
        label_fixups: &label_fixups,
        fpu_live: &fpu_live,
        return_float_width: func.return_float_width,
        return_struct_bytes: func.return_struct_bytes,
        struct_temp_bss_offset: struct_temp_offset,
        float_call_temp_disp,
        fpu_pending_fwait: &fpu_pending_fwait,
        struct_field_temp_base,
        elide_call_cleanup: std::cell::Cell::new(false),
        last_branch_barrier: std::cell::Cell::new(0),
        last_top_stmt: std::cell::Cell::new(false),
    };

    // The last top-level call-bearing statement: if it carries exactly one call
    // and the function uses a slide frame, that call's cdecl cleanup is elided
    // (the epilogue's `mov sp,bp` restores SP). Earlier calls keep their cleanup.
    let last_call_idx: Option<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, s)| crate::codegen::statements::stmt_call_count(s) > 0)
        .last()
        .map(|(i, _)| i);

    let mut reachable = true;
    let mut i = 0;
    while i < body.len() {
        // A label is a join point (a goto may target it), so it revives
        // reachability even after a return / unconditional goto.
        if matches!(&body[i], Stmt::Label(_)) { reachable = true; }
        if !reachable { i += 1; continue; }
        let stmt = &body[i];

        // Arm/disarm the last-call cleanup elision for this statement.
        locals_view.elide_call_cleanup.set(
            frame.is_with_slide()
                && last_call_idx == Some(i)
                && crate::codegen::statements::stmt_single_call(stmt),
        );

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

        // Mark when a trailing no-else `if` can omit its merge-alignment NOP: the
        // merge falls through to the function's exit — either this is the last
        // statement, or the only thing after it is a `return` (whose value +
        // epilogue MSC does not pad). Fixtures 452, 459 (`...; return 0;`), 3602.
        locals_view.last_top_stmt.set(
            i + 1 == body.len() || matches!(body.get(i + 1), Some(Stmt::Return(_))),
        );
        emit_stmt(
            stmt,
            &locals_view,
            frame,
            func.return_int,
            func.return_long,
            &mut bytes,
            &mut fixups,
        );
        if stmt_always_returns(stmt, &locals_view) || stmt_exits(stmt) {
            reachable = false;
        }
        i += 1;
    }

    // Resolve `goto`/label jumps: backpatch each pending rel8 displacement now
    // that every label's byte offset is known.
    {
        let labels = labels.borrow();
        for (name, pos) in label_fixups.borrow().iter() {
            let target = *labels.get(name)
                .unwrap_or_else(|| panic!("goto to undefined label `{name}`"));
            let disp = target as i64 - (*pos as i64 + 1);
            bytes[*pos] = i8::try_from(disp).expect("goto rel8 displacement fits") as u8;
        }
    }
    // Implicit return at the end of functions that fall off without an
    // explicit `return`. MSC emits leave+ret for both void and int-returning
    // functions. The epilogue shape follows the function's frame.
    if reachable {
        push_epilogue(frame, pascal_cleanup, &mut bytes);
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
/// OMF symbol for a function. `pascal` functions are exported UPPERCASE with no
/// leading underscore (MS Pascal convention); cdecl functions get the standard
/// `_name`. Fixtures 1653/2063.
pub(crate) fn fn_symbol(f: &Function) -> String {
    if f.is_pascal { f.name.to_uppercase() } else { symbol_name(&f.name) }
}
/// OMF symbol for a callee referenced by bare C name, given whether it is a
/// known `pascal` function in this unit.
pub(crate) fn callee_symbol(c_name: &str, is_pascal: bool) -> String {
    if is_pascal { c_name.to_uppercase() } else { symbol_name(c_name) }
}
pub(crate) fn param_disp(idx: usize) -> i16 {
    i16::try_from(4 + (idx * 2)).expect("param disp fits in i16")
}
/// Append the function epilogue. For a `pascal` function (`pascal_cleanup > 0`)
/// the trailing `ret` (0xC3) becomes `ret N` (0xC2 + imm16), popping the
/// caller's arguments. cdecl functions emit the frame's plain epilogue.
pub(crate) fn push_epilogue(frame: Frame, pascal_cleanup: u16, out: &mut Vec<u8>) {
    let epi = frame.epilogue_bytes();
    if pascal_cleanup > 0 {
        out.extend_from_slice(&epi[..epi.len() - 1]); // all but the trailing C3
        out.push(0xC2);
        out.extend_from_slice(&pascal_cleanup.to_le_bytes());
    } else {
        out.extend_from_slice(epi);
    }
}
/// Like [`push_epilogue`] but returns the bytes, for sites that need the
/// epilogue's length (short-circuit `return a||b` double-epilogue paths).
pub(crate) fn epilogue_vec(frame: Frame, pascal_cleanup: u16) -> Vec<u8> {
    let mut v = Vec::new();
    push_epilogue(frame, pascal_cleanup, &mut v);
    v
}
