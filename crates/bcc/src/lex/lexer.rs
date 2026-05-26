//! Lexer: produces a stream of [`Token`]s from a source string.
//!
//! Currently covers what the starter fixtures need: identifiers, decimal
//! integer literals, the keywords `int` / `void` / `return`, and a few
//! punctuators. Comments, preprocessor directives, string literals, etc.
//! are added when a fixture demands them.

use super::token::{Span, Token, TokenKind};

#[derive(Debug, thiserror::Error)]
pub enum LexError {
    #[error("unexpected character {ch:?} at byte offset {offset}")]
    UnexpectedChar { ch: char, offset: u32 },
    #[error("integer literal at offset {offset} overflows 32 bits")]
    IntOverflow { offset: u32 },
    #[error("unterminated string literal starting at offset {offset}")]
    UnterminatedString { offset: u32 },
    #[error("unknown escape `\\{ch}` in string literal at offset {offset}")]
    UnknownEscape { ch: char, offset: u32 },
}

#[derive(Debug)]
pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

fn off(pos: usize) -> u32 {
    u32::try_from(pos).unwrap_or(u32::MAX)
}

impl<'a> Lexer<'a> {
    #[must_use]
    pub fn new(src: &'a str) -> Self {
        Self { src: src.as_bytes(), pos: 0 }
    }

