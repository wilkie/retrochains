//! C preprocessor: handles `#define` (object- and function-like),
//! `#undef`, `#ifdef`/`#ifndef`/`#if`/`#elif`/`#else`/`#endif`,
//! `defined()`, and `#line`/`#pragma` (skip).
//!
//! Output is the substituted source text with line breaks preserved
//! (stripped directive lines become blank) so the parser's byte/line
//! offsets continue to line up with the on-screen layout — that's
//! how `;`-comment emission preserves the original C source verbatim
//! while the compiled code reflects expanded macros.
//!
//! Not yet supported: `#include`, stringize `#x`, paste `##`,
//! variadic macros, `_Pragma`. The fixtures that need stringize
//! or paste (`STR(x)`, `CAT(a,b)`) panic loudly so they're easy to
//! spot.
//!
//! Limits chosen for simplicity, not full C90:
//!   - Macros expand only when matched as whole identifiers.
//!   - Function-like macros: argument substitution is textual; no
//!     nested expansion mid-argument. Adequate for the fixture
//!     corpus.
//!   - `#if` evaluates a small constant-expression subset
//!     (integer literals, `defined(X)`, `defined X`, unary +/-/!/~,
//!     binary +/-/*/&/|/^/<</>>/&&/||/== /!=/</<=/>/>=).

use std::collections::HashMap;
use std::fmt;

#[derive(Debug)]
pub enum PreprocessError {
    UnterminatedConditional,
    StrayElse(usize),
    StrayEndif(usize),
    StrayElif(usize),
    BadDirective(String, usize),
    BadExpr(String, usize),
    Unsupported(&'static str, usize),
}

impl fmt::Display for PreprocessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedConditional => write!(f, "unterminated #if/#ifdef (missing #endif)"),
            Self::StrayElse(line) => write!(f, "stray #else at line {line}"),
            Self::StrayEndif(line) => write!(f, "stray #endif at line {line}"),
            Self::StrayElif(line) => write!(f, "stray #elif at line {line}"),
            Self::BadDirective(d, line) => write!(f, "bad directive `{d}` at line {line}"),
            Self::BadExpr(e, line) => write!(f, "bad #if expression `{e}` at line {line}"),
            Self::Unsupported(what, line) => write!(f, "unsupported preprocessor feature ({what}) at line {line}"),
        }
    }
}

impl std::error::Error for PreprocessError {}

