//! Hand-written recursive-descent parser. Single-pass: each top-level
//! function, once parsed, is handed straight to codegen. The parser owns
//! a token stream and exposes `parse_unit` for the simple "whole file at
//! once" case (which is all the early fixtures need; nothing in
//! single-pass forbids building a one-function-at-a-time variant later).

use crate::ast::{
    BinOp, Expr, ExprKind, Function, LogicalOp, Param, Stmt, StmtKind, SwitchCase, Type, UnaryOp,
    Unit, UpdateOp, UpdatePosition,
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
        // Return type is always `int` today; we'll learn about non-int
        // return types from a fixture.
        self.expect(&TokenKind::KwInt)?;
        let name_tok = self.bump();
        let TokenKind::Ident(name) = &name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        let name = name.clone();
        self.expect(&TokenKind::LParen)?;
        let params = self.parse_param_list()?;
        self.expect(&TokenKind::RParen)?;
        self.expect(&TokenKind::LBrace)?;

        let mut body = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            body.push(self.parse_stmt()?);
        }
        let close = self.expect(&TokenKind::RBrace)?;
        let span = Span::new(start, close.span.end);
        Ok(Function { name, params, span, body })
    }

    /// Parameter list inside the `(...)` of a function definition.
    /// Two shapes are recognized:
    ///
    /// - `void` — the C spelling for "no parameters". Returns empty.
    /// - `<type> <name> (, <type> <name>)*` — one or more typed params.
    ///
    /// The caller is responsible for the surrounding parens.
    fn parse_param_list(&mut self) -> Result<Vec<Param>, ParseError> {
        if matches!(self.peek().kind, TokenKind::KwVoid) {
            self.bump();
            return Ok(Vec::new());
        }
        let mut params = Vec::new();
        loop {
            let ty_tok = self.bump();
            let ty = match ty_tok.kind {
                TokenKind::KwInt => Type::Int,
                TokenKind::KwChar => Type::Char,
                _ => return Err(ParseError::Unexpected {
                    expected: "parameter type".to_owned(),
                    found: ty_tok.kind.describe().to_owned(),
                    offset: ty_tok.span.start,
                }),
            };
            let name_tok = self.bump();
            let TokenKind::Ident(name) = name_tok.kind else {
                return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
            };
            params.push(Param { name, ty });
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        Ok(params)
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
            TokenKind::KwInt | TokenKind::KwChar => self.parse_declare(start),
            TokenKind::KwIf => self.parse_if(),
            TokenKind::KwWhile => self.parse_while(),
            TokenKind::KwDo => self.parse_do_while(),
            TokenKind::KwFor => self.parse_for(),
            TokenKind::KwSwitch => self.parse_switch(),
            TokenKind::KwBreak => {
                let tok = self.bump();
                let semi = self.expect(&TokenKind::Semicolon)?;
                Ok(Stmt {
                    kind: StmtKind::Break,
                    span: Span::new(tok.span.start, semi.span.end),
                })
            }
            TokenKind::KwContinue => {
                let tok = self.bump();
                let semi = self.expect(&TokenKind::Semicolon)?;
                Ok(Stmt {
                    kind: StmtKind::Continue,
                    span: Span::new(tok.span.start, semi.span.end),
                })
            }
            // `<ident> = …` is an assignment; `<ident> +=` (and the
            // other compound-assignment ops) becomes CompoundAssign.
            // Otherwise the line is an expression statement, or — for
            // lvalues other than a bare ident — an assignment to that
            // lvalue.
            TokenKind::Ident(_)
                if matches!(self.peek_n(1).kind, TokenKind::Equals)
                    || match_compound_op(&self.peek_n(1).kind).is_some() =>
            {
                let ident_tok = self.bump();
                let TokenKind::Ident(name) = ident_tok.kind else { unreachable!() };
                if let Some(op) = match_compound_op(&self.peek().kind) {
                    self.bump();
                    let value = self.parse_expr()?;
                    let semi = self.expect(&TokenKind::Semicolon)?;
                    Ok(Stmt {
                        kind: StmtKind::CompoundAssign { name, op, value },
                        span: Span::new(start, semi.span.end),
                    })
                } else {
                    self.expect(&TokenKind::Equals)?;
                    let value = self.parse_expr()?;
                    let semi = self.expect(&TokenKind::Semicolon)?;
                    Ok(Stmt {
                        kind: StmtKind::Assign { name, value },
                        span: Span::new(start, semi.span.end),
                    })
                }
            }
            _ => self.parse_expr_or_lvalue_assign(start),
        }
    }

    /// Either a plain expression statement or an assignment whose
    /// LHS is a non-ident lvalue (`*p = v;`, `a[i] = v;`).
    ///
    /// We get here when `parse_stmt`'s prefix dispatch fell through —
    /// the next tokens don't start a `Return` / `Declare` / `If` /
    /// loop / `Break` / `Continue` / `Switch`, and they aren't a bare
    /// `<ident> =` either. Bare-ident assignment got its own path
    /// because it predates the lvalue notion and lots of code still
    /// builds `StmtKind::Assign { name, value }` directly; we route
    /// only the new lvalue shapes through here.
    fn parse_expr_or_lvalue_assign(&mut self, start: u32) -> Result<Stmt, ParseError> {
        let expr = self.parse_expr()?;
        if !matches!(self.peek().kind, TokenKind::Equals) {
            // Plain expression statement.
            let semi = self.expect(&TokenKind::Semicolon)?;
            return Ok(Stmt {
                kind: StmtKind::ExprStmt(expr),
                span: Span::new(start, semi.span.end),
            });
        }
        // Assignment. Validate the parsed expression is a kind we
        // know how to assign to.
        self.bump(); // `=`
        let value = self.parse_expr()?;
        let semi = self.expect(&TokenKind::Semicolon)?;
        let span = Span::new(start, semi.span.end);
        let kind = match expr.kind {
            ExprKind::Deref(ptr) => StmtKind::DerefAssign { target: *ptr, value },
            ExprKind::ArrayIndex { array, index } => StmtKind::ArrayAssign {
                array,
                index: *index,
                value,
            },
            ExprKind::Ident(name) => StmtKind::Assign { name, value },
            _ => {
                return Err(ParseError::Unsupported { offset: expr.span.start });
            }
        };
        Ok(Stmt { kind, span })
    }

    /// `<type-base> <pointer-stars>* <name> ('[' <int> ']')? [= <init>] ;`.
    /// Caller has already peeked the type keyword (int or char).
    ///
    /// Shapes accepted today:
    /// - `int x;` — plain int local
    /// - `int *p;` — pointer-to-int (zero or more `*`s wrap the type)
    /// - `int a[3];` — array; size must be a non-zero int literal
    /// - `char *s = ...;` / `int a[3] = ...;` — initializer not yet
    ///   widely supported in codegen; we'll parse and let the next
    ///   layer reject.
    fn parse_declare(&mut self, start: u32) -> Result<Stmt, ParseError> {
        let ty_tok = self.bump();
        let mut ty = match ty_tok.kind {
            TokenKind::KwInt => Type::Int,
            TokenKind::KwChar => Type::Char,
            _ => unreachable!("caller peeked an int/char keyword"),
        };
        // Pointer stars wrap the base type: `int **pp` is `Pointer(Pointer(Int))`.
        while matches!(self.peek().kind, TokenKind::Star) {
            self.bump();
            ty = Type::Pointer(Box::new(ty));
        }
        let name_tok = self.bump();
        let TokenKind::Ident(name) = &name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        let name = name.clone();
        // Array suffix: `[<int-literal>]`. C also allows `[]` for
        // function parameters (decays to pointer) — we don't have
        // a fixture for that yet.
        if matches!(self.peek().kind, TokenKind::LBracket) {
            self.bump();
            let size_tok = self.bump();
            let TokenKind::IntLit(len) = size_tok.kind else {
                return Err(ParseError::Unexpected {
                    expected: "array size (integer literal)".to_owned(),
                    found: size_tok.kind.describe().to_owned(),
                    offset: size_tok.span.start,
                });
            };
            self.expect(&TokenKind::RBracket)?;
            ty = Type::Array { elem: Box::new(ty), len };
        }
        let init = if matches!(self.peek().kind, TokenKind::Equals) {
            self.bump();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let semi = self.expect(&TokenKind::Semicolon)?;
        Ok(Stmt {
            kind: StmtKind::Declare { ty, name, init },
            span: Span::new(start, semi.span.end),
        })
    }

    /// `while ( <cond> ) <branch>`. Same branch shape as `if`.
    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let while_tok = self.expect(&TokenKind::KwWhile)?;
        self.expect(&TokenKind::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(&TokenKind::RParen)?;
        let body = self.parse_branch()?;
        let end = body.last().map_or(while_tok.span.end, |s| s.span.end);
        Ok(Stmt {
            kind: StmtKind::While { cond, body },
            span: Span::new(while_tok.span.start, end),
        })
    }

    /// `do <branch> while ( <cond> ) ;`.
    fn parse_do_while(&mut self) -> Result<Stmt, ParseError> {
        let do_tok = self.expect(&TokenKind::KwDo)?;
        let body = self.parse_branch()?;
        self.expect(&TokenKind::KwWhile)?;
        self.expect(&TokenKind::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(&TokenKind::RParen)?;
        let semi = self.expect(&TokenKind::Semicolon)?;
        Ok(Stmt {
            kind: StmtKind::DoWhile { body, cond },
            span: Span::new(do_tok.span.start, semi.span.end),
        })
    }

    /// `for ( <init>? ; <cond>? ; <step>? ) <branch>`. Each of
    /// init/cond/step is an optional expression. (C99 declarations
    /// in init are not yet supported — fixture 061 declares its
    /// loop variable outside the `for`.)
    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let for_tok = self.expect(&TokenKind::KwFor)?;
        self.expect(&TokenKind::LParen)?;
        let init = if matches!(self.peek().kind, TokenKind::Semicolon) {
            None
        } else {
            Some(self.parse_for_clause_expr()?)
        };
        self.expect(&TokenKind::Semicolon)?;
        let cond = if matches!(self.peek().kind, TokenKind::Semicolon) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect(&TokenKind::Semicolon)?;
        let step = if matches!(self.peek().kind, TokenKind::RParen) {
            None
        } else {
            Some(self.parse_for_clause_expr()?)
        };
        self.expect(&TokenKind::RParen)?;
        let body = self.parse_branch()?;
        let end = body.last().map_or(for_tok.span.end, |s| s.span.end);
        Ok(Stmt {
            kind: StmtKind::For { init, cond, step, body },
            span: Span::new(for_tok.span.start, end),
        })
    }

    /// Parse a for-loop init/step clause. We accept `<ident> = <rhs>`
    /// (the common form) as an assignment expression; otherwise the
    /// clause is any normal expression (`++i`, function call, …).
    /// Pure C also allows the init clause to be a declaration, but
    /// that's a separate slice.
    fn parse_for_clause_expr(&mut self) -> Result<Expr, ParseError> {
        if matches!(self.peek().kind, TokenKind::Ident(_))
            && matches!(self.peek_n(1).kind, TokenKind::Equals)
        {
            let ident_tok = self.bump();
            let TokenKind::Ident(name) = ident_tok.kind else { unreachable!() };
            self.expect(&TokenKind::Equals)?;
            let rhs = self.parse_expr()?;
            let span = Span::new(ident_tok.span.start, rhs.span.end);
            return Ok(Expr {
                kind: ExprKind::AssignExpr { target: name, value: Box::new(rhs) },
                span,
            });
        }
        self.parse_expr()
    }

    /// `switch ( <expr> ) { (case <int>: <stmts> | default: <stmts>)* }`.
    /// The case arms are kept in source order. Each arm's body extends
    /// until the next `case` / `default` / `}` — `break;` is just a
    /// regular statement inside the body, not a separator. We require
    /// the brace; BCC may permit a single statement, but no fixture
    /// has shown that and the grammar is cleaner this way.
    fn parse_switch(&mut self) -> Result<Stmt, ParseError> {
        let switch_tok = self.expect(&TokenKind::KwSwitch)?;
        self.expect(&TokenKind::LParen)?;
        let scrutinee = self.parse_expr()?;
        self.expect(&TokenKind::RParen)?;
        self.expect(&TokenKind::LBrace)?;
        let mut cases: Vec<SwitchCase> = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let (value, head_start) = match self.peek().kind {
                TokenKind::KwCase => {
                    let kw = self.bump();
                    let int_tok = self.bump();
                    let TokenKind::IntLit(v) = int_tok.kind else {
                        return Err(ParseError::Unexpected {
                            expected: "integer literal in `case`".to_owned(),
                            found: int_tok.kind.describe().to_owned(),
                            offset: int_tok.span.start,
                        });
                    };
                    (Some(v), kw.span.start)
                }
                TokenKind::KwDefault => {
                    let kw = self.bump();
                    (None, kw.span.start)
                }
                _ => {
                    let t = self.peek();
                    return Err(ParseError::Unexpected {
                        expected: "`case`, `default`, or `}`".to_owned(),
                        found: t.kind.describe().to_owned(),
                        offset: t.span.start,
                    });
                }
            };
            let colon = self.expect(&TokenKind::Colon)?;
            let mut body = Vec::new();
            while !matches!(
                self.peek().kind,
                TokenKind::KwCase | TokenKind::KwDefault | TokenKind::RBrace | TokenKind::Eof
            ) {
                body.push(self.parse_stmt()?);
            }
            cases.push(SwitchCase {
                value,
                span: Span::new(head_start, colon.span.end),
                body,
            });
        }
        let close = self.expect(&TokenKind::RBrace)?;
        Ok(Stmt {
            kind: StmtKind::Switch { scrutinee, cases },
            span: Span::new(switch_tok.span.start, close.span.end),
        })
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
        // Precedence ladder, lowest at the top: || < && < | < ^ < & <
        // == != < relational < shift < additive < multiplicative <
        // unary < atom.
        self.parse_logor()
    }

    fn parse_logor(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_logand()?;
        while matches!(self.peek().kind, TokenKind::PipePipe) {
            self.bump();
            let right = self.parse_logand()?;
            let span = Span::new(left.span.start, right.span.end);
            left = Expr {
                kind: ExprKind::Logical {
                    op: LogicalOp::Or,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_logand(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_bitor()?;
        while matches!(self.peek().kind, TokenKind::AmpAmp) {
            self.bump();
            let right = self.parse_bitor()?;
            let span = Span::new(left.span.start, right.span.end);
            left = Expr {
                kind: ExprKind::Logical {
                    op: LogicalOp::And,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            };
        }
        Ok(left)
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
        self.left_assoc(Self::parse_unary, |t| match t {
            TokenKind::Star => Some(BinOp::Mul),
            TokenKind::Slash => Some(BinOp::Div),
            TokenKind::Percent => Some(BinOp::Mod),
            _ => None,
        })
    }

    /// Prefix unary operators. Higher precedence than multiplicative;
    /// right-associative.
    ///
    /// Handles `-e`/`!e`/`~e` (arithmetic, logical, bitwise) plus
    /// `++name`/`--name` (pre-increment / pre-decrement). The latter
    /// require the operand to be a plain identifier today — no compound
    /// LHS like `*p++`.
    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if let Some(update_op) = match_update_op(&self.peek().kind) {
            let op_tok = self.bump();
            let name_tok = self.bump();
            let TokenKind::Ident(name) = name_tok.kind else {
                return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
            };
            let span = Span::new(op_tok.span.start, name_tok.span.end);
            return Ok(Expr {
                kind: ExprKind::Update {
                    target: name,
                    op: update_op,
                    position: UpdatePosition::Pre,
                },
                span,
            });
        }
        // Address-of: only `&<ident>` is supported today. The more
        // general `&<lvalue>` doesn't appear in fixtures.
        if matches!(self.peek().kind, TokenKind::Ampersand) {
            let amp = self.bump();
            let name_tok = self.bump();
            let TokenKind::Ident(name) = name_tok.kind else {
                return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
            };
            let span = Span::new(amp.span.start, name_tok.span.end);
            return Ok(Expr {
                kind: ExprKind::AddressOf(name),
                span,
            });
        }
        // Pointer dereference: `*<unary>`. Lexically `*` is also the
        // multiplication operator; the precedence layering puts unary
        // tighter than multiplicative, so prefix `*` is unambiguous.
        if matches!(self.peek().kind, TokenKind::Star) {
            let star = self.bump();
            let operand = self.parse_unary()?;
            let span = Span::new(star.span.start, operand.span.end);
            return Ok(Expr {
                kind: ExprKind::Deref(Box::new(operand)),
                span,
            });
        }
        let op = match self.peek().kind {
            TokenKind::Minus => UnaryOp::Neg,
            TokenKind::Bang => UnaryOp::Not,
            TokenKind::Tilde => UnaryOp::BitNot,
            _ => return self.parse_atom(),
        };
        let op_tok = self.bump();
        let operand = self.parse_unary()?;
        let span = Span::new(op_tok.span.start, operand.span.end);
        Ok(Expr {
            kind: ExprKind::Unary { op, operand: Box::new(operand) },
            span,
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
                // Postfix `()` makes it a function call.
                if matches!(self.peek().kind, TokenKind::LParen) {
                    self.bump();
                    let mut args = Vec::new();
                    if !matches!(self.peek().kind, TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if matches!(self.peek().kind, TokenKind::Comma) {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    let close = self.expect(&TokenKind::RParen)?;
                    Ok(Expr {
                        kind: ExprKind::Call { name: name.clone(), args },
                        span: Span::new(tok.span.start, close.span.end),
                    })
                } else if let Some(update_op) = match_update_op(&self.peek().kind) {
                    // Postfix `name++` or `name--`.
                    let op_tok = self.bump();
                    let span = Span::new(tok.span.start, op_tok.span.end);
                    Ok(Expr {
                        kind: ExprKind::Update {
                            target: name.clone(),
                            op: update_op,
                            position: UpdatePosition::Post,
                        },
                        span,
                    })
                } else if matches!(self.peek().kind, TokenKind::LBracket) {
                    // Array index `name[<expr>]`. Only one level today
                    // — chained `a[i][j]` would need the indexed value
                    // to itself be an array/pointer, which our type
                    // system doesn't fully model.
                    self.bump();
                    let index = self.parse_expr()?;
                    let close = self.expect(&TokenKind::RBracket)?;
                    Ok(Expr {
                        kind: ExprKind::ArrayIndex {
                            array: name.clone(),
                            index: Box::new(index),
                        },
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
        self.peek_n(0)
    }

    /// Look `n` tokens ahead. Used for the 2-token lookahead in
    /// `parse_stmt` to disambiguate `<ident> =` (assignment) from
    /// `<ident> ++` (expression statement).
    fn peek_n(&self, n: usize) -> &Token {
        // `parse_unit` exits before EOF; once we run off the end, return
        // the last token (always `Eof` after `Lexer::tokenize`).
        self.tokens.get(self.pos + n).unwrap_or_else(|| {
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

fn match_update_op(t: &TokenKind) -> Option<UpdateOp> {
    match t {
        TokenKind::PlusPlus => Some(UpdateOp::Inc),
        TokenKind::MinusMinus => Some(UpdateOp::Dec),
        _ => None,
    }
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
        let ExprKind::Call { ref name, ref args } = e.kind else { panic!() };
        assert_eq!(name, "f");
        assert!(args.is_empty());
    }

    #[test]
    fn call_with_args_parses() {
        let unit = parse("int main(void) { return f(1, 2 + 3); }\n").unwrap();
        let StmtKind::Return(Some(ref e)) = unit.functions[0].body[0].kind else { panic!() };
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