    /// Run the lexer to completion, returning every token plus a final
    /// `Eof` token. Streaming variants can be added later if needed.
    ///
    /// # Errors
    /// Returns [`LexError`] on the first unrecognized byte sequence.
    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut out = Vec::new();
        loop {
            self.skip_whitespace();
            let start = self.pos;
            let Some(&b) = self.src.get(self.pos) else {
                out.push(Token {
                    kind: TokenKind::Eof,
                    span: Span::new(off(start), off(start)),
                });
                return Ok(out);
            };
            let kind = match b {
                b'(' => { self.pos += 1; TokenKind::LParen }
                b')' => { self.pos += 1; TokenKind::RParen }
                b'{' => { self.pos += 1; TokenKind::LBrace }
                b'}' => { self.pos += 1; TokenKind::RBrace }
                b'[' => { self.pos += 1; TokenKind::LBracket }
                b']' => { self.pos += 1; TokenKind::RBracket }
                b';' => { self.pos += 1; TokenKind::Semicolon }
                b':' => { self.pos += 1; TokenKind::Colon }
                b',' => { self.pos += 1; TokenKind::Comma }
                b'.' => { self.pos += 1; TokenKind::Dot }
                b'=' => self.lex_after_eq(),
                b'!' => self.lex_after_bang(),
                b'+' => self.lex_after_plus(),
                b'-' => self.lex_after_minus(),
                b'*' => self.lex_after_simple(TokenKind::Star, TokenKind::StarEq),
                b'/' => self.lex_after_simple(TokenKind::Slash, TokenKind::SlashEq),
                b'%' => self.lex_after_simple(TokenKind::Percent, TokenKind::PercentEq),
                b'&' => self.lex_after_amp(),
                b'|' => self.lex_after_pipe(),
                b'^' => self.lex_after_simple(TokenKind::Caret, TokenKind::CaretEq),
                b'~' => { self.pos += 1; TokenKind::Tilde }
                b'?' => { self.pos += 1; TokenKind::Question }
                b'<' => self.lex_after_lt(),
                b'>' => self.lex_after_gt(),
                b'"' => self.lex_string_literal()?,
                b'\'' => self.lex_char_literal()?,
                b if is_ident_start(b) => self.lex_ident_or_keyword(),
                b if b.is_ascii_digit() => self.lex_int_literal()?,
                other => {
                    return Err(LexError::UnexpectedChar {
                        ch: other as char,
                        offset: off(self.pos),
                    });
                }
            };
            let end = self.pos;
            out.push(Token {
                kind,
                span: Span::new(off(start), off(end)),
            });
        }
    }

    fn skip_whitespace(&mut self) {
        loop {
            let Some(&b) = self.src.get(self.pos) else { return };
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
                continue;
            }
            // C comments. The preprocess pass preserves comment
            // bytes in its output so column offsets stay aligned;
            // the lexer skips them here. `//` runs to end-of-line;
            // `/* … */` runs to the closing delimiter. Fixture
            // 2114 (`// single-line comment`).
            if b == b'/' && self.src.get(self.pos + 1) == Some(&b'/') {
                self.pos += 2;
                while let Some(&c) = self.src.get(self.pos) {
                    if c == b'\n' {
                        break;
                    }
                    self.pos += 1;
                }
                continue;
            }
            if b == b'/' && self.src.get(self.pos + 1) == Some(&b'*') {
                self.pos += 2;
                while let Some(&c) = self.src.get(self.pos) {
                    if c == b'*' && self.src.get(self.pos + 1) == Some(&b'/') {
                        self.pos += 2;
                        break;
                    }
                    self.pos += 1;
                }
                continue;
            }
            return;
        }
    }

    fn lex_ident_or_keyword(&mut self) -> TokenKind {
        let start = self.pos;
        while let Some(&b) = self.src.get(self.pos) {
            if is_ident_continue(b) {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        match text {
            b"int" => TokenKind::KwInt,
            b"char" => TokenKind::KwChar,
            b"void" => TokenKind::KwVoid,
            b"return" => TokenKind::KwReturn,
            b"if" => TokenKind::KwIf,
            b"else" => TokenKind::KwElse,
            b"while" => TokenKind::KwWhile,
            b"for" => TokenKind::KwFor,
            b"do" => TokenKind::KwDo,
            b"break" => TokenKind::KwBreak,
            b"continue" => TokenKind::KwContinue,
            b"switch" => TokenKind::KwSwitch,
            b"case" => TokenKind::KwCase,
            b"default" => TokenKind::KwDefault,
            b"struct" => TokenKind::KwStruct,
            b"typedef" => TokenKind::KwTypedef,
            b"static" => TokenKind::KwStatic,
            b"extern" => TokenKind::KwExtern,
            b"enum" => TokenKind::KwEnum,
            b"sizeof" => TokenKind::KwSizeof,
            b"unsigned" => TokenKind::KwUnsigned,
            b"union" => TokenKind::KwUnion,
            b"long" => TokenKind::KwLong,
            // BCC treats `short` as a 16-bit synonym for `int`; we
            // tokenize it as KwInt directly so the type-parsing paths
            // don't each need a redundant arm. Fixture 930.
            b"short" => TokenKind::KwInt,
            b"goto" => TokenKind::KwGoto,
            b"signed" => TokenKind::KwSigned,
            b"const" => TokenKind::KwConst,
            b"volatile" => TokenKind::KwVolatile,
            b"register" => TokenKind::KwRegister,
            b"float" => TokenKind::KwFloat,
            b"double" => TokenKind::KwDouble,
            other => TokenKind::Ident(String::from_utf8_lossy(other).into_owned()),
        }
    }

    /// Disambiguate `=`: `==` is equality, bare `=` is assignment.
    fn lex_after_eq(&mut self) -> TokenKind {
        self.pos += 1;
        if matches!(self.src.get(self.pos), Some(&b'=')) {
            self.pos += 1;
            TokenKind::EqEq
        } else {
            TokenKind::Equals
        }
    }

    /// Disambiguate `+`: `++` is increment, `+=` is add-assign,
    /// bare `+` is addition.
    fn lex_after_plus(&mut self) -> TokenKind {
        self.pos += 1;
        match self.src.get(self.pos) {
            Some(&b'+') => { self.pos += 1; TokenKind::PlusPlus }
            Some(&b'=') => { self.pos += 1; TokenKind::PlusEq }
            _ => TokenKind::Plus,
        }
    }

    /// Disambiguate `-`: `--` is decrement, `-=` is sub-assign,
    /// `->` is member-via-pointer, bare `-` is subtraction.
    fn lex_after_minus(&mut self) -> TokenKind {
        self.pos += 1;
        match self.src.get(self.pos) {
            Some(&b'-') => { self.pos += 1; TokenKind::MinusMinus }
            Some(&b'=') => { self.pos += 1; TokenKind::MinusEq }
            Some(&b'>') => { self.pos += 1; TokenKind::Arrow }
            _ => TokenKind::Minus,
        }
    }

    /// Helper for the simple "X" vs "X=" punctuation pairs
    /// (`*` / `*=`, `/` / `/=`, `%` / `%=`, `^` / `^=`).
    fn lex_after_simple(&mut self, bare: TokenKind, with_eq: TokenKind) -> TokenKind {
        self.pos += 1;
        if matches!(self.src.get(self.pos), Some(&b'=')) {
            self.pos += 1;
            with_eq
        } else {
            bare
        }
    }

    /// Disambiguate `&`: `&&` is logical-and, `&=` is and-assign,
    /// bare `&` is bitwise-and.
    fn lex_after_amp(&mut self) -> TokenKind {
        self.pos += 1;
        match self.src.get(self.pos) {
            Some(&b'&') => { self.pos += 1; TokenKind::AmpAmp }
            Some(&b'=') => { self.pos += 1; TokenKind::AmpEq }
            _ => TokenKind::Ampersand,
        }
    }

    /// Disambiguate `|`: `||` is logical-or, `|=` is or-assign,
    /// bare `|` is bitwise-or.
    fn lex_after_pipe(&mut self) -> TokenKind {
        self.pos += 1;
        match self.src.get(self.pos) {
            Some(&b'|') => { self.pos += 1; TokenKind::PipePipe }
            Some(&b'=') => { self.pos += 1; TokenKind::PipeEq }
            _ => TokenKind::Pipe,
        }
    }

    /// Disambiguate `!`: `!=` is inequality, bare `!` is logical not.
    fn lex_after_bang(&mut self) -> TokenKind {
        self.pos += 1;
        if matches!(self.src.get(self.pos), Some(&b'=')) {
            self.pos += 1;
            TokenKind::BangEq
        } else {
            TokenKind::Bang
        }
    }

    /// Disambiguate `<`: `<<` is shift, `<=` is less-or-equal, bare `<`
    /// is strict less-than.
    fn lex_after_lt(&mut self) -> TokenKind {
        self.pos += 1;
        match self.src.get(self.pos) {
            Some(&b'<') => {
                self.pos += 1;
                if matches!(self.src.get(self.pos), Some(&b'=')) {
                    self.pos += 1;
                    TokenKind::ShlEq
                } else {
                    TokenKind::ShiftLeft
                }
            }
            Some(&b'=') => { self.pos += 1; TokenKind::Le }
            _ => TokenKind::Lt,
        }
    }

    /// Disambiguate `>`: `>>` is shift, `>=` is greater-or-equal, bare
    /// `>` is strict greater-than.
    fn lex_after_gt(&mut self) -> TokenKind {
        self.pos += 1;
        match self.src.get(self.pos) {
            Some(&b'>') => {
                self.pos += 1;
                if matches!(self.src.get(self.pos), Some(&b'=')) {
                    self.pos += 1;
                    TokenKind::ShrEq
                } else {
                    TokenKind::ShiftRight
                }
            }
            Some(&b'=') => { self.pos += 1; TokenKind::Ge }
            _ => TokenKind::Gt,
        }
    }

    /// `"<chars>"` with simple C-style escape sequences. Calls
    /// `decode_escape` for the per-escape decoder shared with
    /// character literals.
    fn lex_string_literal(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        self.pos += 1; // opening `"`
        let mut bytes = Vec::new();
        loop {
            let Some(&b) = self.src.get(self.pos) else {
                return Err(LexError::UnterminatedString { offset: off(start) });
            };
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(TokenKind::StringLit(bytes));
                }
                b'\\' => {
                    bytes.push(self.decode_escape(start)?);
                }
                _ => {
                    bytes.push(b);
                    self.pos += 1;
                }
            }
        }
    }

    /// `'<char>'` — character constant. C90 says character constants
    /// have type `int`, so we emit `IntLit` directly. Same escape
    /// alphabet as strings via `decode_escape`. Multi-byte constants
    /// (`'ab'`) await a fixture.
    fn lex_char_literal(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        self.pos += 1; // opening `'`
        // Multi-byte character literals (`'AB'`). BCC packs the
        // first char into the LOW byte and the second into the
        // HIGH byte — so `'AB'` is 0x4241 = 16961 (little-endian
        // byte order). Single-char literals are just the byte
        // value. Fixture 3386.
        let mut value: u32 = 0;
        let mut count: u32 = 0;
        loop {
            let Some(&b) = self.src.get(self.pos) else {
                return Err(LexError::UnterminatedString { offset: off(start) });
            };
            if b == b'\'' {
                break;
            }
            let byte = if b == b'\\' {
                self.decode_escape(start)?
            } else {
                self.pos += 1;
                b
            };
            value |= u32::from(byte) << (count * 8);
            count += 1;
            if count > 4 {
                return Err(LexError::UnexpectedChar {
                    ch: byte as char,
                    offset: off(self.pos),
                });
            }
        }
        self.pos += 1; // closing `'`
        if count == 0 {
            return Err(LexError::UnterminatedString { offset: off(start) });
        }
        Ok(TokenKind::IntLit(value))
    }

    /// Decode one C escape sequence starting at the backslash. The
    /// caller passes the literal's start offset for error messages.
    /// Advances `self.pos` past the escape. Returns the decoded byte.
    fn decode_escape(&mut self, lit_start: usize) -> Result<u8, LexError> {
        self.pos += 1; // backslash
        let Some(&esc) = self.src.get(self.pos) else {
            return Err(LexError::UnterminatedString { offset: off(lit_start) });
        };
        if matches!(esc, b'x' | b'X') {
            self.pos += 1;
            let hex_start = self.pos;
            let mut value: u32 = 0;
            while let Some(d) = self.src.get(self.pos).and_then(|b| (*b as char).to_digit(16)) {
                value = value.wrapping_mul(16).wrapping_add(d);
                self.pos += 1;
            }
            if self.pos == hex_start {
                return Err(LexError::UnknownEscape { ch: esc as char, offset: off(hex_start - 1) });
            }
            return Ok((value & 0xFF) as u8);
        }
        // Octal escapes: `\<o1>` / `\<o1><o2>` / `\<o1><o2><o3>` where
        // o[123] are digits 0..=7. Up to three octal digits, stopping at
        // the first non-octal char. Fixture 2423 (`"\003\012\077"`).
        if matches!(esc, b'0'..=b'7') {
            let mut value: u32 = 0;
            for _ in 0..3 {
                let Some(&d) = self.src.get(self.pos) else { break };
                if !(b'0'..=b'7').contains(&d) {
                    break;
                }
                value = value.wrapping_mul(8).wrapping_add(u32::from(d - b'0'));
                self.pos += 1;
            }
            return Ok((value & 0xFF) as u8);
        }
        self.pos += 1;
        Ok(match esc {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'\\' => b'\\',
            b'\'' => b'\'',
            b'"' => b'"',
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0C,
            b'v' => 0x0B,
            other => {
                return Err(LexError::UnknownEscape {
                    ch: other as char,
                    offset: off(self.pos - 1),
                });
            }
        })
    }

    fn lex_int_literal(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let mut value: u64 = 0;
        // C90 prefixes: `0x`/`0X` → hex; bare leading `0` followed by an
        // octal digit → octal; otherwise decimal. A lone `0` (no prefix
        // digit) is decimal zero — handled by the main decimal loop.
        let radix: u32 =
            if matches!(self.src.get(self.pos), Some(b'0'))
                && matches!(self.src.get(self.pos + 1), Some(b'x' | b'X'))
            {
                self.pos += 2;
                16
            } else if matches!(self.src.get(self.pos), Some(b'0'))
                && matches!(self.src.get(self.pos + 1), Some(b'0'..=b'7'))
            {
                self.pos += 1;
                8
            } else {
                10
            };
        while let Some(&b) = self.src.get(self.pos) {
            if let Some(d) = (b as char).to_digit(radix) {
                value = value
                    .checked_mul(u64::from(radix))
                    .and_then(|v| v.checked_add(u64::from(d)))
                    .ok_or(LexError::IntOverflow { offset: off(start) })?;
                self.pos += 1;
            } else {
                break;
            }
        }
        // Float promotion: a `.`, `e`/`E`, or `f`/`F` after the integer
        // part means we're actually lexing a floating-point literal.
        // Only decimal sources can promote — `0x1.fp0` (C99 hex float)
        // isn't in scope.
        if radix == 10 {
            let b = self.src.get(self.pos).copied();
            if matches!(b, Some(b'.' | b'e' | b'E' | b'f' | b'F')) {
                return self.lex_float_tail(start);
            }
        }
        // Optional integer-type suffix. C90 has `L`/`l` for long,
        // `U`/`u` for unsigned, and combinations (`UL`, `LU`, etc.).
        // We accept and discard them — `IntLit(u32)` already holds
        // enough range; the surrounding type context decides the
        // ultimate width (e.g. `long g = 100000L;`, fixture 209).
        while let Some(&b) = self.src.get(self.pos) {
            if matches!(b, b'L' | b'l' | b'U' | b'u') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let v32 = u32::try_from(value).map_err(|_| LexError::IntOverflow { offset: off(start) })?;
        Ok(TokenKind::IntLit(v32))
    }

    /// Tail of a floating-point literal — called after the integer part
    /// (possibly empty for `.5`) has been scanned. Picks up the optional
    /// `.<digits>`, optional `[eE][+-]?<digits>` exponent, and optional
    /// `f`/`F`/`l`/`L` suffix, then converts the lexeme to IEEE 754.
    /// Returns a `FloatLit` if the suffix is `f`/`F`, else `DoubleLit`.
    fn lex_float_tail(&mut self, start: usize) -> Result<TokenKind, LexError> {
        // Fractional part (optional `.<digits>`).
        if matches!(self.src.get(self.pos), Some(b'.')) {
            self.pos += 1;
            while let Some(&b) = self.src.get(self.pos) {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        // Exponent (`[eE][+-]?<digits>`).
        if matches!(self.src.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.src.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while let Some(&b) = self.src.get(self.pos) {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        // Suffix: `f`/`F` forces single-precision; `l`/`L` is `long
        // double` (BCC treats it as double); no suffix is double.
        let is_float = match self.src.get(self.pos) {
            Some(b'f' | b'F') => { self.pos += 1; true }
            Some(b'l' | b'L') => { self.pos += 1; false }
            _ => false,
        };
        let lexeme = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| LexError::UnexpectedChar { ch: '?', offset: off(start) })?;
        // Strip the suffix so Rust's parser accepts the digits.
        let stripped = match lexeme.as_bytes().last() {
            Some(b'f' | b'F' | b'l' | b'L') => &lexeme[..lexeme.len() - 1],
            _ => lexeme,
        };
        if is_float {
            let v: f32 = stripped.parse()
                .map_err(|_| LexError::IntOverflow { offset: off(start) })?;
            Ok(TokenKind::FloatLit(v.to_bits()))
        } else {
            let v: f64 = stripped.parse()
                .map_err(|_| LexError::IntOverflow { offset: off(start) })?;
            Ok(TokenKind::DoubleLit(v.to_bits()))
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

fn is_ident_continue(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        Lexer::new(src).tokenize().unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn fixture_001() {
        assert_eq!(
            kinds("int main(void) { return 0; }\n"),
            vec![
                KwInt,
                Ident("main".into()),
                LParen,
                KwVoid,
                RParen,
                LBrace,
                KwReturn,
                IntLit(0),
                Semicolon,
                RBrace,
                Eof,
            ]
        );
    }

    #[test]
    fn fixture_003() {
        assert_eq!(
            kinds("int main(void) { return 42; }\n"),
            vec![
                KwInt,
                Ident("main".into()),
                LParen,
                KwVoid,
                RParen,
                LBrace,
                KwReturn,
                IntLit(42),
                Semicolon,
                RBrace,
                Eof,
            ]
        );
    }
}
