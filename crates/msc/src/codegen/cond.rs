use crate::*;

/// Just the cmp half of a cond — used by `emit_while` which pairs
/// the comparison with a backward jcc rather than a forward skip.
pub(crate) fn emit_cond_cmp(cond: &Cond, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    emit_cond_cmp_inner(cond, locals, out, fixups);
}
/// Emit `cmp <X>, <Y>; <inverted-jcc> skip` where `skip` is a
/// forward `rel8` displacement equal to `take_then_disp`. The
/// caller has pre-computed the size of the then-body so we can use
/// the 2-byte jcc form without a fixup. Fixtures 4090 / 4091 / 4092.
pub(crate) fn emit_cond_skip(cond: &Cond, take_then_disp: i8, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match cond {
        Cond::And(a, b) => {
            // Pre-emit `b`'s cmp+jcc-to-skip into a scratch buffer
            // so we know its size. `a`'s skip target jumps over `b`
            // AND the body. `b`'s skip target jumps over just the
            // body — the original `take_then_disp`.
            let mut b_buf = Vec::new();
            let mut b_fixups = Vec::new();
            emit_cond_skip(b, take_then_disp, locals, &mut b_buf, &mut b_fixups);
            let a_skip = i8::try_from(b_buf.len() as i32 + take_then_disp as i32)
                .expect("&&-cond skip fits in rel8");
            emit_cond_skip(a, a_skip, locals, out, fixups);
            let b_base = out.len();
            out.extend_from_slice(&b_buf);
            for mut f in b_fixups {
                f.body_offset += b_base;
                fixups.push(f);
            }
        }
        Cond::Or(a, b) => {
            // `a` true → jump into the body (skipping `b`). `a`
            // false → fall through to `b`. `b` evaluates as a
            // normal skip-cond: true → fall into body, false → skip.
            //
            // For `a`'s "take then" direction we need to invert the
            // jcc to a "take when true" form and target the start of
            // the body. We use emit_cond_take helper which emits
            // cmp + take-then-jcc instead of skip-jcc.
            let mut b_buf = Vec::new();
            let mut b_fixups = Vec::new();
            emit_cond_skip(b, take_then_disp, locals, &mut b_buf, &mut b_fixups);
            // a's "take" disp: jump past b's emission to the body.
            let a_take = i8::try_from(b_buf.len() as i32)
                .expect("||-cond take disp fits in rel8");
            emit_cond_take(a, a_take, locals, out, fixups);
            let b_base = out.len();
            out.extend_from_slice(&b_buf);
            for mut f in b_fixups {
                f.body_offset += b_base;
                fixups.push(f);
            }
        }
        Cond::Cmp { op: op @ (RelOp::Lt | RelOp::Ge), left, right }
            if long_operand(left, locals)
                && !long_operand_unsigned(left, locals)
                && matches!(right, Expr::IntLit(0)) =>
        {
            // Signed long `v < 0L` / `v >= 0L` is a pure sign test on the high
            // word: `cmp WORD PTR [hi],0; jge|jl <skip-then>`. The low word is
            // irrelevant. Fixture 3026.
            let (cmp_hi, hi_fx) = long_cmp_word_imm(left, 2, 0, locals);
            let base = out.len();
            out.extend_from_slice(&cmp_hi);
            if let Some((rel, gidx)) = hi_fx {
                fixups.push(Fixup { body_offset: base + rel, kind: FixupKind::GlobalAddr { global_idx: gidx } });
            }
            out.push(if matches!(op, RelOp::Lt) { 0x7D } else { 0x7C }); // jge | jl (skip on false)
            out.push(take_then_disp as u8);
        }
        Cond::Cmp { op, left, right }
            if long_operand(left, locals)
                && (long_operand(right, locals)
                    || matches!(right, Expr::IntLit(k) if *k != 0)
                    // Signed long `v > 0L` / `v <= 0L`: unlike `< 0L` / `>= 0L`
                    // (a pure high-word sign test above), these need the full
                    // word-wise ordering — the high word decides, the low word
                    // breaks the tie at high==0. Routed here with the low-word
                    // tiebreak collapsed against 0 below. Fixture 433.
                    || (matches!(op, RelOp::Gt | RelOp::Le)
                        && matches!(right, Expr::IntLit(0))
                        && !long_operand_unsigned(left, locals))) =>
        {
            // 32-bit comparison: a multi-branch sequence (not the single
            // cmp+jcc int shape). A long compared against a NON-ZERO int
            // constant compares each word against an immediate. (A zero
            // constant falls through to the `mov ax,[lo]; or ax,[hi]` zero
            // idiom in emit_cond_cmp_inner.) See emit_long_cmp_skip.
            let unsigned = long_operand_unsigned(left, locals)
                || long_operand_unsigned(right, locals);
            emit_long_cmp_skip(*op, left, right, take_then_disp, unsigned, locals, out, fixups);
        }
        // `long g <op> int i` — the int operand is promoted to a long (cwd) and
        // the long stays in memory. Distinct byte shape (`cmp reg,[mem]`, op
        // swapped). Only simple int operands (Global/Local/Param). Fixtures
        // 273, 280.
        Cond::Cmp { op, left, right }
            if long_operand(left, locals)
                && !long_operand(right, locals)
                && matches!(right, Expr::Global(_) | Expr::Local(_) | Expr::Param(_)) =>
        {
            let unsigned = long_operand_unsigned(left, locals) || int_is_unsigned(right, locals);
            emit_long_int_cmp_skip(*op, left, right, take_then_disp, unsigned, locals, out, fixups);
        }
        _ => {
            let jcc = match cond {
                Cond::Truthy(_) => 0x74, // je on zero
                Cond::Cmp { op, .. } => {
                    let j = inverted_jcc(*op);
                    if cmp_is_unsigned(cond, locals) { to_unsigned_jcc(j) } else { j }
                }
                _ => unreachable!(),
            };
            emit_cond_cmp_inner(cond, locals, out, fixups);
            out.push(jcc);
            out.push(take_then_disp as u8);
        }
    }
}

/// Lower `if (a <op> b)` for two long operands as the skip-on-false sequence
/// MSC emits. The RHS is loaded into DX:AX; the LHS is compared word-wise
/// from memory. Ordering compares the high word first (signed jg/jl or
/// unsigned ja/jb) with the low word as an unsigned tiebreak; eq/ne compare
/// the low word first. Multiple jccs target two labels — the else-label
/// (false: skip the then-block) and the then-label (true: fall into it) —
/// so we lay the sequence out and compute each rel8 forward displacement.
/// Fixtures 234-237, 2864, 2868, 2869.
fn emit_long_cmp_skip(
    op: RelOp,
    left: &Expr,
    right: &Expr,
    take_then_disp: i8,
    unsigned: bool,
    locals: &Locals<'_>,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // RHS handling: a constant compares each word against an immediate (no
    // DX:AX load — `cmp WORD [lo],K_lo; ...; cmp WORD [hi],K_hi`); a long
    // operand is loaded into DX:AX and the LHS compared word-wise to AX/DX.
    let mut load = Vec::new();
    let mut load_fx = Vec::new();
    let (cmp_lo, lo_fx, cmp_hi, hi_fx) = if let Expr::IntLit(k) = right {
        let lo = (*k as u32 & 0xFFFF) as i16;
        let hi = ((*k >> 16) as u32 & 0xFFFF) as i16;
        let (cl, lf) = long_cmp_word_imm(left, 0, lo, locals);
        let (ch, hf) = long_cmp_word_imm(left, 2, hi, locals);
        (cl, lf, ch, hf)
    } else {
        emit_long_to_dx_ax(right, locals, &mut load, &mut load_fx);
        let (cl, lf) = long_cmp_word(left, 0, false, 0x39, locals);
        let (ch, hf) = long_cmp_word(left, 2, true, 0x39, locals);
        (cl, lf, ch, hf)
    };

    // UNSIGNED ordering against a constant whose HIGH word is 0: the high word
    // can't be negative, so one of the two high-word jccs (testing `high < 0`) is
    // dead. The compare collapses to a single `cmp hi,0; jne <dst>` — high != 0
    // means definitively greater (Gt/Ge → take then) or not-less (Lt/Le → take
    // else). Fixture 3058 (`unsigned long v > 1000`).
    let const_hi_zero = unsigned && matches!(right, Expr::IntLit(k) if ((*k >> 16) as u32 & 0xFFFF) == 0);
    emit_long_cmp_steps(
        op, unsigned, matches!(right, Expr::IntLit(0)), const_hi_zero,
        &load, load_fx, &cmp_lo, &lo_fx, &cmp_hi, &hi_fx, take_then_disp, out, fixups,
    );
}

