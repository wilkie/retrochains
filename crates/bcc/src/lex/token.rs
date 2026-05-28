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
    KwStruct,
    KwTypedef,
    KwStatic,
    KwExtern,
    KwEnum,
    KwSizeof,
    KwUnsigned,
    KwUnion,
    KwLong,
    KwGoto,
    KwSigned,
    KwConst,
    KwVolatile,
    KwRegister,
    KwFloat,
    KwDouble,
    /// `asm` — inline assembly. The following token is always
    /// `AsmBody`, holding the raw source text the lexer captured
    /// after the keyword. Block form (`asm { ... }`) and statement
    /// form (`asm <line>;` / `asm <line>\n`) both reach the parser
    /// as the same `KwAsm` + `AsmBody` pair.
    KwAsm,
    /// Raw inline-assembly body text. Lexer captures everything
    /// inside `asm { ... }` (block form, between the braces) or
    /// everything after `asm` up to the next `;` / `\n` (statement
    /// form). Lines are split by `;` and / or `\n` at codegen
    /// time, not at lex time, so the AST preserves the original
    /// shape verbatim.
    AsmBody(String),
    // Atoms
    Ident(String),
    IntLit(u32),
    /// 32-bit float literal, stored as raw IEEE 754 bits.
    FloatLit(u32),
    /// 64-bit double literal, stored as raw IEEE 754 bits.
    DoubleLit(u64),
    /// `"...."` — string literal. Decoded byte contents, escapes
    /// resolved at lex time. The trailing NUL is implicit (added by
    /// codegen when materializing the literal); the lexer doesn't
    /// include it in the value.
    StringLit(Vec<u8>),
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
    Question,
    PlusPlus,
    MinusMinus,
    Arrow,
    Dot,
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
    ShlEq,
    ShrEq,
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
            Self::KwStruct => "`struct`",
            Self::KwTypedef => "`typedef`",
            Self::KwStatic => "`static`",
            Self::KwExtern => "`extern`",
            Self::KwEnum => "`enum`",
            Self::KwSizeof => "`sizeof`",
            Self::KwUnsigned => "`unsigned`",
            Self::KwUnion => "`union`",
            Self::KwLong => "`long`",
            Self::KwGoto => "`goto`",
            Self::KwSigned => "`signed`",
            Self::KwConst => "`const`",
            Self::KwVolatile => "`volatile`",
            Self::KwRegister => "`register`",
            Self::KwFloat => "`float`",
            Self::KwDouble => "`double`",
            Self::KwAsm => "`asm`",
            Self::AsmBody(_) => "asm body",
            Self::Ident(_) => "identifier",
            Self::IntLit(_) => "integer literal",
            Self::FloatLit(_) => "float literal",
            Self::DoubleLit(_) => "double literal",
            Self::StringLit(_) => "string literal",
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
            Self::Question => "`?`",
            Self::PlusPlus => "`++`",
            Self::MinusMinus => "`--`",
            Self::Arrow => "`->`",
            Self::Dot => "`.`",
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
            Self::ShlEq => "`<<=`",
            Self::ShrEq => "`>>=`",
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
