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
use crate::codegen::fold::try_const_eval;
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
    /// Variant when the function emits `imul` (clobbers DX) but no
    /// call (so DX/BX/CX still survive across the function — just
    /// not across the `imul`). Drop DX from the pool so accumulators
    /// don't land in a reg the multiplier writes. Fixture 1369
    /// (`s += i * j` — s should go to BX, not DX).
    const NON_SI_POOL_NO_DX: [Self; 3] = [Self::Di, Self::Bx, Self::Cx];
    /// Variant when exactly 5 ints are eligible (perfect fit in
    /// 5 registers, no spill). Empirical from fixtures 1850, 1979.
    /// The 6+ "spill" case keeps the standard NON_SI_POOL order.
    const NON_SI_POOL_FIVE_INT: [Self; 4] = [Self::Bx, Self::Di, Self::Cx, Self::Dx];

    /// Reduced int pool when the function makes a call: DX, BX, CX
    /// are all caller-clobbered, so only DI is safe alongside SI.
    /// Fixture 1508 (3 ints + dbl() call → 2 in SI/DI, 1 spills).
    const NON_SI_POOL_WITH_CALL: [Self; 1] = [Self::Di];

    /// Pool used for char eligibles, in source-order assignment.
    /// Fixtures 047/050: a char declared first lands in DL, the next
    /// in BL, the third in CL.
    const CHAR_POOL: [Self; 3] = [Self::Dl, Self::Bl, Self::Cl];

    /// Char pool variant used when the function body contains a signed
    /// `idiv` (any `/` or `%` op, compound or expression). The `cwd`
    /// preceding `idiv` clobbers DX, and BCC's allocator drops DL out
    /// of the pool so the char target ends up in CL (or BL). Probed
    /// via fixture 640.
    const CHAR_POOL_DIV: [Self; 2] = [Self::Cl, Self::Bl];

    /// Char pool variant used when the function body contains an
    /// **unsigned** char compound `/=` / `%=` with non-constant RHS
    /// — BCC emits the 8-bit `div` form (not `idiv`) with
    /// `mov ah, 0` for widening. BCC still drops DL from the pool
    /// (reason not yet pinned down — likely a conservative rule
    /// that fires on any byte-form unsigned divide), but the
    /// remaining order is BL-then-CL rather than the signed
    /// 16-bit form's reversed CL-then-BL (where BL is consumed by
    /// the divisor). Probed via fixture 677.
    const CHAR_POOL_UDIV: [Self; 2] = [Self::Bl, Self::Cl];

    /// Char pool variant when the function emits a true `imul`
    /// (non-const or const-non-power-of-2 multiply) or a char
    /// compound assign with non-byte RHS (the widening dance uses
    /// DL as scratch). Both situations clobber DX, so DL drops.
    /// Fixtures 1295 (`c *= 3`), 1314 (`c += a*b`), 1430
    /// (`c += a*2` — shifts but the compound itself widens through
    /// DL).
    const CHAR_POOL_MUL: [Self; 2] = [Self::Bl, Self::Cl];

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
    /// Signed bp-offset of the 2-byte scratch slot used to materialize
    /// integer operands of `(float)<int>` / `(double)<int>` casts
    /// before `fild`. Allocated once per function whose body contains
    /// any such cast, sized for a 16-bit int. `None` otherwise.
    /// Fixture 1675.
    fild_int_scratch_offset: Option<i16>,
}

#[derive(Debug, Clone)]
struct LocalEntry {
    location: LocalLocation,
    ty: Type,
}

