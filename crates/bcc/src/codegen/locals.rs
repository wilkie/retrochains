//! Local-variable + parameter layout for one function.
//!
//! BCC enregisters some locals and parameters into a small fixed pool
//! of registers (SI, DI, DX, BX in that order) before falling back to
//! stack slots. The decision is driven by a use-count heuristic
//! captured in the investigation fixtures `028`–`032` (locals) and
//! `035` (params), and documented in `specs/bcc/ASM_OUTPUT.md`.
//!
//! - Count every textual occurrence of each declarable name (param or
//!   local), plus one implicit "init use" for the declaration itself.
//! - Names with ≥ 3 occurrences are eligible for a register.
//! - Eligible names receive registers in **source order** (params
//!   first, in their declaration order; then locals, also in
//!   declaration order) from the pool `[SI, DI, DX, BX]`; the rest
//!   stay at their stack slot.
//!
//! Stack-resident locals live at negative bp offsets (below `bp`);
//! stack-resident params live at the positive bp offsets the caller
//! pushed them to (`[bp+4]`, `[bp+6]`, … for the small memory model,
//! whose 2-byte near-call return address sits at `[bp+2]`).
//!
//! Only `int` locals/params are eligible for register allocation; we
//! haven't observed BCC enregistering a `char` and don't have a fixture
//! to pin its shape.

use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, ExprKind, Function, Stmt, StmtKind, Type};
use crate::codegen::plan::{pick_switch_strategy, SwitchStrategy};

/// Round `n` up to the next multiple of `alignment` (a small power of 2).
fn align_up(n: u16, alignment: u16) -> u16 {
    let mask = alignment - 1;
    (n + mask) & !mask
}

/// Where one local or parameter lives for the duration of the function.
#[derive(Debug, Clone, Copy)]
pub enum LocalLocation {
    /// bp-relative stack slot. Negative ⇒ below `bp` (a local).
    /// Positive ⇒ above `bp` (an incoming parameter the caller left
    /// on the stack).
    Stack(i16),
    /// Register-resident.
    Reg(Reg),
}

/// Registers BCC uses for enregistered variables.
///
/// 16-bit `int` locals/params draw from `{Si, Di, Dx, Bx, Cx}` (AX is
/// BCC's working/return register, SP/BP are dedicated to the frame).
/// 8-bit `char` locals/params draw from `{Dl, Bl, Cl}` — the 8086's
/// only addressable byte registers besides AL/AH (AL is the working
/// half; AH/BH/CH/DH are unused by BCC for variables).
///
/// The byte registers alias with the low halves of `Dx`/`Bx`/`Cx`,
/// which means BCC's allocator must coordinate when a function has
/// both ints and chars. We haven't yet captured a fixture that puts
/// pressure on the aliased pool; today the two pools are allocated
/// independently and char enregistration is suppressed when the
/// function makes a call (fixture 055).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg {
    // 16-bit, for ints.
    Si,
    Di,
    Dx,
    Bx,
    Cx,
    // 8-bit, for chars.
    Dl,
    Bl,
    Cl,
}

impl Reg {
    /// The asm name (`si`/`di`/`dx`/`bx`/`cx`/`dl`/`bl`/`cl`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Si => "si",
            Self::Di => "di",
            Self::Dx => "dx",
            Self::Bx => "bx",
            Self::Cx => "cx",
            Self::Dl => "dl",
            Self::Bl => "bl",
            Self::Cl => "cl",
        }
    }

    /// True when this register is one of the 8-bit byte registers
    /// (DL/BL/CL). Used by codegen to pick between word and byte
    /// instruction forms.
    #[must_use]
    pub fn is_byte(self) -> bool {
        matches!(self, Self::Dl | Self::Bl | Self::Cl)
    }

    /// Pool used for the *non-SI* int eligibles, in assignment order.
    /// SI is handed out separately to the most-used eligible (see
    /// `Locals::analyze`).
    const NON_SI_POOL: [Self; 4] = [Self::Di, Self::Dx, Self::Bx, Self::Cx];

    /// Pool used for char eligibles, in source-order assignment.
    /// Fixtures 047/050: a char declared first lands in DL, the next
    /// in BL, the third in CL.
    const CHAR_POOL: [Self; 3] = [Self::Dl, Self::Bl, Self::Cl];

    /// Registers BCC treats as callee-saved, in canonical push order.
    /// Everything else (DX, BX, CX, DL, BL, CL) is used by BCC without
    /// push/pop at the function boundary.
    const CALLEE_SAVED: [Self; 2] = [Self::Si, Self::Di];
}

