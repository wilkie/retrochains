//! Hand-written recursive-descent parser. Single-pass: each top-level
//! function, once parsed, is handed straight to codegen. The parser owns
//! a token stream and exposes `parse_unit` for the simple "whole file at
//! once" case (which is all the early fixtures need; nothing in
//! single-pass forbids building a one-function-at-a-time variant later).

use std::collections::HashMap;

use crate::ast::{
    BinOp, Expr, ExprKind, Function, Global, LogicalOp, MemberKind, Param, Stmt, StmtKind,
    StructField, SwitchCase, TopLevelRef, Type, UnaryOp, Unit, UpdateOp, UpdatePosition,
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
}

impl Parser {
    #[must_use]
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            structs: HashMap::new(),
            typedefs: HashMap::new(),
            pending_static_locals: Vec::new(),
        }
    }

    /// Parse a whole translation unit. Top-level items can be either
    /// function definitions or global-variable declarations, in any
    /// order. The distinction is decided by 2-token lookahead: after
    /// the type and the name, an `(` means it's a function, anything
    /// else (`;`, `=`, `[`) means it's a global.
    ///
    /// # Errors
    /// Returns [`ParseError`] on the first unrecognized construct.
    pub fn parse_unit(&mut self) -> Result<Unit, ParseError> {
        let mut functions = Vec::new();
        let mut globals = Vec::new();
        let mut decl_order = Vec::new();
        while !self.at_eof() {
            // typedef gets its own dispatch — it produces no AST node,
            // just registers a name in the typedef table.
            if matches!(self.peek().kind, TokenKind::KwTypedef) {
                self.parse_typedef()?;
                continue;
            }
            // A standalone `struct <name> { ... } ;` defines a struct
            // type and adds it to the table without declaring any
            // variable. (Our fixtures all combine the struct def with
            // a following declaration, but supporting the bare form
            // is cheap and matches BCC.)
            if matches!(self.peek().kind, TokenKind::KwStruct)
                && self.is_bare_struct_def()
            {
                self.parse_bare_struct_decl()?;
                continue;
            }
            // Optional storage class. Only `static` is supported, and
            // only on globals — `static int foo() {}` would also be
            // valid C but our codegen doesn't yet thread the private
            // attribute through function emission, so we reject it
            // until a fixture demands it.
            let is_static = if matches!(self.peek().kind, TokenKind::KwStatic) {
                self.bump();
                true
            } else {
                false
            };
            // Otherwise this top-level item is either a function or
            // a global. Probe past the type to find the declarator
            // name and decide.
            let mut probe = 0usize;
            // Skip the type prefix (int/char/struct ...). For
            // struct types we need to skip the `struct` keyword
            // plus the tag (and the inline definition braces if
            // any, but those would have been consumed by the
            // bare-struct path above).
            match self.peek_n(probe).kind {
                TokenKind::KwInt | TokenKind::KwChar => probe += 1,
                TokenKind::KwStruct => {
                    probe += 1;
                    if matches!(self.peek_n(probe).kind, TokenKind::Ident(_)) {
                        probe += 1;
                    }
                }
                TokenKind::Ident(ref name) if self.typedefs.contains_key(name) => {
                    probe += 1;
                }
                _ => {
                    let t = self.peek_n(probe);
                    return Err(ParseError::Unexpected {
                        expected: "type at top level".to_owned(),
                        found: t.kind.describe().to_owned(),
                        offset: t.span.start,
                    });
                }
            }
            // Pointer stars (zero or more).
            while matches!(self.peek_n(probe).kind, TokenKind::Star) {
                probe += 1;
            }
            // Name.
            if !matches!(self.peek_n(probe).kind, TokenKind::Ident(_)) {
                let t = self.peek_n(probe);
                return Err(ParseError::Unexpected {
                    expected: "declarator name".to_owned(),
                    found: t.kind.describe().to_owned(),
                    offset: t.span.start,
                });
            }
            probe += 1;
            // The token after the name disambiguates: `(` means
            // function, anything else means global decl.
            if matches!(self.peek_n(probe).kind, TokenKind::LParen) {
                if is_static {
                    let t = self.peek();
                    return Err(ParseError::Unexpected {
                        expected: "global declarator after `static`".to_owned(),
                        found: "function definition".to_owned(),
                        offset: t.span.start,
                    });
                }
                let idx = functions.len();
                functions.push(self.parse_function()?);
                decl_order.push(TopLevelRef::Function(idx));
            } else {
                let idx = globals.len();
                globals.push(self.parse_global(is_static)?);
                decl_order.push(TopLevelRef::Global(idx));
            }
        }
        // Append hoisted static locals after regular globals, keeping
        // their relative source order. They aren't in `decl_order` —
        // the LIFO `public` list only covers top-level symbols, and
        // statics are private to this TU.
        globals.extend(std::mem::take(&mut self.pending_static_locals));
        Ok(Unit { functions, globals, decl_order })
    }

    /// `typedef <type> <name> ;`. Records `name` → type in the
    /// typedef table; no AST node produced.
    fn parse_typedef(&mut self) -> Result<(), ParseError> {
        self.bump(); // `typedef`
        let ty = self.parse_type()?;
        let name_tok = self.bump();
        let TokenKind::Ident(name) = name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        self.expect(&TokenKind::Semicolon)?;
        self.typedefs.insert(name, ty);
        Ok(())
    }

    /// True when the current token is `struct` followed by the
    /// shape `tag { ... } ;` (a bare definition with no following
    /// declarator). This is the K&R/early-C90 `struct point { ... };`
    /// form. With a declarator after the closing `}`, the parser
    /// falls into the function/global path instead.
    fn is_bare_struct_def(&self) -> bool {
        if !matches!(self.peek().kind, TokenKind::KwStruct) {
            return false;
        }
        // struct <ident>? { ... } ;
        let mut probe = 1usize;
        if matches!(self.peek_n(probe).kind, TokenKind::Ident(_)) {
            probe += 1;
        }
        if !matches!(self.peek_n(probe).kind, TokenKind::LBrace) {
            return false;
        }
        // Skip to matching `}`. We only need to find the depth-0
        // close — body content can't have unmatched braces.
        let mut depth = 0i32;
        loop {
            match self.peek_n(probe).kind {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        probe += 1;
                        break;
                    }
                }
                TokenKind::Eof => return false,
                _ => {}
            }
            probe += 1;
        }
        matches!(self.peek_n(probe).kind, TokenKind::Semicolon)
    }

    /// Parse a bare `struct <tag>? { <fields> } ;`. Registers the
    /// type under its tag (required for bare definitions — an
    /// anonymous struct here would be unreferencable) and emits no
    /// AST node.
    fn parse_bare_struct_decl(&mut self) -> Result<(), ParseError> {
        let ty = self.parse_struct_type()?;
        self.expect(&TokenKind::Semicolon)?;
        // The tag was already inserted into `self.structs` by
        // parse_struct_type. Bare-decl semicolon just ends the
        // statement.
        let _ = ty;
        Ok(())
    }

    /// Parse a type expression. Handles `int`, `char`, `struct
    /// <tag> { ... }`, `struct <tag>`, and typedef'd names. Pointer
    /// `*` modifiers are handled by the *caller* — this returns the
    /// base type only.
    fn parse_type(&mut self) -> Result<Type, ParseError> {
        match self.peek().kind {
            TokenKind::KwInt => {
                self.bump();
                Ok(Type::Int)
            }
            TokenKind::KwChar => {
                self.bump();
                Ok(Type::Char)
            }
            TokenKind::KwStruct => self.parse_struct_type(),
            TokenKind::Ident(ref name) if self.typedefs.contains_key(name) => {
                let ty = self.typedefs.get(name).expect("just checked").clone();
                self.bump();
                Ok(ty)
            }
            _ => {
                let t = self.peek();
                Err(ParseError::Unexpected {
                    expected: "a type (`int`, `char`, `struct ...`, or typedef name)".to_owned(),
                    found: t.kind.describe().to_owned(),
                    offset: t.span.start,
                })
            }
        }
    }

    /// `struct <tag>? { <fields> }` (with inline definition) or
    /// `struct <tag>` (reference to a previously-defined tag). Side
    /// effect: when an inline definition appears with a tag, the
    /// resulting type is inserted into `self.structs`.
    fn parse_struct_type(&mut self) -> Result<Type, ParseError> {
        self.expect(&TokenKind::KwStruct)?;
        let tag = if let TokenKind::Ident(name) = &self.peek().kind {
            let name = name.clone();
            self.bump();
            Some(name)
        } else {
            None
        };
        if !matches!(self.peek().kind, TokenKind::LBrace) {
            // Bare reference: `struct point` — must already be in
            // the table.
            let Some(name) = tag else {
                let t = self.peek();
                return Err(ParseError::Unexpected {
                    expected: "struct tag or `{`".to_owned(),
                    found: t.kind.describe().to_owned(),
                    offset: t.span.start,
                });
            };
            return self.structs.get(&name).cloned().ok_or_else(|| {
                ParseError::Unsupported { offset: self.peek().span.start }
            });
        }
        self.bump(); // `{`
        let mut fields: Vec<StructField> = Vec::new();
        let mut offset: u16 = 0;
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            // Each field declaration: <type> <pointer-stars> <name> ;
            let mut ty = self.parse_type()?;
            while matches!(self.peek().kind, TokenKind::Star) {
                self.bump();
                ty = Type::Pointer(Box::new(ty));
            }
            let name_tok = self.bump();
            let TokenKind::Ident(name) = name_tok.kind else {
                return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
            };
            self.expect(&TokenKind::Semicolon)?;
            let field_size = ty.size_bytes();
            fields.push(StructField { name, ty, offset });
            offset += field_size;
        }
        self.expect(&TokenKind::RBrace)?;
        // Round size up to even (fixture 102: `{char c; int n;}` is
        // 4 bytes, not 3).
        let size = if offset % 2 == 1 { offset + 1 } else { offset };
        let ty = Type::Struct { name: tag.clone(), fields, size };
        if let Some(name) = tag {
            self.structs.insert(name, ty.clone());
        }
        Ok(ty)
    }

    /// `<type-base> <pointer-stars>* <name> ('[' <int> ']')? [= <expr>] ;`
    /// at the top level. Same declarator shape as a local declaration
    /// (`parse_declare`); the difference is the resulting AST node
    /// (`Global` vs. `StmtKind::Declare`) and the absence of an
    /// enclosing function context.
    fn parse_global(&mut self, is_static: bool) -> Result<Global, ParseError> {
        let start = self.peek().span.start;
        let mut ty = self.parse_type()?;
        while matches!(self.peek().kind, TokenKind::Star) {
            self.bump();
            ty = Type::Pointer(Box::new(ty));
        }
        let name_tok = self.bump();
        let TokenKind::Ident(name) = &name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        let name = name.clone();
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
        Ok(Global {
            name,
            ty,
            init,
            is_static,
            span: Span::new(start, semi.span.end),
        })
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

        // Prototype (just `;` after the param list) vs definition
        // (a `{ ... }` body). Fixture 097 has the prototype form.
        if matches!(self.peek().kind, TokenKind::Semicolon) {
            let semi = self.bump();
            return Ok(Function {
                name,
                params,
                span: Span::new(start, semi.span.end),
                body: None,
            });
        }

        self.expect(&TokenKind::LBrace)?;
        let mut body = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            body.push(self.parse_stmt()?);
        }
        let close = self.expect(&TokenKind::RBrace)?;
        let span = Span::new(start, close.span.end);
        Ok(Function { name, params, span, body: Some(body) })
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
            let mut ty = self.parse_type()?;
            // Pointer stars wrap the base type, just like in a local
            // declaration (fixture 095: `int sum(int *p)`).
            while matches!(self.peek().kind, TokenKind::Star) {
                self.bump();
                ty = Type::Pointer(Box::new(ty));
            }
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
            TokenKind::KwInt | TokenKind::KwChar | TokenKind::KwStruct => {
                self.parse_declare(start)
            }
            TokenKind::KwStatic => self.parse_declare(start),
            TokenKind::Ident(ref name) if self.typedefs.contains_key(name) => {
                self.parse_declare(start)
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
            ExprKind::ArrayIndex { array, index } => {
                // The array side of an assign must be a named array
                // local (or future global) — string literals are not
                // assignable. Extract the ident name.
                let ExprKind::Ident(name) = array.kind else {
                    return Err(ParseError::Unsupported { offset: array.span.start });
                };
                StmtKind::ArrayAssign {
                    array: name,
                    index: *index,
                    value,
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
        // Optional `static` prefix. The parser hoists the declaration
        // into `static_locals` (flushed onto `unit.globals` at the end
        // of parsing) so codegen can resolve the name through the
        // global table and skip per-call stack allocation/init.
        let is_static = if matches!(self.peek().kind, TokenKind::KwStatic) {
            self.bump();
            true
        } else {
            false
        };
        let mut ty = self.parse_type()?;
        // Pointer stars wrap the base type: `int **pp` is `Pointer(Pointer(Int))`.
        while matches!(self.peek().kind, TokenKind::Star) {
            self.bump();
            ty = Type::Pointer(Box::new(ty));
        }
        // Function-pointer declarator: `<type> ( * <name> ) ( <params> )`.
        // For fixture 110 (`int (*p)(void) = f;`) we don't need to model
        // the function signature — calls through `p` work the same
        // regardless of return type, and we never dereference it. So we
        // collapse the type to `Pointer<Int>` (any pointer is 2 bytes,
        // int-pool-eligible) and skip the param list.
        if matches!(self.peek().kind, TokenKind::LParen) {
            let (name, fp_ty) = self.parse_func_ptr_declarator(ty.clone())?;
            return self.finish_declare(start, fp_ty, name, is_static);
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
        self.finish_declare(start, ty, name, is_static)
    }

    /// Common tail of `parse_declare` after the declarator (name +
    /// pointer/array/func-ptr decoration) is known. Reads the optional
    /// initializer and trailing semicolon, then yields a `Declare`
    /// statement. When `is_static` is true, also pushes a synthetic
    /// `Global` so the name is allocated in `_DATA` instead of on
    /// the stack frame.
    fn finish_declare(
        &mut self,
        start: u32,
        ty: Type,
        name: String,
        is_static: bool,
    ) -> Result<Stmt, ParseError> {
        let init = if matches!(self.peek().kind, TokenKind::Equals) {
            self.bump();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let semi = self.expect(&TokenKind::Semicolon)?;
        let span = Span::new(start, semi.span.end);
        if is_static {
            // The Global owns the initializer expression; the Stmt
            // keeps only the name/type/span so codegen can fold the
            // source line into the next comment block. Hoisting moves
            // the init out so we don't need `Expr: Clone`.
            self.pending_static_locals.push(Global {
                name: name.clone(),
                ty: ty.clone(),
                init,
                is_static: true,
                span,
            });
            return Ok(Stmt {
                kind: StmtKind::Declare { ty, name, init: None, is_static: true },
                span,
            });
        }
        Ok(Stmt {
            kind: StmtKind::Declare { ty, name, init, is_static },
            span,
        })
    }

    /// Parse `( * <name> ) ( <params> )`. The leading `(` is the
    /// current token. Returns `(name, type)`; the type is a generic
    /// near pointer (we don't model function signatures yet).
    fn parse_func_ptr_declarator(
        &mut self,
        _base_return_type: Type,
    ) -> Result<(String, Type), ParseError> {
        self.expect(&TokenKind::LParen)?;
        self.expect(&TokenKind::Star)?;
        let name_tok = self.bump();
        let TokenKind::Ident(name) = &name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        let name = name.clone();
        self.expect(&TokenKind::RParen)?;
        self.expect(&TokenKind::LParen)?;
        // Skip the parameter list. We don't record the signature, so
        // we just step past tokens until the matching `)`.
        let mut depth: u32 = 1;
        while depth > 0 {
            let t = self.bump();
            match t.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth -= 1,
                TokenKind::Eof => {
                    return Err(ParseError::Unexpected {
                        expected: "`)` to close function-pointer parameter list".to_owned(),
                        found: "end of input".to_owned(),
                        offset: t.span.start,
                    });
                }
                _ => {}
            }
        }
        Ok((name, Type::Pointer(Box::new(Type::Int))))
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
        let mut e = self.parse_primary()?;
        // Postfix `.field` and `->field` can chain (`a.b.c`,
        // `p->next->x`). Each step extends the parsed expression
        // by wrapping it in a Member node.
        loop {
            let kind = match self.peek().kind {
                TokenKind::Dot => MemberKind::Dot,
                TokenKind::Arrow => MemberKind::Arrow,
                _ => break,
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
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::LParen => {
                // Parenthesized expression. The parens don't survive
                // into the AST — they only affect parse precedence.
                let inner = self.parse_expr()?;
                let close = self.expect(&TokenKind::RParen)?;
                Ok(Expr {
                    kind: inner.kind,
                    span: Span::new(tok.span.start, close.span.end),
                })
            }
            TokenKind::IntLit(v) => Ok(Expr { kind: ExprKind::IntLit(v), span: tok.span }),
            TokenKind::StringLit(bytes) => {
                let lit = Expr {
                    kind: ExprKind::StringLit(bytes),
                    span: tok.span,
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
                    let array = Expr {
                        kind: ExprKind::Ident(name.clone()),
                        span: tok.span,
                    };
                    Ok(Expr {
                        kind: ExprKind::ArrayIndex {
                            array: Box::new(array),
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