impl Locals {
    #[must_use]
    pub fn analyze(
        function: &Function,
        globals: &crate::codegen::GlobalTable,
    ) -> Self {
        // Pass 1: collect all "declarable" names (params first, then
        // locals in source order). Each gets an `init`-style use plus
        // a textual count.
        let mut declared: Vec<DeclItem> = Vec::new();
        let mut counts: HashMap<String, u32> = HashMap::new();

        // Params: assign each its incoming bp+N slot. When the
        // function returns a struct > 4 bytes, BCC inserts a
        // hidden far-pointer first param (the caller-supplied
        // return-buffer address), bumping all real params by 4
        // bytes. Fixture 3410 (`struct Three make(int, int, int)`
        // returning a 6-byte struct).
        let returns_big_struct = matches!(&function.ret_ty, Type::Struct { .. })
            && function.ret_ty.size_bytes() > 4;
        let mut param_offset = FIRST_PARAM_BP_OFFSET
            + if returns_big_struct { 4 } else { 0 };
        for param in &function.params {
            declared.push(DeclItem {
                name: param.name.clone(),
                ty: param.ty.clone(),
                kind: DeclKind::Param { incoming_offset: param_offset },
                is_register: false,
                is_volatile: false,
            });
            // Every param takes a 2-byte slot on the stack regardless
            // of declared type — `char` gets promoted at the push site
            // by the caller. (We haven't pinned this with a `char`-
            // param fixture; revisit when we have one.) Long params
            // take a 4-byte slot since they don't fit in a single
            // word (fixture 285). Float params get 4 bytes; double
            // params get 8 bytes — the caller `fstp dword|qword`s
            // into the slot rather than pushing word-by-word
            // (fixture 1678).
            param_offset += match &param.ty {
                Type::Long | Type::ULong => 4,
                Type::Float => 4,
                Type::Double => 8,
                _ => 2,
            };
            *counts.entry(param.name.clone()).or_insert(0) += 1;
        }

        // Per-decl sibling-block id — populated in step with
        // `declared`. Used by the offset-assignment loop below to
        // recycle stack slots across sibling Block scopes (fixtures
        // 1966-1969). Params don't get block-id entries; the loop
        // skips that case.
        let param_count = declared.len();
        let mut block_collector = CollectCtx::new();
        for stmt in function.body.as_deref().unwrap_or(&[]) {
            collect_decls_ctx(stmt, &mut declared, &mut block_collector);
            count_uses_stmt(stmt, &mut counts);
        }
        let block_ids = block_collector.block_ids;
        debug_assert_eq!(declared.len() - param_count, block_ids.len(),
            "one block id per non-param decl");

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
        // and never anything whose address was taken. `unsigned`
        // shares the int pool — same byte layout, same load/store
        // shapes (fixture 1216). The `register` keyword overrides
        // the use-count threshold (fixture 1550 / 1560). `volatile`
        // forces stack allocation regardless of use count
        // (fixtures 1548, 2243).
        //
        // Leaf-function param-as-char-array-subscript: BCC promotes
        // the first int-like parameter to SI when it's used as a
        // CHAR-element array subscript at least once, even with use
        // count below threshold. SI-indexed addressing on a byte
        // array (`byte ptr DGROUP:_arr[si]`) saves the `mov bx,
        // [bp+N]` load before each indexed access — `push si / mov
        // si / pop si` costs 4 bytes but eliminates the BX setup at
        // each use. Int-element arrays don't benefit (the index
        // still needs scaling), so BCC keeps int-array params on
        // the stack. Fixtures 2796, 2900, 2926, 3231, 3243, 3450.
        let body_has_loop = body_contains_loop(function.body.as_deref().unwrap_or(&[]));
        let leaf_param_subscript: HashSet<usize> = if !function_makes_call {
            // First int-like declaration (param OR local in
            // straight-line code) used as a char-array subscript.
            // We exclude loops because BCC's reg pick for loop
            // counters has its own heuristic (DX/CX in some
            // shapes) — see prefer_dx_over_si below.
            (0..declared.len())
                .find(|&i| {
                    matches!(
                        declared[i].ty,
                        Type::Int | Type::UInt | Type::Pointer(_),
                    ) && !address_taken.contains(&declared[i].name)
                        && !declared[i].is_volatile
                        && (matches!(declared[i].kind, DeclKind::Param { .. })
                            || !body_has_loop)
                        && name_indexes_char_array(
                            &declared[i].name,
                            function.body.as_deref().unwrap_or(&[]),
                            &declared,
                            globals,
                        )
                })
                .into_iter()
                .collect()
        } else {
            HashSet::new()
        };
        let eligible_int: Vec<usize> = (0..declared.len())
            .filter(|&i| {
                if address_taken.contains(&declared[i].name) {
                    return false;
                }
                if declared[i].is_volatile {
                    return false;
                }
                // When the leaf-param-char-subscript rule fires,
                // BCC limits the int pool to just the subscript
                // identifier — other int-likes (e.g. the char-array
                // pointer itself) stay on the stack regardless of
                // direct-deref bonus. Fixtures 1285, 3559.
                if !leaf_param_subscript.is_empty()
                    && !leaf_param_subscript.contains(&i)
                {
                    return false;
                }
                let uses = counts.get(&declared[i].name).copied().unwrap_or(0);
                match &declared[i].ty {
                    Type::Int | Type::UInt | Type::Pointer(_) => {
                        declared[i].is_register
                            || uses >= ENREGISTER_THRESHOLD
                            || leaf_param_subscript.contains(&i)
                    }
                    _ => false,
                }
            })
            .collect();
        let si_pick = pick_si(&eligible_int, &declared, &counts);

        // SI-vs-DX heuristic for the single-eligible case: when only
        // ONE int is eligible AND it's used as the subscript of a
        // CHAR-element array AND it doesn't appear in the return
        // expr, BCC picks DX instead of SI. The reason: char-element
        // stores naturally touch DL (`mov al, dl; mov [bx], al`),
        // and using DX for the counter avoids push si/pop si.
        // Word-element arrays don't benefit and keep SI (fixture
        // 510). Doesn't fire when the function makes a call (DX
        // would be clobbered). Fixtures 1219, 1257.
        let prefer_dx_over_si = !function_makes_call
            && eligible_int.len() == 1
            && si_pick.is_some()
            && name_is_char_array_index(
                &declared[si_pick.unwrap()].name,
                function.body.as_deref().unwrap_or(&[]),
                &declared,
                globals,
            )
            && !name_in_returns(
                &declared[si_pick.unwrap()].name,
                function.body.as_deref().unwrap_or(&[]),
            );

        let mut reg_of: HashMap<usize, Reg> = HashMap::new();
        if let Some(idx) = si_pick {
            let chosen = if prefer_dx_over_si { Reg::Dx } else { Reg::Si };
            reg_of.insert(idx, chosen);
        }
        // With a function call in the body, DX/BX/CX are caller-
        // clobbered — only SI/DI survive. Restrict the non-SI pool.
        // Without a call but with `imul` in the body, just DX is
        // clobbered (by the imul's high half). Drop DX in that case.
        let function_has_imul_now = body_emits_imul(
            function.body.as_deref().unwrap_or(&[]),
            &declared
                .iter()
                .filter(|item| matches!(item.ty, Type::Char | Type::UChar))
                .map(|item| item.name.as_str())
                .collect::<HashSet<_>>(),
        );
        // Exactly-5-eligible case with VARIED use counts: BCC swaps
        // the secondary order from `[DI, DX, BX, CX]` to `[BX, DI,
        // CX, DX]`. Fixtures 1850, 1979 (use counts vary, swap
        // pool). 1505, 046 (5 ints with all-equal use counts) keep
        // the default order. 4-or-fewer and 6+ keep default
        // regardless of use-count distribution.
        // BCC's "swap pool" empirical condition (fixtures 1850,
        // 1979): exactly 5 eligible ints, all locals (no params
        // mixed in), and use counts vary. When use counts are
        // all equal (1505) or there are any params in the
        // eligible set (046), keep the default pool order.
        let no_eligible_params = eligible_int.iter().all(|&i| {
            matches!(declared[i].kind, DeclKind::Local)
        });
        let eligible_uses_vary = eligible_int.len() == 5
            && no_eligible_params
            && {
                let first_count = counts
                    .get(&declared[eligible_int[0]].name)
                    .copied()
                    .unwrap_or(0);
                eligible_int.iter().any(|&i| {
                    counts.get(&declared[i].name).copied().unwrap_or(0) != first_count
                })
            };
        let non_si_pool: &[Reg] = if function_makes_call {
            &Reg::NON_SI_POOL_WITH_CALL
        } else if function_has_imul_now {
            &Reg::NON_SI_POOL_NO_DX
        } else if eligible_uses_vary {
            &Reg::NON_SI_POOL_FIVE_INT
        } else {
            &Reg::NON_SI_POOL
        };
        let mut non_si_iter = non_si_pool.iter().copied();
        for &i in &eligible_int {
            if Some(i) == si_pick {
                continue;
            }
            let Some(reg) = non_si_iter.next() else { break };
            reg_of.insert(i, reg);
        }

        // Char eligibles — only when the function makes no call, and
        // never for chars whose address was taken. When the body
        // contains `idiv` (any `/` or `%` op), BCC drops DL from the
        // char pool (cwd before idiv clobbers DX). Fixture 640.
        let char_local_names: HashSet<&str> = declared
            .iter()
            .filter(|item| matches!(item.ty, Type::Char | Type::UChar))
            .map(|item| item.name.as_str())
            .collect();
        let uchar_local_names: HashSet<&str> = declared
            .iter()
            .filter(|item| matches!(item.ty, Type::UChar))
            .map(|item| item.name.as_str())
            .collect();
        let function_has_div = body_has_div_or_mod(
            function.body.as_deref().unwrap_or(&[]),
            &char_local_names,
        );
        let function_has_uchar_byte_div = body_has_uchar_byte_div_or_mod(
            function.body.as_deref().unwrap_or(&[]),
            &uchar_local_names,
        );
        // Any multiplication that emits `imul` (i.e., not a const
        // power-of-2 RHS that folds to shifts) clobbers DX, so DL
        // becomes unsafe for char locals. Char compound assigns
        // with a non-byte RHS also use DL as a scratch register
        // in the widening dance (`mov dl, <c>; add dl, al; mov
        // <c>, dl`). Fixtures 1295, 1314, 1430.
        let function_has_imul = body_emits_imul(
            function.body.as_deref().unwrap_or(&[]),
            &char_local_names,
        );
        let function_has_char_compound_int_rhs = body_has_char_compound_int_rhs(
            function.body.as_deref().unwrap_or(&[]),
            &char_local_names,
        );
        let dx_clobbered = function_has_div
            || function_has_uchar_byte_div
            || function_has_imul
            || function_has_char_compound_int_rhs;
        if !function_makes_call {
            let char_pool_slice: &[Reg] = if function_has_uchar_byte_div {
                &Reg::CHAR_POOL_UDIV
            } else if function_has_div {
                &Reg::CHAR_POOL_DIV
            } else if dx_clobbered {
                // MUL-only or compound-widening case: DL drops but
                // BL/CL order stays. Differs from the DIV case
                // (which prefers CL because BX is the typical
                // divisor reg).
                &Reg::CHAR_POOL_MUL
            } else {
                &Reg::CHAR_POOL
            };
            let mut char_pool = char_pool_slice.iter().copied();
            for (i, item) in declared.iter().enumerate() {
                if !matches!(item.ty, Type::Char | Type::UChar) {
                    continue;
                }
                if address_taken.contains(&item.name) {
                    continue;
                }
                if item.is_volatile {
                    continue;
                }
                let uses = counts.get(&item.name).copied().unwrap_or(0);
                if !item.is_register && uses < ENREGISTER_THRESHOLD {
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
        // Track the function-level stack-bytes baseline. When the
        // current decl belongs to a different sibling-block from
        // the previous one, reset stack_bytes back to this baseline
        // so the new block reuses its sibling's slot range. The
        // function-wide frame size is the max stack_bytes seen at
        // any point. Fixtures 1966-1969.
        let mut max_stack_bytes: u16 = 0;
        let mut function_level_bytes: u16 = 0;
        let mut prev_block_id: u32 = 0;
        // Track local-decl index separately so we can index into
        // block_ids (which only covers non-param decls).
        let mut local_decl_idx: usize = 0;
        for (i, item) in declared.iter().enumerate() {
            let location = if let Some(&reg) = reg_of.get(&i) {
                if let DeclKind::Param { incoming_offset } = item.kind {
                    param_loads.push(ParamLoad { reg, incoming_offset });
                }
                // Register-resident locals also consume a block-id
                // slot in the parallel list (for the bookkeeping
                // index). Advance the index when this decl is a
                // non-param local.
                if matches!(item.kind, DeclKind::Local) {
                    local_decl_idx += 1;
                }
                LocalLocation::Reg(reg)
            } else {
                match item.kind {
                    DeclKind::Local => {
                        let block_id = block_ids
                            .get(local_decl_idx)
                            .copied()
                            .unwrap_or(0);
                        local_decl_idx += 1;
                        // Local referenced only by sizeof (which the
                        // parser folds to an int literal) leaves it
                        // unreferenced everywhere else. With a zero
                        // use count and no address taken, BCC skips
                        // the stack allocation entirely — and so do
                        // we. Fixtures 1885, 2498, 3310.
                        let uses = counts.get(&item.name).copied().unwrap_or(0);
                        if uses == 0 && !address_taken.contains(&item.name) {
                            // Synthetic, never-read location — the
                            // local has zero uses so location_of is
                            // never queried.
                            by_name.insert(
                                item.name.clone(),
                                LocalEntry { location: LocalLocation::Stack(0), ty: item.ty.clone() },
                            );
                            continue;
                        }
                        // Sibling-block boundary: reset stack_bytes
                        // back to the function-level baseline so
                        // this block starts allocating from the
                        // same offset as its previous sibling.
                        // Function-level → block transitions also
                        // pin the baseline before the first block.
                        if block_id != prev_block_id {
                            if prev_block_id == 0 {
                                function_level_bytes = stack_bytes;
                            }
                            if block_id == 0 {
                                stack_bytes = function_level_bytes;
                            } else {
                                stack_bytes = function_level_bytes;
                            }
                        }
                        prev_block_id = block_id;
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
                        if stack_bytes > max_stack_bytes {
                            max_stack_bytes = stack_bytes;
                        }
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
        // Use the max stack_bytes ever reached as the final frame
        // size — accounts for sibling blocks restoring SP between
        // each (their slot range is reused but the frame must be
        // large enough for whichever block is tallest).
        stack_bytes = max_stack_bytes;

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

        // `(float)<int>` / `(double)<int>` casts: BCC materializes
        // the int operand into a 2-byte scratch slot at the bottom
        // of the frame and then `fild`s from there (the 8087 has
        // no register/immediate-source variant). One slot suffices
        // for the entire function — each cast site overwrites the
        // slot fresh before the fild. Fixture 1675.
        let fild_int_scratch_offset =
            body_has_int_to_float_cast(function)
                .then(|| {
                    stack_bytes += 2;
                    -i16::try_from(stack_bytes).expect("stack frame fits in i16")
                });

        Self {
            stack_bytes,
            by_name,
            saved_regs,
            param_loads,
            switch_spill_offsets,
            fild_int_scratch_offset,
        }
    }

    #[must_use]
    pub fn stack_bytes(&self) -> u16 {
        self.stack_bytes
    }

    /// Signed bp-offset of the 2-byte scratch slot reserved for
    /// `fild`-based int→float conversions, or `None` if no such
    /// cast is used in this function.
    #[must_use]
    pub fn fild_int_scratch_offset(&self) -> Option<i16> {
        self.fild_int_scratch_offset
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

    /// Iterator over `(name, type)` of every recorded local — params
    /// and stack locals. Used by struct-tag lookups that need to find
    /// a full struct definition that lives only in a local's
    /// declared type.
    pub fn iter_types(&self) -> impl Iterator<Item = (&str, &Type)> {
        self.by_name.iter().map(|(n, e)| (n.as_str(), &e.ty))
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
        StmtKind::Block(body) => {
            for s in body {
                collect_address_taken(s, out);
            }
        }
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
        ExprKind::CompoundAssignExpr { value, .. } => expr_address_taken(value, out),
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
        ExprKind::UpdateLvalue { target, .. } => expr_address_taken(target, out),
        ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::DoubleLit(_)
        | ExprKind::Ident(_)
        | ExprKind::Update { .. }
        | ExprKind::StringLit(_) => {}
    }
}

/// True iff `function` needs the 2-byte FPU scratch slot. Two
/// constructs trigger reservation:
///   1. `(float)<int>` / `(double)<int>` casts — int operand
///      materialized into the scratch slot, then `fild` from there
///      (fixture 1675).
///   2. Float comparisons — `fcomp` result written through `fstsw`
///      to the scratch slot, then read back into AX for `sahf`
///      (fixture 1674).
/// Conservative on operand-type inference (we look at literal kinds
/// and Ident-name-against-declared-locals to weed out the
/// `(float)<float-ident>` no-op case).
fn body_has_int_to_float_cast(function: &Function) -> bool {
    let mut float_names: HashSet<String> = HashSet::new();
    for p in &function.params {
        if p.ty.is_float_like() {
            float_names.insert(p.name.clone());
        }
    }
    fn collect_decls(stmts: &[Stmt], set: &mut HashSet<String>) {
        for s in stmts {
            match &s.kind {
                StmtKind::Declare { name, ty, .. } if ty.is_float_like() => {
                    set.insert(name.clone());
                }
                StmtKind::If { then_branch, else_branch, .. } => {
                    collect_decls(then_branch, set);
                    if let Some(b) = else_branch { collect_decls(b, set); }
                }
                StmtKind::While { body, .. }
                | StmtKind::DoWhile { body, .. } => collect_decls(body, set),
                StmtKind::For { body, .. } => collect_decls(body, set),
                StmtKind::Switch { cases, .. } => {
                    for c in cases { collect_decls(&c.body, set); }
                }
                _ => {}
            }
        }
    }
    collect_decls(function.body.as_deref().unwrap_or(&[]), &mut float_names);

    fn expr_is_integer(e: &Expr, float_names: &HashSet<String>) -> bool {
        match &e.kind {
            ExprKind::IntLit(_) => true,
            ExprKind::Ident(n) => !float_names.contains(n),
            // BinOp / Unary / Update over non-floats stays integer.
            // We err toward "integer" since the codegen branch only
            // emits fild when the operand actually is int, so an
            // over-allocated scratch slot would just sit unused.
            ExprKind::BinOp { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Update { .. }
            | ExprKind::Deref(_)
            | ExprKind::ArrayIndex { .. }
            | ExprKind::Member { .. } => true,
            _ => false,
        }
    }

    fn ident_is_float(e: &Expr, float_names: &HashSet<String>) -> bool {
        match &e.kind {
            ExprKind::FloatLit(_) | ExprKind::DoubleLit(_) => true,
            ExprKind::Ident(n) => float_names.contains(n),
            _ => false,
        }
    }

    fn expr(e: &Expr, float_names: &HashSet<String>) -> bool {
        match &e.kind {
            ExprKind::Cast { ty, operand } => {
                (ty.is_float_like() && expr_is_integer(operand, float_names))
                    || expr(operand, float_names)
            }
            // Float comparison needs the scratch for `fstsw`.
            ExprKind::BinOp { op, left, right } if op.is_comparison()
                && (ident_is_float(left, float_names)
                    || ident_is_float(right, float_names)) => true,
            // Arithmetic on a mixed int+float pair needs the scratch
            // for the implicit fild widening of the int operand.
            // Fixture 1752 (`i + d`).
            ExprKind::BinOp { left, right, .. }
                if (ident_is_float(left, float_names)
                    && expr_is_integer(right, float_names))
                    || (ident_is_float(right, float_names)
                        && expr_is_integer(left, float_names)) => true,
            ExprKind::BinOp { left, right, .. }
            | ExprKind::Logical { left, right, .. }
            | ExprKind::Comma { left, right } => {
                expr(left, float_names) || expr(right, float_names)
            }
            ExprKind::Unary { operand, .. } => expr(operand, float_names),
            ExprKind::Deref(inner) => expr(inner, float_names),
            ExprKind::AssignExpr { value, .. } => expr(value, float_names),
            ExprKind::CompoundAssignExpr { value, .. } => expr(value, float_names),
            ExprKind::Call { args, .. } => args.iter().any(|a| expr(a, float_names)),
            ExprKind::ArrayIndex { array, index } => {
                expr(array, float_names) || expr(index, float_names)
            }
            ExprKind::Member { base, .. } => expr(base, float_names),
            ExprKind::Ternary { cond, then_value, else_value } => {
                expr(cond, float_names)
                    || expr(then_value, float_names)
                    || expr(else_value, float_names)
            }
            ExprKind::InitList { items } => items.iter().any(|i| expr(i, float_names)),
            _ => false,
        }
    }

    fn stmt(s: &Stmt, float_names: &HashSet<String>) -> bool {
        match &s.kind {
            StmtKind::Return(v) => v.as_ref().is_some_and(|e| expr(e, float_names)),
            StmtKind::Declare { init, .. } => {
                init.as_ref().is_some_and(|e| expr(e, float_names))
            }
            StmtKind::Assign { value, .. }
            | StmtKind::CompoundAssign { value, .. }
            | StmtKind::DerefAssign { value, .. }
            | StmtKind::DerefCompoundAssign { value, .. }
            | StmtKind::MemberAssign { value, .. }
            | StmtKind::MemberCompoundAssign { value, .. } => expr(value, float_names),
            StmtKind::ArrayAssign { indices, value, .. }
            | StmtKind::ArrayCompoundAssign { indices, value, .. }
            | StmtKind::MemberArrayAssign { indices, value, .. } => {
                indices.iter().any(|i| expr(i, float_names))
                    || expr(value, float_names)
            }
            StmtKind::If { cond, then_branch, else_branch } => {
                expr(cond, float_names)
                    || then_branch.iter().any(|s| stmt(s, float_names))
                    || else_branch.as_ref().is_some_and(|b|
                        b.iter().any(|s| stmt(s, float_names)))
            }
            StmtKind::While { cond, body } | StmtKind::DoWhile { cond, body } => {
                expr(cond, float_names) || body.iter().any(|s| stmt(s, float_names))
            }
            StmtKind::For { init, cond, step, body } => {
                init.as_ref().is_some_and(|es|
                        es.iter().any(|e| expr(e, float_names)))
                    || cond.as_ref().is_some_and(|e| expr(e, float_names))
                    || step.as_ref().is_some_and(|es|
                        es.iter().any(|e| expr(e, float_names)))
                    || body.iter().any(|s| stmt(s, float_names))
            }
            StmtKind::Switch { scrutinee, cases } => {
                expr(scrutinee, float_names)
                    || cases.iter().any(|c|
                        c.body.iter().any(|s| stmt(s, float_names)))
            }
            StmtKind::ExprStmt(e) => expr(e, float_names),
            _ => false,
        }
    }

    function.body.as_deref().unwrap_or(&[]).iter().any(|s| stmt(s, &float_names))
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
        StmtKind::Block(body) => body.iter().any(stmt_has_call),
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
        ExprKind::CompoundAssignExpr { value, .. } => expr_has_call(value),
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
        ExprKind::UpdateLvalue { target, .. } => expr_has_call(target),
        ExprKind::Update { .. }
        | ExprKind::Ident(_)
        | ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::DoubleLit(_)
        | ExprKind::AddressOf(_)
        | ExprKind::AddressOfArrayElem { .. }
        | ExprKind::StringLit(_) => false,
    }
}

/// True iff any statement in `body` contains an integer divide or
/// modulo (compound or expression form). Used to switch the char
/// register pool from `[DL, BL, CL]` to `[CL, BL]` so the char
/// local doesn't land in DL (which `cwd` would clobber). Fixture
/// 640.
fn body_has_div_or_mod(body: &[Stmt], char_locals: &HashSet<&str>) -> bool {
    body.iter().any(|s| stmt_has_div_or_mod(s, char_locals))
}

fn stmt_has_div_or_mod(stmt: &Stmt, char_locals: &HashSet<&str>) -> bool {
    match &stmt.kind {
        StmtKind::Return(value) => value.as_ref().is_some_and(expr_has_div_or_mod),
        StmtKind::Declare { init, .. } => init.as_ref().is_some_and(expr_has_div_or_mod),
        StmtKind::Assign { value, .. } => expr_has_div_or_mod(value),
        StmtKind::CompoundAssign { name, op, value } => {
            // Char `/=` / `%=` with a non-constant RHS emits an 8-bit
            // `idiv byte ptr <src>` (no `cwd`); DX stays intact, so it
            // does not require dropping DL from CHAR_POOL. Fixture 673.
            // Const-RHS char and any int compound still widen and emit
            // `cwd` — those continue to count.
            let target_is_8bit_form = matches!(op, crate::ast::BinOp::Div | crate::ast::BinOp::Mod)
                && char_locals.contains(name.as_str())
                && try_const_eval(value).is_none();
            (matches!(op, crate::ast::BinOp::Div | crate::ast::BinOp::Mod) && !target_is_8bit_form)
                || expr_has_div_or_mod(value)
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            expr_has_div_or_mod(cond)
                || body_has_div_or_mod(then_branch, char_locals)
                || else_branch.as_ref().is_some_and(|b| body_has_div_or_mod(b, char_locals))
        }
        StmtKind::While { cond, body } => {
            expr_has_div_or_mod(cond) || body_has_div_or_mod(body, char_locals)
        }
        StmtKind::DoWhile { body, cond } => {
            body_has_div_or_mod(body, char_locals) || expr_has_div_or_mod(cond)
        }
        StmtKind::For { init, cond, step, body } => {
            init.as_ref()
                .is_some_and(|es| es.iter().any(expr_has_div_or_mod))
                || cond.as_ref().is_some_and(expr_has_div_or_mod)
                || step
                    .as_ref()
                    .is_some_and(|es| es.iter().any(expr_has_div_or_mod))
                || body_has_div_or_mod(body, char_locals)
        }
        StmtKind::Switch { scrutinee, cases } => {
            expr_has_div_or_mod(scrutinee)
                || cases.iter().any(|c| body_has_div_or_mod(&c.body, char_locals))
        }
        StmtKind::ArrayAssign { indices, value, .. }
        | StmtKind::MemberArrayAssign { indices, value, .. } => {
            indices.iter().any(expr_has_div_or_mod) || expr_has_div_or_mod(value)
        }
        StmtKind::ArrayCompoundAssign { op, indices, value, .. } => {
            matches!(op, crate::ast::BinOp::Div | crate::ast::BinOp::Mod)
                || indices.iter().any(expr_has_div_or_mod)
                || expr_has_div_or_mod(value)
        }
        StmtKind::DerefAssign { target, value } => {
            expr_has_div_or_mod(target) || expr_has_div_or_mod(value)
        }
        StmtKind::DerefCompoundAssign { op, target, value, .. } => {
            matches!(op, crate::ast::BinOp::Div | crate::ast::BinOp::Mod)
                || expr_has_div_or_mod(target)
                || expr_has_div_or_mod(value)
        }
        StmtKind::MemberAssign { base, value, .. } => {
            expr_has_div_or_mod(base) || expr_has_div_or_mod(value)
        }
        StmtKind::MemberCompoundAssign { op, base, value, .. } => {
            matches!(op, crate::ast::BinOp::Div | crate::ast::BinOp::Mod)
                || expr_has_div_or_mod(base)
                || expr_has_div_or_mod(value)
        }
        StmtKind::ExprStmt(e) => expr_has_div_or_mod(e),
        StmtKind::Block(body) => body_has_div_or_mod(body, char_locals),
        StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Goto { .. }
        | StmtKind::Label { .. }
        | StmtKind::Empty => false,
    }
}

/// True iff any statement in `body` contains an *unsigned* char
/// compound `/=` or `%=` with a non-constant RHS — the 8-bit
/// `div`-with-`mov ah, 0` shape that drives the
/// `CHAR_POOL_UDIV` selection (fixture 677).
fn body_has_uchar_byte_div_or_mod(body: &[Stmt], uchar_locals: &HashSet<&str>) -> bool {
    body.iter().any(|s| stmt_has_uchar_byte_div_or_mod(s, uchar_locals))
}

fn stmt_has_uchar_byte_div_or_mod(stmt: &Stmt, uchar_locals: &HashSet<&str>) -> bool {
    match &stmt.kind {
        StmtKind::CompoundAssign { name, op, value } => {
            matches!(op, crate::ast::BinOp::Div | crate::ast::BinOp::Mod)
                && uchar_locals.contains(name.as_str())
                && try_const_eval(value).is_none()
        }
        StmtKind::If { then_branch, else_branch, .. } => {
            body_has_uchar_byte_div_or_mod(then_branch, uchar_locals)
                || else_branch
                    .as_ref()
                    .is_some_and(|b| body_has_uchar_byte_div_or_mod(b, uchar_locals))
        }
        StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
            body_has_uchar_byte_div_or_mod(body, uchar_locals)
        }
        StmtKind::For { body, .. } => body_has_uchar_byte_div_or_mod(body, uchar_locals),
        StmtKind::Switch { cases, .. } => cases
            .iter()
            .any(|c| body_has_uchar_byte_div_or_mod(&c.body, uchar_locals)),
        _ => false,
    }
}

fn expr_has_div_or_mod(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::BinOp { op, left, right } => {
            matches!(op, crate::ast::BinOp::Div | crate::ast::BinOp::Mod)
                || expr_has_div_or_mod(left)
                || expr_has_div_or_mod(right)
        }
        ExprKind::Logical { left, right, .. } => {
            expr_has_div_or_mod(left) || expr_has_div_or_mod(right)
        }
        ExprKind::Unary { operand, .. } => expr_has_div_or_mod(operand),
        ExprKind::Call { args, .. } => args.iter().any(expr_has_div_or_mod),
        ExprKind::AssignExpr { value, .. } => expr_has_div_or_mod(value),
        ExprKind::CompoundAssignExpr { value, .. } => expr_has_div_or_mod(value),
        ExprKind::Deref(operand) => expr_has_div_or_mod(operand),
        ExprKind::ArrayIndex { array, index } => {
            expr_has_div_or_mod(array) || expr_has_div_or_mod(index)
        }
        ExprKind::Member { base, .. } => expr_has_div_or_mod(base),
        ExprKind::Ternary { cond, then_value, else_value } => {
            expr_has_div_or_mod(cond)
                || expr_has_div_or_mod(then_value)
                || expr_has_div_or_mod(else_value)
        }
        ExprKind::Cast { operand, .. } => expr_has_div_or_mod(operand),
        ExprKind::InitList { items } => items.iter().any(expr_has_div_or_mod),
        ExprKind::Comma { left, right } => {
            expr_has_div_or_mod(left) || expr_has_div_or_mod(right)
        }
        ExprKind::UpdateLvalue { target, .. } => expr_has_div_or_mod(target),
        ExprKind::Update { .. }
        | ExprKind::Ident(_)
        | ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::DoubleLit(_)
        | ExprKind::AddressOf(_)
        | ExprKind::AddressOfArrayElem { .. }
        | ExprKind::StringLit(_) => false,
    }
}

/// True iff `name` appears as an identifier in any `return <expr>;`
/// statement in `body`. Used by the SI-vs-DX heuristic to detect
/// whether a candidate "escapes" into the function's return value.
fn name_in_returns(name: &str, body: &[Stmt]) -> bool {
    body.iter().any(|s| stmt_return_mentions(name, s))
}

fn stmt_return_mentions(name: &str, stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(Some(e)) => expr_mentions(name, e),
        StmtKind::If { then_branch, else_branch, .. } => {
            name_in_returns(name, then_branch)
                || else_branch.as_ref().is_some_and(|b| name_in_returns(name, b))
        }
        StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
            name_in_returns(name, body)
        }
        StmtKind::For { body, .. } => name_in_returns(name, body),
        StmtKind::Switch { cases, .. } => {
            cases.iter().any(|c| name_in_returns(name, &c.body))
        }
        _ => false,
    }
}

fn expr_mentions(name: &str, e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Ident(n) => n == name,
        ExprKind::BinOp { left, right, .. }
        | ExprKind::Logical { left, right, .. }
        | ExprKind::Comma { left, right } => {
            expr_mentions(name, left) || expr_mentions(name, right)
        }
        ExprKind::Unary { operand, .. } => expr_mentions(name, operand),
        ExprKind::Cast { operand, .. } => expr_mentions(name, operand),
        ExprKind::Deref(operand) => expr_mentions(name, operand),
        ExprKind::AssignExpr { value, .. } => expr_mentions(name, value),
        ExprKind::CompoundAssignExpr { value, .. } => expr_mentions(name, value),
        ExprKind::Call { args, .. } => args.iter().any(|a| expr_mentions(name, a)),
        ExprKind::ArrayIndex { array, index } => {
            expr_mentions(name, array) || expr_mentions(name, index)
        }
        ExprKind::Member { base, .. } => expr_mentions(name, base),
        ExprKind::Ternary { cond, then_value, else_value } => {
            expr_mentions(name, cond)
                || expr_mentions(name, then_value)
                || expr_mentions(name, else_value)
        }
        ExprKind::InitList { items } => items.iter().any(|i| expr_mentions(name, i)),
        ExprKind::Update { target, .. } => target == name,
        ExprKind::UpdateLvalue { target, .. } => expr_mentions(name, target),
        ExprKind::AddressOf(n) => n == name,
        ExprKind::AddressOfArrayElem { array, .. } => array == name,
        ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::DoubleLit(_)
        | ExprKind::StringLit(_) => false,
    }
}

/// True iff `name` appears anywhere as an array subscript (the
/// index, not the array). Used to detect when a leaf-function
/// parameter would benefit from being in a register (BCC promotes
/// such params to SI even with use count below the int-pool
/// threshold).
fn name_used_as_subscript(name: &str, body: &[Stmt]) -> bool {
    body.iter().any(|s| stmt_has_name_as_subscript(name, s))
}

fn stmt_has_name_as_subscript(name: &str, stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(value) => {
            value.as_ref().is_some_and(|e| expr_has_name_as_subscript(name, e))
        }
        StmtKind::Declare { init, .. } => {
            init.as_ref().is_some_and(|e| expr_has_name_as_subscript(name, e))
        }
        StmtKind::Assign { value, .. }
        | StmtKind::CompoundAssign { value, .. } => {
            expr_has_name_as_subscript(name, value)
        }
        StmtKind::ArrayAssign { indices, value, .. }
        | StmtKind::ArrayCompoundAssign { indices, value, .. } => {
            indices.iter().any(|i| expr_mentions(name, i))
                || expr_has_name_as_subscript(name, value)
        }
        StmtKind::MemberArrayAssign { indices, value, .. } => {
            indices.iter().any(|i| expr_mentions(name, i))
                || expr_has_name_as_subscript(name, value)
        }
        StmtKind::DerefAssign { target, value }
        | StmtKind::DerefCompoundAssign { target, value, .. } => {
            expr_has_name_as_subscript(name, target)
                || expr_has_name_as_subscript(name, value)
        }
        StmtKind::MemberAssign { base, value, .. }
        | StmtKind::MemberCompoundAssign { base, value, .. } => {
            expr_has_name_as_subscript(name, base)
                || expr_has_name_as_subscript(name, value)
        }
        StmtKind::ExprStmt(e) => expr_has_name_as_subscript(name, e),
        StmtKind::Switch { scrutinee, cases } => {
            expr_has_name_as_subscript(name, scrutinee)
                || cases.iter().any(|c| name_used_as_subscript(name, &c.body))
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            expr_has_name_as_subscript(name, cond)
                || name_used_as_subscript(name, then_branch)
                || else_branch.as_ref().is_some_and(|b| name_used_as_subscript(name, b))
        }
        StmtKind::While { cond, body }
        | StmtKind::DoWhile { cond, body, .. } => {
            expr_has_name_as_subscript(name, cond)
                || name_used_as_subscript(name, body)
        }
        StmtKind::For { init, cond, step, body, .. } => {
            init.as_ref().is_some_and(|es| es.iter().any(|e| expr_has_name_as_subscript(name, e)))
                || cond.as_ref().is_some_and(|e| expr_has_name_as_subscript(name, e))
                || step.as_ref().is_some_and(|es| es.iter().any(|e| expr_has_name_as_subscript(name, e)))
                || name_used_as_subscript(name, body)
        }
        StmtKind::Block(body) => name_used_as_subscript(name, body),
        StmtKind::Return(_)
        | StmtKind::Empty
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Goto { .. }
        | StmtKind::Label { .. } => false,
    }
}

fn expr_has_name_as_subscript(name: &str, e: &Expr) -> bool {
    match &e.kind {
        ExprKind::ArrayIndex { array, index } => {
            expr_mentions(name, index) || expr_has_name_as_subscript(name, array)
        }
        ExprKind::BinOp { left, right, .. }
        | ExprKind::Logical { left, right, .. }
        | ExprKind::Comma { left, right } => {
            expr_has_name_as_subscript(name, left)
                || expr_has_name_as_subscript(name, right)
        }
        ExprKind::Unary { operand, .. }
        | ExprKind::Cast { operand, .. }
        | ExprKind::Deref(operand) => expr_has_name_as_subscript(name, operand),
        ExprKind::AssignExpr { value, .. } => expr_has_name_as_subscript(name, value),
        ExprKind::CompoundAssignExpr { value, .. } => expr_has_name_as_subscript(name, value),
        ExprKind::Call { args, .. } => args.iter().any(|a| expr_has_name_as_subscript(name, a)),
        ExprKind::Member { base, .. } => expr_has_name_as_subscript(name, base),
        ExprKind::Ternary { cond, then_value, else_value } => {
            expr_has_name_as_subscript(name, cond)
                || expr_has_name_as_subscript(name, then_value)
                || expr_has_name_as_subscript(name, else_value)
        }
        ExprKind::InitList { items } => items.iter().any(|i| expr_has_name_as_subscript(name, i)),
        _ => false,
    }
}

fn body_contains_loop(body: &[Stmt]) -> bool {
    body.iter().any(stmt_contains_loop)
}

fn stmt_contains_loop(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::While { .. }
        | StmtKind::DoWhile { .. }
        | StmtKind::For { .. } => true,
        StmtKind::If { then_branch, else_branch, .. } => {
            body_contains_loop(then_branch)
                || else_branch.as_ref().is_some_and(|b| body_contains_loop(b))
        }
        StmtKind::Switch { cases, .. } => cases.iter().any(|c| body_contains_loop(&c.body)),
        _ => false,
    }
}

/// True iff `name` appears anywhere as the index of a CHAR-element
/// array (local or global), in any context (read or write).
/// Simpler counterpart of `name_is_char_array_index`, which adds
/// stricter conditions for the DX-vs-SI single-counter heuristic.
/// Used to decide whether a leaf-function int param should be
/// promoted to SI for indexed addressing.
fn name_indexes_char_array(
    name: &str,
    body: &[Stmt],
    declared: &[DeclItem],
    globals: &crate::codegen::GlobalTable,
) -> bool {
    let mut char_arrays: HashSet<String> = HashSet::new();
    for item in declared {
        if let Type::Array { elem, .. } = &item.ty {
            if matches!(&**elem, Type::Char | Type::UChar) {
                char_arrays.insert(item.name.clone());
            }
        }
    }
    for gname in globals.names() {
        if let Some(Type::Array { elem, .. }) = globals.type_of(gname) {
            if matches!(&**elem, Type::Char | Type::UChar) {
                char_arrays.insert(gname.to_string());
            }
        }
    }
    body.iter().any(|s| stmt_indexes_char_array(name, s, &char_arrays))
}

fn stmt_indexes_char_array(name: &str, stmt: &Stmt, ca: &HashSet<String>) -> bool {
    match &stmt.kind {
        StmtKind::Return(Some(e))
        | StmtKind::Assign { value: e, .. }
        | StmtKind::CompoundAssign { value: e, .. }
        | StmtKind::Declare { init: Some(e), .. }
        | StmtKind::ExprStmt(e) => expr_indexes_char_array(name, e, ca),
        StmtKind::ArrayAssign { array, indices, value }
        | StmtKind::ArrayCompoundAssign { array, indices, value, .. } => {
            (ca.contains(array.as_str()) && indices.iter().any(|i| expr_mentions(name, i)))
                || expr_indexes_char_array(name, value, ca)
                || indices.iter().any(|i| expr_indexes_char_array(name, i, ca))
        }
        StmtKind::MemberArrayAssign { indices, value, .. } => {
            indices.iter().any(|i| expr_indexes_char_array(name, i, ca))
                || expr_indexes_char_array(name, value, ca)
        }
        StmtKind::DerefAssign { target, value }
        | StmtKind::DerefCompoundAssign { target, value, .. } => {
            expr_indexes_char_array(name, target, ca)
                || expr_indexes_char_array(name, value, ca)
        }
        StmtKind::MemberAssign { base, value, .. }
        | StmtKind::MemberCompoundAssign { base, value, .. } => {
            expr_indexes_char_array(name, base, ca)
                || expr_indexes_char_array(name, value, ca)
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            expr_indexes_char_array(name, cond, ca)
                || then_branch.iter().any(|s| stmt_indexes_char_array(name, s, ca))
                || else_branch.as_ref().is_some_and(|b| {
                    b.iter().any(|s| stmt_indexes_char_array(name, s, ca))
                })
        }
        StmtKind::While { cond, body }
        | StmtKind::DoWhile { cond, body, .. } => {
            expr_indexes_char_array(name, cond, ca)
                || body.iter().any(|s| stmt_indexes_char_array(name, s, ca))
        }
        StmtKind::For { init, cond, step, body, .. } => {
            init.as_ref().is_some_and(|es| es.iter().any(|e| expr_indexes_char_array(name, e, ca)))
                || cond.as_ref().is_some_and(|e| expr_indexes_char_array(name, e, ca))
                || step.as_ref().is_some_and(|es| es.iter().any(|e| expr_indexes_char_array(name, e, ca)))
                || body.iter().any(|s| stmt_indexes_char_array(name, s, ca))
        }
        StmtKind::Switch { scrutinee, cases } => {
            expr_indexes_char_array(name, scrutinee, ca)
                || cases.iter().any(|c| c.body.iter().any(|s| stmt_indexes_char_array(name, s, ca)))
        }
        _ => false,
    }
}

fn expr_indexes_char_array(name: &str, e: &Expr, ca: &HashSet<String>) -> bool {
    match &e.kind {
        ExprKind::ArrayIndex { array, index } => {
            let arr_is_char = if let ExprKind::Ident(n) = &array.kind {
                ca.contains(n.as_str())
            } else {
                false
            };
            (arr_is_char && expr_mentions(name, index))
                || expr_indexes_char_array(name, array, ca)
                || expr_indexes_char_array(name, index, ca)
        }
        ExprKind::BinOp { left, right, .. }
        | ExprKind::Logical { left, right, .. }
        | ExprKind::Comma { left, right } => {
            expr_indexes_char_array(name, left, ca)
                || expr_indexes_char_array(name, right, ca)
        }
        ExprKind::Unary { operand, .. }
        | ExprKind::Cast { operand, .. }
        | ExprKind::Deref(operand) => expr_indexes_char_array(name, operand, ca),
        ExprKind::AssignExpr { value, .. } => expr_indexes_char_array(name, value, ca),
        ExprKind::CompoundAssignExpr { value, .. } => expr_indexes_char_array(name, value, ca),
        ExprKind::Call { args, .. } => args.iter().any(|a| expr_indexes_char_array(name, a, ca)),
        ExprKind::Member { base, .. } => expr_indexes_char_array(name, base, ca),
        ExprKind::Ternary { cond, then_value, else_value } => {
            expr_indexes_char_array(name, cond, ca)
                || expr_indexes_char_array(name, then_value, ca)
                || expr_indexes_char_array(name, else_value, ca)
        }
        ExprKind::InitList { items } => items.iter().any(|i| expr_indexes_char_array(name, i, ca)),
        _ => false,
    }
}

/// True iff `name` appears as a non-constant index for a CHAR-element
/// array (local or global) anywhere in `body`. Char-element stores
/// touch DL naturally, so BCC prefers DX for the counter to avoid
/// push si.
fn name_is_char_array_index(
    name: &str,
    body: &[Stmt],
    declared: &[DeclItem],
    globals: &crate::codegen::GlobalTable,
) -> bool {
    let mut char_arrays: HashSet<String> = HashSet::new();
    for item in declared {
        if let Type::Array { elem, .. } = &item.ty {
            if matches!(&**elem, Type::Char | Type::UChar) {
                char_arrays.insert(item.name.clone());
            }
        }
    }
    for gname in globals.names() {
        if let Some(Type::Array { elem, .. }) = globals.type_of(gname) {
            if matches!(&**elem, Type::Char | Type::UChar) {
                char_arrays.insert(gname.to_string());
            }
        }
    }
    body.iter()
        .any(|s| stmt_has_name_as_char_array_index(name, s, &char_arrays))
}

fn stmt_has_name_as_char_array_index(
    name: &str,
    stmt: &Stmt,
    char_arrays: &HashSet<String>,
) -> bool {
    match &stmt.kind {
        StmtKind::ArrayAssign { array, indices, value } => {
            // The DX preference fires only when the value is EXACTLY
            // the index identifier (`arr[i] = i`) — that's the
            // pattern where BCC's `mov al, dl` byte-load shape gives
            // the byte-exact byte sequence. Constant value, indexed
            // value (`src[i]`), or arithmetic on i all push BCC to
            // a different register choice. Fixtures 1257 (DX),
            // 1366 (const → SI), 1426 (src[i] → SI), 1276 (a+i → CX).
            char_arrays.contains(array.as_str())
                && indices.iter().any(|i| expr_mentions(name, i))
                && matches!(&value.kind, ExprKind::Ident(n) if n == name)
        }
        StmtKind::ArrayCompoundAssign { array, indices, .. } => {
            // For compound assigns (`+=`), keep the conservative
            // check: any reference to the index in the value side
            // counts.
            char_arrays.contains(array.as_str())
                && indices.iter().any(|i| expr_mentions(name, i))
        }
        StmtKind::If { then_branch, else_branch, .. } => {
            name_in_char_array_index_body(name, then_branch, char_arrays)
                || else_branch
                    .as_ref()
                    .is_some_and(|b| name_in_char_array_index_body(name, b, char_arrays))
        }
        StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
            name_in_char_array_index_body(name, body, char_arrays)
        }
        StmtKind::For { body, .. } => name_in_char_array_index_body(name, body, char_arrays),
        StmtKind::Switch { cases, .. } => cases
            .iter()
            .any(|c| name_in_char_array_index_body(name, &c.body, char_arrays)),
        StmtKind::Return(Some(e))
        | StmtKind::Declare { init: Some(e), .. }
        | StmtKind::Assign { value: e, .. }
        | StmtKind::ExprStmt(e) => expr_has_char_array_index(name, e, char_arrays),
        _ => false,
    }
}