/// One register-promoted parameter that needs an `mov <reg>, word ptr
/// [bp+N]` in the prologue to copy its incoming value out of the
/// caller-built stack slot.
#[derive(Debug, Clone, Copy)]
pub struct ParamLoad {
    pub reg: Reg,
    /// Positive bp offset of the incoming param slot.
    pub incoming_offset: u16,
}

/// Use count threshold for enregistering ints and pointers. A name
/// with `>= ENREGISTER_THRESHOLD` textual occurrences (init + reads
/// + writes, with `*p` and `p[i]` direct-derefs counting as 2) gets
/// a register if one is still available.
///
/// Earlier slices used a lower threshold for pointers (≥ 2) to
/// explain why `int *p` enregistered after just one `*p` use; fixture
/// 092 then disproved that — `int *p = a; ... *(p + i)` keeps p on
/// the stack even with the same nominal count. The correct
/// reconciliation: pointers use the same threshold as ints, but a
/// *direct* deref (`*p` or `p[K]`) contributes 2 to the count.
const ENREGISTER_THRESHOLD: u32 = 3;

/// Offset of the **first** incoming argument relative to `bp` after
/// the standard small-model prologue (`push bp` then `mov bp,sp`):
/// `[bp+0]` saved bp, `[bp+2]` near-call return address, `[bp+4]`
/// first arg.
const FIRST_PARAM_BP_OFFSET: u16 = 4;

#[derive(Debug)]
pub struct Locals {
    /// Total bytes claimed for stack-resident *locals* only. Stack
    /// params don't contribute (they're caller-allocated above `bp`).
    stack_bytes: u16,
    by_name: HashMap<String, LocalEntry>,
    /// Callee-saved registers used by the function, in push order.
    saved_regs: Vec<Reg>,
    /// Register-promoted params; the prologue emits a `mov` per entry.
    param_loads: Vec<ParamLoad>,
    /// For each linear-search switch (keyed by `Stmt.span.start`), the
    /// signed bp-offset of its dedicated scrutinee-spill stack slot.
    /// Empty when the function has no linear-search switch — fixture
    /// 074 is the only one today.
    switch_spill_offsets: HashMap<u32, i16>,
}

#[derive(Debug, Clone)]
struct LocalEntry {
    location: LocalLocation,
    ty: Type,
}

