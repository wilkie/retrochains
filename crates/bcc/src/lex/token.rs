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
    KwChar,
    KwVoid,
    KwReturn,
    KwIf,
    KwElse,
    KwWhile,
    KwFor,
    KwDo,
    KwBreak,
    KwContinue,
    KwSwitch,
    KwCase,
    KwDefault,
    // Atoms
    Ident(String),
    IntLit(u32),
    // Punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semicolon,
    Colon,
    Comma,
    Equals,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Ampersand,
    Pipe,
    Caret,
    Tilde,
    Bang,
    PlusPlus,
    MinusMinus,
    AmpAmp,
    PipePipe,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShiftLeft,
    ShiftRight,
    EqEq,
    BangEq,
    Lt,
    Le,
    Gt,
    Ge,
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
            Self::KwChar => "`char`",
            Self::KwVoid => "`void`",
            Self::KwReturn => "`return`",
            Self::KwIf => "`if`",
            Self::KwElse => "`else`",
            Self::KwWhile => "`while`",
            Self::KwFor => "`for`",
            Self::KwDo => "`do`",
            Self::KwBreak => "`break`",
            Self::KwContinue => "`continue`",
            Self::KwSwitch => "`switch`",
            Self::KwCase => "`case`",
            Self::KwDefault => "`default`",
            Self::Ident(_) => "identifier",
            Self::IntLit(_) => "integer literal",
            Self::LParen => "`(`",
            Self::RParen => "`)`",
            Self::LBrace => "`{`",
            Self::RBrace => "`}`",
            Self::LBracket => "`[`",
            Self::RBracket => "`]`",
            Self::Semicolon => "`;`",
            Self::Colon => "`:`",
            Self::Comma => "`,`",
            Self::Equals => "`=`",
            Self::Plus => "`+`",
            Self::Minus => "`-`",
            Self::Star => "`*`",
            Self::Slash => "`/`",
            Self::Percent => "`%`",
            Self::Ampersand => "`&`",
            Self::Pipe => "`|`",
            Self::Caret => "`^`",
            Self::Tilde => "`~`",
            Self::Bang => "`!`",
            Self::PlusPlus => "`++`",
            Self::MinusMinus => "`--`",
            Self::AmpAmp => "`&&`",
            Self::PipePipe => "`||`",
            Self::PlusEq => "`+=`",
            Self::MinusEq => "`-=`",
            Self::StarEq => "`*=`",
            Self::SlashEq => "`/=`",
            Self::PercentEq => "`%=`",
            Self::AmpEq => "`&=`",
            Self::PipeEq => "`|=`",
            Self::CaretEq => "`^=`",
            Self::ShiftLeft => "`<<`",
            Self::ShiftRight => "`>>`",
            Self::EqEq => "`==`",
            Self::BangEq => "`!=`",
            Self::Lt => "`<`",
            Self::Le => "`<=`",
            Self::Gt => "`>`",
            Self::Ge => "`>=`",
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