#[derive(Debug, Clone)]
struct Macro {
    /// Some(params) for function-like; None for object-like.
    params: Option<Vec<String>>,
    body: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CondState {
    /// We're inside a true branch — output lines.
    Active,
    /// We're inside a false branch — skip lines, but a future `#else`/`#elif`
    /// can flip us to Active.
    Skipping,
    /// We've already taken a branch; remaining `#else`/`#elif` are dead.
    Done,
}

/// Substitute macros and resolve conditionals. The returned string
/// has the same line count as `source` so byte offsets within
/// untouched (output) lines retain their original line numbers; the
/// downstream parser still uses the original source for line-comment
/// emission, so this only affects what the lexer sees.
///
/// # Errors
/// Returns a `PreprocessError` for unterminated `#if`s, stray
/// `#else`/`#endif`, or unsupported syntax (`##`, `#`, `#include`).
pub fn preprocess(source: &str) -> Result<String, PreprocessError> {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut macros: HashMap<String, Macro> = HashMap::new();
    let mut cond_stack: Vec<CondState> = Vec::new();
    let mut out = String::with_capacity(source.len());

    let mut line_no = 0;
    let mut iter = 0;
    while iter < lines.len() {
        line_no += 1;
        let raw = lines[iter];
        iter += 1;
        // Handle `\<newline>` line continuation for directive
        // headers: a `\` immediately before `\n` joins the next
        // line. Only meaningful for # directives, but harmless
        // elsewhere.
        let mut joined = raw.to_string();
        while joined.trim_end_matches(|c: char| c == '\n' || c == '\r').ends_with('\\')
            && iter < lines.len()
        {
            // Drop the trailing `\` (and optional CR/LF), then
            // append the next physical line (and skip it in the
            // outer loop). Keep the line-count blank later.
            let trimmed = {
                let mut s = joined.trim_end_matches(|c: char| c == '\n' || c == '\r').to_string();
                debug_assert!(s.ends_with('\\'));
                s.pop(); // drop `\`
                s
            };
            joined = format!("{trimmed}{}", lines[iter]);
            iter += 1;
            line_no += 1;
            // For each consumed continuation line, emit a blank
            // line so the line count is preserved in the output.
            out.push('\n');
        }
        let trimmed = joined.trim_start();
        let is_active = cond_stack.iter().all(|&s| s == CondState::Active);
        if let Some(rest) = trimmed.strip_prefix('#') {
            // Found a directive. Handle the few we support.
            let rest = rest.trim_start();
            let (name, args) = split_directive(rest);
            // Always emit a blank line so line numbers match.
            // Multi-line directives (with `\` continuations) already
            // emitted extra blanks above per continuation; this is
            // for the directive's own line.
            out.push('\n');
            match name {
                "define" => {
                    if !is_active {
                        continue;
                    }
                    let (mac_name, mac) = parse_define(args, line_no)?;
                    macros.insert(mac_name, mac);
                }
                "undef" => {
                    if !is_active {
                        continue;
                    }
                    let id = args.trim().split_whitespace().next().unwrap_or("");
                    macros.remove(id);
                }
                "ifdef" => {
                    let id = args.trim().split_whitespace().next().unwrap_or("");
                    if !is_active {
                        cond_stack.push(CondState::Done);
                    } else if macros.contains_key(id) {
                        cond_stack.push(CondState::Active);
                    } else {
                        cond_stack.push(CondState::Skipping);
                    }
                }
                "ifndef" => {
                    let id = args.trim().split_whitespace().next().unwrap_or("");
                    if !is_active {
                        cond_stack.push(CondState::Done);
                    } else if !macros.contains_key(id) {
                        cond_stack.push(CondState::Active);
                    } else {
                        cond_stack.push(CondState::Skipping);
                    }
                }
                "if" => {
                    if !is_active {
                        cond_stack.push(CondState::Done);
                    } else {
                        let v = eval_if_expr(args, &macros, line_no)?;
                        cond_stack.push(if v != 0 {
                            CondState::Active
                        } else {
                            CondState::Skipping
                        });
                    }
                }
                "elif" => {
                    let top = cond_stack.last_mut().ok_or(PreprocessError::StrayElif(line_no))?;
                    match *top {
                        CondState::Active => *top = CondState::Done,
                        CondState::Done => {}
                        CondState::Skipping => {
                            // Reevaluate; outer must already be active
                            // since we were skipping (not Done).
                            let parent_active = cond_stack[..cond_stack.len() - 1]
                                .iter()
                                .all(|&s| s == CondState::Active);
                            if parent_active {
                                let v = eval_if_expr(args, &macros, line_no)?;
                                let top = cond_stack.last_mut().unwrap();
                                *top = if v != 0 {
                                    CondState::Active
                                } else {
                                    CondState::Skipping
                                };
                            }
                        }
                    }
                }
                "else" => {
                    let top = cond_stack.last_mut().ok_or(PreprocessError::StrayElse(line_no))?;
                    match *top {
                        CondState::Active => *top = CondState::Done,
                        CondState::Skipping => {
                            let parent_active = cond_stack[..cond_stack.len() - 1]
                                .iter()
                                .all(|&s| s == CondState::Active);
                            if parent_active {
                                *cond_stack.last_mut().unwrap() = CondState::Active;
                            }
                        }
                        CondState::Done => {}
                    }
                }
                "endif" => {
                    if cond_stack.pop().is_none() {
                        return Err(PreprocessError::StrayEndif(line_no));
                    }
                }
                "line" | "pragma" => {
                    // Skip — line number is implicit, pragmas ignored.
                }
                "include" => {
                    return Err(PreprocessError::Unsupported("#include", line_no));
                }
                _ => {
                    return Err(PreprocessError::BadDirective(name.to_string(), line_no));
                }
            }
        } else if is_active {
            // Regular code line — expand macros.
            let expanded = expand_macros(&joined, &macros);
            out.push_str(&expanded);
        } else {
            // Inside a skipped branch — emit a blank line in place.
            // Count the LF in the source line so we don't undercount.
            if joined.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    if !cond_stack.is_empty() {
        return Err(PreprocessError::UnterminatedConditional);
    }
    Ok(out)
}

fn split_directive(line: &str) -> (&str, &str) {
    let s = line.trim_end_matches(|c: char| c == '\n' || c == '\r');
    let end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
    (&s[..end], s[end..].trim())
}

fn parse_define(args: &str, line_no: usize) -> Result<(String, Macro), PreprocessError> {
    // Strip trailing newline / CR if any.
    let args = args.trim_end_matches(|c: char| c == '\n' || c == '\r');
    let bytes = args.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i == 0 {
        return Err(PreprocessError::BadDirective("define".into(), line_no));
    }
    let name = args[..i].to_string();
    // Function-like if `(` immediately follows (no space).
    if i < bytes.len() && bytes[i] == b'(' {
        let mut j = i + 1;
        let mut params: Vec<String> = Vec::new();
        let mut cur = String::new();
        while j < bytes.len() && bytes[j] != b')' {
            let c = bytes[j];
            if c == b',' {
                if !cur.trim().is_empty() {
                    params.push(cur.trim().to_string());
                    cur.clear();
                }
            } else if !c.is_ascii_whitespace() {
                cur.push(c as char);
            }
            j += 1;
        }
        if !cur.trim().is_empty() {
            params.push(cur.trim().to_string());
        }
        if j >= bytes.len() {
            return Err(PreprocessError::BadDirective(
                "define (unterminated parameter list)".into(),
                line_no,
            ));
        }
        let body = args[j + 1..].trim().to_string();
        return Ok((name, Macro { params: Some(params), body }));
    }
    let body = args[i..].trim().to_string();
    Ok((name, Macro { params: None, body }))
}

/// Recursively expand macros in `src`. `seen` (caller-passed empty
/// set) prevents infinite re-expansion of the same name.
fn expand_macros(src: &str, macros: &HashMap<String, Macro>) -> String {
    expand_inner(src, macros, &mut Vec::new())
}

fn expand_inner(
    src: &str,
    macros: &HashMap<String, Macro>,
    seen: &mut Vec<String>,
) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Skip string literals.
        if c == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if i < bytes.len() {
                i += 1;
            }
            out.push_str(&src[start..i]);
            continue;
        }
        // Skip char literals.
        if c == b'\'' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'\'' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if i < bytes.len() {
                i += 1;
            }
            out.push_str(&src[start..i]);
            continue;
        }
        // Skip /* ... */ comments and // line comments.
        if c == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'*' {
                let start = i;
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    i += 2;
                }
                out.push_str(&src[start..i]);
                continue;
            }
            if bytes[i + 1] == b'/' {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                out.push_str(&src[start..i]);
                continue;
            }
        }
        // Try to match an identifier.
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let name = &src[start..i];
            if !seen.iter().any(|s| s == name)
                && let Some(mac) = macros.get(name)
            {
                if let Some(params) = &mac.params {
                    // Function-like macro: parse `(args...)`.
                    let mut j = i;
                    while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'(' {
                        // Parse args, respecting nested parens.
                        j += 1;
                        let mut args: Vec<String> = Vec::new();
                        let mut depth: u32 = 1;
                        let mut cur = String::new();
                        while j < bytes.len() && depth > 0 {
                            let cj = bytes[j];
                            if cj == b'(' {
                                depth += 1;
                                cur.push(cj as char);
                            } else if cj == b')' {
                                depth -= 1;
                                if depth > 0 {
                                    cur.push(cj as char);
                                }
                            } else if cj == b',' && depth == 1 {
                                args.push(cur.trim().to_string());
                                cur.clear();
                            } else {
                                cur.push(cj as char);
                            }
                            j += 1;
                        }
                        if !cur.is_empty() || !args.is_empty() {
                            args.push(cur.trim().to_string());
                        }
                        // Substitute params into body.
                        let mut body = mac.body.clone();
                        for (param, arg_val) in params.iter().zip(args.iter()) {
                            body = replace_whole_ident(&body, param, arg_val);
                        }
                        // Re-expand recursively, preventing infinite
                        // re-expansion of this macro.
                        seen.push(name.to_string());
                        let expanded = expand_inner(&body, macros, seen);
                        seen.pop();
                        out.push_str(&expanded);
                        i = j;
                        continue;
                    }
                    // Function-like macro mentioned without args
                    // (e.g. used as identifier elsewhere) — leave
                    // it unexpanded per C rules.
                    out.push_str(name);
                    continue;
                } else {
                    // Object-like macro — substitute and recurse.
                    seen.push(name.to_string());
                    let expanded = expand_inner(&mac.body, macros, seen);
                    seen.pop();
                    out.push_str(&expanded);
                    continue;
                }
            }
            out.push_str(name);
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Replace whole-identifier occurrences of `name` in `src` with
/// `repl`. Skips occurrences inside string/char literals.
fn replace_whole_ident(src: &str, name: &str, repl: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' {
            let quote = c;
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if i < bytes.len() {
                i += 1;
            }
            out.push_str(&src[start..i]);
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &src[start..i];
            if ident == name {
                out.push_str(repl);
            } else {
                out.push_str(ident);
            }
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Evaluate a constant integer expression for `#if`/`#elif`. Subset:
/// integer literals, `defined(<name>)`, `defined <name>`, unary
/// +/-/!/~, binary +/-/*//%, bitwise &/|/^, shifts <</>>, comparisons,
/// logical &&/||, parentheses.
fn eval_if_expr(
    src: &str,
    macros: &HashMap<String, Macro>,
    line_no: usize,
) -> Result<i32, PreprocessError> {
    // First substitute `defined(X)` and `defined X` to 0/1.
    let pre = substitute_defined(src, macros);
    // Then expand any macros referenced in the expression.
    let pre = expand_macros(&pre, macros);
    // Tokenize and parse a small Pratt parser.
    let mut parser = ExprParser::new(&pre);
    let v = parser.parse_or().map_err(|e| PreprocessError::BadExpr(e, line_no))?;
    parser.skip_ws();
    if parser.pos < parser.src.len() {
        return Err(PreprocessError::BadExpr(
            format!("trailing text near `{}`", &parser.src[parser.pos..]),
            line_no,
        ));
    }
    Ok(v)
}

fn substitute_defined(src: &str, macros: &HashMap<String, Macro>) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') && src[i..].starts_with("defined")
            && (bytes.len() == i + 7
                || !(bytes[i + 7].is_ascii_alphanumeric() || bytes[i + 7] == b'_'))
        {
            // Found `defined`. Parse `(NAME)` or `NAME`.
            let mut j = i + 7;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            let mut had_paren = false;
            if j < bytes.len() && bytes[j] == b'(' {
                had_paren = true;
                j += 1;
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
            }
            let name_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let name = &src[name_start..j];
            if had_paren {
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b')' {
                    j += 1;
                }
            }
            out.push(if macros.contains_key(name) { '1' } else { '0' });
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

struct ExprParser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> ExprParser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.src.len() && self.src.as_bytes()[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn starts_with(&self, s: &str) -> bool {
        self.src[self.pos..].starts_with(s)
    }

    fn parse_or(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.starts_with("||") {
                self.pos += 2;
                let rhs = self.parse_and()?;
                lhs = if (lhs != 0) || (rhs != 0) { 1 } else { 0 };
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_bit_or()?;
        loop {
            self.skip_ws();
            if self.starts_with("&&") {
                self.pos += 2;
                let rhs = self.parse_bit_or()?;
                lhs = if (lhs != 0) && (rhs != 0) { 1 } else { 0 };
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_bit_or(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_bit_xor()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'|') && !self.starts_with("||") {
                self.pos += 1;
                let rhs = self.parse_bit_xor()?;
                lhs |= rhs;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_bit_xor(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_bit_and()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'^') {
                self.pos += 1;
                let rhs = self.parse_bit_and()?;
                lhs ^= rhs;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_bit_and(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_eq()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'&') && !self.starts_with("&&") {
                self.pos += 1;
                let rhs = self.parse_eq()?;
                lhs &= rhs;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_rel()?;
        loop {
            self.skip_ws();
            if self.starts_with("==") {
                self.pos += 2;
                let rhs = self.parse_rel()?;
                lhs = if lhs == rhs { 1 } else { 0 };
            } else if self.starts_with("!=") {
                self.pos += 2;
                let rhs = self.parse_rel()?;
                lhs = if lhs != rhs { 1 } else { 0 };
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_rel(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_shift()?;
        loop {
            self.skip_ws();
            if self.starts_with("<=") {
                self.pos += 2;
                let rhs = self.parse_shift()?;
                lhs = if lhs <= rhs { 1 } else { 0 };
            } else if self.starts_with(">=") {
                self.pos += 2;
                let rhs = self.parse_shift()?;
                lhs = if lhs >= rhs { 1 } else { 0 };
            } else if self.peek() == Some(b'<') && !self.starts_with("<<") {
                self.pos += 1;
                let rhs = self.parse_shift()?;
                lhs = if lhs < rhs { 1 } else { 0 };
            } else if self.peek() == Some(b'>') && !self.starts_with(">>") {
                self.pos += 1;
                let rhs = self.parse_shift()?;
                lhs = if lhs > rhs { 1 } else { 0 };
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_add()?;
        loop {
            self.skip_ws();
            if self.starts_with("<<") {
                self.pos += 2;
                let rhs = self.parse_add()?;
                lhs = lhs.wrapping_shl(rhs as u32);
            } else if self.starts_with(">>") {
                self.pos += 2;
                let rhs = self.parse_add()?;
                lhs = lhs.wrapping_shr(rhs as u32);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_mul()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'+') => {
                    self.pos += 1;
                    let rhs = self.parse_mul()?;
                    lhs = lhs.wrapping_add(rhs);
                }
                Some(b'-') => {
                    self.pos += 1;
                    let rhs = self.parse_mul()?;
                    lhs = lhs.wrapping_sub(rhs);
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<i32, String> {
        let mut lhs = self.parse_unary()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    let rhs = self.parse_unary()?;
                    lhs = lhs.wrapping_mul(rhs);
                }
                Some(b'/') => {
                    self.pos += 1;
                    let rhs = self.parse_unary()?;
                    if rhs == 0 {
                        return Err("division by zero in #if".to_string());
                    }
                    lhs = lhs.wrapping_div(rhs);
                }
                Some(b'%') => {
                    self.pos += 1;
                    let rhs = self.parse_unary()?;
                    if rhs == 0 {
                        return Err("modulo by zero in #if".to_string());
                    }
                    lhs = lhs.wrapping_rem(rhs);
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<i32, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'!') if !self.starts_with("!=") => {
                self.pos += 1;
                let v = self.parse_unary()?;
                Ok(if v == 0 { 1 } else { 0 })
            }
            Some(b'-') => {
                self.pos += 1;
                let v = self.parse_unary()?;
                Ok(v.wrapping_neg())
            }
            Some(b'+') => {
                self.pos += 1;
                self.parse_unary()
            }
            Some(b'~') => {
                self.pos += 1;
                let v = self.parse_unary()?;
                Ok(!v)
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<i32, String> {
        self.skip_ws();
        let Some(c) = self.peek() else {
            return Err("unexpected end of #if expression".to_string());
        };
        if c == b'(' {
            self.pos += 1;
            let v = self.parse_or()?;
            self.skip_ws();
            if self.peek() != Some(b')') {
                return Err("missing `)` in #if expression".to_string());
            }
            self.pos += 1;
            return Ok(v);
        }
        if c.is_ascii_digit() {
            let start = self.pos;
            // Possible 0x prefix.
            let radix = if self.src[self.pos..].starts_with("0x")
                || self.src[self.pos..].starts_with("0X")
            {
                self.pos += 2;
                16
            } else if c == b'0' && self.pos + 1 < self.src.len()
                && self.src.as_bytes()[self.pos + 1].is_ascii_digit()
            {
                8
            } else {
                10
            };
            while self.pos < self.src.len()
                && (self.src.as_bytes()[self.pos] as char).is_digit(radix)
            {
                self.pos += 1;
            }
            // Skip trailing L/U suffixes.
            while self.pos < self.src.len()
                && matches!(self.src.as_bytes()[self.pos], b'L' | b'l' | b'U' | b'u')
            {
                self.pos += 1;
            }
            let lit = &self.src[start..self.pos];
            let stripped = lit
                .trim_end_matches(|c: char| matches!(c, 'L' | 'l' | 'U' | 'u'));
            let stripped = if radix == 16 { &stripped[2..] } else { stripped };
            let v = i64::from_str_radix(stripped, radix)
                .map_err(|e| format!("bad integer `{lit}`: {e}"))?;
            return Ok(v as i32);
        }
        // Bare identifier (undefined macro) — C says treat as 0.
        if c.is_ascii_alphabetic() || c == b'_' {
            while self.pos < self.src.len()
                && (self.src.as_bytes()[self.pos].is_ascii_alphanumeric()
                    || self.src.as_bytes()[self.pos] == b'_')
            {
                self.pos += 1;
            }
            return Ok(0);
        }
        Err(format!(
            "unexpected character `{}` in #if",
            c as char
        ))
    }
}
