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

use std::collections::HashMap;

use crate::ast::{Expr, ExprKind, Function, Stmt, StmtKind, Type};

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

/// The four registers BCC draws from for enregistered locals/params.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg {
    Si,
    Di,
    Dx,
    Bx,
}

impl Reg {
    /// The two-letter asm name (`si`/`di`/`dx`/`bx`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Si => "si",
            Self::Di => "di",
            Self::Dx => "dx",
            Self::Bx => "bx",
        }
    }

    /// True for callee-saved registers (SI, DI). DX and BX are used by
    /// BCC without save/restore at the function boundary.
    #[must_use]
    pub fn is_callee_saved(self) -> bool {
        matches!(self, Self::Si | Self::Di)
    }

    /// The fixed allocation order BCC uses.
    pub const POOL: [Self; 4] = [Self::Si, Self::Di, Self::Dx, Self::Bx];
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

/// Use count threshold for enregistering. A name with `>= THRESHOLD`
/// textual occurrences (init + reads + writes) gets a register if one
/// is still available.
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
}

#[derive(Debug, Clone, Copy)]
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
                ty: param.ty,
                kind: DeclKind::Param { incoming_offset: param_offset },
            });
            // Every param takes a 2-byte slot on the stack regardless
            // of declared type — `char` gets promoted at the push site
            // by the caller. (We haven't pinned this with a `char`-
            // param fixture; revisit when we have one.)
            param_offset += 2;
            *counts.entry(param.name.clone()).or_insert(0) += 1;
        }

        for stmt in &function.body {
            collect_decls(stmt, &mut declared);
            count_uses_stmt(stmt, &mut counts);
        }

        // Pass 2: assign locations.
        let mut by_name = HashMap::new();
        let mut stack_bytes = 0u16;
        let mut saved_regs = Vec::new();
        let mut param_loads = Vec::new();
        let mut next_reg = 0usize;
        for item in &declared {
            let uses = counts.get(&item.name).copied().unwrap_or(0);
            let eligible_for_reg = item.ty == Type::Int;
            let location = if eligible_for_reg
                && uses >= ENREGISTER_THRESHOLD
                && next_reg < Reg::POOL.len()
            {
                let reg = Reg::POOL[next_reg];
                next_reg += 1;
                if reg.is_callee_saved() {
                    saved_regs.push(reg);
                }
                // For params, record the prologue load.
                if let DeclKind::Param { incoming_offset } = item.kind {
                    param_loads.push(ParamLoad { reg, incoming_offset });
                }
                LocalLocation::Reg(reg)
            } else {
                match item.kind {
                    DeclKind::Local => {
                        stack_bytes = align_up(stack_bytes, item.ty.alignment())
                            + item.ty.size_bytes();
                        LocalLocation::Stack(-i16::try_from(stack_bytes).expect("stack frame fits in i16"))
                    }
                    DeclKind::Param { incoming_offset } => {
                        LocalLocation::Stack(i16::try_from(incoming_offset).expect("param offset fits in i16"))
                    }
                }
            };
            by_name.insert(item.name.clone(), LocalEntry { location, ty: item.ty });
        }

        Self { stack_bytes, by_name, saved_regs, param_loads }
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

    #[must_use]
    pub fn type_of(&self, name: &str) -> Type {
        self.entry(name).ty
    }

    fn entry(&self, name: &str) -> &LocalEntry {
        self.by_name
            .get(name)
            .unwrap_or_else(|| panic!("unknown local in codegen: {name}"))
    }
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
        StmtKind::Declare { ty, name, .. } => {
            out.push(DeclItem { name: name.clone(), ty: *ty, kind: DeclKind::Local });
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
        StmtKind::While { body, .. } => {
            for s in body {
                collect_decls(s, out);
            }
        }
        StmtKind::Return(_) | StmtKind::Assign { .. } | StmtKind::ExprStmt(_) => {}
    }
}

fn count_uses_stmt(stmt: &Stmt, counts: &mut HashMap<String, u32>) {
    match &stmt.kind {
        StmtKind::Return(value) => {
            if let Some(e) = value {
                count_uses_expr(e, counts);
            }
        }
        StmtKind::Declare { name, init, .. } => {
            *counts.entry(name.clone()).or_insert(0) += 1;
            if let Some(e) = init {
                count_uses_expr(e, counts);
            }
        }
        StmtKind::Assign { name, value } => {
            *counts.entry(name.clone()).or_insert(0) += 1;
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
        StmtKind::ExprStmt(e) => count_uses_expr(e, counts),
    }
}

fn count_uses_expr(e: &Expr, counts: &mut HashMap<String, u32>) {
    match &e.kind {
        ExprKind::Ident(name) => {
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
        ExprKind::BinOp { left, right, .. } => {
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
        ExprKind::Call { args, .. } => {
            for a in args {
                count_uses_expr(a, counts);
            }
        }
        ExprKind::IntLit(_) => {}
    }
}