fn name_in_char_array_index_body(
    name: &str,
    body: &[Stmt],
    char_arrays: &HashSet<String>,
) -> bool {
    body.iter()
        .any(|s| stmt_has_name_as_char_array_index(name, s, char_arrays))
}

fn expr_has_char_array_index(name: &str, e: &Expr, char_arrays: &HashSet<String>) -> bool {
    match &e.kind {
        ExprKind::ArrayIndex { array, index } => {
            (matches!(&array.kind, ExprKind::Ident(n) if char_arrays.contains(n.as_str()))
                && expr_mentions(name, index))
                || expr_has_char_array_index(name, array, char_arrays)
                || expr_has_char_array_index(name, index, char_arrays)
        }
        ExprKind::BinOp { left, right, .. }
        | ExprKind::Logical { left, right, .. }
        | ExprKind::Comma { left, right } => {
            expr_has_char_array_index(name, left, char_arrays)
                || expr_has_char_array_index(name, right, char_arrays)
        }
        ExprKind::Unary { operand, .. }
        | ExprKind::Cast { operand, .. }
        | ExprKind::Deref(operand) => expr_has_char_array_index(name, operand, char_arrays),
        ExprKind::AssignExpr { value, .. } => expr_has_char_array_index(name, value, char_arrays),
        ExprKind::CompoundAssignExpr { value, .. } => expr_has_char_array_index(name, value, char_arrays),
        ExprKind::Call { args, .. } => args
            .iter()
            .any(|a| expr_has_char_array_index(name, a, char_arrays)),
        ExprKind::Member { base, .. } => expr_has_char_array_index(name, base, char_arrays),
        ExprKind::Ternary { cond, then_value, else_value } => {
            expr_has_char_array_index(name, cond, char_arrays)
                || expr_has_char_array_index(name, then_value, char_arrays)
                || expr_has_char_array_index(name, else_value, char_arrays)
        }
        ExprKind::InitList { items } => items
            .iter()
            .any(|i| expr_has_char_array_index(name, i, char_arrays)),
        _ => false,
    }
}