/// `long g <op> int i`: MSC promotes the INT to a long (`mov ax,[i]; cwd`),
/// keeps the long operand in memory, and compares word-wise with the register
/// as the destination (`cmp dx,[g+2]` / `cmp ax,[g]` — opcode 0x3B). The
/// register holds the promoted int, so the predicate is evaluated as
/// `i <swap(op)> g` (Lt↔Gt, Le↔Ge; Eq/Ne unchanged). Fixtures 273, 280.
fn emit_long_int_cmp_skip(
    op: RelOp,
    long_side: &Expr,
    int_side: &Expr,
    take_then_disp: i8,
    unsigned: bool,
    locals: &Locals<'_>,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // Promote the int into DX:AX: load to AX, then cwd (signed) / sub dx,dx
    // (unsigned).
    let mut load = Vec::new();
    let mut load_fx = Vec::new();
    crate::codegen::expr::emit_expr_to_ax(int_side, locals, &mut load, &mut load_fx);
    if int_is_unsigned(int_side, locals) {
        load.extend_from_slice(&[0x2B, 0xD2]); // sub dx,dx
    } else {
        load.push(0x99); // cwd
    }
    // `cmp dx,[g+2]` / `cmp ax,[g]` (reg=destination, 0x3B). reg_is_dx → high.
    let (cmp_lo, lo_fx) = long_cmp_word(long_side, 0, false, 0x3B, locals);
    let (cmp_hi, hi_fx) = long_cmp_word(long_side, 2, true, 0x3B, locals);
    let swapped = match op {
        RelOp::Lt => RelOp::Gt,
        RelOp::Gt => RelOp::Lt,
        RelOp::Le => RelOp::Ge,
        RelOp::Ge => RelOp::Le,
        other => other, // Eq/Ne symmetric
    };
    emit_long_cmp_steps(
        swapped, unsigned, false, false,
        &load, load_fx, &cmp_lo, &lo_fx, &cmp_hi, &hi_fx, take_then_disp, out, fixups,
    );
}

/// Lay out and emit the word-wise long-comparison step sequence (high-word
/// signed/unsigned decision + low-word unsigned tiebreak, or low-then-high for
/// eq/ne). Shared by the long-vs-long and long-vs-int compare paths. `cmp_lo`/
/// `cmp_hi` are the pre-built compare instructions; `load` is the optional
/// RHS-setup prefix (DX:AX load / int promotion).
#[allow(clippy::too_many_arguments)]
fn emit_long_cmp_steps(
    op: RelOp,
    unsigned: bool,
    lo_against_zero: bool,
    const_hi_zero: bool,
    load: &[u8],
    load_fx: Vec<Fixup>,
    cmp_lo: &[u8],
    lo_fx: &Option<(usize, usize)>,
    cmp_hi: &[u8],
    hi_fx: &Option<(usize, usize)>,
    take_then_disp: i8,
    out: &mut Vec<u8>,
    fixups: &mut Vec<Fixup>,
) {
    // Each step is a cmp (with optional global fixup) or a jcc to a label.
    enum Step<'a> {
        Cmp(&'a [u8], &'a Option<(usize, usize)>),
        Jcc(u8, bool), // (opcode, to_then?)
    }
    let (hi_f, hi_t, lo_f) = long_ordering_jccs(op, unsigned);
    // Low word compared against immediate 0: the unsigned tiebreak collapses,
    // since nothing is unsigned-below 0. `jbe`(Gt) becomes `je` and `ja`(Le)
    // becomes `jne` — MSC emits the collapsed forms. Fixture 433.
    let lo_f = if lo_against_zero {
        match lo_f {
            0x76 => 0x74, // jbe -> jz
            0x77 => 0x75, // ja  -> jnz
            other => other,
        }
    } else {
        lo_f
    };
    let steps: Vec<Step<'_>> = match op {
        RelOp::Eq => vec![
            Step::Cmp(cmp_lo, lo_fx), Step::Jcc(0x75, false), // jne else
            Step::Cmp(cmp_hi, hi_fx), Step::Jcc(0x75, false), // jne else
        ],
        RelOp::Ne => vec![
            Step::Cmp(cmp_lo, lo_fx), Step::Jcc(0x75, true), // jne then
            Step::Cmp(cmp_hi, hi_fx), Step::Jcc(0x74, false), // je else
        ],
        RelOp::Gt | RelOp::Ge if const_hi_zero => vec![
            Step::Cmp(cmp_hi, hi_fx), Step::Jcc(0x75, true), // jne then (high!=0 → greater)
            Step::Cmp(cmp_lo, lo_fx), Step::Jcc(lo_f, false),
        ],
        RelOp::Lt | RelOp::Le if const_hi_zero => vec![
            Step::Cmp(cmp_hi, hi_fx), Step::Jcc(0x75, false), // jne else (high!=0 → not less)
            Step::Cmp(cmp_lo, lo_fx), Step::Jcc(lo_f, false),
        ],
        _ => vec![
            Step::Cmp(cmp_hi, hi_fx),
            Step::Jcc(hi_f, false), // high word decides false
            Step::Jcc(hi_t, true),  // high word decides true
            Step::Cmp(cmp_lo, lo_fx),
            Step::Jcc(lo_f, false), // low word (unsigned) tiebreak
        ],
    };

    let seq_size: usize = load.len()
        + steps.iter().map(|s| match s { Step::Cmp(b, _) => b.len(), Step::Jcc(..) => 2 }).sum::<usize>();
    let then_off = seq_size as i32;
    let else_off = seq_size as i32 + take_then_disp as i32;

    let base = out.len();
    out.extend_from_slice(load);
    for mut f in load_fx {
        f.body_offset += base;
        fixups.push(f);
    }
    let mut pos = load.len();
    for step in steps {
        match step {
            Step::Cmp(bytes, fx) => {
                out.extend_from_slice(bytes);
                if let Some((rel, gidx)) = fx {
                    fixups.push(Fixup {
                        body_offset: base + pos + rel,
                        kind: FixupKind::GlobalAddr { global_idx: *gidx },
                    });
                }
                pos += bytes.len();
            }
            Step::Jcc(opcode, to_then) => {
                let target = if to_then { then_off } else { else_off };
                let disp = target - (pos as i32 + 2);
                out.push(opcode);
                out.push(i8::try_from(disp).expect("long-cmp jcc disp fits rel8") as u8);
                pos += 2;
            }
        }
    }
}

/// Whether an int operand (Global/Local/Param) is `unsigned`.
fn int_is_unsigned(e: &Expr, locals: &Locals<'_>) -> bool {
    match e {
        Expr::Global(g) => locals.is_unsigned_global(*g),
        Expr::Param(i) => locals.is_unsigned_param(*i),
        _ => false,
    }
}

/// `cmp WORD PTR [<left word>], <ax|dx>` for a long LHS in memory. Returns the
/// bytes plus, for a global, the (offset-of-address-within-bytes, global_idx)
/// so the caller can register a GlobalAddr fixup. `word_off` is 0 (low) or 2
/// (high); the placeholder address encodes that addend. `opcode` selects the
/// direction: `0x39` (`cmp [mem], reg`, long-vs-long) or `0x3B` (`cmp reg,
/// [mem]`, the long-vs-int form where the register holds the promoted int) —
/// the modrm bytes are identical for both.
fn long_cmp_word(
    left: &Expr,
    word_off: i16,
    reg_is_dx: bool,
    opcode: u8,
    locals: &Locals<'_>,
) -> (Vec<u8>, Option<(usize, usize)>) {
    let mut b = vec![opcode]; // cmp r/m16,r16 (0x39) | cmp r16,r/m16 (0x3B)
    match left {
        Expr::Global(j) => {
            // The GlobalAddr resolver treats the fixup offset as the byte
            // before the address (it patches body_offset+1/+2), so point it
            // at the modrm byte. The placeholder holds the word_off addend.
            let modrm_at = b.len();
            b.push(if reg_is_dx { 0x16 } else { 0x06 }); // mod00 disp16, reg=dx|ax
            b.extend_from_slice(&(word_off as u16).to_le_bytes()); // placeholder addend
            (b, Some((modrm_at, *j)))
        }
        Expr::Param(i) => {
            let disp = long_param_disp(*i, locals) + word_off;
            b.push(bp_modrm(if reg_is_dx { 0x56 } else { 0x46 }, disp));
            push_bp_disp(&mut b, disp);
            (b, None)
        }
        Expr::Local(i) => {
            let disp = locals.disp(*i) + word_off;
            b.push(bp_modrm(if reg_is_dx { 0x56 } else { 0x46 }, disp));
            push_bp_disp(&mut b, disp);
            (b, None)
        }
        _ => unreachable!("long_operand gates these forms"),
    }
}

