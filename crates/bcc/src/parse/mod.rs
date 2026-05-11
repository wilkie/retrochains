//! Hand-written recursive-descent parser. Single-pass: each top-level
//! function, once parsed, is handed straight to codegen. The parser owns
//! a token stream and exposes `parse_unit` for the simple "whole file at
//! once" case (which is all the early fixtures need; nothing in
//! single-pass forbids building a one-function-at-a-time variant later).

use crate::ast::{BinOp, Expr, ExprKind, Function, Stmt, StmtKind, Type, Unit};
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
}

impl Parser {
    #[must_use]
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Parse a whole translation unit.
    ///
    /// # Errors
    /// Returns [`ParseError`] on the first unrecognized construct.
    pub fn parse_unit(&mut self) -> Result<Unit, ParseError> {
        let mut functions = Vec::new();
        while !self.at_eof() {
            functions.push(self.parse_function()?);
        }
        Ok(Unit { functions })
    }

    fn parse_function(&mut self) -> Result<Function, ParseError> {
        let start = self.peek().span.start;
        // Accept only `int <ident>(void) { … }` for now.
        self.expect(&TokenKind::KwInt)?;
        let name_tok = self.bump();
        let TokenKind::Ident(name) = &name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        let name = name.clone();
        self.expect(&TokenKind::LParen)?;
        self.expect(&TokenKind::KwVoid)?;
        self.expect(&TokenKind::RParen)?;
        self.expect(&TokenKind::LBrace)?;

        let mut body = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            body.push(self.parse_stmt()?);
        }
        let close = self.expect(&TokenKind::RBrace)?;
        let span = Span::new(start, close.span.end);
        Ok(Function { name, span, body })
    }

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        let start = self.peek().span.start;
        match self.peek().kind {
            TokenKind::KwReturn => {
                self.bump();
                let value = if matches!(self.peek().kind, TokenKind::Semicolon) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                let semi = self.expect(&TokenKind::Semicolon)?;
                Ok(Stmt {
                    kind: StmtKind::Return(value),
                    span: Span::new(start, semi.span.end),
                })
            }
            TokenKind::KwInt => {
                // `int <name> [= <init>];`
                self.bump();
                let name_tok = self.bump();
                let TokenKind::Ident(name) = &name_tok.kind else {
                    return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
                };
                let name = name.clone();
                let init = if matches!(self.peek().kind, TokenKind::Equals) {
                    self.bump();
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                let semi = self.expect(&TokenKind::Semicolon)?;
                Ok(Stmt {
                    kind: StmtKind::Declare { ty: Type::Int, name, init },
                    span: Span::new(start, semi.span.end),
                })
            }
            TokenKind::KwIf => self.parse_if(),
            _ => Err(ParseError::Unsupported { offset: start }),
        }
    }

    /// `if ( <expr> ) <branch> [else <branch>]`. A branch is either
    /// a single statement or a brace-delimited block.
    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        let if_tok = self.expect(&TokenKind::KwIf)?;
        self.expect(&TokenKind::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(&TokenKind::RParen)?;
        let then_branch = self.parse_branch()?;
        let else_branch = if matches!(self.peek().kind, TokenKind::KwElse) {
            self.bump();
            Some(self.parse_branch()?)
        } else {
            None
        };
        let end = else_branch
            .as_ref()
            .and_then(|b| b.last())
            .or_else(|| then_branch.last())
            .map_or(if_tok.span.end, |s| s.span.end);
        Ok(Stmt {
            kind: StmtKind::If { cond, then_branch, else_branch },
            span: Span::new(if_tok.span.start, end),
        })
    }

    fn parse_branch(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if matches!(self.peek().kind, TokenKind::LBrace) {
            self.bump();
            let mut body = Vec::new();
            while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
                body.push(self.parse_stmt()?);
            }
            self.expect(&TokenKind::RBrace)?;
            Ok(body)
        } else {
            Ok(vec![self.parse_stmt()?])
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        // Precedence ladder, lowest precedence at the top. We cover what
        // fixtures up through 018 demand: bitwise OR < XOR < AND < shift
        // < additive < multiplicative < atom. Comparison and logical
        // operators (and unary) get inserted as fixtures introduce them.
        self.parse_bitor()
    }

    fn parse_bitor(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_bitxor, |t| {
            matches!(t, TokenKind::Pipe).then_some(BinOp::BitOr)
        })
    }

    fn parse_bitxor(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_bitand, |t| {
            matches!(t, TokenKind::Caret).then_some(BinOp::BitXor)
        })
    }

    fn parse_bitand(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_equality, |t| {
            matches!(t, TokenKind::Ampersand).then_some(BinOp::BitAnd)
        })
    }

    fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_relational, |t| match t {
            TokenKind::EqEq => Some(BinOp::Eq),
            TokenKind::BangEq => Some(BinOp::Ne),
            _ => None,
        })
    }

    fn parse_relational(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_shift, |t| match t {
            TokenKind::Lt => Some(BinOp::Lt),
            TokenKind::Le => Some(BinOp::Le),
            TokenKind::Gt => Some(BinOp::Gt),
            TokenKind::Ge => Some(BinOp::Ge),
            _ => None,
        })
    }

    fn parse_shift(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_additive, |t| match t {
            TokenKind::ShiftLeft => Some(BinOp::Shl),
            TokenKind::ShiftRight => Some(BinOp::Shr),
            _ => None,
        })
    }

    fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_multiplicative, |t| match t {
            TokenKind::Plus => Some(BinOp::Add),
            TokenKind::Minus => Some(BinOp::Sub),
            _ => None,
        })
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_atom, |t| match t {
            TokenKind::Star => Some(BinOp::Mul),
            TokenKind::Slash => Some(BinOp::Div),
            TokenKind::Percent => Some(BinOp::Mod),
            _ => None,
        })
    }

    /// One left-associative precedence level: parses `<sub> (<op> <sub>)*`
    /// where `match_op` decides which token kinds at this level
    /// qualify as an operator (and which `BinOp` they map to).
    fn left_assoc<F, M>(&mut self, sub: F, mut match_op: M) -> Result<Expr, ParseError>
    where
        F: Fn(&mut Self) -> Result<Expr, ParseError>,
        M: FnMut(&TokenKind) -> Option<BinOp>,
    {
        let mut left = sub(self)?;
        while let Some(op) = match_op(&self.peek().kind) {
            self.bump();
            let right = sub(self)?;
            let span = Span::new(left.span.start, right.span.end);
            left = Expr {
                kind: ExprKind::BinOp { op, left: Box::new(left), right: Box::new(right) },
                span,
            };
        }
        Ok(left)
    }

    fn parse_atom(&mut self) -> Result<Expr, ParseError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::IntLit(v) => Ok(Expr { kind: ExprKind::IntLit(v), span: tok.span }),
            TokenKind::Ident(ref name) => {
                // Postfix `()` makes it a function call. Args land later.
                if matches!(self.peek().kind, TokenKind::LParen) {
                    self.bump();
                    let close = self.expect(&TokenKind::RParen)?;
                    Ok(Expr {
                        kind: ExprKind::Call { name: name.clone() },
                        span: Span::new(tok.span.start, close.span.end),
                    })
                } else {
                    Ok(Expr { kind: ExprKind::Ident(name.clone()), span: tok.span })
                }
            }
            _ => Err(ParseError::Unsupported { offset: tok.span.start }),
        }
    }

    // ----- tiny utilities -----------------------------------------------

    fn peek(&self) -> &Token {
        // `parse_unit` exits before EOF; once we run off the end, return
        // the last token (always `Eof` after `Lexer::tokenize`).
        self.tokens.get(self.pos).unwrap_or_else(|| {
            self.tokens.last().expect("lexer always emits at least an EOF token")
        })
    }

    fn bump(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    fn expect(&mut self, want: &TokenKind) -> Result<Token, ParseError> {
        let cur = self.peek();
        if std::mem::discriminant(&cur.kind) == std::mem::discriminant(want) {
            Ok(self.bump())
        } else {
            Err(ParseError::Unexpected {
                expected: want.describe().to_owned(),
                found: cur.kind.describe().to_owned(),
                offset: cur.span.start,
            })
        }
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
        assert_eq!(f.body.len(), 1);
        let StmtKind::Return(Some(ref e)) = f.body[0].kind else { panic!() };
        assert!(matches!(e.kind, ExprKind::IntLit(0)));
    }

    #[test]
    fn fixture_003() {
        let unit = parse("int main(void) { return 42; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body[0].kind else { panic!() };
        assert!(matches!(e.kind, ExprKind::IntLit(42)));
    }

    #[test]
    fn fixture_005_binary_plus() {
        let unit = parse("int main(void) { return 1 + 2; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body[0].kind else { panic!() };
        let ExprKind::BinOp { op: BinOp::Add, ref left, ref right } = e.kind else { panic!() };
        assert!(matches!(left.kind, ExprKind::IntLit(1)));
        assert!(matches!(right.kind, ExprKind::IntLit(2)));
    }

    #[test]
    fn multiplicative_binds_tighter_than_additive() {
        // `1 + 2 * 3` ≡ `1 + (2 * 3)`.
        let unit = parse("int main(void) { return 1 + 2 * 3; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body[0].kind else { panic!() };
        let ExprKind::BinOp { op: BinOp::Add, ref left, ref right } = e.kind else { panic!() };
        assert!(matches!(left.kind, ExprKind::IntLit(1)));
        let ExprKind::BinOp { op: BinOp::Mul, .. } = right.kind else {
            panic!("expected right side to be Mul");
        };
    }

    #[test]
    fn subtraction_parses() {
        let unit = parse("int main(void) { return 9 - 4; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body[0].kind else { panic!() };
        assert!(matches!(e.kind, ExprKind::BinOp { op: BinOp::Sub, .. }));
    }

    #[test]
    fn call_parses() {
        let unit = parse("int main(void) { return f(); }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body[0].kind else { panic!() };
        let ExprKind::Call { ref name } = e.kind else { panic!() };
        assert_eq!(name, "f");
    }

    #[test]
    fn full_precedence_ladder() {
        // `1 | 2 ^ 3 & 4 << 5 + 6 * 7` should parse with `*` tightest
        // and `|` loosest, so the root is BinOp::BitOr.
        let unit = parse("int main(void) { return 1 | 2 ^ 3 & 4 << 5 + 6 * 7; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body[0].kind else { panic!() };
        assert!(matches!(e.kind, ExprKind::BinOp { op: BinOp::BitOr, .. }));
    }

    #[test]
    fn shift_binds_below_additive() {
        // `1 + 2 << 3` ≡ `(1 + 2) << 3` — additive is tighter than shift.
        let unit = parse("int main(void) { return 1 + 2 << 3; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body[0].kind else { panic!() };
        let ExprKind::BinOp { op: BinOp::Shl, ref left, .. } = e.kind else { panic!() };
        assert!(matches!(left.kind, ExprKind::BinOp { op: BinOp::Add, .. }));
    }

    #[test]
    fn additive_is_left_associative() {
        // `1 + 2 + 3` → ((1 + 2) + 3)
        let unit = parse("int main(void) { return 1 + 2 + 3; }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body[0].kind else { panic!() };
        let ExprKind::BinOp { ref left, ref right, .. } = e.kind else { panic!() };
        assert!(matches!(right.kind, ExprKind::IntLit(3)));
        let ExprKind::BinOp { left: ref ll, right: ref lr, .. } = left.kind else { panic!() };
        assert!(matches!(ll.kind, ExprKind::IntLit(1)));
        assert!(matches!(lr.kind, ExprKind::IntLit(2)));
    }
}
