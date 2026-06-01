use super::*;

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
    pub(crate) fn peek(&self) -> &Token {
        self.peek_n(0)
    }
    /// Look `n` tokens ahead. Used for the 2-token lookahead in
    /// `parse_stmt` to disambiguate `<ident> =` (assignment) from
    /// `<ident> ++` (expression statement).
    pub(crate) fn peek_n(&self, n: usize) -> &Token {
        // `parse_unit` exits before EOF; once we run off the end, return
        // the last token (always `Eof` after `Lexer::tokenize`).
        self.tokens.get(self.pos + n).unwrap_or_else(|| {
            self.tokens.last().expect("lexer always emits at least an EOF token")
        })
    }
    pub(crate) fn bump(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }
    pub(crate) fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }
    pub(crate) fn expect(&mut self, want: &TokenKind) -> Result<Token, ParseError> {
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
