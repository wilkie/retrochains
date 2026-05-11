//! Codegen: walk a [`Function`] AST and emit the per-function asm bytes
//! BCC's `-S` would have produced. The format-emitter (`emit_s.rs`) calls
//! us between the file-level scaffolding (preamble + debug records +
//! segment scaffold) and the tail.
//!
//! Single-pass shape: we don't build any IR; we walk the AST in source
//! order and write asm directly. Source-line comments are emitted just
//! before the asm for each new source line we encounter (mirroring BCC's
//! interleaving — see `specs/bcc/ASM_OUTPUT.md`).

use std::collections::HashMap;
use std::io::Write as _;

use crate::ast::{BinOp, Expr, ExprKind, Function, Stmt, StmtKind, Type};

mod fold;
mod line_map;

use fold::try_const_eval;

use line_map::LineMap;

/// Emit the per-function chunk of an `-S` file for one function.
///
/// `source` is the full original source text; we slice it to emit
/// source-line comments.
pub fn emit_function(out: &mut Vec<u8>, source: &str, function: &Function) {
    let mut emitter = FunctionEmitter::new(out, source, function);
    emitter.run();
}

/// What BCC prepends to a C symbol when emitting it in the small memory
/// model. (Other memory models may differ; we'll learn what when a
/// fixture demands it.)
pub fn function_symbol(name: &str) -> String {
    format!("_{name}")
}

/// Locals layout for one function. Currently a flat first-fit allocator
/// at the top of the stack frame (negative offsets from `bp`); fields
/// grow downward.
struct Locals {
    /// Total bytes claimed (always even because we only have `int` locals
    /// today; will need alignment rules when we add `char` etc.).
    used: u16,
    /// Name → byte offset from `bp`. Offset is negative (`[bp-N]`); we
    /// store the magnitude.
    by_name: HashMap<String, u16>,
}

impl Locals {
    fn new() -> Self {
        Self { used: 0, by_name: HashMap::new() }
    }

    fn allocate(&mut self, name: &str, ty: Type) {
        self.used += ty.size_bytes();
        self.by_name.insert(name.to_owned(), self.used);
    }

    fn offset_of(&self, name: &str) -> Option<u16> {
        self.by_name.get(name).copied()
    }
}

struct FunctionEmitter<'a> {
    out: &'a mut Vec<u8>,
    source: &'a str,
    function: &'a Function,
    lines: LineMap,
    /// 1-based source line of the last comment we emitted, or 0 if we
    /// haven't emitted any comment yet for this function.
    current_line: u32,
    locals: Locals,
}

impl<'a> FunctionEmitter<'a> {
    fn new(out: &'a mut Vec<u8>, source: &'a str, function: &'a Function) -> Self {
        Self {
            out,
            source,
            function,
            lines: LineMap::new(source),
            current_line: 0,
            locals: Locals::new(),
        }
    }

