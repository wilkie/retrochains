use super::*;

impl Parser {
    pub(crate) fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        let start = self.peek().span.start;
        // Empty statement (`;`). Produces no asm. Used as a placeholder
        // body in `for(init; cond; step) ;` (fixture 522).
        if matches!(self.peek().kind, TokenKind::Semicolon) {
            let semi = self.bump();
            return Ok(Stmt {
                kind: StmtKind::Empty,
                span: Span::new(start, semi.span.end),
            });
        }
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
            TokenKind::KwInt
            | TokenKind::KwChar
            | TokenKind::KwVoid
            | TokenKind::KwStruct
            | TokenKind::KwUnion
            | TokenKind::KwUnsigned
            | TokenKind::KwLong
            | TokenKind::KwSigned
            | TokenKind::KwFloat
            | TokenKind::KwDouble
            | TokenKind::KwConst
            | TokenKind::KwVolatile
            | TokenKind::KwRegister
            | TokenKind::KwEnum => self.parse_declare(start),
            TokenKind::KwStatic => self.parse_declare(start),
            TokenKind::Ident(ref name) if self.typedefs.contains_key(name) => {
                self.parse_declare(start)
            }
            TokenKind::LBrace => {
                // Bare `{ ... }` block at statement position.
                // Parses the inner statements like a function body
                // but wraps them in a Block node so the locals
                // layout can scope decls and reuse slots across
                // sibling blocks. Fixtures 1743, 1966-1969, 3014.
                let lbrace = self.bump();
                // Push a new lexical scope so inner declarations
                // can shadow outer names via parse-time renaming.
                // Fixtures 2467, 2258, 2316.
                self.block_scopes.push(HashMap::new());
                let mut stmts = Vec::new();
                while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
                    stmts.push(self.parse_stmt()?);
                }
                self.block_scopes.pop();
                let rbrace = self.expect(&TokenKind::RBrace)?;
                Ok(Stmt {
                    kind: StmtKind::Block(stmts),
                    span: Span::new(lbrace.span.start, rbrace.span.end),
                })
            }
            TokenKind::KwAsm => {
                // Inline assembly. Two shapes (the lexer normalizes
                // both): `asm { <body> }` emits `KwAsm LBrace
                // AsmBody RBrace`; `asm <line>;` emits
                // `KwAsm AsmBody Semicolon`. Either way we eat the
                // KwAsm, capture the AsmBody string, and consume
                // whichever closing token the lexer emitted.
                // Fixtures 2303 / 2304 / 2120 (block), 2119 /
                // 2122 (statement).
                let kw = self.bump();
                let is_block = matches!(self.peek().kind, TokenKind::LBrace);
                if is_block {
                    self.bump();
                }
                let body_tok = self.bump();
                let TokenKind::AsmBody(body) = body_tok.kind else {
                    return Err(ParseError::Unexpected {
                        expected: "asm body".to_owned(),
                        found: body_tok.kind.describe().to_owned(),
                        offset: body_tok.span.start,
                    });
                };
                let end_off = if is_block {
                    let rbrace = self.expect(&TokenKind::RBrace)?;
                    rbrace.span.end
                } else if matches!(self.peek().kind, TokenKind::Semicolon) {
                    let semi = self.bump();
                    semi.span.end
                } else {
                    body_tok.span.end
                };
                Ok(Stmt {
                    kind: StmtKind::Asm { body },
                    span: Span::new(kw.span.start, end_off),
                })
            }
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
            TokenKind::KwGoto => {
                let tok = self.bump();
                let name_tok = self.bump();
                let TokenKind::Ident(label) = name_tok.kind else {
                    return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
                };
                let semi = self.expect(&TokenKind::Semicolon)?;
                Ok(Stmt {
                    kind: StmtKind::Goto { label },
                    span: Span::new(tok.span.start, semi.span.end),
                })
            }
            // `<name>:` — label statement. Distinguished from the
            // bare-ident assignment / expression cases by a `:`
            // following the identifier (rather than `=`, `(`, `[`,
            // operator, etc.). Fixture 434.
            TokenKind::Ident(_) if matches!(self.peek_n(1).kind, TokenKind::Colon) => {
                let name_tok = self.bump();
                let TokenKind::Ident(name) = name_tok.kind else { unreachable!() };
                let colon = self.bump();
                Ok(Stmt {
                    kind: StmtKind::Label { name },
                    span: Span::new(name_tok.span.start, colon.span.end),
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
                let name = self
                    .lookup_block_rename(&name)
                    .or_else(|| self.current_static_renames.get(&name).cloned())
                    .unwrap_or(name);
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
                    // Allow `a = b = c = 5;` — the RHS itself can be an
                    // assignment expression. `parse_for_clause_expr`
                    // already handles `<ident> = <rhs>` recursively
                    // (right-associative). Fixture 500.
                    let value = self.parse_for_clause_expr()?;
                    let semi = self.expect(&TokenKind::Semicolon)?;
                    Ok(Stmt {
                        kind: StmtKind::Assign { name, value },
                        span: Span::new(start, semi.span.end),
                    })
                }
            }
            // Statement-position prefix `++lv;` / `--lv;` for an
            // lvalue that isn't a bare ident. The `++ident;` form is
            // handled by parse_unary's existing path; here we catch
            // the postfix-extended cases (`++s.x;`, `--p->x;`,
            // `--a[1];`) and rewrite to `lv += 1` / `lv -= 1`. At
            // statement position the pre-value is discarded, so the
            // compound-assign form produces byte-identical output to
            // what BCC emits for the prefix update — confirmed against
            // fixtures 404–406. Same byte equivalence batch 28 doc-
            // umented for postfix.
            TokenKind::PlusPlus | TokenKind::MinusMinus
                if !(matches!(self.peek_n(1).kind, TokenKind::Ident(_))
                    && !matches!(
                        self.peek_n(2).kind,
                        TokenKind::Dot
                            | TokenKind::Arrow
                            | TokenKind::LBracket
                            | TokenKind::LParen,
                    )) =>
            {
                let update_op = match_update_op(&self.peek().kind).unwrap();
                let op_tok = self.bump();
                let lv = self.parse_unary()?;
                let semi = self.expect(&TokenKind::Semicolon)?;
                let span = Span::new(start, semi.span.end);
                let op = match update_op {
                    UpdateOp::Inc => BinOp::Add,
                    UpdateOp::Dec => BinOp::Sub,
                };
                let value = Expr {
                    kind: ExprKind::IntLit(1),
                    span: Span::new(op_tok.span.start, op_tok.span.end),
                };
                return match lv.kind {
                    ExprKind::Member { base, field, kind: mk } => Ok(Stmt {
                        kind: StmtKind::MemberCompoundAssign {
                            base: *base, field, kind: mk, op, value,
                            from_postfix: false,
                        },
                        span,
                    }),
                    ExprKind::Deref(target) => Ok(Stmt {
                        kind: StmtKind::DerefCompoundAssign {
                            target: *target, op, value, from_postfix: false,
                        },
                        span,
                    }),
                    ExprKind::ArrayIndex { .. } => {
                        let mut indices: Vec<Expr> = Vec::new();
                        let mut cur = lv;
                        let array = loop {
                            match cur.kind {
                                ExprKind::ArrayIndex { array, index } => {
                                    indices.push(*index);
                                    cur = *array;
                                }
                                ExprKind::Ident(name) => break name,
                                _ => return Err(ParseError::Unsupported {
                                    offset: cur.span.start,
                                }),
                            }
                        };
                        indices.reverse();
                        Ok(Stmt {
                            kind: StmtKind::ArrayCompoundAssign {
                                array, indices, op, value, from_postfix: false,
                            },
                            span,
                        })
                    }
                    _ => Err(ParseError::Unsupported { offset: lv.span.start }),
                };
            }
            _ => self.parse_expr_or_lvalue_assign(start),
        }
    }
    /// `while ( <cond> ) <branch>`. Same branch shape as `if`.
    pub(crate) fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let while_tok = self.expect(&TokenKind::KwWhile)?;
        self.expect(&TokenKind::LParen)?;
        // While-condition accepts the full assignment-expression
        // grammar so shapes like `while (*d++ = *s++)` parse.
        // Fixture 1808.
        let cond = self.parse_for_clause_expr()?;
        self.expect(&TokenKind::RParen)?;
        let body = self.parse_branch()?;
        let end = body.last().map_or(while_tok.span.end, |s| s.span.end);
        Ok(Stmt {
            kind: StmtKind::While { cond, body },
            span: Span::new(while_tok.span.start, end),
        })
    }
    /// `do <branch> while ( <cond> ) ;`.
    pub(crate) fn parse_do_while(&mut self) -> Result<Stmt, ParseError> {
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
    pub(crate) fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let for_tok = self.expect(&TokenKind::KwFor)?;
        self.expect(&TokenKind::LParen)?;
        let init = if matches!(self.peek().kind, TokenKind::Semicolon) {
            None
        } else {
            Some(self.parse_for_clause_list(TokenKind::Semicolon)?)
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
            Some(self.parse_for_clause_list(TokenKind::RParen)?)
        };
        self.expect(&TokenKind::RParen)?;
        let body = self.parse_branch()?;
        let end = body.last().map_or(for_tok.span.end, |s| s.span.end);
        Ok(Stmt {
            kind: StmtKind::For { init, cond, step, body },
            span: Span::new(for_tok.span.start, end),
        })
    }
    /// Parse a comma-separated list of for-clause expressions, stopping
    /// at `terminator` (semicolon for init, rparen for step). Each
    /// expression follows `parse_for_clause_expr`'s assign-first rule.
    pub(crate) fn parse_for_clause_list(
        &mut self,
        terminator: TokenKind,
    ) -> Result<Vec<Expr>, ParseError> {
        let mut exprs = vec![self.parse_for_clause_expr()?];
        while matches!(self.peek().kind, TokenKind::Comma) {
            self.bump();
            exprs.push(self.parse_for_clause_expr()?);
        }
        // Caller still expects/consumes the terminator (we just peek
        // to make sure the next token matches what they expect).
        let _ = terminator;
        Ok(exprs)
    }
    /// Parse a for-loop init/step clause. We accept `<ident> = <rhs>`
    /// (the common form) as an assignment expression; otherwise the
    /// clause is any normal expression (`++i`, function call, …).
    /// Pure C also allows the init clause to be a declaration, but
    /// that's a separate slice.
    pub(crate) fn parse_for_clause_expr(&mut self) -> Result<Expr, ParseError> {
        if matches!(self.peek().kind, TokenKind::Ident(_))
            && matches!(self.peek_n(1).kind, TokenKind::Equals)
        {
            let ident_tok = self.bump();
            let TokenKind::Ident(name) = ident_tok.kind else { unreachable!() };
            self.expect(&TokenKind::Equals)?;
            // Right-associative: `a = b = c` parses as `a = (b = c)`.
            // Recurse so the RHS itself can be another assignment.
            // Fixture 500.
            let rhs = self.parse_for_clause_expr()?;
            let span = Span::new(ident_tok.span.start, rhs.span.end);
            return Ok(Expr {
                kind: ExprKind::AssignExpr { target: name, value: Box::new(rhs) },
                span,
            });
        }
        // Compound assigns in for clauses (`i += 2`, `i *= 3`, etc.).
        // Distinct AST shape from a plain `i = i + 2` so codegen
        // can pick the direct register inc/dec peephole instead of
        // the AX-route assign — BCC emits different bytes for the
        // two forms even when they're semantically equivalent.
        // Fixtures 1328, 3150, 3156, 3157, 3161.
        if matches!(self.peek().kind, TokenKind::Ident(_))
            && let Some(op) = match_compound_op(&self.peek_n(1).kind)
        {
            let ident_tok = self.bump();
            let TokenKind::Ident(name) = ident_tok.kind else { unreachable!() };
            self.bump(); // compound-op token
            let rhs = self.parse_expr()?;
            let span = Span::new(ident_tok.span.start, rhs.span.end);
            return Ok(Expr {
                kind: ExprKind::CompoundAssignExpr {
                    target: name,
                    op,
                    value: Box::new(rhs),
                },
                span,
            });
        }
        let lhs = self.parse_expr()?;
        // Lvalue-assignment expression: `<lvalue> = <value>` where
        // the lvalue isn't a bare identifier (`*p = 5`, `a[i] = 42`,
        // `*d++ = *s++`). Parses as a single right-associative
        // expression so it can appear in parens, args, or condition
        // position. Bare-ident `name = value` already hit the
        // dedicated branch above. Fixtures 3333, 1986, 1808.
        if matches!(self.peek().kind, TokenKind::Equals)
            && is_assignable_lvalue(&lhs.kind)
        {
            self.bump(); // `=`
            let rhs = self.parse_for_clause_expr()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            return Ok(Expr {
                kind: ExprKind::AssignLvalueExpr {
                    target: Box::new(lhs),
                    value: Box::new(rhs),
                },
                span,
            });
        }
        Ok(lhs)
    }
    /// `switch ( <expr> ) { (case <int>: <stmts> | default: <stmts>)* }`.
    /// The case arms are kept in source order. Each arm's body extends
    /// until the next `case` / `default` / `}` — `break;` is just a
    /// regular statement inside the body, not a separator. We require
    /// the brace; BCC may permit a single statement, but no fixture
    /// has shown that and the grammar is cleaner this way.
    pub(crate) fn parse_switch(&mut self) -> Result<Stmt, ParseError> {
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
                    // Optional leading `-` for negative case labels
                    // (fixture 525: `case -1:`). The value still
                    // canonicalizes as a u32 — we negate after
                    // reading the bare integer literal.
                    let negate = matches!(self.peek().kind, TokenKind::Minus);
                    if negate {
                        self.bump();
                    }
                    let int_tok = self.bump();
                    let v = match &int_tok.kind {
                        TokenKind::IntLit(v) => *v,
                        TokenKind::Ident(name) => {
                            // Enum constants resolve to their integer
                            // value here, just like at expression
                            // position. Fixtures 2384, 2684, 3054.
                            *self.enum_constants.get(name).ok_or_else(|| {
                                ParseError::Unexpected {
                                    expected: "integer literal or enum constant in `case`".to_owned(),
                                    found: format!("identifier `{name}`"),
                                    offset: int_tok.span.start,
                                }
                            })?
                        }
                        _ => {
                            return Err(ParseError::Unexpected {
                                expected: "integer literal in `case`".to_owned(),
                                found: int_tok.kind.describe().to_owned(),
                                offset: int_tok.span.start,
                            });
                        }
                    };
                    let v = if negate { v.wrapping_neg() } else { v };
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
                body.extend(std::mem::take(&mut self.pending_extra_stmts));
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
    pub(crate) fn parse_if(&mut self) -> Result<Stmt, ParseError> {
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
    pub(crate) fn parse_branch(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if matches!(self.peek().kind, TokenKind::LBrace) {
            self.bump();
            let mut body = Vec::new();
            while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
                body.push(self.parse_stmt()?);
                body.extend(std::mem::take(&mut self.pending_extra_stmts));
            }
            self.expect(&TokenKind::RBrace)?;
            Ok(body)
        } else {
            let stmt = self.parse_stmt()?;
            let mut body = vec![stmt];
            body.extend(std::mem::take(&mut self.pending_extra_stmts));
            Ok(body)
        }
    }
}
