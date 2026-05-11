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
/// source-line comments. `func_idx` is the 1-based index of this
/// function within its translation unit — it ends up in the
/// `@<func_idx>@50` exit label.
pub fn emit_function(out: &mut Vec<u8>, source: &str, function: &Function, func_idx: u32) {
    let mut emitter = FunctionEmitter::new(out, source, function, func_idx);
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
    func_idx: u32,
    lines: LineMap,
    /// 1-based source line of the last comment we emitted, or 0 if we
    /// haven't emitted any comment yet for this function.
    current_line: u32,
    locals: Locals,
}

impl<'a> FunctionEmitter<'a> {
    fn new(
        out: &'a mut Vec<u8>,
        source: &'a str,
        function: &'a Function,
        func_idx: u32,
    ) -> Self {
        Self {
            out,
            source,
            function,
            func_idx,
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

        // Single exit label, `@<func_idx>@50`.
        let _ = write!(self.out, "@{}@50:\r\n", self.func_idx);

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
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        let line = self.lines.line_of(stmt.span.start);
        self.advance_to_line(line);

        match &stmt.kind {
            StmtKind::Return(value) => {
                self.emit_return_value_load(value.as_ref());
                let _ = write!(self.out, "\tjmp\tshort @{}@50\r\n", self.func_idx);
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
            ExprKind::BinOp { op, left, right } => {
                // Load left operand into AX, then apply the right side
                // via the operator-specific pattern (memory-direct or
                // immediate add/sub; single-operand imul).
                self.emit_expr_to_ax(left);
                self.emit_binary_right(*op, right);
            }
            ExprKind::Call { name } => {
                // No-arg call. Result lands in AX; that's exactly where
                // `emit_expr_to_ax` is supposed to leave its result.
                let _ = write!(self.out, "\tcall\tnear ptr _{name}\r\n");
            }
        }
    }

    /// Emit the right-hand side of a binary op, applying it to AX.
    /// Different operators have different shapes:
    ///
    /// - `add`/`sub`/`and`/`or`/`xor`: two-operand `<mnemonic> ax, <src>`.
    /// - `imul`: single-operand `imul <src>` (DX:AX ← AX * src; DX
    ///   discarded for 16-bit int).
    /// - `idiv`: needs `cwd` first (sign-extend AX → DX:AX), then
    ///   single-operand `idiv <src>`. Quotient in AX, remainder in DX.
    /// - `mod`: same as `idiv` plus `mov ax,dx` to surface the remainder.
    /// - shifts: load the right operand's low byte into CL, then
    ///   `shl ax,cl` or `sar ax,cl` (signed `>>` is `sar`, not `shr`).
    fn emit_binary_right(&mut self, op: BinOp, e: &Expr) {
        // Memory operand source. We only need a single string — the bp
        // offset of the local — that the various op-specific paths reuse.
        let src = self.resolve_operand_source(e);
        emit_op_with_source(self.out, op, &src);
    }

    /// Resolve the right operand to a textual asm source operand and
    /// return it. Today either an immediate (constant-foldable) or
    /// `word ptr [bp-N]` for a local. Other shapes (call, nested
    /// non-constant binop) panic — future fixtures will tell us what BCC
    /// does with them.
    fn resolve_operand_source(&self, e: &Expr) -> OperandSource {
        if let Some(v) = try_const_eval(e) {
            return OperandSource::Immediate(v);
        }
        match &e.kind {
            ExprKind::Ident(name) => OperandSource::Local(self.local_offset(name)),
            ExprKind::IntLit(_) => unreachable!("literals fold via try_const_eval"),
            ExprKind::Call { .. } => {
                panic!("call as right operand not yet supported (need to preserve AX)")
            }
            ExprKind::BinOp { .. } => {
                panic!("nested non-constant right operand not yet supported")
            }
        }
    }

}

/// A resolved right-hand operand: either an integer immediate or a local
/// at `[bp-N]`.
enum OperandSource {
    Immediate(u32),
    Local(u16),
}

impl OperandSource {
    /// Format as a `word ptr ...` / immediate that fits into a two-operand
    /// `<mnemonic> ax, <src>` instruction.
    fn word(&self) -> String {
        match self {
            Self::Immediate(v) => v.to_string(),
            Self::Local(off) => format!("word ptr [bp-{off}]"),
        }
    }

    /// Format the *byte* form, used for shift counts (`mov cl, byte ptr ...`).
    /// Immediates use their raw value.
    fn byte(&self) -> String {
        match self {
            Self::Immediate(v) => v.to_string(),
            Self::Local(off) => format!("byte ptr [bp-{off}]"),
        }
    }
}

/// Emit the operator-specific instruction(s) given an already-loaded AX
/// (left operand) and a source string for the right operand. Free
/// function so it doesn't borrow `&mut self`.
fn emit_op_with_source(out: &mut Vec<u8>, op: BinOp, src: &OperandSource) {
    use std::io::Write as _;
    match op {
        BinOp::Add => {
            let _ = write!(out, "\tadd\tax,{}\r\n", src.word());
        }
        BinOp::Sub => {
            let _ = write!(out, "\tsub\tax,{}\r\n", src.word());
        }
        BinOp::BitAnd => {
            let _ = write!(out, "\tand\tax,{}\r\n", src.word());
        }
        BinOp::BitOr => {
            let _ = write!(out, "\tor\tax,{}\r\n", src.word());
        }
        BinOp::BitXor => {
            let _ = write!(out, "\txor\tax,{}\r\n", src.word());
        }
        BinOp::Mul => {
            if matches!(src, OperandSource::Immediate(_)) {
                panic!("imul with immediate not yet supported (80186+ only)");
            }
            let _ = write!(out, "\timul\t{}\r\n", src.word());
        }
        BinOp::Div => {
            if matches!(src, OperandSource::Immediate(_)) {
                panic!("idiv with immediate not supported (no such encoding)");
            }
            // cwd has no operands but still gets the operand-separator
            // tab + empty operand, matching BCC.
            out.extend_from_slice(b"\tcwd\t\r\n");
            let _ = write!(out, "\tidiv\t{}\r\n", src.word());
        }
        BinOp::Mod => {
            if matches!(src, OperandSource::Immediate(_)) {
                panic!("idiv with immediate not supported (no such encoding)");
            }
            out.extend_from_slice(b"\tcwd\t\r\n");
            let _ = write!(out, "\tidiv\t{}\r\n", src.word());
            out.extend_from_slice(b"\tmov\tax,dx\r\n");
        }
        BinOp::Shl | BinOp::Shr => {
            // Load only the low byte of the right operand into CL (BCC
            // reads `byte ptr [bp-N]` even when the local is wider).
            let _ = write!(out, "\tmov\tcl,{}\r\n", src.byte());
            let mnemonic = match op {
                BinOp::Shl => "shl",
                // Signed right shift uses SAR. Unsigned types (when we
                // get them) will need a separate Shr variant.
                BinOp::Shr => "sar",
                _ => unreachable!(),
            };
            let _ = write!(out, "\t{mnemonic}\tax,cl\r\n");
        }
    }
}

impl FunctionEmitter<'_> {
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
