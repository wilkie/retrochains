use super::*;

impl Parser {
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
            // `enum [tag] { ... };` — registers integer constants and
            // emits no AST node. Only fire when an `{` follows the
            // (optional) tag — otherwise `enum <tag> <decl>` is a
            // use of the enum tag as a type and should fall through
            // to the global/function path. Fixture 470.
            if matches!(self.peek().kind, TokenKind::KwEnum) {
                let body_starts = if matches!(self.peek_n(1).kind, TokenKind::LBrace) {
                    true
                } else {
                    matches!(self.peek_n(1).kind, TokenKind::Ident(_))
                        && matches!(self.peek_n(2).kind, TokenKind::LBrace)
                };
                if body_starts {
                    self.parse_enum_decl()?;
                    continue;
                }
            }
            // A standalone `struct <name> { ... } ;` defines a struct
            // type and adds it to the table without declaring any
            // variable. (Our fixtures all combine the struct def with
            // a following declaration, but supporting the bare form
            // is cheap and matches BCC.)
            if matches!(self.peek().kind, TokenKind::KwStruct | TokenKind::KwUnion)
                && self.is_bare_record_def()
            {
                self.parse_bare_record_decl()?;
                continue;
            }
            // Forward declaration: `struct <tag>;` (no body, no
            // declarator). Registers an opaque placeholder so later
            // `struct <tag> *p;` can resolve. Fixture 495.
            if matches!(self.peek().kind, TokenKind::KwStruct | TokenKind::KwUnion)
                && matches!(self.peek_n(1).kind, TokenKind::Ident(_))
                && matches!(self.peek_n(2).kind, TokenKind::Semicolon)
            {
                self.bump(); // `struct`/`union`
                let tag_tok = self.bump();
                let TokenKind::Ident(tag) = tag_tok.kind else {
                    unreachable!("peek_n(1) just matched Ident");
                };
                self.expect(&TokenKind::Semicolon)?;
                self.structs.entry(tag.clone()).or_insert_with(|| {
                    Type::Struct {
                        name: Some(tag),
                        fields: Vec::new(),
                        size: 0,
                    }
                });
                continue;
            }
            // Optional storage class. `static` and `extern` are
            // mutually exclusive prefixes. We support both on globals
            // but neither on function definitions — codegen doesn't
            // yet thread the private/external attribute through
            // function emission.
            let mut is_static = false;
            let mut is_extern = false;
            match self.peek().kind {
                TokenKind::KwStatic => {
                    self.bump();
                    is_static = true;
                }
                TokenKind::KwExtern => {
                    self.bump();
                    is_extern = true;
                }
                _ => {}
            }
            // K&R implicit-int function definition `name(args) {body}`.
            // No explicit type → default to int. Only fire when the
            // ident is followed by `(`, which is the unambiguous
            // function-decl shape; otherwise fall through to the
            // typed-decl probe so typedef-named globals still work.
            // Fixture 2163.
            if let TokenKind::Ident(name) = &self.peek().kind
                && !self.typedefs.contains_key(name)
                && matches!(self.peek_n(1).kind, TokenKind::LParen)
            {
                let idx = functions.len();
                let mut f = self.parse_function()?;
                f.is_static = is_static;
                functions.push(f);
                decl_order.push(TopLevelRef::Function(idx));
                continue;
            }
            // Otherwise this top-level item is either a function or
            // a global. Probe past the type to find the declarator
            // name and decide.
            let mut probe = 0usize;
            // `const`, `volatile`, `register` are discardable
            // qualifiers — accept any number of them before the type
            // prefix. Fixtures 475, 476, 477.
            while matches!(
                self.peek_n(probe).kind,
                TokenKind::KwConst | TokenKind::KwVolatile | TokenKind::KwRegister
            ) {
                probe += 1;
            }
            // Skip the type prefix (int/char/struct ...). For
            // struct types we need to skip the `struct` keyword
            // plus the tag (and the inline definition braces if
            // any, but those would have been consumed by the
            // bare-struct path above).
            match self.peek_n(probe).kind {
                TokenKind::KwInt
                | TokenKind::KwChar
                | TokenKind::KwVoid
                | TokenKind::KwFloat
                | TokenKind::KwDouble => probe += 1,
                TokenKind::KwUnsigned | TokenKind::KwLong | TokenKind::KwSigned => {
                    probe += 1;
                    // `unsigned long`, `long unsigned`, `signed long`,
                    // `long signed` are all valid pairings — consume
                    // the partner keyword if present.
                    if matches!(
                        self.peek_n(probe).kind,
                        TokenKind::KwLong | TokenKind::KwUnsigned | TokenKind::KwSigned
                    ) {
                        probe += 1;
                    }
                    // `unsigned char` / `signed char` — single-byte form.
                    if matches!(self.peek_n(probe).kind, TokenKind::KwChar) {
                        probe += 1;
                    }
                    if matches!(self.peek_n(probe).kind, TokenKind::KwInt) {
                        probe += 1;
                    }
                }
                TokenKind::KwStruct | TokenKind::KwUnion => {
                    probe += 1;
                    if matches!(self.peek_n(probe).kind, TokenKind::Ident(_)) {
                        probe += 1;
                    }
                    // Combined `struct <tag> { ... } <decl>;` shape: the
                    // brace-enclosed body sits between the tag and the
                    // declarator. Skip-balance over it so the probe lands
                    // on the actual declarator name. Fixtures 3420 (bit
                    // fields, body is filtered later) and 3443 (`struct
                    // S { int x; } s;`).
                    if matches!(self.peek_n(probe).kind, TokenKind::LBrace) {
                        let mut depth = 0i32;
                        loop {
                            match self.peek_n(probe).kind {
                                TokenKind::LBrace => {
                                    depth += 1;
                                    probe += 1;
                                }
                                TokenKind::RBrace => {
                                    depth -= 1;
                                    probe += 1;
                                    if depth == 0 {
                                        break;
                                    }
                                }
                                TokenKind::Eof => break,
                                _ => probe += 1,
                            }
                        }
                    }
                }
                TokenKind::KwEnum => {
                    // `enum <tag> <decl>` — enum tag as a type. The
                    // standalone `enum [tag] { ... };` form is handled
                    // by the dispatcher above and never reaches here.
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
            // Skip BCC cc-modifier keywords (`pascal`, `cdecl`, ...)
            // — they sit between the type and the declarator name.
            // Fixtures 1653, 1654, 1655, 1656.
            while let TokenKind::Ident(name) = &self.peek_n(probe).kind {
                if matches!(
                    name.as_str(),
                    "pascal" | "cdecl" | "far" | "near" | "huge" | "interrupt"
                    | "_pascal" | "_cdecl" | "_far" | "_near" | "_huge" | "_interrupt"
                    | "__pascal" | "__cdecl" | "__far" | "__near" | "__huge" | "__interrupt"
                ) {
                    probe += 1;
                } else {
                    break;
                }
            }
            // Function-pointer global declarator: `T (*name)(...)`.
            // Route to parse_global, which already knows how to parse
            // the `(*name)(...)` shape (fixtures 2607, 3212, 3567,
            // 3643). Function-returning-function-pointer declarator
            // `T (*name(args))(more)` looks the same in the prefix
            // but has `(` (not `)`) after `name` — that case routes
            // to parse_function. Fixtures 2336, 3255, 3324.
            if matches!(self.peek_n(probe).kind, TokenKind::LParen)
                && matches!(self.peek_n(probe + 1).kind, TokenKind::Star)
            {
                let after_name_is_lparen = matches!(
                    self.peek_n(probe + 2).kind,
                    TokenKind::Ident(_),
                ) && matches!(
                    self.peek_n(probe + 3).kind,
                    TokenKind::LParen,
                );
                if after_name_is_lparen {
                    let idx = functions.len();
                    let mut f = self.parse_function()?;
                    f.is_static = is_static;
                    functions.push(f);
                    decl_order.push(TopLevelRef::Function(idx));
                    continue;
                }
                let new_globals = self.parse_global(is_static, is_extern)?;
                for g in new_globals {
                    let idx = globals.len();
                    globals.push(g);
                    decl_order.push(TopLevelRef::Global(idx));
                }
                continue;
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
                // `extern T f(...);` is a function prototype, not a
                // definition — the `extern` keyword is harmless for
                // function decls (declarations are extern by default).
                // parse_function already handles the body-less prototype
                // shape, so just route through it. Fixtures 1741, 2153.
                let idx = functions.len();
                let mut f = self.parse_function()?;
                f.is_static = is_static;
                functions.push(f);
                decl_order.push(TopLevelRef::Function(idx));
            } else {
                let new_globals = self.parse_global(is_static, is_extern)?;
                for g in new_globals {
                    let idx = globals.len();
                    globals.push(g);
                    decl_order.push(TopLevelRef::Global(idx));
                }
            }
        }
        // Append hoisted static locals after regular globals, keeping
        // their relative source order. They aren't in `decl_order` —
        // the LIFO `public` list only covers top-level symbols, and
        // statics are private to this TU.
        globals.extend(std::mem::take(&mut self.pending_static_locals));
        Ok(Unit { functions, globals, decl_order })
    }
    /// `<type-base> <pointer-stars>* <name> ('[' <int> ']')? [= <expr>] ;`
    /// at the top level. Same declarator shape as a local declaration
    /// (`parse_declare`); the difference is the resulting AST node
    /// (`Global` vs. `StmtKind::Declare`) and the absence of an
    /// enclosing function context.
    pub(crate) fn parse_global(&mut self, is_static: bool, is_extern: bool) -> Result<Vec<Global>, ParseError> {
        let start = self.peek().span.start;
        let base_ty = self.parse_type()?;
        let mut globals = Vec::new();
        loop {
            // Per-declarator pointer stars: `int *a, b;` makes `a`
            // an `int*` and `b` a plain `int`. Capture `far` / `huge`
            // / `near` so each level becomes FarPointer / NearPointer
            // when qualified.
            let (_, mut is_far, mut is_huge, _, mut is_near, mut seg, mut is_seg) =
                self.consume_cc_modifiers_seg();
            let mut ty = base_ty.clone();
            while matches!(self.peek().kind, TokenKind::Star) {
                self.bump();
                ty = Self::make_ptr_ty_seg(ty, is_far, is_huge, is_near, seg, is_seg);
                let (_, f, h, _, n, s, sg) = self.consume_cc_modifiers_seg();
                is_far = f;
                is_huge = h;
                is_near = n;
                seg = s;
                is_seg = sg;
            }
            // Function-pointer global declarator: `int (*name)(...)`.
            // Mirrors the param / local paths — the signature isn't
            // modeled, so the variable's type collapses to a generic
            // near pointer (2 bytes). Fixtures 2607, 3212, 3567,
            // 3643.
            let (name, name_end) = if matches!(self.peek().kind, TokenKind::LParen) {
                let (fp_name, fp_ty) = self.parse_func_ptr_declarator(ty.clone())?;
                ty = fp_ty;
                let end = self.peek().span.start;
                (fp_name, end)
            } else {
                let name_tok = self.bump();
                let TokenKind::Ident(name) = &name_tok.kind else {
                    return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
                };
                (name.clone(), name_tok.span.end)
            };
            // Array suffix. `[N]` gives an explicit count; `[]`
            // defers the count until an initializer is seen
            // (fixture 191's `char s[] = "hi";` → len 3).
            // Array suffix tail. First `[N]` (or `[]`) is special-
            // cased: `[]` defers the length to be inferred from the
            // initializer. Subsequent dimensions must be explicit.
            // Multi-dim wraps innermost-first like `parse_declare`
            // so `int a[2][3]` becomes `Array{2, Array{3, Int}}`.
            // Fixture 492. Multi-dim flex (`int m[][3]`): only the
            // outermost dim can be `[]`; the rest carry explicit
            // sizes. We remember the marker and fill it after the
            // trailing dims wrap into ty. Fixture 2763.
            let mut inferred_len_marker: bool = false;
            let mut array_lens: Vec<u32> = Vec::new();
            let mut first_suffix = true;
            while matches!(self.peek().kind, TokenKind::LBracket) {
                self.bump();
                if first_suffix && matches!(self.peek().kind, TokenKind::RBracket) {
                    self.bump();
                    inferred_len_marker = true;
                    first_suffix = false;
                    continue;
                }
                let size_tok = self.bump();
                // Enum constants are accepted as array sizes —
                // `enum { N = 4 }; int a[N];`. Fixture 1004.
                let len = match &size_tok.kind {
                    TokenKind::IntLit(n) => *n,
                    TokenKind::Ident(name) => {
                        let Some(v) = self.enum_constants.get(name).copied() else {
                            return Err(ParseError::Unexpected {
                                expected: "array size (integer literal or enum constant)"
                                    .to_owned(),
                                found: size_tok.kind.describe().to_owned(),
                                offset: size_tok.span.start,
                            });
                        };
                        v
                    }
                    _ => {
                        return Err(ParseError::Unexpected {
                            expected: "array size (integer literal or enum constant)".to_owned(),
                            found: size_tok.kind.describe().to_owned(),
                            offset: size_tok.span.start,
                        });
                    }
                };
                self.expect(&TokenKind::RBracket)?;
                array_lens.push(len);
                first_suffix = false;
            }
            for len in array_lens.into_iter().rev() {
                ty = Type::Array { elem: Box::new(ty), len };
            }
            let init = if matches!(self.peek().kind, TokenKind::Equals) {
                self.bump();
                Some(self.parse_initializer()?)
            } else {
                None
            };
            if inferred_len_marker {
                let len = match init.as_ref().map(|i| &i.kind) {
                    Some(ExprKind::StringLit(bytes)) => {
                        u32::try_from(bytes.len() + 1).expect("string length fits in u32")
                    }
                    Some(ExprKind::InitList { items }) => {
                        u32::try_from(items.len()).expect("init count fits in u32")
                    }
                    // \`extern T arr[];\` — incomplete-array declared
                    // somewhere else. We can't know the length, but
                    // codegen only needs a placeholder type — the
                    // size never goes into the OBJ since the storage
                    // lives in another TU. Fixture 2312.
                    None if is_extern => 0,
                    _ => {
                        let t = self.peek();
                        return Err(ParseError::Unexpected {
                            expected: "initializer to infer array length from `[]`".to_owned(),
                            found: "no initializer or unsupported init form".to_owned(),
                            offset: t.span.start,
                        });
                    }
                };
                // Wrap the already-built inner type (with trailing
                // explicit dims) in the outermost inferred dim.
                ty = Type::Array { elem: Box::new(ty), len };
            }
            self.global_types.insert(name.clone(), ty.clone());
            globals.push(Global {
                name,
                ty,
                init,
                is_static,
                is_extern,
                span: Span::new(start, name_end),
            });
            // Multi-declarator continuation: `int a, b, c;`. Each
            // tail declarator re-applies pointer stars to a fresh
            // copy of the base type. Fixture 478.
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect(&TokenKind::Semicolon)?;
        Ok(globals)
    }
    pub(crate) fn parse_function(&mut self) -> Result<Function, ParseError> {
        let start = self.peek().span.start;
        // Each function has its own static-local namespace. Reset
        // the rename map so a `static int counter` here doesn't see
        // the previous function's `counter@N` redirect.
        self.current_static_renames.clear();
        self.block_scopes.clear();
        self.block_scopes.push(HashMap::new());
        // K&R implicit-int return type: the first token is an
        // identifier that *isn't* a typedef name (typedefs go through
        // the regular parse_type path). The C89 default is `int`.
        // Fixture 2163 (`helper(int x) { return x + 1; }`).
        let implicit_int = matches!(&self.peek().kind, TokenKind::Ident(n)
            if !self.typedefs.contains_key(n))
            && matches!(self.peek_n(1).kind, TokenKind::LParen);
        // Parse the return type via the standard `parse_type` path.
        // `int`, `long`, etc. all flow through here; fixture 212
        // introduced the first non-int return type (`long get()`).
        let mut ret_ty = if implicit_int {
            Type::Int
        } else {
            self.parse_type()?
        };
        // Pointer stars between the return type and the function
        // name — `int *f(void) { ... }`. Fixture 496.
        while matches!(self.peek().kind, TokenKind::Star) {
            self.bump();
            ret_ty = Type::Pointer(Box::new(ret_ty));
        }
        let (is_pascal, is_far, is_interrupt, is_near) = self.consume_cc_modifiers_collect();
        // Function-returning-function-pointer declarator:
        // `<ret> (* <name> ( <params> )) ( <fp-params> )`. We don't
        // model function signatures — collapse the return type to a
        // generic near pointer (2 bytes) and skip the trailing
        // `(<fp-params>)` after the body's `)`. The function's own
        // parameter list (the inner `()`) is parsed normally below.
        // Fixtures 2336, 3255, 3324.
        let returns_fn_ptr =
            matches!(self.peek().kind, TokenKind::LParen)
                && matches!(self.peek_n(1).kind, TokenKind::Star)
                && matches!(self.peek_n(2).kind, TokenKind::Ident(_));
        if returns_fn_ptr {
            self.bump(); // (
            self.bump(); // *
            ret_ty = Type::Pointer(Box::new(Type::Int));
        }
        let name_tok = self.bump();
        let TokenKind::Ident(name) = &name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        let name = name.clone();
        self.expect(&TokenKind::LParen)?;
        let (mut params, _is_ansi_proto) = self.parse_param_list()?;
        self.expect(&TokenKind::RParen)?;
        // If this declaration is a function returning a function
        // pointer, we already consumed the wrapping `(* name(...))`.
        // Now skip the trailing `)( <fp-params> )` that follows.
        if returns_fn_ptr {
            self.expect(&TokenKind::RParen)?;
            self.expect(&TokenKind::LParen)?;
            let mut depth: u32 = 1;
            while depth > 0 {
                let t = self.bump();
                match t.kind {
                    TokenKind::LParen => depth += 1,
                    TokenKind::RParen => depth -= 1,
                    TokenKind::Eof => break,
                    _ => {}
                }
            }
        }
        // Reset the per-function local-type map and seed it with the
        // params so `sizeof <param>` resolves inside the body.
        self.function_locals.clear();
        for p in &params {
            self.function_locals.insert(p.name.clone(), p.ty.clone());
        }

        // K&R-style param-type declarations: a sequence of
        // `<type> <name>;` between `)` and `{`. Each names a
        // parameter from the bare-ident list and supplies its
        // type. The bare-ident params were inserted with a
        // placeholder `Int` type by `parse_param_list`; we
        // overwrite as the declarations arrive. (BC2's headers
        // use this form extensively.)
        while !matches!(
            self.peek().kind,
            TokenKind::LBrace | TokenKind::Semicolon | TokenKind::Eof,
        ) {
            let base_ty = self.parse_type()?;
            // Comma-separated declarators share the base type:
            // `int a, b;` declares both as int. Fixture 2164.
            loop {
                let mut ty = base_ty.clone();
                while matches!(self.peek().kind, TokenKind::Star) {
                    self.bump();
                    ty = Type::Pointer(Box::new(ty));
                }
                let name_tok = self.bump();
                let TokenKind::Ident(pname) = &name_tok.kind else {
                    return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
                };
                let pname = pname.clone();
                if let Some(p) = params.iter_mut().find(|p| p.name == pname) {
                    p.ty = ty.clone();
                } else {
                    return Err(ParseError::Unexpected {
                        expected: "K&R type for a declared parameter".to_owned(),
                        found: format!("declaration of `{pname}` which isn't in the param list"),
                        offset: name_tok.span.start,
                    });
                }
                self.function_locals.insert(pname, ty);
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            self.expect(&TokenKind::Semicolon)?;
        }

        // Prototype (just `;` after the param list) vs definition
        // (a `{ ... }` body). Fixture 097 has the prototype form.
        if matches!(self.peek().kind, TokenKind::Semicolon) {
            let semi = self.bump();
            return Ok(Function {
                name,
                params,
                ret_ty,
                span: Span::new(start, semi.span.end),
                body: None,
                is_static: false,
                is_pascal,
                is_far,
                is_interrupt,
                is_near,
            });
        }

        self.expect(&TokenKind::LBrace)?;
        let mut body = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            body.push(self.parse_stmt()?);
            body.extend(std::mem::take(&mut self.pending_extra_stmts));
        }
        let close = self.expect(&TokenKind::RBrace)?;
        let span = Span::new(start, close.span.end);
        Ok(Function {
            name,
            params,
            ret_ty,
            span,
            body: Some(body),
            is_static: false,
            is_pascal,
            is_far,
            is_interrupt,
            is_near,
        })
    }
    /// Parameter list inside the `(...)` of a function definition.
    /// Two shapes are recognized:
    ///
    /// - `void` — the C spelling for "no parameters". Returns empty.
    /// - `<type> <name> (, <type> <name>)*` — one or more typed params.
    ///
    /// The caller is responsible for the surrounding parens.
    /// Parse an initializer expression — either a plain scalar
    /// `parse_expr`, or a `{ <expr>, <expr>, ... }` aggregate list.
    /// The list form is currently only meaningful at file scope
    /// against an array type (fixture 189); the parser accepts it
    /// anywhere a Declare/Global init slot allows, and the codegen
    /// rejects unsupported shapes downstream.
    pub(crate) fn parse_initializer(&mut self) -> Result<Expr, ParseError> {
        if !matches!(self.peek().kind, TokenKind::LBrace) {
            return self.parse_expr();
        }
        let lbrace = self.bump();
        let mut items = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RBrace) {
            loop {
                // Recurse for nested braces — multi-dim array
                // initializers like `int a[2][3] = {{1,2,3},
                // {4,5,6}};` (fixture 492) embed `InitList` inside
                // `InitList`.
                items.push(self.parse_initializer()?);
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                    // Allow a trailing comma before `}`.
                    if matches!(self.peek().kind, TokenKind::RBrace) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        let close = self.expect(&TokenKind::RBrace)?;
        Ok(Expr {
            kind: ExprKind::InitList { items },
            span: Span::new(lbrace.span.start, close.span.end),
        })
    }
    /// Returns `(params, has_ansi_typed_param)` — the bool is true
    /// when the parameter list named at least one typed parameter
    /// (e.g. `int f(int x)` — fixture 535). Empty `(void)` / `()` and
    /// K&R bare-ident lists both report `false`, since neither pins
    /// a prototype signature at the call site.
    pub(crate) fn parse_param_list(&mut self) -> Result<(Vec<Param>, bool), ParseError> {
        // `(void)` — empty param list. Distinguished from `(void *p)`
        // by 1-token lookahead: the latter has a `*` or other type
        // suffix after the keyword.
        if matches!(self.peek().kind, TokenKind::KwVoid)
            && matches!(self.peek_n(1).kind, TokenKind::RParen)
        {
            self.bump();
            return Ok((Vec::new(), false));
        }
        // Empty list `()` — no params declared. Accepts both prototype
        // and K&R callers that pass through.
        if matches!(self.peek().kind, TokenKind::RParen) {
            return Ok((Vec::new(), false));
        }
        // K&R-style: the first token is a plain identifier (and NOT a
        // typedef-name acting as a type). Parse a bare comma-separated
        // ident list and seed each param with a placeholder `Int` type;
        // the post-`)` type-declaration block in `parse_function` will
        // overwrite the placeholder where it has a matching entry.
        if let TokenKind::Ident(ref name) = self.peek().kind
            && !self.typedefs.contains_key(name)
        {
            let mut params = Vec::new();
            loop {
                let name_tok = self.bump();
                let TokenKind::Ident(name) = name_tok.kind else {
                    return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
                };
                params.push(Param { name, ty: Type::Int });
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            return Ok((params, false));
        }
        let mut params = Vec::new();
        loop {
            // Variadic marker `...`. The lexer doesn't have a dedicated
            // ellipsis token, so this is three consecutive Dots. We
            // record varargs as a flag on the function (currently just
            // consumed to satisfy parsing — codegen doesn't materialize
            // the variadic ABI yet). Fixtures 2153, 2194.
            if matches!(self.peek().kind, TokenKind::Dot)
                && matches!(self.peek_n(1).kind, TokenKind::Dot)
                && matches!(self.peek_n(2).kind, TokenKind::Dot)
            {
                self.bump();
                self.bump();
                self.bump();
                return Ok((params, true));
            }
            let mut ty = self.parse_type()?;
            let (_, mut is_far, mut is_huge, _, mut is_near, mut seg, mut is_seg) =
                self.consume_cc_modifiers_seg();
            // Pointer stars wrap the base type, just like in a local
            // declaration (fixture 095: `int sum(int *p)`).
            while matches!(self.peek().kind, TokenKind::Star) {
                self.bump();
                ty = Self::make_ptr_ty_seg(ty, is_far, is_huge, is_near, seg, is_seg);
                let (_, f, h, _, n, s, sg) = self.consume_cc_modifiers_seg();
                is_far = f;
                is_huge = h;
                is_near = n;
                seg = s;
                is_seg = sg;
            }
            // Function-pointer parameter: `<ret> ( * <name> ) ( <params> )`.
            // Mirrors the local-declaration path — we don't model the
            // function signature, so the param's type collapses to a
            // generic near pointer (2 bytes, int-pool-eligible).
            // Fixture 3671 (`int apply(int (*op)(int, int), int a, int b)`).
            if matches!(self.peek().kind, TokenKind::LParen) {
                let (fp_name, fp_ty) = self.parse_func_ptr_declarator(ty.clone())?;
                params.push(Param { name: fp_name, ty: fp_ty });
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                    continue;
                }
                break;
            }
            // Anonymous parameter — common in prototypes
            // (`int helper(int);`, fixture 506). Synthesize a unique
            // placeholder name; codegen ignores prototype params.
            let name = if matches!(
                self.peek().kind,
                TokenKind::Comma | TokenKind::RParen,
            ) {
                format!("__anon_param_{}", params.len())
            } else {
                let name_tok = self.bump();
                let TokenKind::Ident(name) = name_tok.kind else {
                    return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
                };
                name
            };
            // `int a[]` / `int a[N]` / `int a[N][M]...` parameter
            // syntax. C decays the OUTERMOST dimension to a pointer
            // and preserves the inner dims as part of the pointer's
            // pointee type. We mirror that: collect explicit inner
            // sizes into an Array chain, then wrap in Pointer.
            // Fixtures 1236 (`int sum(int a[])`), 1239 (char arr),
            // 2236 (`int sum(int m[3][3])` — pointee = Array{3,
            // Int}), 2487, 2493.
            if matches!(self.peek().kind, TokenKind::LBracket) {
                let mut dims: Vec<Option<u32>> = Vec::new();
                while matches!(self.peek().kind, TokenKind::LBracket) {
                    self.bump();
                    let size = if matches!(self.peek().kind, TokenKind::RBracket) {
                        None
                    } else {
                        let size_tok = self.bump();
                        let n = match &size_tok.kind {
                            TokenKind::IntLit(v) => Some(*v),
                            _ => None,
                        };
                        // Skip any extra tokens until ']'.
                        while !matches!(
                            self.peek().kind,
                            TokenKind::RBracket | TokenKind::Eof
                        ) {
                            self.bump();
                        }
                        n
                    };
                    self.expect(&TokenKind::RBracket)?;
                    dims.push(size);
                }
                // Inner dims (everything except the outermost) wrap
                // the element type from innermost-out. The outermost
                // dim then decays to a pointer to that chain.
                if dims.len() > 1 {
                    for d in dims.iter().skip(1).rev() {
                        let len = d.unwrap_or(0);
                        ty = Type::Array { elem: Box::new(ty), len };
                    }
                }
                ty = Type::Pointer(Box::new(ty));
            }
            // Typedef-array param (`Vec v` where Vec is int[3]) —
            // C decays array-typed parameters to pointer-to-element.
            // The `int v[]` syntactic case is handled above; this
            // covers the case where the array type comes through
            // typedef expansion. Fixture 2497.
            let ty = if let Type::Array { elem, .. } = ty {
                Type::Pointer(elem)
            } else {
                ty
            };
            params.push(Param { name, ty });
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        Ok((params, true))
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
    pub(crate) fn parse_declare(&mut self, start: u32) -> Result<Stmt, ParseError> {
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
        // Optional `register` prefix — recorded so the locals
        // allocator can lower its enregister threshold. C allows
        // `register` anywhere in the type-qualifier soup, but BCC
        // only honors it at the start of the declarator; we accept
        // it here and also let `parse_type` drop a second one
        // silently. Fixtures 1550, 1560.
        //
        // Optional `volatile` prefix — recorded so the locals
        // allocator forces a stack slot regardless of use count.
        // Each read/write touches memory. Fixtures 1548, 2243.
        let mut is_register = false;
        let mut is_volatile = false;
        loop {
            match self.peek().kind {
                TokenKind::KwRegister => {
                    self.bump();
                    is_register = true;
                }
                TokenKind::KwVolatile => {
                    self.bump();
                    is_volatile = true;
                }
                _ => break,
            }
        }
        let base_ty = self.parse_type()?;
        // BCC cc/memory-model modifiers between the type and the
        // pointer-star/declarator (`int far *p`, `int huge *p`,
        // `int near *p`). Capture all three qualifier flags so the
        // pointer level picks the right variant. Fixtures 1649,
        // 1650, 1651, 1652, 2058, 2250 (far / huge); 1748 (near in
        // far-data model).
        let (_, mut is_far, mut is_huge, _, mut is_near, mut seg, mut is_seg) =
            self.consume_cc_modifiers_seg();
        // Pointer stars wrap the base type: `int **pp` is `Pointer(Pointer(Int))`.
        // Stars are per-declarator — `int *a, b;` makes `a` an `int*`
        // and `b` a plain `int`, so we keep `base_ty` clean for the
        // multi-declarator tail loop and decorate a separate `ty`
        // copy for this first declarator.
        let mut ty = base_ty.clone();
        while matches!(self.peek().kind, TokenKind::Star) {
            self.bump();
            ty = Self::make_ptr_ty_seg(ty, is_far, is_huge, is_near, seg, is_seg);
            // `T far *` / `T *far` — modifiers can also appear AFTER
            // a pointer star. Consume them and let the next `*` (if
            // any) pick them up. Also accept `T * const p`
            // (const-qualified pointer). Fixture 2380.
            let (_, f, h, _, n, s, sg) = self.consume_cc_modifiers_seg();
            is_far = f;
            is_huge = h;
            is_near = n;
            seg = s;
            is_seg = sg;
            while matches!(
                self.peek().kind,
                TokenKind::KwConst | TokenKind::KwVolatile
            ) {
                self.bump();
            }
        }
        // Function-pointer declarator: `<type> ( * <name> ) ( <params> )`.
        // For fixture 110 (`int (*p)(void) = f;`) we don't need to model
        // the function signature — calls through `p` work the same
        // regardless of return type, and we never dereference it. So we
        // collapse the type to `Pointer<Int>` (any pointer is 2 bytes,
        // int-pool-eligible) and skip the param list.
        if matches!(self.peek().kind, TokenKind::LParen) {
            let (name, fp_ty) = self.parse_func_ptr_declarator(ty.clone())?;
            return self.finish_declare(start, base_ty, fp_ty, name, is_static, is_register, is_volatile);
        }
        let name_tok = self.bump();
        let TokenKind::Ident(name) = &name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        let name = name.clone();
        // Array suffix: `[<int-literal>]`, repeated for multi-dim.
        // Lengths are collected left-to-right, then wrapped innermost-
        // first so `int a[2][3]` becomes `Array{2, Array{3, Int}}` —
        // i.e. `a[i]` yields an `int[3]`. `[]` (empty) is permitted
        // on the outermost dimension only, marking the size as
        // "infer from initializer". Fixtures 1712, 1883, 2094.
        let mut array_lens: Vec<Option<u32>> = Vec::new();
        while matches!(self.peek().kind, TokenKind::LBracket) {
            self.bump();
            if matches!(self.peek().kind, TokenKind::RBracket) {
                self.bump();
                array_lens.push(None);
                continue;
            }
            // Accept a constant expression for the dimension (`a[3+2]`,
            // `a[N*4]` for an enum/macro constant). The expression is
            // const-folded at parse time; non-foldable forms still
            // error. Fixture 1226 (`int a[3+2];`).
            let size_start = self.peek().span.start;
            let size_expr = self.parse_expr()?;
            let len = crate::codegen::fold::try_const_eval(&size_expr)
                .ok_or_else(|| ParseError::Unexpected {
                    expected: "array size (constant expression)".to_owned(),
                    found: "non-constant expression".to_owned(),
                    offset: size_start,
                })?;
            self.expect(&TokenKind::RBracket)?;
            array_lens.push(Some(len));
        }
        // Reverse to innermost-first wrapping. Only the OUTERMOST
        // dimension may be `None` (`[]`); inner `[]` is illegal C.
        // The outermost is the last in the iter-reversed walk.
        let mut deferred_outer_unsized = false;
        let mut wrapped_lens: Vec<u32> = Vec::with_capacity(array_lens.len());
        let mut iter = array_lens.into_iter().peekable();
        while let Some(len_opt) = iter.next() {
            match len_opt {
                Some(len) => wrapped_lens.push(len),
                None => {
                    if iter.peek().is_some() {
                        // `int a[][3]` etc. — inner dim unspecified.
                        return Err(ParseError::Unexpected {
                            expected: "array size (integer literal)".to_owned(),
                            found: "`]` (empty brackets are only allowed on the outermost array dimension)".to_owned(),
                            offset: self.peek().span.start,
                        });
                    }
                    deferred_outer_unsized = true;
                    // Use 0 as placeholder; finish_declare fills it
                    // from the initializer.
                    wrapped_lens.push(0);
                }
            }
        }
        for len in wrapped_lens.into_iter().rev() {
            ty = Type::Array { elem: Box::new(ty), len };
        }
        self.finish_declare_unsized(start, base_ty, ty, name, is_static, is_register, is_volatile, deferred_outer_unsized)
    }
    pub(crate) fn finish_declare_unsized(
        &mut self,
        start: u32,
        base_ty: Type,
        mut ty: Type,
        name: String,
        is_static: bool,
        is_register: bool,
        is_volatile: bool,
        deferred_outer_unsized: bool,
    ) -> Result<Stmt, ParseError> {
        if !deferred_outer_unsized {
            return self.finish_declare(start, base_ty, ty, name, is_static, is_register, is_volatile);
        }
        // Need to peek at the initializer to determine the array
        // size. Require `= <init>`.
        if !matches!(self.peek().kind, TokenKind::Equals) {
            return Err(ParseError::Unexpected {
                expected: "`=` (\"[]\" requires an initializer to determine size)".to_owned(),
                found: self.peek().kind.describe().to_owned(),
                offset: self.peek().span.start,
            });
        }
        // Look ahead at the initializer to figure out the size.
        // Save parser state, parse the init, restore.
        let saved_pos = self.pos;
        self.bump(); // consume `=`
        let init_peek = self.parse_initializer()?;
        // Restore position so finish_declare re-parses the same init.
        self.pos = saved_pos;
        // Compute the outer-dim size from the initializer shape.
        // `Array { elem: <inner>, len: 0 }` — fill len.
        let len = match &init_peek.kind {
            ExprKind::StringLit(bytes) => (bytes.len() as u32) + 1,
            ExprKind::InitList { items } => items.len() as u32,
            _ => {
                return Err(ParseError::Unexpected {
                    expected: "initializer list or string literal for unsized array".to_owned(),
                    found: "scalar expression".to_owned(),
                    offset: init_peek.span.start,
                });
            }
        };
        if let Type::Array { elem, len: l } = &mut ty {
            *l = len;
            let _ = elem;
        }
        self.finish_declare(start, base_ty, ty, name, is_static, is_register, is_volatile)
    }
    /// Common tail of `parse_declare` after the declarator (name +
    /// pointer/array/func-ptr decoration) is known. Reads the optional
    /// initializer and trailing semicolon, then yields a `Declare`
    /// statement. When `is_static` is true, also pushes a synthetic
    /// `Global` so the name is allocated in `_DATA` instead of on
    /// the stack frame.
    pub(crate) fn finish_declare(
        &mut self,
        start: u32,
        base_ty: Type,
        ty: Type,
        name: String,
        is_static: bool,
        is_register: bool,
        is_volatile: bool,
    ) -> Result<Stmt, ParseError> {
        let init = if matches!(self.peek().kind, TokenKind::Equals) {
            self.bump();
            // `parse_initializer` accepts a braced `{ … }` initializer
            // for aggregate locals (fixture 493: static int a[3] =
            // {10, 20, 30};). Falls through to `parse_expr` for the
            // scalar case.
            Some(self.parse_initializer()?)
        } else {
            None
        };
        // Multi-declarator: `int i, j, sum;`. Each comma-separated
        // tail declarator re-applies pointer stars and array suffix
        // to a fresh copy of the base type and produces its own
        // `Stmt::Declare`, pushed onto the pending queue so the
        // block-parse drain picks them up after the primary stmt.
        while matches!(self.peek().kind, TokenKind::Comma) {
            self.bump();
            let mut tail_ty = base_ty.clone();
            while matches!(self.peek().kind, TokenKind::Star) {
                self.bump();
                tail_ty = Type::Pointer(Box::new(tail_ty));
            }
            let tail_name_tok = self.bump();
            let TokenKind::Ident(tail_name) = &tail_name_tok.kind else {
                return Err(ParseError::NotAnIdent { offset: tail_name_tok.span.start });
            };
            let tail_name = tail_name.clone();
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
                tail_ty = Type::Array { elem: Box::new(tail_ty), len };
            }
            let tail_init = if matches!(self.peek().kind, TokenKind::Equals) {
                self.bump();
                Some(self.parse_initializer()?)
            } else {
                None
            };
            let tail_span = Span::new(tail_name_tok.span.start, tail_name_tok.span.end);
            if is_static {
                // Static locals act like globals at the codegen level
                // (file-scope storage, linker-resolved address), so
                // register the name in global_types as well. This
                // lets `&<static_arr>[K]` and other globals-table
                // consumers find it. Fixtures 2241, 2269.
                //
                // To avoid name collisions across functions that
                // share a static identifier (e.g., two functions
                // each with `static int counter`), append a unique
                // suffix and record the rename so body Idents in
                // *this* function resolve to the unique global.
                // Fixture 2264.
                let unique = if self.global_types.contains_key(&tail_name) {
                    self.static_local_counter += 1;
                    let renamed = format!("{}@{}", tail_name, self.static_local_counter);
                    self.current_static_renames.insert(tail_name.clone(), renamed.clone());
                    renamed
                } else {
                    tail_name.clone()
                };
                self.global_types.insert(unique.clone(), tail_ty.clone());
                self.pending_static_locals.push(Global {
                    name: unique.clone(),
                    ty: tail_ty.clone(),
                    init: tail_init,
                    is_static: true,
                    is_extern: false,
                    span: tail_span,
                });
                self.pending_extra_stmts.push(Stmt {
                    kind: StmtKind::Declare {
                        ty: tail_ty,
                        name: unique,
                        init: None,
                        is_static: true,
                        is_register: false,
                        is_volatile: false,
                    },
                    span: tail_span,
                });
            } else {
                let unique = self.rename_shadowed_local(&tail_name);
                self.function_locals.insert(unique.clone(), tail_ty.clone());
                self.pending_extra_stmts.push(Stmt {
                    kind: StmtKind::Declare {
                        ty: tail_ty,
                        name: unique,
                        init: tail_init,
                        is_static: false,
                        is_register,
                        is_volatile,
                    },
                    span: tail_span,
                });
            }
        }
        let semi = self.expect(&TokenKind::Semicolon)?;
        let span = Span::new(start, semi.span.end);
        if is_static {
            // Static locals act like globals at the codegen level
            // (file-scope storage, linker-resolved address), so
            // register the name in global_types as well. Fixtures
            // 2241, 2269. Unique-suffix the name when it collides
            // with an already-seen global (cross-function static
            // sharing — fixture 2264).
            let unique = if self.global_types.contains_key(&name) {
                self.static_local_counter += 1;
                let renamed = format!("{}@{}", name, self.static_local_counter);
                self.current_static_renames.insert(name.clone(), renamed.clone());
                renamed
            } else {
                name.clone()
            };
            self.global_types.insert(unique.clone(), ty.clone());
            // The Global owns the initializer expression; the Stmt
            // keeps only the name/type/span so codegen can fold the
            // source line into the next comment block. Hoisting moves
            // the init out so we don't need `Expr: Clone`.
            self.pending_static_locals.push(Global {
                name: unique.clone(),
                ty: ty.clone(),
                init,
                is_static: true,
                is_extern: false,
                span,
            });
            return Ok(Stmt {
                kind: StmtKind::Declare {
                    ty,
                    name: unique,
                    init: None,
                    is_static: true,
                    is_register: false,
                    is_volatile: false,
                },
                span,
            });
        }
        let unique = self.rename_shadowed_local(&name);
        self.function_locals.insert(unique.clone(), ty.clone());
        Ok(Stmt {
            kind: StmtKind::Declare {
                ty,
                name: unique,
                init,
                is_static,
                is_register,
                is_volatile,
            },
            span,
        })
    }
    /// Generate a unique name for a non-static local that shadows
    /// either a name in an active outer block scope (registered via
    /// `block_scopes`) or another current-function local. Returns
    /// the original name if no collision, or a `<name>@<N>` form
    /// otherwise (and records the rename in the current scope so
    /// subsequent ident lookups inside this block resolve correctly).
    pub(crate) fn rename_shadowed_local(&mut self, name: &str) -> String {
        let shadows_outer = self
            .block_scopes
            .iter()
            .any(|sc| sc.contains_key(name))
            || self.function_locals.contains_key(name);
        let unique = if shadows_outer {
            self.static_local_counter += 1;
            format!("{}@{}", name, self.static_local_counter)
        } else {
            name.to_string()
        };
        if let Some(top) = self.block_scopes.last_mut() {
            top.insert(name.to_string(), unique.clone());
        }
        unique
    }
    /// Look up an identifier through the active block scopes
    /// (innermost first), returning the renamed name if it matches a
    /// shadowed local, or `None` if no rename applies.
    pub(crate) fn lookup_block_rename(&self, name: &str) -> Option<String> {
        for scope in self.block_scopes.iter().rev() {
            if let Some(renamed) = scope.get(name) {
                return Some(renamed.clone());
            }
        }
        None
    }
    /// Parse `( * <name> ) ( <params> )` (function pointer) or
    /// `( * <name> ) [ N ]...` (pointer-to-array). Returns
    /// `(name, type)`; the type collapses to a generic near pointer
    /// since we don't model function signatures or nested array
    /// types here.
    pub(crate) fn parse_func_ptr_declarator(
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
        // Array-of-function-pointers declarator shape:
        // `<ret> ( * <name> [ N ]... ) ( args )`. Capture the
        // outer-most `[N]` dim so the type becomes `Array{N,
        // Pointer<Int>}` — earlier slices collapsed this to a
        // flat pointer, but the indirect-call path (`arr[i](x)`)
        // needs the array's element-stride to scale the index.
        // Fixtures 2305, 2308, 2343, 2944, 3481, 3696.
        let mut fn_ptr_arr_dim: Option<u32> = None;
        while matches!(self.peek().kind, TokenKind::LBracket) {
            self.bump();
            if fn_ptr_arr_dim.is_none() {
                if let TokenKind::IntLit(n) = self.peek().kind {
                    fn_ptr_arr_dim = Some(u32::try_from(n).unwrap_or(1));
                }
            }
            while !matches!(self.peek().kind, TokenKind::RBracket | TokenKind::Eof) {
                self.bump();
            }
            self.expect(&TokenKind::RBracket)?;
        }
        self.expect(&TokenKind::RParen)?;
        // Pointer-to-array shape: `( * name ) [ N ] [ M ] ...`.
        // Capture the inner dims as a chained `Array{N, ...}` inside
        // the resulting `Pointer(...)`, so codegen sees the true
        // element stride for `(*p)[K]`. Fixtures 2329, 2493.
        if matches!(self.peek().kind, TokenKind::LBracket) {
            let mut dims: Vec<u32> = Vec::new();
            while matches!(self.peek().kind, TokenKind::LBracket) {
                self.bump();
                // Capture a leading integer literal as the dim size;
                // fall back to 1 if absent. Skip remaining tokens up
                // to `]`.
                let mut dim: u32 = 1;
                if let TokenKind::IntLit(n) = self.peek().kind {
                    dim = u32::try_from(n).unwrap_or(1);
                }
                while !matches!(self.peek().kind, TokenKind::RBracket | TokenKind::Eof) {
                    self.bump();
                }
                self.expect(&TokenKind::RBracket)?;
                dims.push(dim);
            }
            let mut ty = Type::Int;
            for dim in dims.into_iter().rev() {
                ty = Type::Array { len: dim, elem: Box::new(ty) };
            }
            return Ok((name, Type::Pointer(Box::new(ty))));
        }
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
        // Function-pointer declarators get tagged with the
        // parser-side `FnPointer` marker. The post-parse pass in
        // `emit_s.rs` rewrites it to either `Pointer` (near-code
        // models: tiny / small / compact — the function lives in
        // the same _TEXT segment as the caller) or `FarPointer`
        // (medium / large / huge — the function pointer is
        // segment:offset). Array-of-fn-pointers wraps the marker
        // in `Array{N, FnPointer}`; the post-pass walks into
        // arrays and rewrites the element type. Fixtures 110,
        // 2211 (medium fn-ptr).
        let ty = match fn_ptr_arr_dim {
            Some(n) => Type::Array { len: n, elem: Box::new(Type::FnPointer) },
            None => Type::FnPointer,
        };
        Ok((name, ty))
    }
}
