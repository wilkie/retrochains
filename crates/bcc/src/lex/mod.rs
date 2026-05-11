//! Tokenization. Hand-written; see `specs/bcc/PARSER.md`.

mod lexer;
mod token;

pub use lexer::{LexError, Lexer};
pub use token::{Span, Token, TokenKind};
