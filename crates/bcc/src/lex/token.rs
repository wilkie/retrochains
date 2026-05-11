//! Tokens and spans. Spans are byte offsets into the source string; we
//! resolve to (line, column) lazily when a diagnostic needs to be rendered.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    // Keywords (extend as fixtures demand)
    KwInt,
    KwVoid,
    KwReturn,
    // Atoms
    Ident(String),
    IntLit(u32),
    // Punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    Semicolon,
    Equals,
    Plus,
    // End-of-input sentinel
    Eof,
}

impl TokenKind {
    /// Short, human-readable name for use in diagnostics
    /// (e.g. "`int`" / "identifier" / "`(`").
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::KwInt => "`int`",
            Self::KwVoid => "`void`",
            Self::KwReturn => "`return`",
            Self::Ident(_) => "identifier",
            Self::IntLit(_) => "integer literal",
            Self::LParen => "`(`",
            Self::RParen => "`)`",
            Self::LBrace => "`{`",
            Self::RBrace => "`}`",
            Self::Semicolon => "`;`",
            Self::Equals => "`=`",
            Self::Plus => "`+`",
            Self::Eof => "end of input",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first byte of the token in the source.
    pub start: u32,
    /// Byte offset one past the last byte of the token.
    pub end: u32,
}

impl Span {
    #[must_use]
    pub fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }
}