/// `cmp WORD PTR [<left word>], <imm>` for a long LHS in memory against an
/// immediate (the constant-RHS form). `83 /7 imm8sx` when `imm` fits i8, else
/// `81 /7 imm16`. Like [`long_cmp_word`] it returns the bytes plus, for a
/// global, the (modrm-offset, global_idx) for a GlobalAddr fixup.
fn long_cmp_word_imm(
    left: &Expr,
    word_off: i16,
    imm: i16,
    locals: &Locals<'_>,
) -> (Vec<u8>, Option<(usize, usize)>) {
    let use_imm8 = i8::try_from(imm).is_ok();
    let push_imm = |b: &mut Vec<u8>| {
        if use_imm8 { b.push(imm as i8 as u8); } else { b.extend_from_slice(&(imm as u16).to_le_bytes()); }
    };
    let mut b = vec![if use_imm8 { 0x83u8 } else { 0x81 }]; // cmp r/m16, imm8sx | imm16  (/7)
    match left {
        Expr::Global(j) => {
            let modrm_at = b.len();
            b.push(0x3E); // mod00 r/m110 reg=7 → [disp16]
            b.extend_from_slice(&(word_off as u16).to_le_bytes()); // placeholder addend
            push_imm(&mut b);
            (b, Some((modrm_at, *j)))
        }
        Expr::Param(i) => {
            let disp = long_param_disp(*i, locals) + word_off;
            b.push(bp_modrm(0x7E, disp)); // /7 with bp
            push_bp_disp(&mut b, disp);
            push_imm(&mut b);
            (b, None)
        }
        Expr::Local(i) => {
            let disp = locals.disp(*i) + word_off;
            b.push(bp_modrm(0x7E, disp));
            push_bp_disp(&mut b, disp);
            push_imm(&mut b);
            (b, None)
        }
        _ => unreachable!("long_operand gates these forms"),
    }
}
/// (high-word false jcc, high-word true jcc, low-word false jcc) for an
/// ordering comparison. High word uses signed jg/jl (or unsigned ja/jb); the
/// low word is always an unsigned tiebreak.
const fn long_ordering_jccs(op: RelOp, unsigned: bool) -> (u8, u8, u8) {
    match (op, unsigned) {
        (RelOp::Lt, false) => (0x7F, 0x7C, 0x73), // jg / jl / jae
        (RelOp::Lt, true) => (0x77, 0x72, 0x73),  // ja / jb / jae
        (RelOp::Gt, false) => (0x7C, 0x7F, 0x76), // jl / jg / jbe
        (RelOp::Gt, true) => (0x72, 0x77, 0x76),  // jb / ja / jbe
        (RelOp::Le, false) => (0x7F, 0x7C, 0x77), // jg / jl / ja
        (RelOp::Le, true) => (0x77, 0x72, 0x77),  // ja / jb / ja
        (RelOp::Ge, false) => (0x7C, 0x7F, 0x72), // jl / jg / jb
        (RelOp::Ge, true) => (0x72, 0x77, 0x72),  // jb / ja / jb
        _ => (0, 0, 0),                            // eq/ne don't use this
    }
}
/// Counterpart of `emit_cond_skip` — emits cmp + a jcc that fires
/// when the condition is *true*, skipping ahead by `take_disp`
/// bytes. Used by Cond::Or short-circuit so `a` jumps into the body
/// when satisfied.
pub(crate) fn emit_cond_take(cond: &Cond, take_disp: i8, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match cond {
        Cond::And(_, _) | Cond::Or(_, _) => {
            panic!("nested &&/|| in emit_cond_take not yet supported");
        }
        _ => {
            let jcc = match cond {
                Cond::Truthy(_) => 0x75, // jne (take when nonzero)
                Cond::Cmp { op, .. } => loop_back_jcc(*op),
                _ => unreachable!(),
            };
            emit_cond_cmp_inner(cond, locals, out, fixups);
            out.push(jcc);
            out.push(take_disp as u8);
        }
    }
}
/// Emit only the cmp half of a Cond. Used by both emit_cond_skip
/// (forward jcc for `if`) and emit_cond_cmp (backward jcc for loops).
/// Emit an `&local`/`&global` address into AX (`to_cx == false`) or CX
/// (`to_cx == true`): `lea ax/cx, [bp+disp]` for a local, `mov ax/cx,
/// OFFSET g` for a global. Used by the address-vs-address comparison.
fn emit_addr_to_reg(e: &Expr, to_cx: bool, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    match e {
        Expr::AddrOfLocal(idx) => {
            let disp = locals.disp(*idx);
            let base = if to_cx { 0x4E } else { 0x46 };
            out.push(0x8D);
            out.push(bp_modrm(base, disp));
            push_bp_disp(out, disp);
        }
        Expr::AddrOfGlobal(idx) => {
            let body_offset = out.len();
            out.push(if to_cx { 0xB9 } else { 0xB8 }); // mov cx/ax, imm16
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset, kind: FixupKind::GlobalAddr { global_idx: *idx } });
        }
        _ => unreachable!("emit_addr_to_reg expects an address-of expression"),
    }
}
pub(crate) fn emit_cond_cmp_inner(cond: &Cond, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    // `(c = <char rvalue>) != 0` / `== 0` — a comma-assign whose value is the
    // same char local just stored. The byte store leaves the value in AL, so
    // MSC reuses AL with `or al,al` instead of reloading the slot + widening
    // (`mov al,[c]; cbw; or ax,ax`). The caller's jne/je reads ZF the same way.
    // Fixture 3653 (`while ((c = arr[i++]) != 0) s += c`).
    if let Cond::Cmp { op: RelOp::Ne | RelOp::Eq, left, right } = cond
        && matches!(right, Expr::IntLit(0))
        && let Expr::Seq { sides, value } = left
        && let Expr::Local(v) = value.as_ref()
        && locals.size(*v) == 1
        && let Some(Stmt::Assign { target: AssignTarget::Local(t), .. }) = sides.last()
        && *t == *v
    {
        for s in sides {
            emit_stmt(s, locals, Frame::BpOnly, true, false, out, fixups);
        }
        out.extend_from_slice(&[0x0A, 0xC0]); // or al,al
        return;
    }
    // `while (--n > 0)` — a pre-mutation on the LEFT of a comparison: do the
    // in-place inc/dec first, then compare the slot directly (`dec [n]; cmp
    // [n],0`). Rewrite the operand to the plain slot read and re-enter so the
    // normal `cmp [slot],imm` path emits. Fixture 3644.
    if let Cond::Cmp { op, left, right } = cond {
        let slot = match left {
            Expr::PreMutateLocal { local_idx, step } => {
                crate::codegen::assign::emit_postmutate_local(*step, locals.size(*local_idx), locals.disp(*local_idx), out);
                Some(Expr::Local(*local_idx))
            }
            Expr::PreMutateParam { param_idx, step } if !locals.is_long_param(*param_idx) => {
                let size = if locals.is_char_param(*param_idx) { 1 } else { 2 };
                crate::codegen::assign::emit_postmutate_local(*step, size, param_disp(*param_idx), out);
                Some(Expr::Param(*param_idx))
            }
            _ => None,
        };
        if let Some(slot) = slot {
            let rewritten = Cond::Cmp { op: *op, left: slot, right: right.clone() };
            emit_cond_cmp_inner(&rewritten, locals, out, fixups);
            return;
        }
        // `if (i++ < n)` — a POST-mutation on the LEFT compares the OLD value:
        // load it into AX, bump the slot in place, then compare AX against the
        // right-hand memory operand. Fixture 3527.
        let post = match left {
            Expr::PostMutateParam { param_idx, step }
                if !locals.is_long_param(*param_idx) && !locals.is_char_param(*param_idx) =>
                Some((param_disp(*param_idx), *step)),
            Expr::PostMutateLocal { local_idx, step }
                if locals.size(*local_idx) == 2 && !locals.is_long_local(*local_idx) =>
                Some((locals.disp(*local_idx), *step)),
            _ => None,
        };
        let right_mem = match right {
            Expr::Param(i) if !locals.is_long_param(*i) && !locals.is_char_param(*i) =>
                Some(param_disp(*i)),
            Expr::Local(i) if locals.size(*i) == 2 && !locals.is_long_local(*i) =>
                Some(locals.disp(*i)),
            _ => None,
        };
        if let (Some((ld, step)), Some(rd)) = (post, right_mem) {
            out.push(0x8B); out.push(bp_modrm(0x46, ld)); push_bp_disp(out, ld); // mov ax,[bp+ld]
            crate::codegen::assign::emit_postmutate_local(step, 2, ld, out);     // inc/dec word [bp+ld]
            out.push(0x3B); out.push(bp_modrm(0x46, rd)); push_bp_disp(out, rd); // cmp ax,[bp+rd]
            return;
        }
    }
    // Arithmetic result compared against zero — MSC reuses the ALU flags
    // rather than a separate compare:
    //   `(x - y) == 0` / `!= 0` → rewrite to `Cmp(op, x, y)`; the `cmp`
    //     instruction IS the subtraction (`mov ax,y; cmp [x],ax`). 3257.
    //   `(x + y) == 0` / `(x | y) == 0` / `(x ^ y) == 0` → evaluate the
    //     binop into AX (`add/or/xor` already sets ZF) and drop the test
    //     entirely. 3258. (BitAnd keeps its dedicated `test mem,imm` path.)
    if let Cond::Cmp { op, left, right } = cond
        && matches!(op, RelOp::Eq | RelOp::Ne)
    {
        let arith = match (left, right) {
            (e @ Expr::BinOp { .. }, Expr::IntLit(0)) => Some(e),
            (Expr::IntLit(0), e @ Expr::BinOp { .. }) => Some(e),
            _ => None,
        };
        if let Some(e @ Expr::BinOp { op: bo, left: a, right: b }) = arith {
            match bo {
                BinOp::Sub => {
                    let rewritten = Cond::Cmp {
                        op: *op,
                        left: (**a).clone(),
                        right: (**b).clone(),
                    };
                    emit_cond_cmp_inner(&rewritten, locals, out, fixups);
                    return;
                }
                BinOp::Add | BinOp::BitOr | BinOp::BitXor => {
                    emit_expr_to_ax(e, locals, out, fixups);
                    return;
                }
                _ => {}
            }
        }
    }
    // `while (*s++)` — test the OLD pointee for zero while advancing the pointer:
    // `mov bx,[s]; <advance [s]>; cmp byte/word [bx],0`. The caller's jcc (jne for
    // truthy) takes the loop while the pointee is non-zero. Fixture 2027.
    if let Cond::Truthy(Expr::PostIncDeref { ptr, step, is_byte }) = cond {
        crate::codegen::expr::emit_postinc_ptr_to_bx(ptr, *step, locals, out, fixups);
        if *is_byte { out.extend_from_slice(&[0x80, 0x3F, 0x00]); } // cmp byte [bx],0
        else { out.extend_from_slice(&[0x83, 0x3F, 0x00]); }        // cmp word [bx],0
        return;
    }
    // Assignment as a condition: `while (*d++ = *s++)` — emit the store
    // (leaving the value in AL/AX), then `or al,al` (byte pointee) / `or ax,ax`
    // (word) so the caller's jne loops while the copied value is non-zero.
    // Fixture 1808.
    if let Cond::Truthy(e @ Expr::AssignExpr { target, .. }) = cond
        && let AssignTarget::DerefPostMutateParam { step, .. }
            | AssignTarget::DerefPostMutateLocal { step, .. } = target
    {
        let byte = step.unsigned_abs() == 1;
        crate::codegen::expr::emit_expr_to_ax(e, locals, out, fixups);
        if byte { out.extend_from_slice(&[0x0A, 0xC0]); } // or al,al
        else { out.extend_from_slice(&[0x0B, 0xC0]); }    // or ax,ax
        return;
    }
    // `while (--n)` / `if (++n)` — a PRE-mutation of a local/global as the sole
    // condition tests the MUTATED value, and the in-place inc/dec/add/sub
    // already sets ZF, so emit just that (no load+or) and let the caller's jcc
    // test it. (Post-mutation `n--` tests the OLD value and keeps the generic
    // load-then-mutate path.) Fixtures 1844, 2045, 2473, 2408.
    if let Cond::Truthy(Expr::PreMutateLocal { local_idx, step }) = cond {
        crate::codegen::assign::emit_postmutate_local(*step, locals.size(*local_idx), locals.disp(*local_idx), out);
        return;
    }
    if let Cond::Truthy(Expr::PreMutateGlobal { global_idx, step }) = cond {
        crate::codegen::assign::emit_postmutate_global(*step, *global_idx, locals.is_char_global(*global_idx), out, fixups);
        return;
    }
    if let Cond::Truthy(Expr::PreMutateParam { param_idx, step }) = cond
        && !locals.is_long_param(*param_idx)
    {
        let size = if locals.is_char_param(*param_idx) { 1 } else { 2 };
        crate::codegen::assign::emit_postmutate_local(*step, size, param_disp(*param_idx), out);
        return;
    }
    // Pointer deref as a condition: `while (*s)` / `if (*p)` / `while (*++p)` →
    // `[<advance p>;] mov bx,[ptr]; cmp byte/word [bx],0` instead of loading the
    // pointee into AX and testing it. A plain pointer param/local loads directly;
    // a pre-mutated pointer (`*++p`) is advanced in place first, then its slot is
    // loaded. Fixtures 3351, 1311.
    // Accept both the truthy form `while (*s)` and the explicit-compare form
    // `while (*s != 0)` / `if (*p == 0)` (the caller's jcc reads ZF the same way).
    // Fixture 1408 (`while (*s != 0)` → `mov bx,[s]; cmp byte [bx],0`).
    // The compare constant may be nonzero too: `if (*p == 42)` → `mov bx,[p];
    // cmp word [bx],42` (fixture 2693). `k` carries that immediate (0 for the
    // truthy forms).
    let deref_cond: Option<(bool, &Expr, i32)> = match cond {
        Cond::Truthy(Expr::DerefByte { ptr }) => Some((true, ptr.as_ref(), 0)),
        Cond::Truthy(Expr::DerefWord { ptr }) => Some((false, ptr.as_ref(), 0)),
        Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left, right: Expr::IntLit(k) } => match left {
            Expr::DerefByte { ptr } => Some((true, ptr.as_ref(), *k)),
            Expr::DerefWord { ptr } => Some((false, ptr.as_ref(), *k)),
            _ => None,
        },
        _ => None,
    };
    if let Some((is_byte, ptr, k)) = deref_cond {
        let loaded = match ptr {
            Expr::Param(_) | Expr::Local(_) => {
                crate::codegen::expr::emit_load_bx(ptr, locals, out, fixups);
                true
            }
            Expr::PreMutateLocal { local_idx, step } => {
                crate::codegen::assign::emit_postmutate_local(*step, locals.size(*local_idx), locals.disp(*local_idx), out);
                crate::codegen::expr::emit_load_bx(&Expr::Local(*local_idx), locals, out, fixups);
                true
            }
            _ => false,
        };
        if loaded {
            if is_byte {
                out.extend_from_slice(&[0x80, 0x3F, (k as u32 & 0xFF) as u8]); // cmp byte [bx],k
            } else if let Ok(k8) = i8::try_from(k) {
                out.extend_from_slice(&[0x83, 0x3F, k8 as u8]); // cmp word [bx],imm8sx
            } else {
                out.extend_from_slice(&[0x81, 0x3F]); // cmp word [bx],imm16
                out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
            }
            return;
        }
    }
    // `p->f == K` / `p->f != K` (struct-pointer field compared to a constant),
    // where p is a LOCAL or PARAM pointer: load p into BX, then compare the field
    // directly through the pointer (`cmp word [bx+off],K`) rather than loading the
    // field into AX first. Fixtures 1007 (local), 3267 (param).
    let pf = if let Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left, right: Expr::IntLit(k) } = cond {
        match left {
            Expr::DerefLocalField { ptr_local, byte_off, size } => Some((Some(*ptr_local), 0usize, *byte_off, *size, *k)),
            Expr::DerefParamField { ptr_param, byte_off, size } => Some((None, *ptr_param, *byte_off, *size, *k)),
            _ => None,
        }
    } else { None };
    if let Some((local, param, byte_off, size, k)) = pf {
        match local {
            Some(li) => crate::codegen::expr::emit_ptr_local_to_bx(locals.disp(li), locals, out),
            None => { out.push(0x8B); out.push(0x5E); out.push(param_disp(param) as u8); } // mov bx,[bp+pd]
        }
        let off = byte_off as u8;
        let rm = |base: u8| if off == 0 { base } else { base | 0x40 }; // [bx] vs [bx+disp8]
        if size == 1 {
            out.push(0x80); out.push(rm(0x3F)); if off != 0 { out.push(off); } out.push((k as u32 & 0xFF) as u8);
        } else if let Ok(k8) = i8::try_from(k) {
            out.push(0x83); out.push(rm(0x3F)); if off != 0 { out.push(off); } out.push(k8 as u8);
        } else {
            out.push(0x81); out.push(rm(0x3F)); if off != 0 { out.push(off); }
            out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
        }
        return;
    }
    // `(unsigned char)x == K` / `!= K` — including `(x & 0xFF) == K`, which the
    // parser lowers to a CastChar — compares the low byte in place:
    // `cmp byte [x], K`. Fixtures 3264, 3269. K must fit a byte.
    if let Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::CastChar { value, .. }, right: Expr::IntLit(k) } = cond
        && (0..=0xFF).contains(k)
    {
        let kb = *k as u8;
        match value.as_ref() {
            Expr::Local(i) if locals.size(*i) >= 1 => {
                let d = locals.disp(*i);
                out.push(0x80); out.push(bp_modrm(0x7E, d)); push_bp_disp(out, d); out.push(kb);
                return;
            }
            Expr::Param(i) => {
                let d = param_disp(*i);
                out.push(0x80); out.push(bp_modrm(0x7E, d)); push_bp_disp(out, d); out.push(kb);
                return;
            }
            Expr::Global(g) => {
                out.push(0x80); out.push(0x3E);
                let pos = out.len(); out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: pos - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
                out.push(kb);
                return;
            }
            _ => {}
        }
    }
    // Long-global vs zero idiom: `mov ax, [g]; or ax, [g+2]` — ZF set
    // iff both halves are zero. Covers `if (g == 0)`, `if (!g)`, and
    // (with inverted jcc) `if (g != 0)`.
    if let Some(idx) = match cond {
        Cond::Truthy(Expr::Global(idx)) if locals.is_long_global(*idx) => Some(*idx),
        Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::Global(idx), right: Expr::IntLit(0) }
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::IntLit(0), right: Expr::Global(idx) }
            if locals.is_long_global(*idx) => Some(*idx),
        _ => None,
    } {
        // mov ax, [g]
        let off = out.len();
        out.extend_from_slice(&[0xA1, 0x00, 0x00]);
        fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: idx } });
        // or ax, [g+2]  →  0b 06 <off+2>
        out.push(0x0B);
        out.push(0x06);
        let off = out.len();
        out.extend_from_slice(&2u16.to_le_bytes());
        fixups.push(Fixup { body_offset: off - 1, kind: FixupKind::GlobalAddr { global_idx: idx } });
        return;
    }
    // Long local/param vs zero idiom: `mov ax,[lo]; or ax,[hi]` — ZF set iff
    // both halves are zero (a long is 0 iff its low and high words are both 0).
    // Covers `if (v)`, `if (v == 0L)`, `if (v != 0L)` for a long local or param;
    // the generic path would wrongly test only the low word. Fixtures
    // 3261/3263/3265/3298.
    fn long_lp_disp(e: &Expr, locals: &Locals<'_>) -> Option<i16> {
        match e {
            Expr::Local(i) if locals.is_long_local(*i) => Some(locals.disp(*i)),
            Expr::Param(i) if locals.is_long_param(*i) => Some(long_param_disp(*i, locals)),
            _ => None,
        }
    }
    if let Some(disp) = match cond {
        Cond::Truthy(e) => long_lp_disp(e, locals),
        Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left, right } if matches!(right, Expr::IntLit(0)) =>
            long_lp_disp(left, locals),
        Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left, right } if matches!(left, Expr::IntLit(0)) =>
            long_lp_disp(right, locals),
        _ => None,
    } {
        let hi = disp + 2;
        out.push(0x8B); out.push(bp_modrm(0x46, disp)); push_bp_disp(out, disp); // mov ax,[bp+lo]
        out.push(0x0B); out.push(bp_modrm(0x46, hi)); push_bp_disp(out, hi);     // or ax,[bp+hi]
        return;
    }
    // Pointer-as-condition: `if (p)` / `if (p == 0)` where p holds `&x`/`&g`.
    //   &local x at [bp-K] → `cmp bp, K`  (the pointer is 0 iff bp == K)
    //   &global g          → `mov ax, OFFSET g; or ax, ax`
    if let Cond::Truthy(Expr::AddrOfLocal(x))
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::AddrOfLocal(x), right: Expr::IntLit(0) }
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::IntLit(0), right: Expr::AddrOfLocal(x) } = cond
    {
        let k = -locals.disp(*x);
        if let Ok(k8) = i8::try_from(k) {
            out.extend_from_slice(&[0x83, 0xFD, k8 as u8]); // cmp bp, imm8sx
        } else {
            out.push(0x81); out.push(0xFD); out.extend_from_slice(&(k as u16).to_le_bytes());
        }
        return;
    }
    if let Cond::Truthy(Expr::AddrOfGlobal(g))
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::AddrOfGlobal(g), right: Expr::IntLit(0) }
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::IntLit(0), right: Expr::AddrOfGlobal(g) } = cond
    {
        let off = out.len();
        out.extend_from_slice(&[0xB8, 0x00, 0x00]); // mov ax, OFFSET g
        fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: *g } });
        out.extend_from_slice(&[0x0B, 0xC0]); // or ax, ax
        return;
    }
    // Global array element in a condition: `if (a[K])` / `if (a[K] <op> imm)`
    // (incl. `p[K]` resolved through a global-pointer alias) → a direct
    // `cmp word/byte ptr [_a+off], imm` rather than load-then-test. The element
    // index is a constant here (folded earlier). Fixtures 889, 891, 892, 898.
    if let Some((array, k, cmp_to)) = match cond {
        Cond::Truthy(Expr::Index { array, index } | Expr::IndexByte { array, index }) => {
            if let Expr::IntLit(k) = index.as_ref() { Some((*array, *k, 0)) } else { None }
        }
        Cond::Cmp { op: _, left: Expr::Index { array, index } | Expr::IndexByte { array, index }, right: Expr::IntLit(m) }
        | Cond::Cmp { op: _, left: Expr::IntLit(m), right: Expr::Index { array, index } | Expr::IndexByte { array, index } } => {
            if let Expr::IntLit(k) = index.as_ref() { Some((*array, *k, *m)) } else { None }
        }
        _ => None,
    } {
        let is_byte = matches!(cond,
            Cond::Truthy(Expr::IndexByte { .. })
            | Cond::Cmp { left: Expr::IndexByte { .. }, .. }
            | Cond::Cmp { right: Expr::IndexByte { .. }, .. });
        let elem = if is_byte { 1 } else { 2 };
        let byte_off = (k as u16).wrapping_mul(elem);
        emit_cmp_global_off_imm(array, byte_off, cmp_to, is_byte, out, fixups);
        return;
    }
    // Bitwise-AND condition: `if (x & K)` and `if ((x & K) == 0 / != 0)` lower to
    // `test byte/word [mem], K`. The caller's jcc (Truthy/Ne → jne, Eq → je) and
    // the skip path's inverted jcc both read ZF after the test consistently.
    // Fixtures 1704, 2679, 3270, 3264, 3269, 3536, 3540, …
    if let Some((operand, k)) = match cond {
        Cond::Truthy(Expr::BinOp { op: BinOp::BitAnd, left, right }) => bitand_mask(left, right),
        Cond::Cmp { op: RelOp::Eq | RelOp::Ne,
            left: Expr::BinOp { op: BinOp::BitAnd, left: a, right: b }, right: Expr::IntLit(0) }
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::IntLit(0),
            right: Expr::BinOp { op: BinOp::BitAnd, left: a, right: b } } => bitand_mask(a, b),
        _ => None,
    } {
        emit_test_mem_imm(operand, k, locals, out, fixups);
        return;
    }
    // Bitwise-AND condition with a VARIABLE mask: `if ((x & mask) != 0)` where
    // both operands are simple word lvalues → `mov ax,[mask]; test [x],ax`
    // (the `test` sets ZF without a destructive AND). Fixture 3539.
    if let Some((left, right)) = match cond {
        Cond::Truthy(Expr::BinOp { op: BinOp::BitAnd, left, right }) => bitand_word_vars(left, right, locals),
        Cond::Cmp { op: RelOp::Eq | RelOp::Ne,
            left: Expr::BinOp { op: BinOp::BitAnd, left: a, right: b }, right: Expr::IntLit(0) }
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::IntLit(0),
            right: Expr::BinOp { op: BinOp::BitAnd, left: a, right: b } } => bitand_word_vars(a, b, locals),
        _ => None,
    } {
        emit_expr_to_ax(right, locals, out, fixups); // mov ax, [mask]
        match left {
            Expr::Local(i) => { let d = locals.disp(*i); out.push(0x85); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); }
            Expr::Param(i) => { let d = param_disp(*i); out.push(0x85); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); }
            Expr::Global(i) => {
                out.push(0x85); out.push(0x06);
                let pos = out.len(); out.extend_from_slice(&[0x00, 0x00]);
                fixups.push(Fixup { body_offset: pos - 1, kind: FixupKind::GlobalAddr { global_idx: *i } });
            }
            _ => unreachable!(),
        }
        return;
    }
    match cond {
        Cond::Truthy(Expr::Local(idx)) => emit_cmp_local_imm(*idx, locals, 0, out),
        Cond::Truthy(Expr::Param(idx)) => {
            if locals.is_char_param(*idx) {
                out.extend_from_slice(&[0x80, 0x7E, param_disp(*idx) as u8, 0x00]);
            } else {
                emit_cmp_bp_imm(param_disp(*idx), 0, out);
            }
        }
        Cond::Truthy(Expr::Global(idx)) => {
            if locals.is_char_global(*idx) {
                emit_cmp_global_off_imm(*idx, 0, 0, true, out, fixups); // cmp byte [g], 0
            } else {
                emit_cmp_global_imm(*idx, 0, out, fixups);
            }
        }
        // `<register local> OP const` → `cmp si,K` (83 /7 FE for imm8sx, else
        // 81 /7 FE imm16). Fixture 2245 (`i <= 10`).
        Cond::Cmp { op: _, left: Expr::Local(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Local(idx) }
            if locals.reg_for_local(*idx).is_some() =>
        {
            let reg = locals.reg_for_local(*idx).unwrap();
            let modrm = 0xC0 | (7 << 3) | reg; // /7 = cmp, rm = si/di
            if let Ok(k8) = i8::try_from(*k) {
                out.extend_from_slice(&[0x83, modrm, k8 as u8]);
            } else {
                out.push(0x81); out.push(modrm);
                out.extend_from_slice(&((*k as u32 & 0xFFFF) as u16).to_le_bytes());
            }
        }
        // `<memory> OP <register local>` → `cmp [mem],si` (39 form, register in
        // the reg field). The emit_for normalization puts the register on the
        // right; left is a param/local/global memory operand. Fixture 3305
        // (`cmp [bp+4],si`).
        Cond::Cmp { op: _, left, right: Expr::Local(rj) }
            if locals.reg_for_local(*rj).is_some()
                && matches!(left, Expr::Param(_) | Expr::Local(_) | Expr::Global(_)) =>
        {
            let reg = locals.reg_for_local(*rj).unwrap();
            let modrm_reg = reg << 3;
            out.push(0x39);
            match left {
                Expr::Param(pi) => { out.push(bp_modrm(0x40 | modrm_reg | 0x06, param_disp(*pi))); push_bp_disp(out, param_disp(*pi)); }
                Expr::Local(li) => { let d = locals.disp(*li); out.push(bp_modrm(0x40 | modrm_reg | 0x06, d)); push_bp_disp(out, d); }
                Expr::Global(g) => {
                    out.push(modrm_reg | 0x06); // mod=00 rm=110 disp16
                    let p = out.len(); out.extend_from_slice(&[0x00, 0x00]);
                    fixups.push(Fixup { body_offset: p - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
                }
                _ => unreachable!(),
            }
        }
        Cond::Cmp { op: _, left: Expr::Local(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Local(idx) } => {
            emit_cmp_local_imm(*idx, locals, *k, out);
        }
        Cond::Cmp { op: _, left: Expr::Param(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Param(idx) } => {
            if locals.is_char_param(*idx) {
                // `cmp byte ptr [bp+disp], imm8` — MSC uses byte compare
                // for char params (fixtures 3130, 3131, 3144, 3345 etc).
                out.extend_from_slice(&[0x80, 0x7E, param_disp(*idx) as u8, (*k as u32 & 0xFF) as u8]);
            } else {
                emit_cmp_bp_imm(param_disp(*idx), *k, out);
            }
        }
        Cond::Cmp { op: _, left: Expr::Local(li), right: Expr::Param(pi) } => {
            // MSC loads right (param) into AX, then `cmp [local], ax`.
            emit_load_param(*pi, out);
            let l_disp = locals.disp(*li);
            out.push(0x39); out.push(bp_modrm(0x46, l_disp)); push_bp_disp(out, l_disp);
        }
        Cond::Cmp { op: _, left: Expr::Local(li), right: Expr::Local(rj) } => {
            // MSC loads right into AX, then `cmp [left], ax`. If the right local
            // was just stored from AX (`mov [rj],ax`), it's still live — skip the
            // reload (the straight-line analog of the return AX-reuse).
            if !word_local_live_in_ax(*rj, locals, out) {
                emit_load_local(*rj, locals, out);
            }
            let l_disp = locals.disp(*li);
            out.push(0x39); out.push(bp_modrm(0x46, l_disp)); push_bp_disp(out, l_disp);
        }
        // `<param|local> <op> <global>` — load the global into AX (a char global
        // widens via emit_expr_to_ax), then `cmp [param|local], ax`. Fixture 3435.
        Cond::Cmp { op: _, left: Expr::Param(pi), right: Expr::Global(g) }
            if !locals.is_long_global(*g) =>
        {
            emit_expr_to_ax(&Expr::Global(*g), locals, out, fixups);
            out.extend_from_slice(&[0x39, 0x46, param_disp(*pi) as u8]); // cmp [bp+pd], ax
        }
        Cond::Cmp { op: _, left: Expr::Local(li), right: Expr::Global(g) }
            if !locals.is_long_global(*g) =>
        {
            emit_expr_to_ax(&Expr::Global(*g), locals, out, fixups);
            let d = locals.disp(*li);
            out.push(0x39); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); // cmp [local], ax
        }
        Cond::Cmp { op: _, left: Expr::Param(li), right: Expr::Param(rj) }
            if locals.is_char_param(*li) && locals.is_char_param(*rj) =>
        {
            // Two char params: byte load right into AL, byte `cmp [left], al`
            // (fixtures 3565/2984/3542). `8a 46 rd; 38 46 ld`.
            out.extend_from_slice(&[0x8A, 0x46, param_disp(*rj) as u8]);
            out.extend_from_slice(&[0x38, 0x46, param_disp(*li) as u8]);
        }
        Cond::Cmp { op: _, left: Expr::Param(li), right: Expr::Param(rj) }
            if locals.is_char_param(*li) && !locals.is_char_param(*rj) =>
        {
            // char (left) vs int (right): widen the char into AX, then word
            // `cmp ax, [right]` — result = left - right (fixture 3619).
            out.extend_from_slice(&[0x8A, 0x46, param_disp(*li) as u8, 0x98]); // mov al,[li]; cbw
            out.extend_from_slice(&[0x3B, 0x46, param_disp(*rj) as u8]); // cmp ax,[rj]
        }
        Cond::Cmp { op: _, left: Expr::Param(li), right: Expr::Param(rj) } => {
            // MSC loads right (rj) into AX, then `cmp [left], ax`.
            emit_load_param(*rj, out);
            let l_disp = param_disp(*li);
            out.push(0x39); out.push(0x46); out.push(l_disp as u8); // params always small
        }
        Cond::Cmp { op: _, left: Expr::Param(pi), right: Expr::Local(li) } => {
            // MSC loads right (local) into AX, then `cmp [param], ax`. Reuse a live
            // AX when the local was just stored from it (`m = a; if (b < m)`).
            // Fixture 1445.
            if !word_local_live_in_ax(*li, locals, out) {
                emit_load_local(*li, locals, out);
            }
            let p_disp = param_disp(*pi);
            out.push(0x39); out.push(0x46); out.push(p_disp as u8); // params always small
        }
        Cond::Cmp { op: _, left: Expr::Global(idx), right: Expr::IntLit(k) }
        | Cond::Cmp { op: _, left: Expr::IntLit(k), right: Expr::Global(idx) } => {
            if locals.is_char_global(*idx) {
                emit_cmp_global_off_imm(*idx, 0, *k, true, out, fixups); // cmp byte [g], imm8
            } else {
                emit_cmp_global_imm(*idx, *k, out, fixups);
            }
        }
        // Global-global comparison (non-long): load right into AX, then cmp [left], ax.
        // Result = left - right, so inverted_jcc / loop_back_jcc semantics are standard.
        Cond::Cmp { op: _, left: Expr::Global(li), right: Expr::Global(ri) }
            if !locals.is_long_global(*li) && !locals.is_long_global(*ri) =>
        {
            let off = out.len();
            out.extend_from_slice(&[0xA1, 0x00, 0x00]); // mov ax, [right]
            fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: *ri } });
            let off = out.len();
            out.extend_from_slice(&[0x39, 0x06, 0x00, 0x00]); // cmp [left], ax
            fixups.push(Fixup { body_offset: off + 1, kind: FixupKind::GlobalAddr { global_idx: *li } });
        }
        // Address-vs-address comparison: two pointer locals holding distinct
        // `&x`/`&g` values compared with `== / !=`. MSC does NOT fold even when
        // the bases are provably distinct — it materializes both addresses.
        //
        // Both global addresses use the immediate form (fixtures 3944-3947):
        //   `mov ax, OFFSET g_left; cmp ax, OFFSET g_right`
        Cond::Cmp { op: _, left: Expr::AddrOfGlobal(lg), right: Expr::AddrOfGlobal(rg) } => {
            let off = out.len();
            out.extend_from_slice(&[0xB8, 0x00, 0x00]); // mov ax, OFFSET left
            fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: *lg } });
            let off = out.len();
            out.extend_from_slice(&[0x3D, 0x00, 0x00]); // cmp ax, OFFSET right
            fixups.push(Fixup { body_offset: off, kind: FixupKind::GlobalAddr { global_idx: *rg } });
        }
        // Any local address involved: re-materialize right into AX, left into
        // CX (lea for locals, mov-OFFSET for globals), then `cmp cx, ax`.
        // Fixture 1235.
        Cond::Cmp { op: _, left, right }
            if matches!(left, Expr::AddrOfLocal(_) | Expr::AddrOfGlobal(_))
                && matches!(right, Expr::AddrOfLocal(_) | Expr::AddrOfGlobal(_)) =>
        {
            emit_addr_to_reg(right, false, locals, out, fixups); // right → AX
            emit_addr_to_reg(left, true, locals, out, fixups); // left → CX
            out.extend_from_slice(&[0x3B, 0xC8]); // cmp cx, ax
        }
        // `a[i] == 0` / `a[i] != 0` (runtime index, word global array) → the same
        // `mov bx,[i]; shl bx,1; cmp word [bx+_a],0` as `Truthy(a[i])` below — the
        // caller's jcc (Eq→je / Ne→jne) reads ZF identically. Must precede the
        // generic `Cmp{_, IntLit}` fallback, which would otherwise load to AX and
        // `or ax,ax`. Fixture 3232 (`while (i<n && data[i] != 0)`).
        Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::Index { array, index }, right: Expr::IntLit(0) }
        | Cond::Cmp { op: RelOp::Eq | RelOp::Ne, left: Expr::IntLit(0), right: Expr::Index { array, index } }
            if index.fold(locals.inits).is_none()
                && matches!(index.as_ref(), Expr::Local(_) | Expr::Param(_)) =>
        {
            match index.as_ref() {
                Expr::Local(i) => { let disp = locals.disp(*i); out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); }
                Expr::Param(i) => { out.extend_from_slice(&[0x8B, 0x5E, param_disp(*i) as u8]); }
                _ => unreachable!(),
            }
            out.extend_from_slice(&[0xD1, 0xE3]); // shl bx,1
            let body_offset = out.len();
            out.extend_from_slice(&[0x83, 0xBF, 0x00, 0x00, 0x00]); // cmp word [bx+addr],0
            fixups.push(Fixup { body_offset: body_offset + 1, kind: FixupKind::GlobalAddr { global_idx: *array } });
        }
        // Generic fallback for unsupported cond shapes: evaluate LHS
        // into AX, then `cmp ax, K` (`3d K K` for word, `83 f8 K` for
        // imm8sx). Used for `p->x == K` and other DerefLocalField cases.
        Cond::Cmp { op: _, left, right: Expr::IntLit(k) } => {
            emit_expr_to_ax(left, locals, out, fixups);
            emit_cmp_ax_imm(*k, out);
        }
        Cond::Cmp { op: _, left: Expr::IntLit(k), right } => {
            emit_expr_to_ax(right, locals, out, fixups);
            emit_cmp_ax_imm(*k, out);
        }
        // COMPUTED LHS (arithmetic) vs a simple word memory RHS (`(a+b) == c`):
        // evaluate the LHS into AX, then `cmp ax,[rhs]` directly — no BX round-
        // trip for the RHS. Result = left - right → standard jcc. A memory LHS
        // (deref/index) uses the opposite `cmp [lhs],ax` form (not handled here);
        // char/long RHS fall through. Fixture 3596.
        Cond::Cmp { op: _, left: left @ Expr::BinOp { .. }, right }
            if matches!(right,
                Expr::Param(i) if !locals.is_char_param(*i) && !locals.is_long_param(*i))
                || matches!(right,
                    Expr::Local(i) if locals.size(*i) == 2 && !locals.is_long_local(*i) && !locals.is_float_local(*i))
                || matches!(right,
                    Expr::Global(g) if !locals.is_char_global(*g) && !locals.is_long_global(*g)) =>
        {
            emit_expr_to_ax(left, locals, out, fixups);
            match right {
                Expr::Param(i) => { let d = param_disp(*i); out.push(0x3B); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); }
                Expr::Local(i) => { let d = locals.disp(*i); out.push(0x3B); out.push(bp_modrm(0x46, d)); push_bp_disp(out, d); }
                Expr::Global(g) => {
                    out.extend_from_slice(&[0x3B, 0x06]);
                    let p = out.len(); out.extend_from_slice(&[0x00, 0x00]);
                    fixups.push(Fixup { body_offset: p - 1, kind: FixupKind::GlobalAddr { global_idx: *g } });
                }
                _ => unreachable!(),
            }
        }
        // Generic fallback for two-sided non-literal comparisons:
        // evaluate right into AX, save to BX, evaluate left into AX, cmp ax, bx.
        // Result = left - right → standard jcc semantics.
        Cond::Cmp { op: _, left, right } => {
            emit_expr_to_ax(right, locals, out, fixups);
            out.extend_from_slice(&[0x8B, 0xD8]); // mov bx, ax
            emit_expr_to_ax(left, locals, out, fixups);
            out.extend_from_slice(&[0x3B, 0xC3]); // cmp ax, bx
        }
        // `while (a[i])` / `if (a[i])` where `a` is a global int array
        // and `i` is a runtime variable — MSC emits `mov bx, [i];
        // shl bx, 1; cmp WORD PTR [bx+_a], 0` (saves one byte vs
        // loading to AX then `or ax,ax`). Fixture 1309.
        // `while (a[i])` / `if (a[i])` where `a` is a global int array
        // and `i` is a runtime variable — MSC emits `mov bx, [i];
        // shl bx, 1; cmp WORD PTR [bx+_a], 0` (saves one byte vs
        // loading to AX then `or ax,ax`). Fixture 1309.
        Cond::Truthy(Expr::Index { array, index })
            if index.fold(locals.inits).is_none() =>
        {
            match index.as_ref() {
                Expr::Local(i) => {
                    let disp = locals.disp(*i);
                    { out.push(0x8B); out.push(bp_modrm(0x5E, disp)); push_bp_disp(out, disp); } // mov bx, [bp+disp]
                }
                Expr::Param(i) => {
                    let pdisp = param_disp(*i) as i8;
                    out.extend_from_slice(&[0x8B, 0x5E, pdisp as u8]);
                }
                _ => {
                    // Fallback: load to AX, test
                    emit_expr_to_ax(&Expr::Index { array: *array, index: index.clone() }, locals, out, fixups);
                    out.extend_from_slice(&[0x0B, 0xC0]);
                    return;
                }
            }
            out.extend_from_slice(&[0xD1, 0xE3]); // shl bx, 1
            let body_offset = out.len();
            out.extend_from_slice(&[0x83, 0xBF, 0x00, 0x00, 0x00]); // cmp word [bx+addr], 0
            fixups.push(Fixup {
                body_offset: body_offset + 1,
                kind: FixupKind::GlobalAddr { global_idx: *array },
            });
        }
        Cond::Truthy(expr) => {
            // `if (<expr>)` — evaluate to AX, then test ax, ax (or
            // cmp ax, 0). MSC picks `or ax, ax` (`0b c0`) as the
            // 2-byte test. Most truthy-on-local/global cases were
            // handled earlier; this is the fallback.
            emit_expr_to_ax(expr, locals, out, fixups);
            out.extend_from_slice(&[0x0B, 0xC0]);
        }
        other => panic!("Slice 5 cond shape not yet supported: {other:?}"),
    }
}
/// `cmp ax, imm` — MSC uses `3d imm16` (cmp ax, imm16) for a nonzero
/// immediate, but tests against zero with the shorter `or ax,ax`
/// (`0b c0`), which sets ZF/SF identically (CF/OF cleared, so signed and
/// equality jcc both read correctly). Fixtures 3639, 3548, 3267, 2693.
pub(crate) fn emit_cmp_ax_imm(k: i32, out: &mut Vec<u8>) {
    if k == 0 {
        out.extend_from_slice(&[0x0B, 0xC0]); // or ax, ax
        return;
    }
    let k16 = (k as u32 & 0xFFFF) as u16;
    out.push(0x3D);
    out.extend_from_slice(&k16.to_le_bytes());
}
/// The signed conditional-jump opcode for **falling through to the
/// then-block** when `op` is satisfied. Used by `emit_cond_skip`: the
/// emitted `jcc skip` triggers when the cond is *false*, so this is
/// the **inversion** of the source-level relop. MSC's signed-jcc
/// mnemonic family is `7c..7f` for the disp8 forms.
/// Map a signed conditional-jump opcode to its unsigned counterpart for the
/// relational forms (jge→jae, jg→ja, jle→jbe, jl→jb); je/jne are unchanged.
pub(crate) fn to_unsigned_jcc(signed: u8) -> u8 {
    match signed {
        0x7D => 0x73, // jge → jae
        0x7F => 0x77, // jg  → ja
        0x7E => 0x76, // jle → jbe
        0x7C => 0x72, // jl  → jb
        other => other,
    }
}
/// True when a comparison's operands are unsigned (any unsigned operand makes
/// the comparison unsigned), so the jcc must use the unsigned variant.
pub(crate) fn cmp_is_unsigned(cond: &Cond, locals: &Locals<'_>) -> bool {
    fn operand_unsigned(e: &Expr, locals: &Locals<'_>) -> bool {
        // A pointer operand compares UNSIGNED (addresses) — ordering uses jb/jae,
        // not jl/jge. Fixtures 2934 (int*), 2929 (struct*).
        match e {
            Expr::Param(i) => locals.is_unsigned_param(*i) || locals.param_pointee_size(*i) > 0,
            Expr::Local(i) => locals.is_unsigned_local(*i) || locals.local_pointee_size(*i) > 0,
            Expr::Global(g) => locals.is_unsigned_global(*g),
            // A materialized address (`&g`/`&x`, from pointer-alias
            // substitution) is an unsigned operand. Fixtures 3938-3943.
            Expr::AddrOfGlobal(_) | Expr::AddrOfLocal(_) | Expr::AddrOfIndexedGlobal { .. } => true,
            _ => false,
        }
    }
    matches!(cond, Cond::Cmp { left, right, .. }
        if operand_unsigned(left, locals) || operand_unsigned(right, locals))
}
pub(crate) fn inverted_jcc(op: RelOp) -> u8 {
    match op {
        RelOp::Eq => 0x75, // jne — skip then-block when not-equal
        RelOp::Ne => 0x74, // je
        RelOp::Lt => 0x7D, // jge — signed
        RelOp::Le => 0x7F, // jg
        RelOp::Gt => 0x7E, // jle
        RelOp::Ge => 0x7C, // jl
    }
}
/// Loop-back jcc opcode — fires when cond is *true*, so it's NOT
/// inverted. Used by `emit_while` / `emit_do_while` for the tail
/// branch back to the loop top.
/// True when `out` ends with the exact `mov [bp+disp(idx)], ax` store of word
/// local `idx`, so AX still holds its value and a reload can be skipped. Gated to
/// plain word (int) locals — char/long/float use other load shapes.
fn word_local_live_in_ax(idx: usize, locals: &Locals<'_>, out: &[u8]) -> bool {
    if locals.size(idx) != 2 || locals.is_long_local(idx) || locals.is_float_local(idx) {
        return false;
    }
    let d = locals.disp(idx);
    let mut store = vec![0x89, bp_modrm(0x46, d)];
    push_bp_disp(&mut store, d);
    if out.len() < store.len() || out[out.len() - store.len()..] != *store {
        return false;
    }
    // The store must be STRAIGHT-LINE — at or past the last branch merge — so AX
    // isn't assumed live across a conditional branch (MSC reloads after a merge).
    out.len() - store.len() >= locals.last_branch_barrier.get()
}
pub(crate) fn loop_back_jcc(op: RelOp) -> u8 {
    match op {
        RelOp::Eq => 0x74, // je
        RelOp::Ne => 0x75, // jne
        RelOp::Lt => 0x7C, // jl
        RelOp::Le => 0x7E, // jle
        RelOp::Gt => 0x7F, // jg
        RelOp::Ge => 0x7D, // jge
    }
}
/// `cmp word ptr [<global-addr>], imm8` — MSC picks the `83 3e disp imm8`
/// form for global compares against a sign-extended byte (fixtures
/// 4129, 4133). The placeholder address gets a GlobalAddr FIXUP.
/// `cmp word/byte ptr [_g + byte_off], imm` — a global array element compared to
/// an immediate. The element address placeholder carries byte_off as the
/// GlobalAddr addend. Used for `a[K]`/`p[K]` in conditions. Fixtures 889, 891.
pub(crate) fn emit_cmp_global_off_imm(global_idx: usize, byte_off: u16, k: i32, is_byte: bool, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let emit_addr = |out: &mut Vec<u8>, fixups: &mut Vec<Fixup>| {
        let bo = out.len();
        out.extend_from_slice(&byte_off.to_le_bytes());
        fixups.push(Fixup { body_offset: bo - 1, kind: FixupKind::GlobalAddr { global_idx } });
    };
    if is_byte {
        out.push(0x80); out.push(0x3E); emit_addr(out, fixups); out.push((k as u32 & 0xFF) as u8);
    } else if let Ok(k8) = i8::try_from(k) {
        out.push(0x83); out.push(0x3E); emit_addr(out, fixups); out.push(k8 as u8);
    } else {
        out.push(0x81); out.push(0x3E); emit_addr(out, fixups);
        out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes());
    }
}
pub(crate) fn emit_cmp_global_imm(global_idx: usize, k: i32, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    if let Ok(k_i8) = i8::try_from(k) {
        out.push(0x83);
        out.push(0x3E);
        let body_offset = out.len();
        out.extend_from_slice(&[0x00, 0x00, k_i8 as u8]);
        fixups.push(Fixup { body_offset: body_offset - 1, kind: FixupKind::GlobalAddr { global_idx } });
    } else {
        // 16-bit immediate form: `81 3e addr_lo addr_hi imm_lo imm_hi`
        out.push(0x81);
        out.push(0x3E);
        let body_offset = out.len();
        out.extend_from_slice(&[0x00, 0x00]);
        fixups.push(Fixup { body_offset: body_offset - 1, kind: FixupKind::GlobalAddr { global_idx } });
        let imm = (k as u32 & 0xFFFF) as u16;
        out.extend_from_slice(&imm.to_le_bytes());
    }
}
/// `cmp word ptr [bp-disp], imm` — MSC picks the `83 /7 r/m imm8sx`
/// form when the immediate fits in a sign-extended byte (which is
/// every fixture exercised by Slice 5 today). The 5-byte
/// `81 7e disp imm16` form is reserved for larger constants.
pub(crate) fn emit_cmp_local_imm(idx: usize, locals: &Locals<'_>, k: i32, out: &mut Vec<u8>) {
    if locals.size(idx) == 1 {
        // `cmp byte [bp-disp], imm8` — `80 /7 r/m imm8`. Fixture 124.
        out.extend_from_slice(&[0x80, 0x7E, locals.disp(idx) as u8, (k as u32 & 0xFF) as u8]);
        return;
    }
    emit_cmp_bp_imm(locals.disp(idx), k, out);
}
/// `cmp word ptr [bp+disp], imm` — covers both local (negative disp)
/// and param (positive disp) compares. Picks `83 /7 r/m imm8sx` for
/// the small-immediate form, `81 /7 r/m imm16` for the wide form.
pub(crate) fn emit_cmp_bp_imm(disp: i16, k: i32, out: &mut Vec<u8>) {
    if let Ok(k_i8) = i8::try_from(k) {
        out.push(0x83);
        out.push(bp_modrm(0x7E, disp)); // /7=CMP, mod=01/10 bp-rel
        push_bp_disp(out, disp);
        out.push(k_i8 as u8);
    } else {
        let k16 = (k as u32 & 0xFFFF) as u16;
        out.push(0x81);
        out.push(bp_modrm(0x7E, disp));
        push_bp_disp(out, disp);
        out.extend_from_slice(&k16.to_le_bytes());
    }
}
/// If one operand of a `&` is a mask literal and the other a simple memory
/// operand (Local/Param/Global), return `(operand, mask)` for a `test`.
fn bitand_mask<'a>(a: &'a Expr, b: &'a Expr) -> Option<(&'a Expr, i32)> {
    let simple = |e: &Expr| matches!(e, Expr::Local(_) | Expr::Param(_) | Expr::Global(_));
    match (a, b) {
        (m, Expr::IntLit(k)) if simple(m) => Some((m, *k)),
        (Expr::IntLit(k), m) if simple(m) => Some((m, *k)),
        _ => None,
    }
}
/// `x & mask` where BOTH operands are simple word (int) lvalues — neither a
/// literal. Returns `(left, right)`; the caller loads `right` into AX and emits
/// `test [left], ax`. Char/long operands are excluded (different widths).
fn bitand_word_vars<'a>(a: &'a Expr, b: &'a Expr, locals: &Locals<'_>) -> Option<(&'a Expr, &'a Expr)> {
    let word = |e: &Expr| match e {
        Expr::Local(i) => locals.size(*i) == 2 && !locals.is_long_local(*i),
        Expr::Param(i) => !locals.is_char_param(*i) && !locals.is_long_param(*i),
        Expr::Global(i) => !locals.is_char_global(*i) && !locals.is_long_global(*i),
        _ => false,
    };
    // Both simple word lvalues: evaluate b into AX, `test [a],ax` (3539).
    // One simple + one complex (non-literal — const masks take the
    // `test [mem],imm` path above): evaluate the complex side into AX and
    // test against the simple one in memory — `mask & (1 << i)` →
    // `shl ax,cl; test ax,[mask]` (2399).
    if word(a) && !matches!(b, Expr::IntLit(_)) { return Some((a, b)); }
    if word(b) && !matches!(a, Expr::IntLit(_)) { return Some((b, a)); }
    None
}
/// `test byte/word [mem], imm` — MSC's lowering for a bitwise-AND condition.
/// The byte form (`f6 /0`) is used when the mask fits in 8 bits.
fn emit_test_mem_imm(operand: &Expr, k: i32, locals: &Locals<'_>, out: &mut Vec<u8>, fixups: &mut Vec<Fixup>) {
    let is_byte = (0..=0xFF).contains(&k);
    let push_imm = |out: &mut Vec<u8>| {
        if is_byte { out.push((k as u32 & 0xFF) as u8); }
        else { out.extend_from_slice(&((k as u32 & 0xFFFF) as u16).to_le_bytes()); }
    };
    let opcode = if is_byte { 0xF6 } else { 0xF7 };
    match operand {
        Expr::Local(i) => {
            let disp = locals.disp(*i);
            out.push(opcode);
            out.push(bp_modrm(0x46, disp)); // /0=TEST, bp-rel
            push_bp_disp(out, disp);
            push_imm(out);
        }
        Expr::Param(i) => {
            let disp = param_disp(*i);
            out.push(opcode);
            out.push(bp_modrm(0x46, disp));
            push_bp_disp(out, disp);
            push_imm(out);
        }
        Expr::Global(i) => {
            out.push(opcode);
            out.push(0x06); // /0=TEST, disp16
            let pos = out.len();
            out.extend_from_slice(&[0x00, 0x00]);
            fixups.push(Fixup { body_offset: pos - 1, kind: FixupKind::GlobalAddr { global_idx: *i } });
            push_imm(out);
        }
        _ => unreachable!("emit_test_mem_imm: non-simple operand"),
    }
}