impl Locals {
    #[must_use]
    pub fn analyze(function: &Function) -> Self {
        // Pass 1: collect all "declarable" names (params first, then
        // locals in source order). Each gets an `init`-style use plus
        // a textual count.
        let mut declared: Vec<DeclItem> = Vec::new();
        let mut counts: HashMap<String, u32> = HashMap::new();

        // Params: assign each its incoming bp+N slot.
        let mut param_offset = FIRST_PARAM_BP_OFFSET;
        for param in &function.params {
            declared.push(DeclItem {
                name: param.name.clone(),
                ty: param.ty.clone(),
                kind: DeclKind::Param { incoming_offset: param_offset },
            });
            // Every param takes a 2-byte slot on the stack regardless
            // of declared type — `char` gets promoted at the push site
            // by the caller. (We haven't pinned this with a `char`-
            // param fixture; revisit when we have one.) Long params
            // take a 4-byte slot since they don't fit in a single
            // word (fixture 285).
            param_offset += if param.ty.is_long_like() { 4 } else { 2 };
            *counts.entry(param.name.clone()).or_insert(0) += 1;
        }

        for stmt in function.body.as_deref().unwrap_or(&[]) {
            collect_decls(stmt, &mut declared);
            count_uses_stmt(stmt, &mut counts);
        }

        // Pass 2: figure out the register assignment.
        //
        // Int rule (fixtures 028–048):
        //   1. SI goes to the *most-used* eligible Int variable.
        //      Ties broken by source order — earliest wins.
        //   2. The remaining eligibles (in source order) fill the
        //      `[DI, DX, BX, CX]` pool.
        //   3. Beyond 5 eligibles, the rest spill to the stack.
        //
        // Char rule (fixtures 047, 050, 051):
        //   1. Chars draw from `[DL, BL, CL]` in source order.
        //   2. *But* char enregistration is suppressed entirely if
        //      the function makes a call (fixture 055): DL/BL/CL all
        //      alias with caller-clobbered DX/BX/CX, so a char in
        //      one of them would be lost across the call. Ints are
        //      not similarly restricted today — none of our fixtures
        //      exercise an int enregistered across a call, so we
        //      leave that path alone until a fixture forces a choice.
        let function_makes_call = body_has_call(function.body.as_deref().unwrap_or(&[]));

        // Variables whose address is taken with `&x` anywhere in the
        // function must live on the stack — a register has no address
        // to give. Fixture 080 demonstrates: `int x = 5; int *p = &x;
        // return *p;` puts x at [bp-2] even though it would otherwise
        // be a candidate.
        let mut address_taken: HashSet<String> = HashSet::new();
        for stmt in function.body.as_deref().unwrap_or(&[]) {
            collect_address_taken(stmt, &mut address_taken);
        }

        // Int-pool eligibles: ints with ≥ 3 uses, pointers with ≥ 2,
        // and never anything whose address was taken.
        let eligible_int: Vec<usize> = (0..declared.len())
            .filter(|&i| {
                if address_taken.contains(&declared[i].name) {
                    return false;
                }
                let uses = counts.get(&declared[i].name).copied().unwrap_or(0);
                match &declared[i].ty {
                    Type::Int | Type::Pointer(_) => uses >= ENREGISTER_THRESHOLD,
                    _ => false,
                }
            })
            .collect();
        let si_pick = pick_si(&eligible_int, &declared, &counts);

        let mut reg_of: HashMap<usize, Reg> = HashMap::new();
        if let Some(idx) = si_pick {
            reg_of.insert(idx, Reg::Si);
        }
        let mut non_si_iter = Reg::NON_SI_POOL.iter().copied();
        for &i in &eligible_int {
            if Some(i) == si_pick {
                continue;
            }
            let Some(reg) = non_si_iter.next() else { break };
            reg_of.insert(i, reg);
        }

        // Char eligibles — only when the function makes no call, and
        // never for chars whose address was taken.
        if !function_makes_call {
            let mut char_pool = Reg::CHAR_POOL.iter().copied();
            for (i, item) in declared.iter().enumerate() {
                if !matches!(item.ty, Type::Char) {
                    continue;
                }
                if address_taken.contains(&item.name) {
                    continue;
                }
                let uses = counts.get(&item.name).copied().unwrap_or(0);
                if uses < ENREGISTER_THRESHOLD {
                    continue;
                }
                let Some(reg) = char_pool.next() else { break };
                reg_of.insert(i, reg);
            }
        }

        // Pass 3: fill in LocalEntry, saved_regs, param_loads. The
        // order of saved_regs/param_loads is the function's natural
        // emission order:
        //
        // - saved_regs: SI first (if used), then DI (if used).
        //   Maintained by iterating in `declared` order and adding any
        //   newly-seen callee-saved register.
        // - param_loads: source order of the *params*.
        let mut by_name = HashMap::new();
        let mut stack_bytes = 0u16;
        let mut saved_regs = Vec::new();
        let mut param_loads = Vec::new();
        // Push callee-saved registers in canonical order (SI, then DI).
        // Both fixtures 046 and 048 emit `push si / push di` even when
        // DI is the first to be assigned in source order.
        for reg in Reg::CALLEE_SAVED {
            if reg_of.values().any(|&r| r == reg) {
                saved_regs.push(reg);
            }
        }
        for (i, item) in declared.iter().enumerate() {
            let location = if let Some(&reg) = reg_of.get(&i) {
                if let DeclKind::Param { incoming_offset } = item.kind {
                    param_loads.push(ParamLoad { reg, incoming_offset });
                }
                LocalLocation::Reg(reg)
            } else {
                match item.kind {
                    DeclKind::Local => {
                        // Round the slot's size up to the type's
                        // alignment so that e.g. a 3-byte struct
                        // (`{char; int;}`) occupies a 4-byte slot
                        // with the struct base at the low address —
                        // matches BCC's layout for fixture 102 once
                        // the struct's intrinsic size is the raw
                        // field-sum (3) rather than pre-rounded (4).
                        // For char arrays (alignment 1 but odd size),
                        // BCC additionally pads the slot to even
                        // bytes (fixture 577: `char s[3]` occupies a
                        // 4-byte slot with `s[2]` at the highest
                        // address byte and the byte above untouched).
                        // Apply the same even-byte pad to any local
                        // whose size_bytes is odd.
                        let mut slot_size =
                            align_up(item.ty.size_bytes(), item.ty.alignment());
                        if matches!(item.ty, Type::Array { .. })
                            && slot_size % 2 == 1
                        {
                            slot_size += 1;
                        }
                        stack_bytes = align_up(stack_bytes, item.ty.alignment())
                            + slot_size;
                        LocalLocation::Stack(
                            -i16::try_from(stack_bytes).expect("stack frame fits in i16"),
                        )
                    }
                    DeclKind::Param { incoming_offset } => LocalLocation::Stack(
                        i16::try_from(incoming_offset).expect("param offset fits in i16"),
                    ),
                }
            };
            by_name.insert(item.name.clone(), LocalEntry { location, ty: item.ty.clone() });
        }

        // Round the local frame up to an even byte count. BCC's stack
        // is word-aligned for everything that comes after the locals
        // (saved registers, callee state) — a single-char frame
        // emits two `dec sp`s, not one (fixture 055).
        stack_bytes = align_up(stack_bytes, 2);

        // Linear-search switches need a 2-byte scrutinee spill slot
        // (fixture 074: scrutinee at `[bp-4]` after the `int x` at
        // `[bp-2]`). Allocate one slot per linear-search switch in
        // the function — sequential switches each get their own slot
        // even though the values can't outlast the switch; this
        // matches what fixture 074 demonstrates (only the one switch,
        // exactly one extra slot).
        let mut switch_spill_offsets = HashMap::new();
        collect_linear_search_switches(function.body.as_deref().unwrap_or(&[]), |span_start| {
            stack_bytes += 2;
            let off = -i16::try_from(stack_bytes).expect("stack frame fits in i16");
            switch_spill_offsets.insert(span_start, off);
        });

        Self {
            stack_bytes,
            by_name,
            saved_regs,
            param_loads,
            switch_spill_offsets,
        }
    }

