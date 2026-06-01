//! Hand-written recursive-descent parser. Single-pass: each top-level
//! function, once parsed, is handed straight to codegen. The parser owns
//! a token stream and exposes `parse_unit` for the simple "whole file at
//! once" case (which is all the early fixtures need; nothing in
//! single-pass forbids building a one-function-at-a-time variant later).

use std::collections::HashMap;

use crate::ast::{
    BinOp, BitfieldInfo, Expr, ExprKind, Function, Global, LogicalOp, MemberKind, Param,
    Stmt, StmtKind, StructField, SwitchCase, TopLevelRef, Type, UnaryOp, Unit, UpdateOp,
    UpdatePosition,
};
use crate::lex::{Span, Token, TokenKind};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("at byte {offset}: expected {expected}, got {found}")]
    Unexpected { expected: String, found: String, offset: u32 },
    #[error("at byte {offset}: function name must be a plain identifier")]
    NotAnIdent { offset: u32 },
    #[error("at byte {offset}: only `int main(void)` and a `return <int-literal>;` body are supported so far")]
    Unsupported { offset: u32 },
}

#[derive(Debug)]
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Tagged struct definitions seen so far. Looking up `struct point`
    /// as a type returns the recorded `Type::Struct{...}` here.
    structs: HashMap<String, Type>,
    /// typedef aliases. Each entry maps a name to its underlying type
    /// (with structs already resolved). Looking up a name in this
    /// table as a type returns the aliased type — same byte layout
    /// as using the original name (fixture 104).
    typedefs: HashMap<String, Type>,
    /// Static locals hoisted out of function bodies. Each `static`
    /// declaration inside a function adds a synthetic `Global` here;
    /// `parse_unit` appends them after the regular file-scope globals
    /// so they appear in `_DATA` and `GlobalTable` for the rest of
    /// codegen.
    pending_static_locals: Vec<Global>,
    /// Enum constants. Each `enum { ... }` member maps to its integer
    /// value; `parse_primary` folds matching identifiers to `IntLit`
    /// so codegen sees a pure constant and never has to model enum
    /// as a runtime type. Anonymous enums and explicit `= N` initializers
    /// both flow through this table.
    enum_constants: HashMap<String, u32>,
    /// Extra `Stmt`s produced by a single source-level declaration —
    /// specifically the secondary declarators of `int i, j, sum;`.
    /// `parse_stmt` returns the first declarator; the parse-stmt
    /// callers drain this queue and append to the enclosing block.
    pending_extra_stmts: Vec<Stmt>,
    /// Per-function rename map for hoisted static locals. A static
    /// in one function shouldn't collide with the same-named static
    /// in another, so the parser appends a unique suffix to the
    /// global name; this map translates body-level idents.
    /// Reset at the start of every `parse_function`.
    current_static_renames: HashMap<String, String>,
    /// Per-block-scope rename map stack for shadowed locals. When a
    /// nested block declares `int x` that shadows an outer `x`, the
    /// parser appends a unique `@N` suffix and records the rewrite
    /// here; lookups walk the stack innermost-first. Reset at the
    /// start of every `parse_function` to a single empty scope.
    /// Fixtures 2316, 2467, 2258.
    block_scopes: Vec<HashMap<String, String>>,
    /// Monotonic counter to derive unique suffixes for hoisted
    /// statics.
    static_local_counter: u32,
    /// Symbol → type for file-scope variables seen so far in the
    /// source. `parse_global` inserts here; `sizeof <ident>` consults
    /// this (plus `function_locals` for the current function) to fold
    /// to a literal size at parse time. Lookups are exact-match, not
    /// scoped — a redeclaration would overwrite the earlier entry.
    global_types: HashMap<String, Type>,
    /// Symbol → type for the locals + params of the function currently
    /// being parsed. Reset on every `parse_function` entry, updated by
    /// `finish_declare` and by K&R type declarations. Used by `sizeof
    /// <ident>` to look up an operand's type without a separate
    /// type-checker pass.
    function_locals: HashMap<String, Type>,
}

// impl Parser is split across concern modules; each holds its
// own `impl Parser` block. See scripts/codegen_split/.
mod parse_core;
mod parse_decls;
mod parse_exprs;
mod parse_stmts;
mod parse_types;

fn match_update_op(t: &TokenKind) -> Option<UpdateOp> {
    match t {
        TokenKind::PlusPlus => Some(UpdateOp::Inc),
        TokenKind::MinusMinus => Some(UpdateOp::Dec),
        _ => None,
    }
}

/// Is `kind` a parsed expression that can sit on the LHS of an
/// `<expr> = <value>` assignment? Bare identifiers are handled by
/// the dedicated `AssignExpr` branch — this is for the other
/// lvalue shapes that pair with `AssignLvalueExpr`: `*p`, `a[i]`,
/// `p->x`, and the postfix-update wrappers (`*d++` lhs of a
/// strcpy-style copy). Fixtures 3333, 1986, 1808.
fn is_assignable_lvalue(kind: &ExprKind) -> bool {
    matches!(
        kind,
        ExprKind::Deref(_)
            | ExprKind::ArrayIndex { .. }
            | ExprKind::Member { .. }
            | ExprKind::UpdateLvalue { .. }
    )
}

