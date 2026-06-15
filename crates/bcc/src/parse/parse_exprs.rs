use super::*;

impl Parser {
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
    pub(crate) fn parse_expr_or_lvalue_assign(&mut self, start: u32) -> Result<Stmt, ParseError> {
        let expr = self.parse_expr()?;
        // Statement-position postfix `++`/`--` on an lvalue. When the
        // value isn't used, `lv++` is byte-identical to `lv += 1`, so
        // we rewrite to the compound-assign form rather than threading
        // a separate Update statement through every lvalue shape.
        // Pre-form (`++lv`) is already parsed in `parse_unary` as a
        // bare-ident-only Update; the lvalue cases reach here as the
        // *result* of `parse_expr` and we handle them uniformly.
        // Fixtures 401 (`s.x++;`), 402 (`p->x++;`), 403 (`a[1]++;`).
        if let Some(update_op) = match_update_op(&self.peek().kind) {
            let op_tok = self.bump();
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
            return match expr.kind {
                ExprKind::Member { base, field, kind: mk } => Ok(Stmt {
                    kind: StmtKind::MemberCompoundAssign {
                        base: *base, field, kind: mk, op, value,
                        from_postfix: true,
                    },
                    span,
                }),
                ExprKind::Deref(target) => Ok(Stmt {
                    kind: StmtKind::DerefCompoundAssign {
                        target: *target, op, value, from_postfix: true,
                    },
                    span,
                }),
                ExprKind::ArrayIndex { .. } => {
                    let mut indices: Vec<Expr> = Vec::new();
                    let mut cur = expr;
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
                            array, indices, op, value, from_postfix: true,
                        },
                        span,
                    })
                }
                _ => Err(ParseError::Unsupported { offset: expr.span.start }),
            };
        }
        if !matches!(self.peek().kind, TokenKind::Equals)
            && match_compound_op(&self.peek().kind).is_none()
        {
            // Plain expression statement. If parse_atom already
            // consumed a postfix `++`/`--` on an ArrayIndex (because
            // an expression-position rule fired), rewrite the
            // statement to the matching ArrayCompoundAssign so it
            // hits the existing stmt-level codegen path. The value
            // is discarded, so `a[i]++;` is byte-identical to
            // `a[i] += 1;`. Fixtures 3375, 2700.
            if let ExprKind::UpdateLvalue { target, op, position: _ } = expr.kind {
                if let ExprKind::ArrayIndex { .. } = &target.kind {
                    let semi = self.expect(&TokenKind::Semicolon)?;
                    let span = Span::new(start, semi.span.end);
                    let bin_op = match op {
                        UpdateOp::Inc => BinOp::Add,
                        UpdateOp::Dec => BinOp::Sub,
                    };
                    let value = Expr {
                        kind: ExprKind::IntLit(1),
                        span: Span::new(target.span.end, target.span.end),
                    };
                    let mut indices: Vec<Expr> = Vec::new();
                    let mut cur = *target;
                    let array = loop {
                        match cur.kind {
                            ExprKind::ArrayIndex { array, index } => {
                                indices.push(*index);
                                cur = *array;
                            }
                            ExprKind::Ident(name) => break name,
                            _ => {
                                return Err(ParseError::Unsupported {
                                    offset: cur.span.start,
                                });
                            }
                        }
                    };
                    indices.reverse();
                    return Ok(Stmt {
                        kind: StmtKind::ArrayCompoundAssign {
                            array, indices, op: bin_op, value,
                            from_postfix: true,
                        },
                        span,
                    });
                }
                // Deref target: leave the UpdateLvalue ExprStmt to
                // the existing codegen path that handles `(*p)++;`.
                let semi = self.expect(&TokenKind::Semicolon)?;
                return Ok(Stmt {
                    kind: StmtKind::ExprStmt(Expr {
                        kind: ExprKind::UpdateLvalue {
                            target, op, position: UpdatePosition::Post,
                        },
                        span: expr.span,
                    }),
                    span: Span::new(start, semi.span.end),
                });
            }
            // Plain expression statement.
            let semi = self.expect(&TokenKind::Semicolon)?;
            return Ok(Stmt {
                kind: StmtKind::ExprStmt(expr),
                span: Span::new(start, semi.span.end),
            });
        }
        // Compound assignment on a member lvalue: `p->x += v;` etc.
        // Bare-ident compound assigns took the early path in
        // `parse_stmt`; here we only see non-ident lvalues.
        if let Some(op) = match_compound_op(&self.peek().kind) {
            self.bump();
            let value = self.parse_expr()?;
            let semi = self.expect(&TokenKind::Semicolon)?;
            let span = Span::new(start, semi.span.end);
            return match expr.kind {
                ExprKind::Member { base, field, kind: mk } => Ok(Stmt {
                    kind: StmtKind::MemberCompoundAssign {
                        base: *base,
                        field,
                        kind: mk,
                        op,
                        value,
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
                    // Walk the nested chain to the base ident, same as
                    // the plain `ArrayAssign` path.
                    let mut indices: Vec<Expr> = Vec::new();
                    let mut cur = expr;
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
                _ => Err(ParseError::Unsupported { offset: expr.span.start }),
            };
        }
        // Plain assignment. Validate the parsed expression is a kind
        // we know how to assign to.
        self.bump(); // `=`
        let value = self.parse_expr()?;
        let semi = self.expect(&TokenKind::Semicolon)?;
        let span = Span::new(start, semi.span.end);
        let kind = match expr.kind {
            ExprKind::Deref(ptr) => StmtKind::DerefAssign { target: *ptr, value },
            ExprKind::ArrayIndex { .. } => {
                // The LHS is potentially a nested chain `a[i][j]...`,
                // optionally rooted at a struct member access
                // (`b.data[i]`). Walk it, collecting indices innermost-
                // first, then reverse to source order. The root is
                // either a bare `Ident` (→ ArrayAssign) or a `Member`
                // whose base is an `Ident` (→ MemberArrayAssign, fixture
                // 497).
                let lv_span_start = expr.span.start;
                let mut indices: Vec<Expr> = Vec::new();
                let mut cur = expr;
                let root_kind;
                loop {
                    match cur.kind {
                        ExprKind::ArrayIndex { array, index } => {
                            indices.push(*index);
                            cur = *array;
                        }
                        _ => {
                            root_kind = cur.kind;
                            break;
                        }
                    }
                }
                indices.reverse();
                match root_kind {
                    ExprKind::Ident(array) => StmtKind::ArrayAssign { array, indices, value },
                    ExprKind::Member {
                        base,
                        field,
                        kind: crate::ast::MemberKind::Dot,
                    } => {
                        let ExprKind::Ident(base_name) = base.kind else {
                            return Err(ParseError::Unsupported { offset: base.span.start });
                        };
                        StmtKind::MemberArrayAssign {
                            base: base_name,
                            field,
                            indices,
                            value,
                        }
                    }
                    _ => return Err(ParseError::Unsupported { offset: lv_span_start }),
                }
            }
            ExprKind::Member { base, field, kind: mk } => StmtKind::MemberAssign {
                base: *base,
                field,
                kind: mk,
                value,
            },
            ExprKind::Ident(name) => StmtKind::Assign { name, value },
            _ => {
                return Err(ParseError::Unsupported { offset: expr.span.start });
            }
        };
        Ok(Stmt { kind, span })
    }
    pub(crate) fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        // Precedence ladder, lowest at the top: `?:` < || < && < | < ^
        // < & < == != < relational < shift < additive < multiplicative
        // < unary < atom. `?:` is right-associative.
        self.parse_conditional()
    }
    /// `<cond> ? <then> : <else-conditional>` — right-associative.
    pub(crate) fn parse_conditional(&mut self) -> Result<Expr, ParseError> {
        let cond = self.parse_logor()?;
        if !matches!(self.peek().kind, TokenKind::Question) {
            return Ok(cond);
        }
        self.bump(); // `?`
        let then_value = self.parse_expr()?; // `:` separates; then-value can be a full expr
        self.expect(&TokenKind::Colon)?;
        let else_value = self.parse_conditional()?; // right-associative
        let span = Span::new(cond.span.start, else_value.span.end);
        Ok(Expr {
            kind: ExprKind::Ternary {
                cond: Box::new(cond),
                then_value: Box::new(then_value),
                else_value: Box::new(else_value),
            },
            span,
        })
    }
    pub(crate) fn parse_logor(&mut self) -> Result<Expr, ParseError> {
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
    pub(crate) fn parse_logand(&mut self) -> Result<Expr, ParseError> {
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
    pub(crate) fn parse_bitor(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_bitxor, |t| {
            matches!(t, TokenKind::Pipe).then_some(BinOp::BitOr)
        })
    }
    pub(crate) fn parse_bitxor(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_bitand, |t| {
            matches!(t, TokenKind::Caret).then_some(BinOp::BitXor)
        })
    }
    pub(crate) fn parse_bitand(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_equality, |t| {
            matches!(t, TokenKind::Ampersand).then_some(BinOp::BitAnd)
        })
    }
    pub(crate) fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_relational, |t| match t {
            TokenKind::EqEq => Some(BinOp::Eq),
            TokenKind::BangEq => Some(BinOp::Ne),
            _ => None,
        })
    }
    pub(crate) fn parse_relational(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_shift, |t| match t {
            TokenKind::Lt => Some(BinOp::Lt),
            TokenKind::Le => Some(BinOp::Le),
            TokenKind::Gt => Some(BinOp::Gt),
            TokenKind::Ge => Some(BinOp::Ge),
            _ => None,
        })
    }
    pub(crate) fn parse_shift(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_additive, |t| match t {
            TokenKind::ShiftLeft => Some(BinOp::Shl),
            TokenKind::ShiftRight => Some(BinOp::Shr),
            _ => None,
        })
    }
    pub(crate) fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        self.left_assoc(Self::parse_multiplicative, |t| match t {
            TokenKind::Plus => Some(BinOp::Add),
            TokenKind::Minus => Some(BinOp::Sub),
            _ => None,
        })
    }
    pub(crate) fn parse_multiplicative(&mut self) -> Result<Expr, ParseError> {
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
    /// Return the static byte size of an expression, when known at
    /// parse time. Today only bare-Ident operands are supported (look
    /// the name up in the local-of-current-function or file-scope
    /// tables); compound expressions return `None` and let the caller
    /// report the failure. `sizeof` is the only consumer.
    pub(crate) fn expr_static_size(&self, e: &Expr) -> Option<u16> {
        match &e.kind {
            ExprKind::Ident(name) => {
                if let Some(ty) = self.function_locals.get(name) {
                    return Some(ty.size_bytes());
                }
                self.global_types.get(name).map(|ty| ty.size_bytes())
            }
            // `sizeof("hi")` — string literal sizes include the NUL
            // terminator. Fixture 511.
            ExprKind::StringLit(bytes) => Some(u16::try_from(bytes.len() + 1).ok()?),
            // `sizeof(a[K])` — array element size. The index value
            // doesn't matter; only the element type does. Fixture
            // 1327.
            ExprKind::ArrayIndex { array, .. } => {
                let ExprKind::Ident(name) = &array.kind else { return None };
                let ty = self
                    .function_locals
                    .get(name)
                    .or_else(|| self.global_types.get(name))?;
                ty.array_elem().map(Type::size_bytes)
            }
            // `sizeof(*p)` — pointee size.
            ExprKind::Deref(inner) => {
                let ExprKind::Ident(name) = &inner.kind else { return None };
                let ty = self
                    .function_locals
                    .get(name)
                    .or_else(|| self.global_types.get(name))?;
                ty.pointee().map(Type::size_bytes)
            }
            // `sizeof(<binop>)` — result type follows C's usual
            // arithmetic conversions; for our int/char/long mix the
            // result is the wider of the two operand sizes (long
            // beats int beats char). Fixture 2498.
            ExprKind::BinOp { left, right, .. } => {
                let l = self.expr_static_size(left).unwrap_or(2);
                let r = self.expr_static_size(right).unwrap_or(2);
                Some(l.max(r))
            }
            // `sizeof <base>.<field>` / `sizeof <base>-><field>` —
            // the size of the named struct member. Resolve the base's
            // struct type (directly for `.`, through one pointer level
            // for `->`), look the field up, and report its size. The
            // base must be a bare identifier with a known struct type.
            ExprKind::Member { base, field, kind } => {
                let ExprKind::Ident(name) = &base.kind else { return None };
                let base_ty = self
                    .function_locals
                    .get(name)
                    .or_else(|| self.global_types.get(name))?;
                let struct_ty = match kind {
                    crate::ast::MemberKind::Dot => base_ty,
                    crate::ast::MemberKind::Arrow => base_ty.pointee()?,
                };
                struct_ty.struct_field(field).map(|f| f.ty.size_bytes())
            }
            // `sizeof(<cast>)` — cast determines the result type.
            ExprKind::Cast { ty, .. } => Some(ty.size_bytes()),
            // `sizeof(<intlit>)` — int.
            ExprKind::IntLit(_) => Some(2),
            // `sizeof(<unary>)` — same width as the operand.
            ExprKind::Unary { operand, .. } => self.expr_static_size(operand),
            // `sizeof(++name)` / `sizeof(name--)` — the C standard
            // says sizeof never evaluates its operand, so the
            // increment is dead and the size is whatever the
            // operand's type is. Fixture 2298.
            ExprKind::Update { target, .. } => {
                if let Some(ty) = self.function_locals.get(target) {
                    return Some(ty.size_bytes());
                }
                self.global_types.get(target).map(|ty| ty.size_bytes())
            }
            _ => None,
        }
    }
    /// True if the current `(` is followed by a type-name keyword
    /// (or a typedef alias). Caller is responsible for the `(` check;
    /// this just inspects token index 1.
    pub(crate) fn is_type_name_after_lparen(&self) -> bool {
        match self.peek_n(1).kind {
            TokenKind::KwInt
            | TokenKind::KwChar
            | TokenKind::KwVoid
            | TokenKind::KwUnsigned
            | TokenKind::KwLong
            | TokenKind::KwFloat
            | TokenKind::KwDouble
            | TokenKind::KwStruct
            | TokenKind::KwUnion => true,
            TokenKind::Ident(ref name) if self.typedefs.contains_key(name) => true,
            _ => false,
        }
    }
    pub(crate) fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        // `sizeof(<type>)` folds to an integer literal at parse time.
        // We support only the parenthesized type-name form today —
        // `sizeof <expr>` would need a type checker to compute the
        // operand's type, and no fixture forces it yet.
        if matches!(self.peek().kind, TokenKind::KwSizeof) {
            let kw = self.bump();
            // Two operand shapes:
            //   1. `sizeof ( <type> )` — fold via `parse_type_name`.
            //   2. `sizeof <unary>`    — fold via the operand's static
            //      type. `(<ident>)` is ambiguous with shape 1; we
            //      resolve based on whether the next token after `(`
            //      is a type-name keyword/typedef.
            if matches!(self.peek().kind, TokenKind::LParen)
                && self.is_type_name_after_lparen()
            {
                self.bump(); // `(`
                let ty = self.parse_type_name()?;
                let close = self.expect(&TokenKind::RParen)?;
                return Ok(Expr {
                    kind: ExprKind::IntLit(u32::from(ty.size_bytes())),
                    span: Span::new(kw.span.start, close.span.end),
                });
            }
            let operand = self.parse_unary()?;
            let size = self.expr_static_size(&operand).ok_or_else(|| {
                ParseError::Unsupported { offset: operand.span.start }
            })?;
            return Ok(Expr {
                kind: ExprKind::IntLit(u32::from(size)),
                span: Span::new(kw.span.start, operand.span.end),
            });
        }
        // Cast: `(<type>) <unary>`. Disambiguated from a parenthesized
        // expression by 1-token lookahead past the `(` — if the next
        // token is a type-name keyword (or a typedef alias), it's a
        // cast. Otherwise it's a parenthesized expression and falls
        // through to `parse_primary`.
        if matches!(self.peek().kind, TokenKind::LParen) && self.is_type_name_after_lparen() {
            let lparen = self.bump();
            let ty = self.parse_type_name()?;
            self.expect(&TokenKind::RParen)?;
            let operand = self.parse_unary()?;
            let span = Span::new(lparen.span.start, operand.span.end);
            return Ok(Expr {
                kind: ExprKind::Cast { ty, operand: Box::new(operand) },
                span,
            });
        }
        if let Some(update_op) = match_update_op(&self.peek().kind) {
            let op_tok = self.bump();
            // `++(*p)` / `--(*p)` — prefix update on a paren-deref.
            // Parse the parenthesized operand via parse_unary so the
            // result is the inner Deref expression, then wrap in
            // UpdateLvalue with Position::Pre. Fixtures 2762, 3110.
            if matches!(self.peek().kind, TokenKind::LParen) {
                let operand = self.parse_unary()?;
                if matches!(operand.kind, ExprKind::Deref(_)) {
                    let span = Span::new(op_tok.span.start, operand.span.end);
                    return Ok(Expr {
                        kind: ExprKind::UpdateLvalue {
                            target: Box::new(operand),
                            op: update_op,
                            position: UpdatePosition::Pre,
                        },
                        span,
                    });
                }
                // Fallback: not a deref — defer to the original
                // bare-ident path which will error.
                return Err(ParseError::Unsupported { offset: operand.span.start });
            }
            // Prefix `++arr[i]` / `--arr[i]` — parse the atom (an
            // ArrayIndex) and wrap in UpdateLvalue::Pre. Falls
            // through to bare-ident if not followed by `[`.
            // Fixtures 2616, 2937.
            if matches!(self.peek().kind, TokenKind::Ident(_))
                && matches!(self.peek_n(1).kind, TokenKind::LBracket)
            {
                let operand = self.parse_atom()?;
                if matches!(operand.kind, ExprKind::ArrayIndex { .. }) {
                    let span = Span::new(op_tok.span.start, operand.span.end);
                    return Ok(Expr {
                        kind: ExprKind::UpdateLvalue {
                            target: Box::new(operand),
                            op: update_op,
                            position: UpdatePosition::Pre,
                        },
                        span,
                    });
                }
                // Not an array index — bail out.
                return Err(ParseError::Unsupported { offset: operand.span.start });
            }
            // Prefix `++s.x` / `--p->x` — parse the atom (a Member)
            // and wrap in UpdateLvalue::Pre. Fixture 3444.
            if matches!(self.peek().kind, TokenKind::Ident(_))
                && matches!(
                    self.peek_n(1).kind,
                    TokenKind::Dot | TokenKind::Arrow,
                )
            {
                let operand = self.parse_atom()?;
                if matches!(operand.kind, ExprKind::Member { .. }) {
                    let span = Span::new(op_tok.span.start, operand.span.end);
                    return Ok(Expr {
                        kind: ExprKind::UpdateLvalue {
                            target: Box::new(operand),
                            op: update_op,
                            position: UpdatePosition::Pre,
                        },
                        span,
                    });
                }
                return Err(ParseError::Unsupported { offset: operand.span.start });
            }
            let name_tok = self.bump();
            let TokenKind::Ident(name) = name_tok.kind else {
                return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
            };
            let name = self
                .lookup_block_rename(&name)
                .or_else(|| self.current_static_renames.get(&name).cloned())
                .unwrap_or(name);
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
        // Address-of: `&<ident>` for the bare-name case, plus
        // `&<ident>[<const>]` for array-element addressing (fixture
        // 198) and `&<ident>.<field>` for struct-field addressing
        // (fixture 485). The more general `&<lvalue>` form still
        // isn't fixtured.
        if matches!(self.peek().kind, TokenKind::Ampersand) {
            let amp = self.bump();
            let name_tok = self.bump();
            let TokenKind::Ident(name) = name_tok.kind else {
                return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
            };
            if matches!(self.peek().kind, TokenKind::LBracket) {
                self.bump();
                // Variable-index path: parse the index as a general
                // expression and produce AddressOfArrayElemVar.
                // Constant index continues to fold at parse time.
                // Fixtures 3249, 3645.
                let elem_size = match self.global_types.get(&name).or_else(|| self.function_locals.get(&name)) {
                    Some(Type::Array { elem, .. }) => elem.size_bytes(),
                    Some(Type::Pointer(elem)) => elem.size_bytes(),
                    _ => {
                        return Err(ParseError::Unexpected {
                            offset: name_tok.span.start,
                            expected: format!("array name in `&{name}[K]`"),
                            found: "non-array or unknown identifier".to_owned(),
                        });
                    }
                };
                let idx_expr = self.parse_expr()?;
                let rb = self.expect(&TokenKind::RBracket)?;
                let span = Span::new(amp.span.start, rb.span.end);
                if let Some(idx) = crate::codegen::fold::try_const_eval(&idx_expr) {
                    let byte_offset = (idx as i32).wrapping_mul(i32::from(elem_size));
                    return Ok(Expr {
                        kind: ExprKind::AddressOfArrayElem { array: name, byte_offset },
                        span,
                    });
                }
                return Ok(Expr {
                    kind: ExprKind::AddressOfArrayElemVar {
                        array: name,
                        index: Box::new(idx_expr),
                        elem_size,
                    },
                    span,
                });
            }
            // `&<ident>.<field>[.<field>]*` — chain of dot accesses
            // into a struct value. Resolve to AddressOfArrayElem with
            // the cumulative field byte_offset. Fixture 485 hits a
            // single-step chain on a global struct.
            if matches!(self.peek().kind, TokenKind::Dot) {
                let base_ty = self.global_types.get(&name)
                    .or_else(|| self.function_locals.get(&name))
                    .cloned();
                let Some(mut cur_ty) = base_ty else {
                    return Err(ParseError::Unexpected {
                        offset: name_tok.span.start,
                        expected: format!("known struct in `&{name}.field`"),
                        found: "unknown identifier".to_owned(),
                    });
                };
                let mut total_off: i32 = 0;
                let mut end = name_tok.span.end;
                while matches!(self.peek().kind, TokenKind::Dot) {
                    self.bump();
                    let field_tok = self.bump();
                    let TokenKind::Ident(field) = field_tok.kind else {
                        return Err(ParseError::NotAnIdent { offset: field_tok.span.start });
                    };
                    let Some((field_off, field_ty)) = cur_ty.field(&field) else {
                        return Err(ParseError::Unexpected {
                            offset: field_tok.span.start,
                            expected: format!("known field in `{cur_ty:?}`"),
                            found: format!("`{field}`"),
                        });
                    };
                    total_off = total_off.checked_add(i32::from(field_off))
                        .expect("field offset fits in i32");
                    cur_ty = field_ty;
                    end = field_tok.span.end;
                }
                let span = Span::new(amp.span.start, end);
                return Ok(Expr {
                    kind: ExprKind::AddressOfArrayElem { array: name, byte_offset: total_off },
                    span,
                });
            }
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
            let mut operand = self.parse_unary()?;
            // `*(<lv>)++` — postfix `++`/`--` on a paren-wrapped
            // lvalue. C precedence binds postfix tighter than prefix
            // `*`, so the `++` applies to the inner expression and
            // the outer `*` dereferences the post-update value. The
            // *stmt-level* postfix handler (parse_stmt) catches the
            // top-level `lv++;` shape, but when the `++` sits
            // mid-expression (here, between an inner paren and an
            // outer prefix-`*`) we wrap the operand in UpdateLvalue
            // ourselves. Fixture 3662 (`*(*pp)++` for an
            // `int **pp` parameter).
            if matches!(
                self.peek().kind,
                TokenKind::PlusPlus | TokenKind::MinusMinus,
            ) {
                let op_tok = self.bump();
                let op = match op_tok.kind {
                    TokenKind::PlusPlus => UpdateOp::Inc,
                    TokenKind::MinusMinus => UpdateOp::Dec,
                    _ => unreachable!(),
                };
                let span = Span::new(operand.span.start, op_tok.span.end);
                operand = Expr {
                    kind: ExprKind::UpdateLvalue {
                        target: Box::new(operand),
                        op,
                        position: UpdatePosition::Post,
                    },
                    span,
                };
            }
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
    pub(crate) fn left_assoc<F, M>(&mut self, sub: F, mut match_op: M) -> Result<Expr, ParseError>
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
    pub(crate) fn parse_atom(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.parse_primary()?;
        // Postfix `.field`, `->field`, and `[expr]` can chain (`a.b.c`,
        // `p->next->x`, `b.data[2]`). Each step extends the parsed
        // expression by wrapping it in a Member or ArrayIndex node.
        loop {
            match self.peek().kind {
                TokenKind::Dot | TokenKind::Arrow => {
                    let kind = if matches!(self.peek().kind, TokenKind::Dot) {
                        MemberKind::Dot
                    } else {
                        MemberKind::Arrow
                    };
                    self.bump();
                    let field_tok = self.bump();
                    let TokenKind::Ident(field) = field_tok.kind else {
                        return Err(ParseError::NotAnIdent { offset: field_tok.span.start });
                    };
                    let span = Span::new(e.span.start, field_tok.span.end);
                    e = Expr {
                        kind: ExprKind::Member { base: Box::new(e), field, kind },
                        span,
                    };
                }
                TokenKind::LBracket => {
                    self.bump();
                    let index = self.parse_expr()?;
                    let close = self.expect(&TokenKind::RBracket)?;
                    let span = Span::new(e.span.start, close.span.end);
                    e = Expr {
                        kind: ExprKind::ArrayIndex {
                            array: Box::new(e),
                            index: Box::new(index),
                        },
                        span,
                    };
                }
                // Postfix ++/-- on a `(*p)` primary in expression
                // position. The bare-ident form is consumed inside
                // parse_primary; the *(*pp)++ outer-deref shape is
                // caught in parse_unary's `*` arm. Here we cover
                // expression-position uses where the primary is
                // already a `Deref(...)` and we're NOT inside an
                // outer `*` (e.g. `return (*p)++;`, `r = (*p)--;`).
                // Stmt-level `(*p)++;` is still caught by
                // parse_stmt's postfix handler because UpdateLvalue
                // in ExprStmt context falls through the same way an
                // Update would. Fixtures 2857, 3107, 2449.
                TokenKind::PlusPlus | TokenKind::MinusMinus
                    if matches!(
                        e.kind,
                        ExprKind::Deref(_) | ExprKind::ArrayIndex { .. }
                    ) =>
                {
                    let op_tok = self.bump();
                    let op = match op_tok.kind {
                        TokenKind::PlusPlus => UpdateOp::Inc,
                        TokenKind::MinusMinus => UpdateOp::Dec,
                        _ => unreachable!(),
                    };
                    let span = Span::new(e.span.start, op_tok.span.end);
                    e = Expr {
                        kind: ExprKind::UpdateLvalue {
                            target: Box::new(e),
                            op,
                            position: UpdatePosition::Post,
                        },
                        span,
                    };
                }
                // `<arr-index-or-member>(args)` — indirect call
                // through a function pointer obtained from an
                // ArrayIndex or Member expression. Fixtures 2308
                // (static fn-ptr array), 2944 / 3481 / 3696 (extern
                // fn-ptr array), 1812 / 2378 / 2209 (struct field
                // fn-ptr).
                TokenKind::LParen
                    if matches!(
                        &e.kind,
                        ExprKind::ArrayIndex { .. } | ExprKind::Member { .. }
                    ) =>
                {
                    self.bump();
                    let mut args: Vec<Expr> = Vec::new();
                    if !matches!(self.peek().kind, TokenKind::RParen) {
                        loop {
                            args.push(self.parse_for_clause_expr()?);
                            if matches!(self.peek().kind, TokenKind::Comma) {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    let close = self.expect(&TokenKind::RParen)?;
                    let span = Span::new(e.span.start, close.span.end);
                    e = Expr {
                        kind: ExprKind::CallVia { addr: Box::new(e), args },
                        span,
                    };
                }
                // `(*pfn)(args)` — explicit-deref call through a
                // function pointer. `*pfn` and `pfn` are equivalent
                // when `pfn` is a function pointer, so collapse the
                // Deref and emit a regular Call on the underlying
                // ident. Fixture 2414.
                TokenKind::LParen
                    if matches!(
                        &e.kind,
                        ExprKind::Deref(inner)
                            if matches!(inner.kind, ExprKind::Ident(_))
                    ) =>
                {
                    let ExprKind::Deref(inner) = e.kind else { unreachable!() };
                    let ExprKind::Ident(name) = inner.kind else { unreachable!() };
                    self.bump();
                    let mut args: Vec<Expr> = Vec::new();
                    if !matches!(self.peek().kind, TokenKind::RParen) {
                        loop {
                            args.push(self.parse_for_clause_expr()?);
                            if matches!(self.peek().kind, TokenKind::Comma) {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    let close = self.expect(&TokenKind::RParen)?;
                    let span = Span::new(inner.span.start, close.span.end);
                    e = Expr {
                        kind: ExprKind::Call { name, args },
                        span,
                    };
                }
                // `(*ops[0])(args)` — explicit-deref call through a
                // function pointer stored in an array element or a
                // struct field. `*expr` and `expr` are equivalent
                // when `expr` is a function pointer, so collapse the
                // Deref and route to a CallVia on the underlying
                // lvalue (ArrayIndex / Member). Mirrors the implicit
                // `ops[0](args)` form handled above. Fixture 4199.
                TokenKind::LParen
                    if matches!(
                        &e.kind,
                        ExprKind::Deref(inner)
                            if matches!(
                                inner.kind,
                                ExprKind::ArrayIndex { .. }
                                    | ExprKind::Member { .. }
                            )
                    ) =>
                {
                    let ExprKind::Deref(inner) = e.kind else { unreachable!() };
                    self.bump();
                    let mut args: Vec<Expr> = Vec::new();
                    if !matches!(self.peek().kind, TokenKind::RParen) {
                        loop {
                            args.push(self.parse_for_clause_expr()?);
                            if matches!(self.peek().kind, TokenKind::Comma) {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    let close = self.expect(&TokenKind::RParen)?;
                    let span = Span::new(inner.span.start, close.span.end);
                    e = Expr {
                        kind: ExprKind::CallVia { addr: inner, args },
                        span,
                    };
                }
                _ => break,
            }
        }
        Ok(e)
    }
    pub(crate) fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::LParen => {
                // Parenthesized expression. The parens don't survive
                // into the AST — they only affect parse precedence.
                // Comma operator is permitted inside parens: chain
                // `<ident-or-expr>, <expr>, ...` into nested `Comma {
                // left, right }` nodes (left-associative). Each
                // element parses via `parse_for_clause_expr` so that
                // `(a = 1, b = 2, a + b)` recognizes the assignment-
                // expressions as well. Fixture 469.
                let mut inner = self.parse_for_clause_expr()?;
                while matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                    let right = self.parse_for_clause_expr()?;
                    let span = Span::new(inner.span.start, right.span.end);
                    inner = Expr {
                        kind: ExprKind::Comma {
                            left: Box::new(inner),
                            right: Box::new(right),
                        },
                        span,
                    };
                }
                let close = self.expect(&TokenKind::RParen)?;
                Ok(Expr {
                    kind: inner.kind,
                    span: Span::new(tok.span.start, close.span.end),
                })
            }
            TokenKind::IntLit(v) => Ok(Expr { kind: ExprKind::IntLit(v), span: tok.span }),
            TokenKind::FloatLit(bits) => {
                Ok(Expr { kind: ExprKind::FloatLit(bits), span: tok.span })
            }
            TokenKind::DoubleLit(bits) => {
                Ok(Expr { kind: ExprKind::DoubleLit(bits), span: tok.span })
            }
            TokenKind::StringLit(bytes) => {
                // Adjacent string literals concatenate at parse time
                // (`"hello, " "world"` → `"hello, world"`). Fixture 508.
                let mut all = bytes;
                let mut end = tok.span.end;
                while let TokenKind::StringLit(_) = self.peek().kind {
                    let next = self.bump();
                    let TokenKind::StringLit(more) = next.kind else { unreachable!() };
                    all.extend(more);
                    end = next.span.end;
                }
                let lit = Expr {
                    kind: ExprKind::StringLit(all),
                    span: Span::new(tok.span.start, end),
                };
                // String literals can be indexed in place: `"hi"[0]`.
                if matches!(self.peek().kind, TokenKind::LBracket) {
                    self.bump();
                    let index = self.parse_expr()?;
                    let close = self.expect(&TokenKind::RBracket)?;
                    return Ok(Expr {
                        kind: ExprKind::ArrayIndex {
                            array: Box::new(lit),
                            index: Box::new(index),
                        },
                        span: Span::new(tok.span.start, close.span.end),
                    });
                }
                Ok(lit)
            }
            TokenKind::Ident(ref raw_name) => {
                // Enum constants fold to `IntLit` here — BCC's `-S`
                // output never mentions the enum name (verified
                // against fixture 164: `return B;` → `mov ax,1`).
                if let Some(&value) = self.enum_constants.get(raw_name) {
                    return Ok(Expr {
                        kind: ExprKind::IntLit(value),
                        span: tok.span,
                    });
                }
                // Per-function static-local rename: `static int counter`
                // in `next_a` hoists to a uniquely-named global so two
                // sibling functions don't share storage. The body's
                // bare ident references the renamed global. Fixture
                // 2264 (`next_a()` and `next_b()` both with `static
                // int counter`).
                let renamed = self
                    .lookup_block_rename(raw_name)
                    .or_else(|| self.current_static_renames.get(raw_name).cloned());
                let name = renamed.as_deref().unwrap_or(raw_name).to_string();
                let name = &name;
                // Postfix `()` makes it a function call.
                if matches!(self.peek().kind, TokenKind::LParen) {
                    self.bump();
                    let mut args = Vec::new();
                    if !matches!(self.peek().kind, TokenKind::RParen) {
                        loop {
                            // parse_for_clause_expr accepts the
                            // bare-ident assignment-expression
                            // shape (`n = 7`) on top of the regular
                            // expression grammar, so an arg like
                            // `sqr(n = 7)` parses correctly. Fixture
                            // 1816.
                            args.push(self.parse_for_clause_expr()?);
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
                    // Array index `name[<expr>]`, chained as `a[i][j]`.
                    // Each `[k]` wraps the previous expression in
                    // another `ArrayIndex` — codegen walks the chain
                    // when folding constant indices to a single offset.
                    let mut acc = Expr {
                        kind: ExprKind::Ident(name.clone()),
                        span: tok.span,
                    };
                    while matches!(self.peek().kind, TokenKind::LBracket) {
                        self.bump();
                        let index = self.parse_expr()?;
                        let close = self.expect(&TokenKind::RBracket)?;
                        let span = Span::new(acc.span.start, close.span.end);
                        acc = Expr {
                            kind: ExprKind::ArrayIndex {
                                array: Box::new(acc),
                                index: Box::new(index),
                            },
                            span,
                        };
                    }
                    Ok(acc)
                } else {
                    Ok(Expr { kind: ExprKind::Ident(name.clone()), span: tok.span })
                }
            }
            _ => Err(ParseError::Unsupported { offset: tok.span.start }),
        }
    }
}