    #[must_use]
    pub fn stack_bytes(&self) -> u16 {
        self.stack_bytes
    }

    #[must_use]
    pub fn saved_regs(&self) -> &[Reg] {
        &self.saved_regs
    }

    #[must_use]
    pub fn param_loads(&self) -> &[ParamLoad] {
        &self.param_loads
    }

    #[must_use]
    pub fn location_of(&self, name: &str) -> LocalLocation {
        self.entry(name).location
    }

    /// Whether `name` refers to a local declared in this function.
    /// False for function names, globals, externs, and unrelated
    /// identifiers — letting callers disambiguate before trying a
    /// `location_of` that would panic.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    #[must_use]
    pub fn type_of(&self, name: &str) -> &Type {
        &self.entry(name).ty
    }

    /// Signed bp-offset of the scrutinee-spill stack slot for a
    /// linear-search switch (keyed by its statement span_start).
    /// Panics for switches that aren't linear-search — only that
    /// strategy reserves a spill slot.
    #[must_use]
    pub fn switch_spill_offset(&self, switch_span_start: u32) -> i16 {
        *self.switch_spill_offsets.get(&switch_span_start).unwrap_or_else(|| {
            panic!(
                "no spill slot reserved for switch at byte {switch_span_start} \
                 — should only be queried for linear-search switches"
            )
        })
    }

    fn entry(&self, name: &str) -> &LocalEntry {
        self.by_name
            .get(name)
            .unwrap_or_else(|| panic!("unknown local in codegen: {name}"))
    }
}

/// True when any statement in `body` contains a function-call
/// expression. Used to gate char enregistration — chars live in
/// caller-clobbered registers (DL/BL/CL), so a char that needs to
/// survive a call must stay on the stack (fixture 055).
fn body_has_call(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_call)
}

/// If `e` is a "direct deref target" — `<ident>` or
/// `<ident> + <constant>` or `<ident> - <constant>` — return the
/// ident's name. Otherwise `None`. Used to decide whether `*<e>`
/// should give the pointer name the enregistration bonus.
fn direct_deref_target(e: &Expr) -> Option<String> {
    use crate::ast::BinOp;
    match &e.kind {
        ExprKind::Ident(name) => Some(name.clone()),
        ExprKind::BinOp { op, left, right }
            if matches!(op, BinOp::Add | BinOp::Sub)
                && crate::codegen::fold_const_global(right).is_some() =>
        {
            if let ExprKind::Ident(name) = &left.kind {
                return Some(name.clone());
            }
            None
        }
        _ => None,
    }
}

