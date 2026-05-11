//! Local-variable layout for one function.
//!
//! BCC enregisters some locals into a small fixed pool of registers
//! (SI, DI, DX, BX in that order) before falling back to stack slots.
//! The decision is driven by a use-count heuristic captured in the
//! investigation fixtures `028`–`032` and documented in
//! `specs/bcc/ASM_OUTPUT.md`. Briefly:
//!
//! - Count every textual occurrence of each local, including the
//!   initializer of its declaration and the LHS of any assignment.
//! - Locals with ≥ 3 occurrences are eligible for a register.
//! - Eligible locals receive registers in declaration order from the
//!   pool `[SI, DI, DX, BX]`; the rest land on the stack.
//!
//! Once we layout the function, codegen can ask `location_of(name)`
//! for any local and emit either a register operand or a `word ptr
//! [bp-N]` memory operand accordingly.

use std::collections::HashMap;

use crate::ast::{Expr, ExprKind, Function, Stmt, StmtKind, Type};

/// Round `n` up to the next multiple of `alignment` (a small power of 2).
fn align_up(n: u16, alignment: u16) -> u16 {
    let mask = alignment - 1;
    (n + mask) & !mask
}

/// Where one local variable lives for the duration of the function.
#[derive(Debug, Clone, Copy)]
pub enum LocalLocation {
    /// Memory at `[bp - offset]`. The offset is the magnitude (positive).
    Stack(u16),
    /// Register-resident.
    Reg(Reg),
}

/// The four registers BCC draws from for enregistered locals.
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

/// Use count threshold for enregistering. A local with `>= THRESHOLD`
/// textual occurrences (init + reads + writes) gets a register if one
/// is still available. Determined empirically from fixture `030`
/// (`limit` with 2 uses stays on the stack) and fixture `032` (`i`
/// with 3 uses goes into SI).
const ENREGISTER_THRESHOLD: u32 = 3;

#[derive(Debug)]
pub struct Locals {
    /// Total bytes claimed for stack-resident locals only.
    stack_bytes: u16,
    /// Per-local placement + type. Type is kept so codegen can pick the
    /// right asm operand width (`byte ptr` vs `word ptr`).
    by_name: HashMap<String, LocalEntry>,
    /// Which callee-saved registers we actually used (in order, for
    /// emitting matching push/pop sequences).
    saved_regs: Vec<Reg>,
}

#[derive(Debug, Clone, Copy)]
struct LocalEntry {
    location: LocalLocation,
    ty: Type,
}

impl Locals {
    /// Compute the layout from the function body. Walks all statements
    /// once to count uses, then assigns locations in declaration order.
    #[must_use]
    pub fn analyze(function: &Function) -> Self {
        // Pass 1: collect declarations in source order and count uses.
        let mut declared: Vec<(String, Type)> = Vec::new();
        let mut counts: HashMap<String, u32> = HashMap::new();
        for stmt in &function.body {
            collect_decls(stmt, &mut declared);
            count_uses_stmt(stmt, &mut counts);
        }

        // Pass 2: assign locations.
        //
        // Stack offsets grow downward from `bp`; we track the cumulative
        // distance below `bp` in `stack_bytes`. Each `int` slot must
        // sit on an even bp-offset (BCC pads with a byte when the
        // cursor is on an odd offset, as in fixture 011: `char c`
        // takes [bp-1], then `int i` lands at [bp-4] with [bp-2]
        // padding).
        //
        // Only `int` locals are eligible for register allocation; we
        // haven't observed BCC enregistering a `char` and don't have a
        // fixture to pin its shape.
        let mut by_name = HashMap::new();
        let mut stack_bytes = 0u16;
        let mut saved_regs = Vec::new();
        let mut next_reg = 0usize;
        for (name, ty) in &declared {
            let uses = counts.get(name).copied().unwrap_or(0);
            let eligible_for_reg = *ty == Type::Int;
            let location = if eligible_for_reg
                && uses >= ENREGISTER_THRESHOLD
                && next_reg < Reg::POOL.len()
            {
                let reg = Reg::POOL[next_reg];
                next_reg += 1;
                if reg.is_callee_saved() {
                    saved_regs.push(reg);
                }
                LocalLocation::Reg(reg)
            } else {
                stack_bytes = align_up(stack_bytes, ty.alignment()) + ty.size_bytes();
                LocalLocation::Stack(stack_bytes)
            };
            by_name.insert(name.clone(), LocalEntry { location, ty: *ty });
        }

        Self { stack_bytes, by_name, saved_regs }
    }

    #[must_use]
    pub fn stack_bytes(&self) -> u16 {
        self.stack_bytes
    }

    /// Callee-saved registers we used, in the order we pushed them.
    /// The epilogue should pop them in reverse.
    #[must_use]
    pub fn saved_regs(&self) -> &[Reg] {
        &self.saved_regs
    }

    /// Where does `name` live? Panics on an unknown name (codegen bug).
    #[must_use]
    pub fn location_of(&self, name: &str) -> LocalLocation {
        self.entry(name).location
    }

    /// Declared type of `name`. Used to pick `byte`-vs-`word` operand
    /// widths in codegen.
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

fn collect_decls(stmt: &Stmt, out: &mut Vec<(String, Type)>) {
    match &stmt.kind {
        StmtKind::Declare { ty, name, .. } => {
            out.push((name.clone(), *ty));
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
        StmtKind::Return(_) | StmtKind::Assign { .. } => {}
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
            // The declaration line is itself a use of the name (the
            // initializer write). This matches BCC's heuristic — a
            // declared-but-otherwise-unread local with only an init
            // still counts as having one use, not zero.
            *counts.entry(name.clone()).or_insert(0) += 1;
            if let Some(e) = init {
                count_uses_expr(e, counts);
            }
        }
        StmtKind::Assign { name, value } => {
            // Both the LHS and any names in the RHS count.
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
        // No-arg calls and literals don't reference any names.
        ExprKind::IntLit(_) | ExprKind::Call { .. } => {}
    }
}
