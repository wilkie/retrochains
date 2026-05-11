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
                b';' => { self.pos += 1; TokenKind::Semicolon }
                b'=' => { self.pos += 1; TokenKind::Equals }
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
            b"void" => TokenKind::KwVoid,
            b"return" => TokenKind::KwReturn,
            other => TokenKind::Ident(String::from_utf8_lossy(other).into_owned()),
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