/// Collect every name that appears as the target of an `&x`
/// (address-of) anywhere in `stmt`. The resulting set is used to
/// force those variables to stack-resident — register-resident
/// locals have no address to take.
fn collect_address_taken(stmt: &Stmt, out: &mut HashSet<String>) {
    match &stmt.kind {
        StmtKind::Return(value) => {
            if let Some(e) = value {
                expr_address_taken(e, out);
            }
        }
        StmtKind::Declare { init, .. } => {
            if let Some(e) = init {
                expr_address_taken(e, out);
            }
        }
        StmtKind::Assign { value, .. } | StmtKind::CompoundAssign { value, .. } => {
            expr_address_taken(value, out);
        }
        StmtKind::ArrayAssign { indices, value, .. }
        | StmtKind::ArrayCompoundAssign { indices, value, .. }
        | StmtKind::MemberArrayAssign { indices, value, .. } => {
            for ix in indices {
                expr_address_taken(ix, out);
            }
            expr_address_taken(value, out);
        }
        StmtKind::DerefAssign { target, value }
        | StmtKind::DerefCompoundAssign { target, value, .. } => {
            expr_address_taken(target, out);
            expr_address_taken(value, out);
        }
        StmtKind::MemberAssign { base, value, .. }
        | StmtKind::MemberCompoundAssign { base, value, .. } => {
            expr_address_taken(base, out);
            expr_address_taken(value, out);
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            expr_address_taken(cond, out);
            for s in then_branch {
                collect_address_taken(s, out);
            }
            if let Some(b) = else_branch {
                for s in b {
                    collect_address_taken(s, out);
                }
            }
        }
        StmtKind::While { cond, body } => {
            expr_address_taken(cond, out);
            for s in body {
                collect_address_taken(s, out);
            }
        }
        StmtKind::DoWhile { body, cond } => {
            for s in body {
                collect_address_taken(s, out);
            }
            expr_address_taken(cond, out);
        }
        StmtKind::For { init, cond, step, body } => {
            if let Some(exprs) = init {
                for e in exprs {
                    expr_address_taken(e, out);
                }
            }
            if let Some(e) = cond {
                expr_address_taken(e, out);
            }
            if let Some(exprs) = step {
                for e in exprs {
                    expr_address_taken(e, out);
                }
            }
            for s in body {
                collect_address_taken(s, out);
            }
        }
        StmtKind::Switch { scrutinee, cases } => {
            expr_address_taken(scrutinee, out);
            for c in cases {
                for s in &c.body {
                    collect_address_taken(s, out);
                }
            }
        }
        StmtKind::Break | StmtKind::Continue => {}
        StmtKind::Goto { .. } | StmtKind::Label { .. } | StmtKind::Empty => {}
        StmtKind::ExprStmt(e) => expr_address_taken(e, out),
    }
}

fn expr_address_taken(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::AddressOf(name) => {
            out.insert(name.clone());
        }
        ExprKind::AddressOfArrayElem { array, .. } => {
            out.insert(array.clone());
        }
        ExprKind::BinOp { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            expr_address_taken(left, out);
            expr_address_taken(right, out);
        }
        ExprKind::Unary { operand, .. } | ExprKind::Deref(operand) => {
            expr_address_taken(operand, out);
        }
        ExprKind::AssignExpr { value, .. } => expr_address_taken(value, out),
        ExprKind::ArrayIndex { array, index } => {
            expr_address_taken(array, out);
            expr_address_taken(index, out);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                expr_address_taken(a, out);
            }
        }
        ExprKind::Member { base, .. } => expr_address_taken(base, out),
        ExprKind::Ternary { cond, then_value, else_value } => {
            expr_address_taken(cond, out);
            expr_address_taken(then_value, out);
            expr_address_taken(else_value, out);
        }
        ExprKind::Cast { operand, .. } => expr_address_taken(operand, out),
        ExprKind::InitList { items } => {
            for item in items {
                expr_address_taken(item, out);
            }
        }
        ExprKind::Comma { left, right } => {
            expr_address_taken(left, out);
            expr_address_taken(right, out);
        }
        ExprKind::IntLit(_)
        | ExprKind::Ident(_)
        | ExprKind::Update { .. }
        | ExprKind::StringLit(_) => {}
    }
}

/// Walk `stmts` and call `f(stmt.span.start)` for each `switch`
/// statement whose strategy is `LinearSearch`. Recurses into nested
/// constructs (loop bodies, if-branches, other switches' bodies).
fn collect_linear_search_switches<F: FnMut(u32)>(stmts: &[Stmt], mut f: F) {
    fn walk<F: FnMut(u32)>(stmts: &[Stmt], f: &mut F) {
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Switch { cases, .. } => {
                    if matches!(pick_switch_strategy(cases), SwitchStrategy::LinearSearch) {
                        f(stmt.span.start);
                    }
                    for c in cases {
                        walk(&c.body, f);
                    }
                }
                StmtKind::If { then_branch, else_branch, .. } => {
                    walk(then_branch, f);
                    if let Some(b) = else_branch {
                        walk(b, f);
                    }
                }
                StmtKind::While { body, .. }
                | StmtKind::DoWhile { body, .. }
                | StmtKind::For { body, .. } => {
                    walk(body, f);
                }
                _ => {}
            }
        }
    }
    walk(stmts, &mut f);
}