/// True iff the function emits any `imul` (clobbers DX). Detects
/// Mul binops where the RHS isn't a const power of two ≤ 256 — those
/// fold to a shift chain and leave DX alone. Char Mul/Div paths fold
/// to `imul/idiv byte ptr <src>` when the RHS is a byte lvalue, but
/// those still don't touch DX/DL on the 8-bit form. The pessimistic
/// rule here: any Mul with non-pow2 const, or any non-const Mul,
/// emits a `imul` that clobbers DX. Used by the char-pool decision.
fn body_emits_imul(body: &[Stmt], char_locals: &HashSet<&str>) -> bool {
    body.iter().any(|s| stmt_emits_imul(s, char_locals))
}

fn stmt_emits_imul(stmt: &Stmt, char_locals: &HashSet<&str>) -> bool {
    match &stmt.kind {
        StmtKind::Return(value) => value.as_ref().is_some_and(expr_emits_imul),
        StmtKind::Declare { init, .. } => init.as_ref().is_some_and(expr_emits_imul),
        StmtKind::Assign { value, .. } => expr_emits_imul(value),
        StmtKind::CompoundAssign { op, value, name } => {
            let target_is_8bit_mul = matches!(op, crate::ast::BinOp::Mul)
                && char_locals.contains(name.as_str())
                && try_const_eval(value).is_none();
            if target_is_8bit_mul {
                return false; // 8-bit imul byte form doesn't clobber DX.
            }
            // `*= K` (const RHS): only emits imul when K isn't a small power of 2.
            if matches!(op, crate::ast::BinOp::Mul) {
                if let Some(k) = try_const_eval(value) {
                    let v = k & 0xFFFF;
                    if v != 0 && (v & (v - 1)) == 0 && v <= 256 {
                        return expr_emits_imul(value);
                    }
                    return true;
                }
                return true;
            }
            expr_emits_imul(value)
        }
        StmtKind::If { cond, then_branch, else_branch } => {
            expr_emits_imul(cond)
                || body_emits_imul(then_branch, char_locals)
                || else_branch.as_ref().is_some_and(|b| body_emits_imul(b, char_locals))
        }
        StmtKind::While { cond, body } => {
            expr_emits_imul(cond) || body_emits_imul(body, char_locals)
        }
        StmtKind::DoWhile { body, cond } => {
            body_emits_imul(body, char_locals) || expr_emits_imul(cond)
        }
        StmtKind::For { init, cond, step, body } => {
            init.as_ref().is_some_and(|es| es.iter().any(expr_emits_imul))
                || cond.as_ref().is_some_and(expr_emits_imul)
                || step.as_ref().is_some_and(|es| es.iter().any(expr_emits_imul))
                || body_emits_imul(body, char_locals)
        }
        StmtKind::Switch { scrutinee, cases } => {
            expr_emits_imul(scrutinee)
                || cases.iter().any(|c| body_emits_imul(&c.body, char_locals))
        }
        StmtKind::ArrayAssign { indices, value, .. }
        | StmtKind::MemberArrayAssign { indices, value, .. } => {
            indices.iter().any(expr_emits_imul) || expr_emits_imul(value)
        }
        StmtKind::ArrayCompoundAssign { op, indices, value, .. } => {
            let mul_emits = matches!(op, crate::ast::BinOp::Mul)
                && try_const_eval(value).map_or(true, |k| {
                    let v = k & 0xFFFF;
                    !(v != 0 && (v & (v - 1)) == 0 && v <= 256)
                });
            mul_emits || indices.iter().any(expr_emits_imul) || expr_emits_imul(value)
        }
        StmtKind::DerefAssign { target, value } => {
            expr_emits_imul(target) || expr_emits_imul(value)
        }
        StmtKind::DerefCompoundAssign { op, target, value, .. } => {
            let mul_emits = matches!(op, crate::ast::BinOp::Mul)
                && try_const_eval(value).map_or(true, |k| {
                    let v = k & 0xFFFF;
                    !(v != 0 && (v & (v - 1)) == 0 && v <= 256)
                });
            mul_emits || expr_emits_imul(target) || expr_emits_imul(value)
        }
        StmtKind::MemberAssign { base, value, .. } => {
            expr_emits_imul(base) || expr_emits_imul(value)
        }
        StmtKind::MemberCompoundAssign { op, base, value, .. } => {
            let mul_emits = matches!(op, crate::ast::BinOp::Mul)
                && try_const_eval(value).map_or(true, |k| {
                    let v = k & 0xFFFF;
                    !(v != 0 && (v & (v - 1)) == 0 && v <= 256)
                });
            mul_emits || expr_emits_imul(base) || expr_emits_imul(value)
        }
        StmtKind::ExprStmt(e) => expr_emits_imul(e),
        StmtKind::Block(body) => body_emits_imul(body, char_locals),
        StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Goto { .. }
        | StmtKind::Label { .. }
        | StmtKind::Empty => false,
    }
}

