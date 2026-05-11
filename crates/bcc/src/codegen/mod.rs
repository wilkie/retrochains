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

use crate::ast::{Expr, ExprKind, Function, Stmt, StmtKind, Type};

mod line_map;

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
        // Locals allocation: BCC emits one `dec sp` per *byte* of frame
        // (the instruction decrements SP by 1) for small frames, rather
        // than `sub sp,N`. So for a single `int` local (2 bytes), we
        // emit two `dec sp`. There's presumably a threshold beyond which
        // `sub sp,N` wins on code size — we'll learn it when a fixture
        // demands it.
        for _ in 0..self.locals.used {
            self.out.extend_from_slice(b"\tdec\tsp\r\n");
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
        match &e.kind {
            ExprKind::IntLit(0) => {
                self.out.extend_from_slice(b"\txor\tax,ax\r\n");
            }
            ExprKind::IntLit(n) => {
                let _ = write!(self.out, "\tmov\tax,{n}\r\n");
            }
            ExprKind::Ident(name) => {
                let offset = self
                    .locals
                    .offset_of(name)
                    .unwrap_or_else(|| panic!("unknown local in codegen: {name}"));
                let _ = write!(self.out, "\tmov\tax,word ptr [bp-{offset}]\r\n");
            }
        }
    }

    fn emit_store_local(&mut self, offset: u16, init: &Expr) {
        match &init.kind {
            ExprKind::IntLit(n) => {
                let _ = write!(
                    self.out,
                    "\tmov\tword ptr [bp-{offset}],{n}\r\n"
                );
            }
            ExprKind::Ident(name) => {
                // a = b; — first load to AX, then store. Not in any fixture
                // yet; included for completeness so 004's grammar doesn't
                // dead-end if extended.
                let src = self
                    .locals
                    .offset_of(name)
                    .unwrap_or_else(|| panic!("unknown local in codegen: {name}"));
                let _ = write!(self.out, "\tmov\tax,word ptr [bp-{src}]\r\n");
                let _ = write!(self.out, "\tmov\tword ptr [bp-{offset}],ax\r\n");
            }
        }
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
