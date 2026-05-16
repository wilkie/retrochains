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
        while let Some(&b) = self.src.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
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

    /// `"<chars>"` with simple C-style escape sequences. We handle
    /// the escapes BCC's stdio formats most commonly want — newline,
    /// tab, the two quote forms, backslash, and null. Fancier ones
    /// (`\x`, `\<octal>`, `\a`, `\v`) wait for a fixture.
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
                    self.pos += 1;
                    let Some(&esc) = self.src.get(self.pos) else {
                        return Err(LexError::UnterminatedString { offset: off(start) });
                    };
                    self.pos += 1;
                    let decoded = match esc {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'0' => 0u8,
                        b'\\' => b'\\',
                        b'\'' => b'\'',
                        b'"' => b'"',
                        other => {
                            return Err(LexError::UnknownEscape {
                                ch: other as char,
                                offset: off(self.pos - 1),
                            });
                        }
                    };
                    bytes.push(decoded);
                }
                _ => {
                    bytes.push(b);
                    self.pos += 1;
                }
            }
        }
    }

    fn lex_int_literal(&mut self) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let mut value: u64 = 0;
        while let Some(&b) = self.src.get(self.pos) {
            if let Some(d) = (b as char).to_digit(10) {
                value = value
                    .checked_mul(10)
                    .and_then(|v| v.checked_add(u64::from(d)))
                    .ok_or(LexError::IntOverflow { offset: off(start) })?;
                self.pos += 1;
            } else {
                break;
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