fn expr_emits_imul(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::BinOp { op, left, right } => {
            let this_emits = matches!(op, crate::ast::BinOp::Mul)
                && {
                    // For const RHS power-of-2 ≤ 256: shift, no imul.
                    if let Some(k) = try_const_eval(right) {
                        let v = k & 0xFFFF;
                        !(v != 0 && (v & (v - 1)) == 0 && v <= 256)
                    } else {
                        true
                    }
                };
            this_emits || expr_emits_imul(left) || expr_emits_imul(right)
        }
        ExprKind::Logical { left, right, .. } => {
            expr_emits_imul(left) || expr_emits_imul(right)
        }
        ExprKind::Unary { operand, .. } => expr_emits_imul(operand),
        ExprKind::Call { args, .. } => args.iter().any(expr_emits_imul),
        ExprKind::AssignExpr { value, .. } => expr_emits_imul(value),
        ExprKind::CompoundAssignExpr { value, .. } => expr_emits_imul(value),
        ExprKind::Deref(operand) => expr_emits_imul(operand),
        ExprKind::ArrayIndex { array, index } => {
            expr_emits_imul(array) || expr_emits_imul(index)
        }
        ExprKind::Member { base, .. } => expr_emits_imul(base),
        ExprKind::Ternary { cond, then_value, else_value } => {
            expr_emits_imul(cond)
                || expr_emits_imul(then_value)
                || expr_emits_imul(else_value)
        }
        ExprKind::Cast { operand, .. } => expr_emits_imul(operand),
        ExprKind::InitList { items } => items.iter().any(expr_emits_imul),
        ExprKind::Comma { left, right } => {
            expr_emits_imul(left) || expr_emits_imul(right)
        }
        ExprKind::UpdateLvalue { target, .. } => expr_emits_imul(target),
        ExprKind::Update { .. }
        | ExprKind::Ident(_)
        | ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::DoubleLit(_)
        | ExprKind::AddressOf(_)
        | ExprKind::AddressOfArrayElem { .. }
        | ExprKind::StringLit(_) => false,
    }
}