fn stmt_has_call(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(value) => value.as_ref().is_some_and(expr_has_call),
        StmtKind::Declare { init, .. } => init.as_ref().is_some_and(expr_has_call),
        StmtKind::Assign { value, .. } | StmtKind::CompoundAssign { value, .. } => {
            expr_has_call(value)
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            expr_has_call(cond)
                || body_has_call(then_branch)
                || else_branch.as_ref().is_some_and(|b| body_has_call(b))
        }
        StmtKind::While { cond, body } => expr_has_call(cond) || body_has_call(body),
        StmtKind::DoWhile { body, cond } => body_has_call(body) || expr_has_call(cond),
        StmtKind::For { init, cond, step, body } => {
            init.as_ref()
                .is_some_and(|es| es.iter().any(expr_has_call))
                || cond.as_ref().is_some_and(expr_has_call)
                || step
                    .as_ref()
                    .is_some_and(|es| es.iter().any(expr_has_call))
                || body_has_call(body)
        }
        StmtKind::Break | StmtKind::Continue => false,
        StmtKind::Switch { scrutinee, cases } => {
            expr_has_call(scrutinee) || cases.iter().any(|c| body_has_call(&c.body))
        }
        StmtKind::ArrayAssign { indices, value, .. }
        | StmtKind::ArrayCompoundAssign { indices, value, .. }
        | StmtKind::MemberArrayAssign { indices, value, .. } => {
            indices.iter().any(expr_has_call) || expr_has_call(value)
        }
        StmtKind::DerefAssign { target, value }
        | StmtKind::DerefCompoundAssign { target, value, .. } => {
            expr_has_call(target) || expr_has_call(value)
        }
        StmtKind::MemberAssign { base, value, .. }
        | StmtKind::MemberCompoundAssign { base, value, .. } => {
            expr_has_call(base) || expr_has_call(value)
        }
        StmtKind::Goto { .. } | StmtKind::Label { .. } | StmtKind::Empty => false,
        StmtKind::ExprStmt(e) => expr_has_call(e),
    }
}

fn expr_has_call(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call { .. } => true,
        ExprKind::BinOp { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            expr_has_call(left) || expr_has_call(right)
        }
        ExprKind::Unary { operand, .. } => expr_has_call(operand),
        ExprKind::AssignExpr { value, .. } => expr_has_call(value),
        ExprKind::Deref(operand) => expr_has_call(operand),
        ExprKind::ArrayIndex { array, index } => {
            expr_has_call(array) || expr_has_call(index)
        }
        ExprKind::Member { base, .. } => expr_has_call(base),
        ExprKind::Ternary { cond, then_value, else_value } => {
            expr_has_call(cond) || expr_has_call(then_value) || expr_has_call(else_value)
        }
        ExprKind::Cast { operand, .. } => expr_has_call(operand),
        ExprKind::InitList { items } => items.iter().any(expr_has_call),
        ExprKind::Comma { left, right } => expr_has_call(left) || expr_has_call(right),
        ExprKind::Update { .. }
        | ExprKind::Ident(_)
        | ExprKind::IntLit(_)
        | ExprKind::AddressOf(_)
        | ExprKind::AddressOfArrayElem { .. }
        | ExprKind::StringLit(_) => false,
    }
}

/// Choose which eligible name gets SI: the one with the highest use
/// count, ties broken by earliest source order. Returns the index into
/// `declared` (not `eligible`).
fn pick_si(
    eligible: &[usize],
    declared: &[DeclItem],
    counts: &HashMap<String, u32>,
) -> Option<usize> {
    let mut best: Option<(usize, u32)> = None;
    for &i in eligible {
        let uses = counts.get(&declared[i].name).copied().unwrap_or(0);
        if best.is_none_or(|(_, b)| uses > b) {
            best = Some((i, uses));
        }
    }
    best.map(|(i, _)| i)
}

struct DeclItem {
    name: String,
    ty: Type,
    kind: DeclKind,
}

#[derive(Clone, Copy)]
enum DeclKind {
    Local,
    Param { incoming_offset: u16 },
}

fn collect_decls(stmt: &Stmt, out: &mut Vec<DeclItem>) {
    match &stmt.kind {
        StmtKind::Declare { ty, name, is_static, .. } => {
            if !*is_static {
                out.push(DeclItem { name: name.clone(), ty: ty.clone(), kind: DeclKind::Local });
            }
        }
        StmtKind::If { then_branch, else_branch, .. } => {
            for s in then_branch {
                collect_decls(s, out);
            }
            if let Some(else_branch) = else_branch {
                for s in else_branch {
                    collect_decls(s, out);
                }
            }
        }
        StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } | StmtKind::For { body, .. } => {
            for s in body {
                collect_decls(s, out);
            }
        }
        StmtKind::Switch { cases, .. } => {
            for c in cases {
                for s in &c.body {
                    collect_decls(s, out);
                }
            }
        }
        StmtKind::Return(_)
        | StmtKind::Assign { .. }
        | StmtKind::CompoundAssign { .. }
        | StmtKind::ArrayAssign { .. }
        | StmtKind::ArrayCompoundAssign { .. }
        | StmtKind::MemberArrayAssign { .. }
        | StmtKind::DerefAssign { .. }
        | StmtKind::DerefCompoundAssign { .. }
        | StmtKind::MemberAssign { .. }
        | StmtKind::MemberCompoundAssign { .. }
        | StmtKind::ExprStmt(_)
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Goto { .. }
        | StmtKind::Label { .. }
        | StmtKind::Empty => {}
    }
}