fn match_compound_op(t: &TokenKind) -> Option<BinOp> {
    match t {
        TokenKind::PlusEq => Some(BinOp::Add),
        TokenKind::MinusEq => Some(BinOp::Sub),
        TokenKind::StarEq => Some(BinOp::Mul),
        TokenKind::SlashEq => Some(BinOp::Div),
        TokenKind::PercentEq => Some(BinOp::Mod),
        TokenKind::AmpEq => Some(BinOp::BitAnd),
        TokenKind::PipeEq => Some(BinOp::BitOr),
        TokenKind::CaretEq => Some(BinOp::BitXor),
        TokenKind::ShlEq => Some(BinOp::Shl),
        TokenKind::ShrEq => Some(BinOp::Shr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::Lexer;

    fn parse(src: &str) -> Result<Unit, ParseError> {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        Parser::new(tokens).parse_unit()
    }

    #[test]
    fn fixture_001() {
        let unit = parse("int main(void) { return 0; }\n").unwrap();
        assert_eq!(unit.functions.len(), 1);
        let f = &unit.functions[0];
        assert_eq!(f.name, "main");
        assert_eq!(f.body.as_ref().unwrap().len(), 1);
        let StmtKind::Return(Some(ref e)) = f.body.as_ref().unwrap()[0].kind else { panic!() };
        assert!(matches!(e.kind, ExprKind::IntLit(0)));
    }

    #[test]
    fn fixture_003() {
        let unit = parse("int main(void) { return 42; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body.as_ref().unwrap()[0].kind else { panic!() };
        assert!(matches!(e.kind, ExprKind::IntLit(42)));
    }

    #[test]
    fn fixture_005_binary_plus() {
        let unit = parse("int main(void) { return 1 + 2; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body.as_ref().unwrap()[0].kind else { panic!() };
        let ExprKind::BinOp { op: BinOp::Add, ref left, ref right } = e.kind else { panic!() };
        assert!(matches!(left.kind, ExprKind::IntLit(1)));
        assert!(matches!(right.kind, ExprKind::IntLit(2)));
    }

    #[test]
    fn multiplicative_binds_tighter_than_additive() {
        // `1 + 2 * 3` ≡ `1 + (2 * 3)`.
        let unit = parse("int main(void) { return 1 + 2 * 3; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body.as_ref().unwrap()[0].kind else { panic!() };
        let ExprKind::BinOp { op: BinOp::Add, ref left, ref right } = e.kind else { panic!() };
        assert!(matches!(left.kind, ExprKind::IntLit(1)));
        let ExprKind::BinOp { op: BinOp::Mul, .. } = right.kind else {
            panic!("expected right side to be Mul");
        };
    }

    #[test]
    fn subtraction_parses() {
        let unit = parse("int main(void) { return 9 - 4; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body.as_ref().unwrap()[0].kind else { panic!() };
        assert!(matches!(e.kind, ExprKind::BinOp { op: BinOp::Sub, .. }));
    }

    #[test]
    fn call_parses() {
        let unit = parse("int main(void) { return f(); }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body.as_ref().unwrap()[0].kind else { panic!() };
        let ExprKind::Call { ref name, ref args } = e.kind else { panic!() };
        assert_eq!(name, "f");
        assert!(args.is_empty());
    }

    #[test]
    fn call_with_args_parses() {
        let unit = parse("int main(void) { return f(1, 2 + 3); }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body.as_ref().unwrap()[0].kind else { panic!() };
        let ExprKind::Call { ref args, .. } = e.kind else { panic!() };
        assert_eq!(args.len(), 2);
        assert!(matches!(args[0].kind, ExprKind::IntLit(1)));
        assert!(matches!(args[1].kind, ExprKind::BinOp { op: BinOp::Add, .. }));
    }

    #[test]
    fn function_with_params_parses() {
        let unit = parse("int f(int x, int y) { return x; }\n").unwrap();
        let f = &unit.functions[0];
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.params[0].name, "x");
        assert_eq!(f.params[1].name, "y");
    }

    #[test]
    fn full_precedence_ladder() {
        // `1 | 2 ^ 3 & 4 << 5 + 6 * 7` should parse with `*` tightest
        // and `|` loosest, so the root is BinOp::BitOr.
        let unit = parse("int main(void) { return 1 | 2 ^ 3 & 4 << 5 + 6 * 7; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body.as_ref().unwrap()[0].kind else { panic!() };
        assert!(matches!(e.kind, ExprKind::BinOp { op: BinOp::BitOr, .. }));
    }

    #[test]
    fn shift_binds_below_additive() {
        // `1 + 2 << 3` ≡ `(1 + 2) << 3` — additive is tighter than shift.
        let unit = parse("int main(void) { return 1 + 2 << 3; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body.as_ref().unwrap()[0].kind else { panic!() };
        let ExprKind::BinOp { op: BinOp::Shl, ref left, .. } = e.kind else { panic!() };
        assert!(matches!(left.kind, ExprKind::BinOp { op: BinOp::Add, .. }));
    }

    #[test]
    fn additive_is_left_associative() {
        // `1 + 2 + 3` → ((1 + 2) + 3)
        let unit = parse("int main(void) { return 1 + 2 + 3; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body.as_ref().unwrap()[0].kind else { panic!() };
        let ExprKind::BinOp { ref left, ref right, .. } = e.kind else { panic!() };
        assert!(matches!(right.kind, ExprKind::IntLit(3)));
        let ExprKind::BinOp { left: ref ll, right: ref lr, .. } = left.kind else { panic!() };
        assert!(matches!(ll.kind, ExprKind::IntLit(1)));
        assert!(matches!(lr.kind, ExprKind::IntLit(2)));
    }
}