/// True iff any char compound assign has a non-byte RHS, which
/// triggers the widening dance that uses DL as a scratch reg
/// (`mov dl, <c>; add dl, al; mov <c>, dl`). Fixture 1430.
fn body_has_char_compound_int_rhs(body: &[Stmt], char_locals: &HashSet<&str>) -> bool {
    body.iter().any(|s| stmt_has_char_compound_int_rhs(s, char_locals))
}

fn stmt_has_char_compound_int_rhs(stmt: &Stmt, char_locals: &HashSet<&str>) -> bool {
    match &stmt.kind {
        StmtKind::CompoundAssign { name, op, value } => {
            // The DL-as-scratch dance only fires when the RHS
            // computation actually OWNS AL. For a single int lvalue
            // (`c += n` where n is int local/global), BCC emits
            // `mov al, <c>; add al, [n]; mov <c>, al` — AL holds c,
            // so c can stay in DL. Fixture 1213 (`c += n`).
            // When the RHS is a binop/cast/ternary/call/etc. that
            // computes into AL, c must move out of DL. Fixture
            // 1430 (`c += a*2`).
            // Bitwise ops (AND/OR/XOR) are also exempt — fixture
            // 1254 (`c |= n`).
            char_locals.contains(name.as_str())
                && matches!(
                    op,
                    crate::ast::BinOp::Add | crate::ast::BinOp::Sub | crate::ast::BinOp::Mul
                )
                && rhs_owns_al(value, char_locals)
        }
        StmtKind::If { then_branch, else_branch, .. } => {
            body_has_char_compound_int_rhs(then_branch, char_locals)
                || else_branch.as_ref().is_some_and(|b| body_has_char_compound_int_rhs(b, char_locals))
        }
        StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
            body_has_char_compound_int_rhs(body, char_locals)
        }
        StmtKind::For { body, .. } => body_has_char_compound_int_rhs(body, char_locals),
        StmtKind::Switch { cases, .. } => {
            cases.iter().any(|c| body_has_char_compound_int_rhs(&c.body, char_locals))
        }
        _ => false,
    }
}

