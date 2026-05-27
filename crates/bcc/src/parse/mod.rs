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

impl Parser {
    #[must_use]
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            structs: HashMap::new(),
            typedefs: HashMap::new(),
            pending_static_locals: Vec::new(),
            enum_constants: HashMap::new(),
            pending_extra_stmts: Vec::new(),
            current_static_renames: HashMap::new(),
            block_scopes: vec![HashMap::new()],
            static_local_counter: 0,
            global_types: HashMap::new(),
            function_locals: HashMap::new(),
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
            // 3643).
            if matches!(self.peek_n(probe).kind, TokenKind::LParen)
                && matches!(self.peek_n(probe + 1).kind, TokenKind::Star)
            {
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

    /// `enum [tag] { id [= int-lit] [, ...] [,] } ;`. Each member is
    /// registered in `enum_constants`; the next value defaults to
    /// `previous + 1` (or 0 for the first), explicit initializers
    /// reset the counter. No AST node produced — references to the
    /// member names fold to `IntLit` in `parse_primary`.
    fn parse_enum_decl(&mut self) -> Result<(), ParseError> {
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
    fn parse_enum_body(&mut self) -> Result<(), ParseError> {
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
    fn parse_typedef(&mut self) -> Result<(), ParseError> {
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
    fn is_bare_record_def(&self) -> bool {
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
    fn parse_bare_record_decl(&mut self) -> Result<(), ParseError> {
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
    fn consume_cc_modifiers(&mut self) {
        let _ = self.consume_cc_modifiers_collect();
    }

    /// Like `consume_cc_modifiers` but also records which cc-modifier
    /// keywords (if any) appeared. Returns `(is_pascal, is_far,
    /// is_interrupt)`.
    fn consume_cc_modifiers_collect(&mut self) -> (bool, bool, bool) {
        let mut is_pascal = false;
        let mut is_far = false;
        let mut is_interrupt = false;
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
                "interrupt" | "_interrupt" | "__interrupt" => {
                    is_interrupt = true;
                    self.bump();
                }
                "cdecl" | "near" | "huge"
                | "_cdecl" | "_near" | "_huge"
                | "__cdecl" | "__near" | "__huge" => {
                    self.bump();
                }
                _ => break,
            }
        }
        (is_pascal, is_far, is_interrupt)
    }

    fn parse_type(&mut self) -> Result<Type, ParseError> {
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
    fn parse_type_name(&mut self) -> Result<Type, ParseError> {
        let mut ty = self.parse_type()?;
        // CC modifiers (`far`, `near`, `huge`, `pascal`, etc.) sit
        // between the base type and pointer stars in cast/sizeof
        // contexts. We don't model the modifier, so just skip.
        // Fixture 1649 (`(int far *)&x`).
        self.consume_cc_modifiers();
        while matches!(self.peek().kind, TokenKind::Star) {
            self.bump();
            ty = Type::Pointer(Box::new(ty));
            self.consume_cc_modifiers();
        }
        Ok(ty)
    }

    /// `struct <tag>? { <fields> }` (with inline definition) or
    /// `struct <tag>` (reference to a previously-defined tag). Side
    /// effect: when an inline definition appears with a tag, the
    /// resulting type is inserted into `self.structs`.
    fn parse_struct_type(&mut self) -> Result<Type, ParseError> {
        self.parse_record_type(false)
    }

    fn parse_union_type(&mut self) -> Result<Type, ParseError> {
        self.parse_record_type(true)
    }

    /// `struct <tag>? { <fields> }` or `union <tag>? { <fields> }`.
    /// The body is identical; only field layout differs. For a union,
    /// every field is at offset 0 and the total size is the max of
    /// the field sizes (rounded up to a word). `Type::Struct` carries
    /// the result regardless — codegen looks up offsets via the field
    /// table either way, so the all-zero offsets just produce the
    /// right addressing for unions.
    fn parse_record_type(&mut self, is_union: bool) -> Result<Type, ParseError> {
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

    /// `<type-base> <pointer-stars>* <name> ('[' <int> ']')? [= <expr>] ;`
    /// at the top level. Same declarator shape as a local declaration
    /// (`parse_declare`); the difference is the resulting AST node
    /// (`Global` vs. `StmtKind::Declare`) and the absence of an
    /// enclosing function context.
    fn parse_global(&mut self, is_static: bool, is_extern: bool) -> Result<Vec<Global>, ParseError> {
        let start = self.peek().span.start;
        let base_ty = self.parse_type()?;
        let mut globals = Vec::new();
        loop {
            // Per-declarator pointer stars: `int *a, b;` makes `a`
            // an `int*` and `b` a plain `int`.
            self.consume_cc_modifiers();
            let mut ty = base_ty.clone();
            while matches!(self.peek().kind, TokenKind::Star) {
                self.bump();
                ty = Type::Pointer(Box::new(ty));
                self.consume_cc_modifiers();
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

    fn parse_function(&mut self) -> Result<Function, ParseError> {
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
        let (is_pascal, is_far, is_interrupt) = self.consume_cc_modifiers_collect();
        let name_tok = self.bump();
        let TokenKind::Ident(name) = &name_tok.kind else {
            return Err(ParseError::NotAnIdent { offset: name_tok.span.start });
        };
        let name = name.clone();
        self.expect(&TokenKind::LParen)?;
        let (mut params, _is_ansi_proto) = self.parse_param_list()?;
        self.expect(&TokenKind::RParen)?;
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
    fn parse_initializer(&mut self) -> Result<Expr, ParseError> {
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
    fn parse_param_list(&mut self) -> Result<(Vec<Param>, bool), ParseError> {
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
            self.consume_cc_modifiers();
            // Pointer stars wrap the base type, just like in a local
            // declaration (fixture 095: `int sum(int *p)`).
            while matches!(self.peek().kind, TokenKind::Star) {
                self.bump();
                ty = Type::Pointer(Box::new(ty));
                self.consume_cc_modifiers();
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

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
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
        // pointer-star/declarator (`int far *p`). Discarded.
        self.consume_cc_modifiers();
        // Pointer stars wrap the base type: `int **pp` is `Pointer(Pointer(Int))`.
        // Stars are per-declarator — `int *a, b;` makes `a` an `int*`
        // and `b` a plain `int`, so we keep `base_ty` clean for the
        // multi-declarator tail loop and decorate a separate `ty`
        // copy for this first declarator.
        let mut ty = base_ty.clone();
        while matches!(self.peek().kind, TokenKind::Star) {
            self.bump();
            ty = Type::Pointer(Box::new(ty));
            // `T far *` / `T *far` — modifiers can also appear AFTER
            // a pointer star. Consume them silently. Also accept
            // `T * const p` (const-qualified pointer). Fixture 2380.
            self.consume_cc_modifiers();
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

    fn finish_declare_unsized(
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
    fn finish_declare(
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
    fn rename_shadowed_local(&mut self, name: &str) -> String {
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
    fn lookup_block_rename(&self, name: &str) -> Option<String> {
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
        // Array-of-function-pointers declarator shape:
        // `<ret> ( * <name> [ N ]... ) ( args )`. Skip the inner
        // `[N]` dimensions — the codegen treats the whole thing as
        // a flat array (just like a function-pointer array). Fixtures
        // 2305, 2308, 2343, etc.
        while matches!(self.peek().kind, TokenKind::LBracket) {
            self.bump();
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
    fn parse_for_clause_list(
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
    fn parse_for_clause_expr(&mut self) -> Result<Expr, ParseError> {
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

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        // Precedence ladder, lowest at the top: `?:` < || < && < | < ^
        // < & < == != < relational < shift < additive < multiplicative
        // < unary < atom. `?:` is right-associative.
        self.parse_conditional()
    }

    /// `<cond> ? <then> : <else-conditional>` — right-associative.
    fn parse_conditional(&mut self) -> Result<Expr, ParseError> {
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
    /// Return the static byte size of an expression, when known at
    /// parse time. Today only bare-Ident operands are supported (look
    /// the name up in the local-of-current-function or file-scope
    /// tables); compound expressions return `None` and let the caller
    /// report the failure. `sizeof` is the only consumer.
    fn expr_static_size(&self, e: &Expr) -> Option<u16> {
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
    fn is_type_name_after_lparen(&self) -> bool {
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

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
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
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
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