fn count_uses_stmt(stmt: &Stmt, counts: &mut HashMap<String, u32>) {
    match &stmt.kind {
        StmtKind::Return(value) => {
            if let Some(e) = value {
                count_uses_expr(e, counts);
            }
        }
        StmtKind::Declare { name, init, is_static, .. } => {
            // A declaration counts as a use of the name only when it
            // initializes (since the init is a write). Uninitialized
            // `int x;` produces no asm and shouldn't compete with
            // initialized locals for the SI slot (fixture 066:
            // `int i = 0; int j;` ⇒ i → SI even though j has more
            // textual uses overall). Static locals don't compete for
            // registers at all.
            if *is_static {
                return;
            }
            if let Some(e) = init {
                *counts.entry(name.clone()).or_insert(0) += 1;
                count_uses_expr(e, counts);
            }
        }
        StmtKind::Assign { name, value } => {
            *counts.entry(name.clone()).or_insert(0) += 1;
            count_uses_expr(value, counts);
        }
        StmtKind::CompoundAssign { name, value, .. } => {
            // `x += y` is a read + write of x. Same count as Update.
            *counts.entry(name.clone()).or_insert(0) += 2;
            count_uses_expr(value, counts);
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            count_uses_expr(cond, counts);
            for s in then_branch {
                count_uses_stmt(s, counts);
            }
            if let Some(else_branch) = else_branch {
                for s in else_branch {
                    count_uses_stmt(s, counts);
                }
            }
        }
        StmtKind::While { cond, body } => {
            count_uses_expr(cond, counts);
            for s in body {
                count_uses_stmt(s, counts);
            }
        }
        StmtKind::DoWhile { body, cond } => {
            for s in body {
                count_uses_stmt(s, counts);
            }
            count_uses_expr(cond, counts);
        }
        StmtKind::For { init, cond, step, body } => {
            if let Some(exprs) = init {
                for e in exprs {
                    count_uses_expr(e, counts);
                }
            }
            if let Some(e) = cond {
                count_uses_expr(e, counts);
            }
            if let Some(exprs) = step {
                for e in exprs {
                    count_uses_expr(e, counts);
                }
            }
            for s in body {
                count_uses_stmt(s, counts);
            }
        }
        StmtKind::Break | StmtKind::Continue => {}
        StmtKind::Switch { scrutinee, cases } => {
            count_uses_expr(scrutinee, counts);
            for c in cases {
                for s in &c.body {
                    count_uses_stmt(s, counts);
                }
            }
        }
        StmtKind::ArrayAssign { array, indices, value }
        | StmtKind::ArrayCompoundAssign { array, indices, value, .. } => {
            // `a[i] = v;` mirrors `a[i]` as an rvalue: direct deref
            // counts the base name as 2 uses (read of address + use
            // of memory). Arrays never enregister anyway, but the
            // same statement could be a pointer-target indexed
            // assign (`p[i] = v`) in a future fixture.
            *counts.entry(array.clone()).or_insert(0) += 2;
            for ix in indices {
                count_uses_expr(ix, counts);
            }
            count_uses_expr(value, counts);
        }
        StmtKind::MemberArrayAssign { base, indices, value, .. } => {
            // `b.data[i] = v;` — same shape as ArrayAssign over `b`'s
            // storage. Fixture 497.
            *counts.entry(base.clone()).or_insert(0) += 2;
            for ix in indices {
                count_uses_expr(ix, counts);
            }
            count_uses_expr(value, counts);
        }
        StmtKind::DerefAssign { target, value }
        | StmtKind::DerefCompoundAssign { target, value, .. } => {
            if let ExprKind::Ident(name) = &target.kind {
                // Same direct-deref bonus as `*p` in rvalue position.
                *counts.entry(name.clone()).or_insert(0) += 2;
            } else {
                count_uses_expr(target, counts);
            }
            count_uses_expr(value, counts);
        }
        StmtKind::MemberAssign { base, value, kind, .. }
        | StmtKind::MemberCompoundAssign { base, value, kind, .. } => {
            // For `.` (Dot), the base is a struct lvalue — same
            // counting as any other expression use. For `->` (Arrow),
            // the base is a pointer that's about to be deref'd, so
            // it gets the same +2 bonus we give `*p` (fixture 105
            // expects `p` to enregister with init + p->x as direct
            // deref + return p->x as another).
            match kind {
                crate::ast::MemberKind::Arrow => {
                    if let ExprKind::Ident(name) = &base.kind {
                        *counts.entry(name.clone()).or_insert(0) += 2;
                    } else {
                        count_uses_expr(base, counts);
                    }
                }
                crate::ast::MemberKind::Dot => count_uses_expr(base, counts),
            }
            count_uses_expr(value, counts);
        }
        StmtKind::Goto { .. } | StmtKind::Label { .. } | StmtKind::Empty => {}
        StmtKind::ExprStmt(e) => count_uses_expr(e, counts),
    }
}

