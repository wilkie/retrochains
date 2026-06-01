use super::*;

impl Parser {
    /// `enum [tag] { id [= int-lit] [, ...] [,] } ;`. Each member is
    /// registered in `enum_constants`; the next value defaults to
    /// `previous + 1` (or 0 for the first), explicit initializers
    /// reset the counter. No AST node produced — references to the
    /// member names fold to `IntLit` in `parse_primary`.
    pub(crate) fn parse_enum_decl(&mut self) -> Result<(), ParseError> {
        self.bump(); // `enum`
        // Optional tag — we don't yet use it as a type, but accept it
        // so the keyword form `enum colors { RED, GREEN };` parses.
        if matches!(self.peek().kind, TokenKind::Ident(_)) {
            self.bump();
        }
        self.parse_enum_body()?;
        self.expect(&TokenKind::Semicolon)?;
        Ok(())
    }
    /// Parse the `{ NAME [= int] (, NAME [= int])* [,] }` body of an
    /// enum declaration. The caller has already consumed `enum
    /// [<tag>]`. Registers each member in `enum_constants` and
    /// consumes through the closing `}`. Used both by the standalone
    /// `enum [tag] { … };` form (via `parse_enum_decl`) and by the
    /// type-position `enum [tag] { … } <decl>` form (via
    /// `parse_type`).
    pub(crate) fn parse_enum_body(&mut self) -> Result<(), ParseError> {
        self.expect(&TokenKind::LBrace)?;
        let mut next: u32 = 0;
        loop {
            if matches!(self.peek().kind, TokenKind::RBrace) {
                break;
            }
            let name_tok = self.bump();
            let TokenKind::Ident(name) = name_tok.kind else {
                return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
            };
            let value = if matches!(self.peek().kind, TokenKind::Equals) {
                self.bump();
                // Accept an optional leading `-` for negative enum
                // values (`NEG = -1`). Fixtures 2716, 3424. Positive
                // literals come through directly. Wider const-expression
                // initializers aren't fixture-tested yet.
                let negate = if matches!(self.peek().kind, TokenKind::Minus) {
                    self.bump();
                    true
                } else {
                    false
                };
                let lit_tok = self.bump();
                let TokenKind::IntLit(v) = lit_tok.kind else {
                    return Err(ParseError::Unexpected {
                        expected: "integer literal after `=` in enum".to_owned(),
                        found: lit_tok.kind.describe().to_owned(),
                        offset: lit_tok.span.start,
                    });
                };
                if negate { 0u32.wrapping_sub(v) } else { v }
            } else {
                next
            };
            self.enum_constants.insert(name, value);
            next = value.wrapping_add(1);
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(())
    }
    /// `typedef <type> [*]* <name> [\[N\]]* ;`. Records `name` →
    /// type in the typedef table; no AST node produced. Pointer
    /// stars wrap the base type so `typedef int *INTP;` records
    /// `INTP → Pointer(Int)` (fixture 487). Array suffixes wrap
    /// innermost-first so `typedef int IARR[3];` records
    /// `IARR → Array{elem: Int, len: 3}` (fixture 488).
    pub(crate) fn parse_typedef(&mut self) -> Result<(), ParseError> {
        self.bump(); // `typedef`
        let mut ty = self.parse_type()?;
        while matches!(self.peek().kind, TokenKind::Star) {
            self.bump();
            ty = Type::Pointer(Box::new(ty));
        }
        // Function-pointer typedef `typedef <ret> (*name)(<args>);` —
        // reuse the parameter-side declarator helper. The recorded
        // type is a generic near pointer (we don't model function
        // signatures). Fixture 1744.
        if matches!(self.peek().kind, TokenKind::LParen)
            && matches!(self.peek_n(1).kind, TokenKind::Star)
        {
            let (name, fp_ty) = self.parse_func_ptr_declarator(ty)?;
            self.expect(&TokenKind::Semicolon)?;
            self.typedefs.insert(name, fp_ty);
            return Ok(());
        }
        let name_tok = self.bump();
        let TokenKind::Ident(name) = name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        let mut array_lens: Vec<u32> = Vec::new();
        while matches!(self.peek().kind, TokenKind::LBracket) {
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
            array_lens.push(len);
        }
        for len in array_lens.into_iter().rev() {
            ty = Type::Array { elem: Box::new(ty), len };
        }
        self.expect(&TokenKind::Semicolon)?;
        self.typedefs.insert(name, ty);
        Ok(())
    }
    /// True when the current token is `struct` or `union` followed by
    /// the shape `tag { ... } ;` (a bare definition with no following
    /// declarator). This is the K&R/early-C90 `struct point { ... };`
    /// form. With a declarator after the closing `}`, the parser
    /// falls into the function/global path instead.
    pub(crate) fn is_bare_record_def(&self) -> bool {
        if !matches!(self.peek().kind, TokenKind::KwStruct | TokenKind::KwUnion) {
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
    /// Parse a bare `struct <tag>? { <fields> } ;` or `union ...`.
    /// Registers the type under its tag (required for bare
    /// definitions — an anonymous record here would be unreferencable)
    /// and emits no AST node.
    pub(crate) fn parse_bare_record_decl(&mut self) -> Result<(), ParseError> {
        let ty = if matches!(self.peek().kind, TokenKind::KwUnion) {
            self.parse_union_type()?
        } else {
            self.parse_struct_type()?
        };
        self.expect(&TokenKind::Semicolon)?;
        // The tag was already inserted into `self.structs` by the
        // parser; the bare-decl semicolon just ends the statement.
        let _ = ty;
        Ok(())
    }
    /// Parse a type expression. Handles `int`, `char`, `struct
    /// <tag> { ... }`, `struct <tag>`, and typedef'd names. Pointer
    /// `*` modifiers are handled by the *caller* — this returns the
    /// base type only.
    /// Discard BCC-specific calling-convention / memory-model
    /// modifier keywords (`pascal`, `cdecl`, `far`, `near`, `huge`,
    /// `interrupt`, plus `_`-prefixed variants). We don't implement
    /// these but accept them as no-ops so fixtures that use them
    /// can parse. Fixtures 1653 (pascal), 1654 (far), 1655
    /// (interrupt), 1656 (cdecl).
    pub(crate) fn consume_cc_modifiers(&mut self) {
        let _ = self.consume_cc_modifiers_collect();
    }
    /// Like `consume_cc_modifiers` but also records which cc-modifier
    /// keywords (if any) appeared. Returns `(is_pascal, is_far,
    /// is_interrupt, is_near)`. `near` is tracked separately so
    /// far-code models can spot the explicit override and skip the
    /// implicit-far promotion. Fixture 2061.
    pub(crate) fn consume_cc_modifiers_collect(&mut self) -> (bool, bool, bool, bool) {
        let (p, f, _h, i, n) = self.consume_cc_modifiers_full();
        (p, f, i, n)
    }
    /// Full variant that also separately tracks `huge` (true when
    /// `huge` was seen). Used by the pointer-declarator path so an
    /// `int huge *p` produces `Type::FarPointer { is_huge: true }`
    /// while `int far *p` produces `is_huge: false`. Both share the
    /// 4-byte slot and `les`-style deref shape; the distinction only
    /// matters for pointer arithmetic (huge normalizes the seg:off
    /// pair). Returns `(is_pascal, is_far, is_huge, is_interrupt,
    /// is_near)`. The segment-prefix qualifier (`_ss/_es/_cs/_ds`),
    /// when one is seen, is returned by the `_seg` variant below.
    pub(crate) fn consume_cc_modifiers_full(&mut self) -> (bool, bool, bool, bool, bool) {
        let (p, f, h, i, n, _seg, _is_seg) = self.consume_cc_modifiers_seg();
        (p, f, h, i, n)
    }
    /// Like `consume_cc_modifiers_full` but also collects a
    /// segment-prefix qualifier (`_ss/_es/_cs/_ds`) and the
    /// `_seg`-pointer marker. Only the pointer-declarator paths
    /// care about these — function-modifier sites can use the
    /// shorter form. Fixtures 4063–4068 (seg qualifiers), 4069–4074
    /// (_seg pointer type).
    pub(crate) fn consume_cc_modifiers_seg(
        &mut self,
    ) -> (bool, bool, bool, bool, bool, Option<crate::ast::SegReg>, bool) {
        use crate::ast::SegReg;
        let mut is_pascal = false;
        let mut is_far = false;
        let mut is_huge = false;
        let mut is_interrupt = false;
        let mut is_near = false;
        let mut seg: Option<SegReg> = None;
        let mut is_seg = false;
        while let TokenKind::Ident(name) = &self.peek().kind {
            match name.as_str() {
                "pascal" | "_pascal" | "__pascal" => {
                    is_pascal = true;
                    self.bump();
                }
                "far" | "_far" | "__far" => {
                    is_far = true;
                    self.bump();
                }
                "huge" | "_huge" | "__huge" => {
                    is_huge = true;
                    self.bump();
                }
                "interrupt" | "_interrupt" | "__interrupt" => {
                    is_interrupt = true;
                    self.bump();
                }
                "near" | "_near" | "__near" => {
                    is_near = true;
                    self.bump();
                }
                "cdecl" | "_cdecl" | "__cdecl" => {
                    self.bump();
                }
                "_ss" | "__ss" => {
                    seg = Some(SegReg::Ss);
                    self.bump();
                }
                "_es" | "__es" => {
                    seg = Some(SegReg::Es);
                    self.bump();
                }
                "_cs" | "__cs" => {
                    seg = Some(SegReg::Cs);
                    self.bump();
                }
                "_ds" | "__ds" => {
                    seg = Some(SegReg::Ds);
                    self.bump();
                }
                "_seg" | "__seg" => {
                    is_seg = true;
                    self.bump();
                }
                _ => break,
            }
        }
        (is_pascal, is_far, is_huge, is_interrupt, is_near, seg, is_seg)
    }
    /// Helper: build the right pointer Type given an inner type and
    /// the qualifier state. `far` / `huge` → `FarPointer` (with
    /// `is_huge` propagated); explicit `near` → `NearPointer`
    /// (parser-side marker; the promotion pass collapses it back to
    /// `Pointer` and skips the implicit-far promotion for it);
    /// otherwise plain `Pointer`. Fixture 1748 needs the explicit-
    /// near tag so large model leaves it 2-byte.
    pub(crate) fn make_ptr_ty(inner: Type, is_far: bool, is_huge: bool, is_near: bool) -> Type {
        Self::make_ptr_ty_seg(inner, is_far, is_huge, is_near, None, false)
    }
    /// Like `make_ptr_ty` but accepts a segment-prefix qualifier
    /// (`_ss/_es/_cs/_ds`) and the `_seg`-pointer marker. When
    /// `is_seg` is set, builds a `Type::SegSelector` (segment-only
    /// 2-byte pointer); the seg-prefix qualifier builds
    /// `Type::SegPointer`. Both trump `near`/`far` in this position.
    /// Fixtures 4063–4068 (qualifier), 4069–4074 (`_seg`).
    pub(crate) fn make_ptr_ty_seg(
        inner: Type,
        is_far: bool,
        is_huge: bool,
        is_near: bool,
        seg: Option<crate::ast::SegReg>,
        is_seg: bool,
    ) -> Type {
        if is_seg {
            Type::SegSelector { pointee: Box::new(inner) }
        } else if let Some(seg) = seg {
            Type::SegPointer { pointee: Box::new(inner), seg }
        } else if is_far || is_huge {
            Type::FarPointer { pointee: Box::new(inner), is_huge }
        } else if is_near {
            Type::NearPointer(Box::new(inner))
        } else {
            Type::Pointer(Box::new(inner))
        }
    }
    pub(crate) fn parse_type(&mut self) -> Result<Type, ParseError> {
        // `const`, `volatile`, `register` are purely front-end
        // qualifiers — BCC accepts and discards them. Fixtures 475
        // (const), 476 (volatile), 477 (register).
        while matches!(
            self.peek().kind,
            TokenKind::KwConst | TokenKind::KwVolatile | TokenKind::KwRegister
        ) {
            self.bump();
        }
        self.consume_cc_modifiers();
        match self.peek().kind {
            TokenKind::KwInt => {
                self.bump();
                // \`short int\` — the lexer already aliases \`short\`
                // to KwInt, so this consumes the trailing \`int\` in
                // the C standard's full \`short int\` spelling.
                // Fixture 2504.
                if matches!(self.peek().kind, TokenKind::KwInt) {
                    self.bump();
                }
                Ok(Type::Int)
            }
            TokenKind::KwVoid => {
                // `void` as a return type. We don't have a dedicated
                // `Type::Void` variant — Int serves as the placeholder
                // since codegen treats functions with no `return <expr>`
                // statements the same way regardless of declared ret
                // type. Fixture 552 (`void set(int *p) { *p = 99; }`).
                self.bump();
                Ok(Type::Int)
            }
            TokenKind::KwChar => {
                self.bump();
                Ok(Type::Char)
            }
            TokenKind::KwUnsigned => {
                self.bump();
                // `unsigned long [int]` — 32-bit unsigned.
                if matches!(self.peek().kind, TokenKind::KwLong) {
                    self.bump();
                    if matches!(self.peek().kind, TokenKind::KwInt) {
                        self.bump();
                    }
                    return Ok(Type::ULong);
                }
                // `unsigned char` — 1-byte unsigned.
                if matches!(self.peek().kind, TokenKind::KwChar) {
                    self.bump();
                    return Ok(Type::UChar);
                }
                // `unsigned int` and bare `unsigned` are both
                // unsigned-int; consume the optional `int`.
                if matches!(self.peek().kind, TokenKind::KwInt) {
                    self.bump();
                }
                Ok(Type::UInt)
            }
            TokenKind::KwSigned => {
                self.bump();
                // `signed long [int]` — 32-bit signed (same as `long`).
                if matches!(self.peek().kind, TokenKind::KwLong) {
                    self.bump();
                    if matches!(self.peek().kind, TokenKind::KwInt) {
                        self.bump();
                    }
                    return Ok(Type::Long);
                }
                // `signed char` — 1-byte signed (same as plain `char`).
                if matches!(self.peek().kind, TokenKind::KwChar) {
                    self.bump();
                    return Ok(Type::Char);
                }
                // `signed int` and bare `signed` are both signed-int
                // (= plain `int`); consume the optional `int`.
                if matches!(self.peek().kind, TokenKind::KwInt) {
                    self.bump();
                }
                Ok(Type::Int)
            }
            TokenKind::KwLong => {
                self.bump();
                // `long unsigned [int]` — 32-bit unsigned (mirrors
                // the `unsigned long` form above).
                if matches!(self.peek().kind, TokenKind::KwUnsigned) {
                    self.bump();
                    if matches!(self.peek().kind, TokenKind::KwInt) {
                        self.bump();
                    }
                    return Ok(Type::ULong);
                }
                // `long int` and bare `long` both mean `long` — 32-bit
                // signed under the small model (fixture 203). Consume
                // the optional `int`.
                if matches!(self.peek().kind, TokenKind::KwInt) {
                    self.bump();
                }
                Ok(Type::Long)
            }
            TokenKind::KwFloat => {
                self.bump();
                Ok(Type::Float)
            }
            TokenKind::KwDouble => {
                self.bump();
                Ok(Type::Double)
            }
            TokenKind::KwStruct => self.parse_struct_type(),
            TokenKind::KwUnion => self.parse_union_type(),
            TokenKind::KwEnum => {
                // `enum [<tag>] [{ … }]` as a type — enums are int-
                // sized in BCC. Tag is consumed if present; we don't
                // require it to be registered since enum members were
                // already registered at the definition site. The
                // body form (`enum { A, B } x;`) is also valid here:
                // we parse the body to register members. Fixtures
                // 470 (bare) and 474 (with body).
                self.bump();
                if matches!(self.peek().kind, TokenKind::Ident(_)) {
                    self.bump();
                }
                if matches!(self.peek().kind, TokenKind::LBrace) {
                    self.parse_enum_body()?;
                }
                Ok(Type::Int)
            }
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
    /// A complete type-name — base type plus trailing pointer stars.
    /// Used by `sizeof(<type>)` and casts `(<type>) <expr>`, where the
    /// stars apply to the type as a whole. Declarators must NOT use
    /// this: in `int *a, b;` the `*` binds to `a` only and `b` is a
    /// plain int, which the declarator path handles per-name.
    pub(crate) fn parse_type_name(&mut self) -> Result<Type, ParseError> {
        let mut ty = self.parse_type()?;
        // CC modifiers (`far`, `near`, `huge`, `pascal`, etc.) sit
        // between the base type and pointer stars in cast/sizeof
        // contexts. Capture `far` / `huge` so the pointer level
        // built from the next `*` becomes a `Type::FarPointer`.
        // Fixture 1649 (`(int far *)&x`), 1652 (`(int huge *)&x`).
        let (_, mut is_far, mut is_huge, _, mut is_near, mut seg, mut is_seg) =
            self.consume_cc_modifiers_seg();
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
        // Abstract function-pointer declarator: `( * ) ( <args> )`
        // — appears in casts like `(int (*)(void))vp`. Fn-ptr
        // signatures aren't modeled, so the result is a generic
        // near `Pointer(Int)` (2-byte, int-pool-eligible) and the
        // arg list is skipped without parsing. Fixture 2332.
        if matches!(self.peek().kind, TokenKind::LParen)
            && matches!(self.peek_n(1).kind, TokenKind::Star)
            && matches!(self.peek_n(2).kind, TokenKind::RParen)
        {
            self.bump(); // `(`
            self.bump(); // `*`
            self.bump(); // `)`
            self.expect(&TokenKind::LParen)?;
            let mut depth = 1;
            while depth > 0 {
                let tok = self.bump();
                match tok.kind {
                    TokenKind::LParen => depth += 1,
                    TokenKind::RParen => depth -= 1,
                    TokenKind::Eof => {
                        return Err(ParseError::Unexpected {
                            expected: "`)`".to_owned(),
                            found: "end of input".to_owned(),
                            offset: tok.span.start,
                        });
                    }
                    _ => {}
                }
            }
            ty = Type::Pointer(Box::new(Type::Int));
        }
        Ok(ty)
    }
    /// `struct <tag>? { <fields> }` (with inline definition) or
    /// `struct <tag>` (reference to a previously-defined tag). Side
    /// effect: when an inline definition appears with a tag, the
    /// resulting type is inserted into `self.structs`.
    pub(crate) fn parse_struct_type(&mut self) -> Result<Type, ParseError> {
        self.parse_record_type(false)
    }
    pub(crate) fn parse_union_type(&mut self) -> Result<Type, ParseError> {
        self.parse_record_type(true)
    }
    /// `struct <tag>? { <fields> }` or `union <tag>? { <fields> }`.
    /// The body is identical; only field layout differs. For a union,
    /// every field is at offset 0 and the total size is the max of
    /// the field sizes (rounded up to a word). `Type::Struct` carries
    /// the result regardless — codegen looks up offsets via the field
    /// table either way, so the all-zero offsets just produce the
    /// right addressing for unions.
    pub(crate) fn parse_record_type(&mut self, is_union: bool) -> Result<Type, ParseError> {
        let kw = if is_union {
            TokenKind::KwUnion
        } else {
            TokenKind::KwStruct
        };
        self.expect(&kw)?;
        let tag = if let TokenKind::Ident(name) = &self.peek().kind {
            let name = name.clone();
            self.bump();
            Some(name)
        } else {
            None
        };
        if !matches!(self.peek().kind, TokenKind::LBrace) {
            // Bare reference: `struct point` / `union u` — must already
            // be in the table.
            let Some(name) = tag else {
                let t = self.peek();
                return Err(ParseError::Unexpected {
                    expected: "record tag or `{`".to_owned(),
                    found: t.kind.describe().to_owned(),
                    offset: t.span.start,
                });
            };
            return self.structs.get(&name).cloned().ok_or_else(|| {
                ParseError::Unsupported { offset: self.peek().span.start }
            });
        }
        self.bump(); // `{`
        // Pre-register the tag with an empty placeholder so the body
        // can reference `struct <tag> *next` (self-pointer) without
        // a forward-declaration error. The placeholder is replaced
        // with the complete type once all fields are parsed. Fixture
        // 494 (`struct node { int value; struct node *next; };`).
        if let Some(tag_name) = &tag {
            self.structs.insert(
                tag_name.clone(),
                Type::Struct {
                    name: Some(tag_name.clone()),
                    fields: Vec::new(),
                    size: 0,
                },
            );
        }
        let mut fields: Vec<StructField> = Vec::new();
        let mut struct_offset: u16 = 0;
        let mut union_max: u16 = 0;
        // Bitfield packing state: the current container's starting
        // byte offset, how many of its 16 bits are claimed, and
        // whether the container has already been expanded to 2
        // bytes. Reset to None whenever a non-bitfield field lands
        // or a `: 0` separator forces alignment.
        let mut bit_container_offset: Option<u16> = None;
        let mut bits_used_in_container: u8 = 0;
        let mut container_is_word: bool = false;
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            // Each field declaration: <type> <pointer-stars> <name>
            // ('[' <int> ']')* ; — or bitfield: <type> <name> : <width> ;
            let mut ty = self.parse_type()?;
            while matches!(self.peek().kind, TokenKind::Star) {
                self.bump();
                ty = Type::Pointer(Box::new(ty));
            }
            // Anonymous bitfield: `<type> : <width>;`. Width 0 is
            // the alignment-separator form (`unsigned : 0;`) — it
            // forces the next bitfield into a fresh container.
            // Non-zero anonymous widths just consume bits without
            // emitting a named field. Fixture 2302.
            if matches!(self.peek().kind, TokenKind::Colon) {
                self.bump();
                let width_tok = self.bump();
                let TokenKind::IntLit(width) = width_tok.kind else {
                    return Err(ParseError::Unexpected {
                        expected: "anonymous bitfield width".to_owned(),
                        found: width_tok.kind.describe().to_owned(),
                        offset: width_tok.span.start,
                    });
                };
                self.expect(&TokenKind::Semicolon)?;
                if width == 0 {
                    if bit_container_offset.is_some() {
                        bit_container_offset = None;
                        bits_used_in_container = 0;
                        container_is_word = false;
                    }
                } else {
                    let width_u8 = u8::try_from(width).map_err(|_| {
                        ParseError::Unsupported { offset: width_tok.span.start }
                    })?;
                    if bit_container_offset.is_none()
                        || bits_used_in_container + width_u8 > 16
                    {
                        bit_container_offset = Some(struct_offset);
                        bits_used_in_container = 0;
                        container_is_word = false;
                        if !is_union {
                            struct_offset += 1;
                        }
                    }
                    bits_used_in_container += width_u8;
                    if !is_union && !container_is_word && bits_used_in_container > 8 {
                        container_is_word = true;
                        struct_offset += 1;
                    }
                }
                continue;
            }
            // Function-pointer struct field: `<ret> (*<name>)(args);`.
            // Mirrors the local/param/global declarator path — the
            // signature is collapsed to a generic near pointer so the
            // field is 2 bytes. Fixture 2378.
            let name = if matches!(self.peek().kind, TokenKind::LParen) {
                let (fp_name, fp_ty) = self.parse_func_ptr_declarator(ty.clone())?;
                ty = fp_ty;
                fp_name
            } else {
                let name_tok = self.bump();
                let TokenKind::Ident(name) = name_tok.kind else {
                    return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
                };
                name
            };
            // Bitfield: `<int-type> <name> : <width>;`. Only ints
            // (signed/unsigned) carry bitfields. Fixture 1691.
            if matches!(self.peek().kind, TokenKind::Colon) {
                self.bump();
                let width_tok = self.bump();
                let TokenKind::IntLit(width) = width_tok.kind else {
                    return Err(ParseError::Unexpected {
                        expected: "bitfield width (integer literal)".to_owned(),
                        found: width_tok.kind.describe().to_owned(),
                        offset: width_tok.span.start,
                    });
                };
                self.expect(&TokenKind::Semicolon)?;
                let width_u8 = u8::try_from(width).map_err(|_| {
                    ParseError::Unsupported { offset: width_tok.span.start }
                })?;
                // Open the container lazily — first bitfield in a
                // run gets 1 byte; we grow to 2 bytes only when the
                // accumulated bits cross the 8-bit boundary. A new
                // bitfield that wouldn't fit in 16 bits closes the
                // current container and starts a fresh one.
                if bit_container_offset.is_none()
                    || bits_used_in_container + width_u8 > 16
                {
                    bit_container_offset = Some(struct_offset);
                    bits_used_in_container = 0;
                    container_is_word = false;
                    if !is_union {
                        struct_offset += 1;
                    }
                }
                let cont_off = bit_container_offset.expect("just set");
                let bit_off_in_container = bits_used_in_container;
                bits_used_in_container += width_u8;
                if !is_union && !container_is_word && bits_used_in_container > 8 {
                    // Container grows from 1 byte to 2 bytes.
                    container_is_word = true;
                    struct_offset += 1;
                }
                let byte_off_within = bit_off_in_container / 8;
                let bit_off_within = bit_off_in_container % 8;
                fields.push(StructField {
                    name,
                    ty,
                    offset: cont_off + u16::from(byte_off_within),
                    bitfield: Some(BitfieldInfo {
                        bit_offset: bit_off_within,
                        bit_width: width_u8,
                    }),
                });
                if is_union {
                    // A bitfield in a union just claims its width
                    // against the union's max footprint.
                    let bytes = width_u8.div_ceil(8) as u16;
                    if bytes > union_max {
                        union_max = bytes;
                    }
                }
                continue;
            }
            // Closing out any pending bitfield container before a
            // regular field — the next field starts at the byte
            // after the container.
            if bit_container_offset.is_some() {
                bit_container_offset = None;
                bits_used_in_container = 0;
                container_is_word = false;
            }
            // Array suffix on the field (`int data[4];` — fixture
            // 496). Multi-dim wraps innermost-first.
            let mut array_lens: Vec<u32> = Vec::new();
            while matches!(self.peek().kind, TokenKind::LBracket) {
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
                array_lens.push(len);
            }
            let mut full_ty = ty.clone();
            for len in array_lens.into_iter().rev() {
                full_ty = Type::Array { elem: Box::new(full_ty), len };
            }
            let field_size = full_ty.size_bytes();
            let offset = if is_union { 0 } else { struct_offset };
            fields.push(StructField { name, ty: full_ty, offset, bitfield: None });
            if is_union {
                if field_size > union_max {
                    union_max = field_size;
                }
            } else {
                struct_offset += field_size;
            }
            // Comma-list of additional names with the same base type:
            // `int a, b, c;`. Each tail name parses its own array
            // suffixes and contributes its own field entry. Fixture
            // 3612.
            while matches!(self.peek().kind, TokenKind::Comma) {
                self.bump();
                let tail_name_tok = self.bump();
                let TokenKind::Ident(tail_name) = tail_name_tok.kind else {
                    return Err(ParseError::NotAnIdent { offset: tail_name_tok.span.start });
                };
                let mut tail_array_lens: Vec<u32> = Vec::new();
                while matches!(self.peek().kind, TokenKind::LBracket) {
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
                    tail_array_lens.push(len);
                }
                let mut tail_ty = ty.clone();
                for len in tail_array_lens.into_iter().rev() {
                    tail_ty = Type::Array { elem: Box::new(tail_ty), len };
                }
                let tail_size = tail_ty.size_bytes();
                let tail_offset = if is_union { 0 } else { struct_offset };
                fields.push(StructField {
                    name: tail_name, ty: tail_ty, offset: tail_offset, bitfield: None,
                });
                if is_union {
                    if tail_size > union_max { union_max = tail_size; }
                } else {
                    struct_offset += tail_size;
                }
            }
            self.expect(&TokenKind::Semicolon)?;
        }
        self.expect(&TokenKind::RBrace)?;
        // Struct size is the raw sum of field sizes (no inter-field
        // padding, no end rounding). Fixture 462's BSS layout pins
        // `{unsigned char b; int x;}` as 3 bytes, not 4. Stack-frame
        // alignment to word boundary happens at frame allocation
        // time, not here. Unions still need width rounding because
        // every field overlaps at offset 0 and a single-char union
        // would otherwise produce a 1-byte slot that mis-aligns the
        // next int local (no fixture pins this yet — keep the round
        // for unions to preserve fixture-103 behavior).
        let size = if is_union { union_max } else { struct_offset };
        let ty = Type::Struct { name: tag.clone(), fields, size };
        if let Some(name) = tag {
            self.structs.insert(name, ty.clone());
        }
        Ok(ty)
    }
}