    fn run(&mut self) {
        // Pre-walk to total the locals' stack space. We do this before
        // emitting the prologue because the prologue size depends on the
        // total. (Single-pass-y: still one walk for emission; a separate
        // O(n) walk just to total.)
        for stmt in &self.function.body {
            if let StmtKind::Declare { ty, name, .. } = &stmt.kind {
                self.locals.allocate(name, *ty);
            }
        }

        self.out.extend_from_slice(b"_TEXT\tsegment byte public 'CODE'\r\n");

        // Header line: emit `;` comment block for the line where the
        // function definition starts, before the prologue.
        let head_line = self.lines.line_of(self.function.span.start);
        self.advance_to_line(head_line);

        self.out.extend_from_slice(b"\tassume\tcs:_TEXT\r\n");
        let sym = function_symbol(&self.function.name);
        let _ = write!(self.out, "{sym}\tproc\tnear\r\n");

        // Prologue.
        self.out.extend_from_slice(b"\tpush\tbp\r\n");
        self.out.extend_from_slice(b"\tmov\tbp,sp\r\n");
        // Locals allocation. Up to 2 bytes BCC emits per-byte `dec sp`
        // (2 single-byte instructions); above that it switches to
        // `sub sp,N` (3-byte instruction, immediately cheaper than
        // 3+ `dec sp`s). The exact crossover (between 2 and 4 in our
        // fixtures) is documented in specs/bcc/ASM_OUTPUT.md.
        match self.locals.used {
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

        // Body.
        for stmt in &self.function.body {
            self.emit_stmt(stmt);
        }

        // Single exit label.
        self.out.extend_from_slice(b"@1@50:\r\n");

        // Closing-brace line gets its own comment block. Span end is the
        // byte just past `}`, so back up by one to get the brace itself.
        let close_offset = self.function.span.end.saturating_sub(1);
        let close_line = self.lines.line_of(close_offset);
        self.advance_to_line(close_line);

        // Epilogue. Need `mov sp,bp` to undo the `dec sp`s when we have
        // locals. (For the empty-frame case `pop bp` alone suffices.)
        if self.locals.used > 0 {
            self.out.extend_from_slice(b"\tmov\tsp,bp\r\n");
        }
        self.out.extend_from_slice(b"\tpop\tbp\r\n");
        self.out.extend_from_slice(b"\tret\t\r\n");

        let _ = write!(self.out, "{sym}\tendp\r\n");
        self.out.extend_from_slice(b"\t?debug\tC E9\r\n");
        self.out.extend_from_slice(b"_TEXT\tends\r\n");
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        let line = self.lines.line_of(stmt.span.start);
        self.advance_to_line(line);

        match &stmt.kind {
            StmtKind::Return(value) => {
                self.emit_return_value_load(value.as_ref());
                self.out.extend_from_slice(b"\tjmp\tshort @1@50\r\n");
            }
            StmtKind::Declare { name, init, .. } => {
                let offset = self
                    .locals
                    .offset_of(name)
                    .expect("declaration pre-walked into locals");
                if let Some(init) = init {
                    self.emit_store_local(offset, init);
                }
            }
        }
    }

    fn emit_return_value_load(&mut self, value: Option<&Expr>) {
        let Some(e) = value else { return };
        self.emit_expr_to_ax(e);
    }

    fn emit_store_local(&mut self, offset: u16, init: &Expr) {
        // If the initializer folds to a constant, store it directly with a
        // single `mov word ptr [bp-N],K`. Otherwise compute the RHS into
        // AX and store from AX.
        if let Some(v) = try_const_eval(init) {
            let _ = write!(self.out, "\tmov\tword ptr [bp-{offset}],{v}\r\n");
            return;
        }
        self.emit_expr_to_ax(init);
        let _ = write!(self.out, "\tmov\tword ptr [bp-{offset}],ax\r\n");
    }

    /// Emit code that leaves the value of `e` in AX. If `e` folds to a
    /// constant we take the constant path (`xor ax,ax` for zero,
    /// otherwise `mov ax,K`). Otherwise we emit the runtime pattern.
    fn emit_expr_to_ax(&mut self, e: &Expr) {
        if let Some(v) = try_const_eval(e) {
            if v == 0 {
                self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            } else {
                let _ = write!(self.out, "\tmov\tax,{v}\r\n");
            }
            return;
        }
        match &e.kind {
            ExprKind::IntLit(_) => unreachable!("literals fold via try_const_eval"),
            ExprKind::Ident(name) => {
                let offset = self.local_offset(name);
                let _ = write!(self.out, "\tmov\tax,word ptr [bp-{offset}]\r\n");
            }
            ExprKind::BinOp { op: BinOp::Add, left, right } => {
                // Load left operand into AX, then add right with a
                // memory-direct (or immediate) `add`. This matches the
                // pattern observed in fixture 006.
                self.emit_expr_to_ax(left);
                self.emit_add_right(right);
            }
        }
    }

    /// Emit the right-hand side of a `+` as an `add ax, <operand>`.
    /// Sub-expressions on the right side aren't yet supported because no
    /// fixture exercises them; we'll grow this when one does.
    fn emit_add_right(&mut self, e: &Expr) {
        if let Some(v) = try_const_eval(e) {
            let _ = write!(self.out, "\tadd\tax,{v}\r\n");
            return;
        }
        match &e.kind {
            ExprKind::Ident(name) => {
                let offset = self.local_offset(name);
                let _ = write!(self.out, "\tadd\tax,word ptr [bp-{offset}]\r\n");
            }
            ExprKind::IntLit(_) => unreachable!("literals fold via try_const_eval"),
            ExprKind::BinOp { .. } => {
                // A nested non-constant right-hand binary operand would
                // need register save/restore; no fixture forces this
                // yet. Fail loudly so we notice the shape when it lands.
                panic!("nested non-constant right operand not yet supported");
            }
        }
    }

    fn local_offset(&self, name: &str) -> u16 {
        self.locals
            .offset_of(name)
            .unwrap_or_else(|| panic!("unknown local in codegen: {name}"))
    }

    /// Emit `;` source-comment block(s) for any source line(s) up to and
    /// including `line` that we haven't already emitted a comment for.
    /// For now we only emit the *target* line, not every line in between
    /// — fixtures haven't yet exercised a case with skipped lines.
    fn advance_to_line(&mut self, line: u32) {
        if line <= self.current_line {
            return;
        }
        let content = self.lines.line_content(self.source, line);
        self.out.extend_from_slice(b"   ;\t\r\n");
        let _ = write!(self.out, "   ;\t{content}\r\n");
        self.out.extend_from_slice(b"   ;\t\r\n");
        self.current_line = line;
    }
}