fn count_uses_expr(e: &Expr, counts: &mut HashMap<String, u32>) {
    match &e.kind {
        ExprKind::Ident(name) => {
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
        ExprKind::BinOp { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            count_uses_expr(left, counts);
            count_uses_expr(right, counts);
        }
        ExprKind::Unary { operand, .. } => {
            count_uses_expr(operand, counts);
        }
        ExprKind::Update { target, .. } => {
            // `++x` is a read + a write of x. Empirically (fixture
            // 040: `int x = 5; ++x; return 0;` puts x in SI) it
            // contributes 2 to the use-count, just like `x = x + 1`
            // would.
            *counts.entry(target.clone()).or_insert(0) += 2;
        }
        ExprKind::AssignExpr { target, value } => {
            // Like a statement-level Assign: LHS + RHS uses.
            *counts.entry(target.clone()).or_insert(0) += 1;
            count_uses_expr(value, counts);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                count_uses_expr(a, counts);
            }
        }
        ExprKind::AddressOf(name) => {
            // `&x` is itself a use, *and* it forces x to the stack
            // (its address must be a real memory address). The
            // "force to stack" half is handled separately when we
            // decide eligibility below; the count contribution here
            // is the use itself.
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
        ExprKind::AddressOfArrayElem { array, .. } => {
            *counts.entry(array.clone()).or_insert(0) += 1;
        }
        ExprKind::Deref(operand) => {
            // A "direct" deref gives the pointer-name a +2 bonus
            // toward enregistration. We treat three forms as direct:
            //   `*p`                — bare ident
            //   `*(p + <constant>)` — constant offset (BCC seems to
            //                          fold this internally into the
            //                          same shape as p[K])
            //   `*(p - <constant>)` — same, with a negative offset
            // The bonus does NOT apply to `*(p + <variable>)`
            // (fixture 092 vs. 091): keeping the threshold gate at
            // 3 with this distinction matches the captures.
            if let Some(name) = direct_deref_target(operand) {
                *counts.entry(name).or_insert(0) += 2;
            } else {
                count_uses_expr(operand, counts);
            }
        }
        ExprKind::ArrayIndex { array, index } => {
            // `a[i]` (or `p[i]` for a pointer) gives the base name
            // the same direct-deref bonus as `*p`. Fixture 088: `s`
            // enregisters when used as `s[0]`. For non-Ident bases
            // (e.g. string literals), no use-count contribution.
            if let ExprKind::Ident(name) = &array.kind {
                *counts.entry(name.clone()).or_insert(0) += 2;
            } else {
                count_uses_expr(array, counts);
            }
            count_uses_expr(index, counts);
        }
        ExprKind::StringLit(_) => {}
        ExprKind::IntLit(_) => {}
        ExprKind::Member { base, kind, .. } => {
            // `p->x` direct-derefs the pointer; `a.x` is just an
            // access to a struct lvalue.
            match kind {
                crate::ast::MemberKind::Arrow => {
                    if let ExprKind::Ident(name) = &base.kind {
                        *counts.entry(name.clone()).or_insert(0) += 2;
                    } else {
                        count_uses_expr(base, counts);
                    }
                }
                crate::ast::MemberKind::Dot => count_uses_expr(base, counts),
            }
        }
        ExprKind::Ternary { cond, then_value, else_value } => {
            count_uses_expr(cond, counts);
            count_uses_expr(then_value, counts);
            count_uses_expr(else_value, counts);
        }
        ExprKind::Cast { operand, .. } => count_uses_expr(operand, counts),
        ExprKind::InitList { items } => {
            for item in items {
                count_uses_expr(item, counts);
            }
        }
        ExprKind::Comma { left, right } => {
            count_uses_expr(left, counts);
            count_uses_expr(right, counts);
        }
    }
}

