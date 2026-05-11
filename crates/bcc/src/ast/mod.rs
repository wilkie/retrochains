//! AST types. Faithful to source order; every node carries the byte span
//! it covers so codegen can find the original source line(s) that backed
//! it (BCC interleaves source lines as `;` comments — we need the
//! original bytes to reproduce that).

use crate::lex::Span;

/// A whole translation unit (one `.C` file).
#[derive(Debug)]
pub struct Unit {
    pub functions: Vec<Function>,
}

#[derive(Debug)]
pub struct Function {
    pub name: String,
    /// The full byte range of the function definition, from the
    /// `int main…` opening through the closing `}`. Used to compute
    /// which source lines this function spans, which in turn drives the
    /// `;` source-comment emission in the asm.
    pub span: Span,
    pub body: Vec<Stmt>,
}

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum StmtKind {
    Return(Option<Expr>),
    /// `<type> <name> [= <init>];`. For now only `int` and a single
    /// declarator with an optional initializer.
    Declare { ty: Type, name: String, init: Option<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    /// `int` — 16-bit signed under the small memory model.
    Int,
}

impl Type {
    /// Size in bytes for stack-frame allocation. Memory-model-specific
    /// details (far pointers, etc.) will come later.
    #[must_use]
    pub fn size_bytes(self) -> u16 {
        match self {
            Self::Int => 2,
        }
    }
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum ExprKind {
    IntLit(u32),
    Ident(String),
    BinOp { op: BinOp, left: Box<Expr>, right: Box<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
}