fn is_char_typed_expr(e: &Expr, char_locals: &HashSet<&str>) -> bool {
    match &e.kind {
        ExprKind::Ident(name) => char_locals.contains(name.as_str()),
        ExprKind::IntLit(_) => true, // Small const fits in a byte and BCC keeps the byte form.
        ExprKind::Cast { ty, .. } => ty.is_char_like(),
        _ => false,
    }
}

/// True iff the RHS expression's codegen "owns" AL — i.e., AL
/// holds the RHS result, leaving no room for the char target to
/// also live in AL. When this fires for a char compound assign,
/// BCC moves the char target out of DL (whose AL alias would
/// be its working register).
///
/// Returns false for:
/// - Char-typed expressions (constant, char ident, char cast) —
///   the byte form passes through cleanly.
/// - Plain int idents — BCC emits `mov al, <c>; add al, [n]`,
///   so AL holds c, not the RHS.
///
/// Returns true for:
/// - BinOp, Cast (to non-char), Ternary, Call, Comma, Update —
///   anything that computes a value through AL/AX.
fn rhs_owns_al(e: &Expr, char_locals: &HashSet<&str>) -> bool {
    if is_char_typed_expr(e, char_locals) {
        return false;
    }
    match &e.kind {
        // Single int lvalue load: BCC uses `<op> al, [mem]` directly,
        // AL still belongs to the char target.
        ExprKind::Ident(_) => false,
        // Anything more complex computes the RHS through AL.
        ExprKind::BinOp { .. }
        | ExprKind::Cast { .. }
        | ExprKind::Ternary { .. }
        | ExprKind::Call { .. }
        | ExprKind::Comma { .. }
        | ExprKind::Update { .. }
        | ExprKind::Logical { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Deref(_)
        | ExprKind::ArrayIndex { .. }
        | ExprKind::Member { .. } => true,
        _ => false,
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
    /// True if the C source marked this local with the `register`
    /// storage class. The eligibility threshold drops to 1 use for
    /// these — BCC honors the hint even for variables that wouldn't
    /// otherwise qualify. Fixtures 1550, 1560. Always false for
    /// params (the storage class isn't passed in the calling
    /// convention) and statics.
    is_register: bool,
    /// True if the C source marked this local `volatile`. Forces
    /// stack allocation regardless of use count — each access must
    /// read/write memory, so the value cannot live in a register.
    /// Fixtures 1548, 2243.
    is_volatile: bool,
}

#[derive(Clone, Copy)]
enum DeclKind {
    Local,
    Param { incoming_offset: u16 },
}

/// Walker state passed alongside the flat decl list. Tracks which
/// sibling-block each upcoming decl belongs to so offset assignment
/// can reuse slots across siblings. Today only flat sibling blocks
/// (no nesting beyond depth 1) are modeled — that's what fixtures
/// 1743, 1966-1969 exercise.
struct CollectCtx {
    /// Per-decl sibling-block id: 0 = function level, 1+ = the Nth
    /// statement-position `{ ... }` block (in source order).
    block_ids: Vec<u32>,
    next_block_id: u32,
    current_block_id: u32,
}

impl CollectCtx {
    fn new() -> Self {
        Self { block_ids: Vec::new(), next_block_id: 1, current_block_id: 0 }
    }
}

fn collect_decls_ctx(stmt: &Stmt, out: &mut Vec<DeclItem>, ctx: &mut CollectCtx) {
    match &stmt.kind {
        StmtKind::Declare { ty, name, is_static, is_register, is_volatile, .. } => {
            if !*is_static {
                out.push(DeclItem {
                    name: name.clone(),
                    ty: ty.clone(),
                    kind: DeclKind::Local,
                    is_register: *is_register,
                    is_volatile: *is_volatile,
                });
                ctx.block_ids.push(ctx.current_block_id);
            }
        }
        StmtKind::Block(body) => {
            // Bare `{ ... }` opens a fresh sibling block at this
            // nesting depth. Save / restore the current_block_id so
            // nested blocks don't accidentally share IDs with their
            // outer level — though we don't yet handle nested-block
            // shadowing properly (fixture 2467), the bookkeeping is
            // right for the flat case.
            let saved = ctx.current_block_id;
            let id = ctx.next_block_id;
            ctx.next_block_id += 1;
            ctx.current_block_id = id;
            for s in body {
                collect_decls_ctx(s, out, ctx);
            }
            ctx.current_block_id = saved;
        }
        StmtKind::If { then_branch, else_branch, .. } => {
            for s in then_branch {
                collect_decls_ctx(s, out, ctx);
            }
            if let Some(else_branch) = else_branch {
                for s in else_branch {
                    collect_decls_ctx(s, out, ctx);
                }
            }
        }
        StmtKind::While { body, .. }
        | StmtKind::DoWhile { body, .. }
        | StmtKind::For { body, .. } => {
            for s in body {
                collect_decls_ctx(s, out, ctx);
            }
        }
        StmtKind::Switch { cases, .. } => {
            for c in cases {
                for s in &c.body {
                    collect_decls_ctx(s, out, ctx);
                }
            }
        }
        _ => {}
    }
}

fn collect_decls(stmt: &Stmt, out: &mut Vec<DeclItem>) {
    match &stmt.kind {
        StmtKind::Declare { ty, name, is_static, is_register, is_volatile, .. } => {
            if !*is_static {
                out.push(DeclItem {
                    name: name.clone(),
                    ty: ty.clone(),
                    kind: DeclKind::Local,
                    is_register: *is_register,
                    is_volatile: *is_volatile,
                });
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
        StmtKind::Block(body) => {
            for s in body {
                collect_decls(s, out);
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
            // `a[i] = v;` mirrors `a[i]` as an rvalue: const index
            // gives the +2 direct-deref bonus, variable index gives
            // +1. Matches the read-side ArrayIndex rule. Arrays
            // never enregister anyway, but the same statement could
            // be a pointer-target indexed assign (`p[i] = v`); for
            // those, variable index keeps the pointer below the
            // enregister threshold (fixtures 1285, 3559).
            let const_idx = indices.len() == 1
                && try_const_eval(&indices[0]).is_some();
            let bonus = if const_idx { 2 } else { 1 };
            *counts.entry(array.clone()).or_insert(0) += bonus;
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
        StmtKind::Block(body) => {
            for s in body {
                count_uses_stmt(s, counts);
            }
        }
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
        ExprKind::CompoundAssignExpr { target, value, .. } => {
            // Compound assign expression: target is both read and
            // written, like StmtKind::CompoundAssign — count it
            // twice.
            *counts.entry(target.clone()).or_insert(0) += 2;
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
            // `a[K]` / `p[K]` with a constant index gives the base
            // name the direct-deref bonus (folds to a single load
            // with a baked-in offset). With a variable index, the
            // access needs BX setup regardless of where the base
            // lives, so the bonus doesn't apply — fixture 1339
            // (`p[i]` keeps p on the stack). For non-Ident bases
            // (e.g. string literals), no use-count contribution.
            let const_idx = try_const_eval(index).is_some();
            if let ExprKind::Ident(name) = &array.kind {
                let bonus = if const_idx { 2 } else { 1 };
                *counts.entry(name.clone()).or_insert(0) += bonus;
            } else {
                count_uses_expr(array, counts);
            }
            count_uses_expr(index, counts);
        }
        ExprKind::StringLit(_) => {}
        ExprKind::IntLit(_) | ExprKind::FloatLit(_) | ExprKind::DoubleLit(_) => {}
        ExprKind::Member { base, kind, .. } => {
            // `p->x` direct-derefs the pointer; `a.x` is just an
            // access to a struct lvalue. `(p ± K)->x` for a constant
            // K is also a direct-deref form — BCC bakes the offset
            // into the indexed addressing (`mov ax, [si-4]` for
            // `(p - 1)->x` with sizeof(P) = 4). Fixture 3251.
            match kind {
                crate::ast::MemberKind::Arrow => {
                    if let Some(name) = direct_deref_target(base) {
                        *counts.entry(name).or_insert(0) += 2;
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
        ExprKind::UpdateLvalue { target, .. } => {
            // Same shape as `Update`'s read-then-write — the
            // generalized lvalue target contributes two uses to
            // any name it mentions. Walk it via the existing
            // expr-walk so chains like `(*pp)++` reach the
            // pointer ident.
            count_uses_expr(target, counts);
        }
    }
}

