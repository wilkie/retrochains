//! A small C preprocessor pass applied to the source text before lexing.
//! Handles object- and function-like `#define` macros (with `#`/`##`),
//! `#undef`, and conditional compilation (`#if`/`#ifdef`/`#ifndef`/`#elif`/
//! `#else`/`#endif`, including the `defined` operator). Unknown directives
//! (`#include`, `#pragma`, …) are dropped, matching the prior lexer behavior of
//! treating each `#` line as an ignored `PreprocLine`.

use crate::EmitError;
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
struct Macro {
    /// `Some(params)` for a function-like macro; `None` for object-like.
    params: Option<Vec<String>>,
    body: String,
}

struct Frame {
    /// Whether the current branch's lines are emitted.
    active: bool,
    /// Whether any branch of this `#if` group has been taken yet.
    taken: bool,
    /// Whether the enclosing context was active when this group opened.
    enclosing_active: bool,
}

/// Returns true when an identifier character (for C identifiers / macro names).
fn is_ident_byte(b: u8, first: bool) -> bool {
    b == b'_' || b.is_ascii_alphabetic() || (!first && b.is_ascii_digit())
}

/// Returns the expanded source plus a flag set when an active `#include`
/// directive was seen. MSC's EXTDEF ordering for an implicitly-declared
/// function depends on whether the TU pulled in a header: with a `#include`
/// the implicit extern leads and `__chkstk` trails (fixture 4103); without
/// one the layout matches a no-user-extern TU — `__chkstk` early, implicit
/// extern trailing (fixture 108).
pub(crate) fn preprocess(source: &str) -> Result<(String, bool), EmitError> {
    // Splice line continuations (`\` immediately before a newline).
    let joined = source.replace("\\\r\n", "").replace("\\\n", "");
    let mut macros: HashMap<String, Macro> = HashMap::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut out = String::new();
    let mut had_include = false;
    let all_active = |s: &[Frame]| s.iter().all(|f| f.active);

    for line in joined.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let rest = rest.trim_start();
            let (word, args) = split_directive(rest);
            match word {
                "define" if all_active(&stack) => define(args, &mut macros)?,
                "undef" if all_active(&stack) => { macros.remove(first_ident(args)); }
                "ifdef" => {
                    let enc = all_active(&stack);
                    let cond = enc && macros.contains_key(first_ident(args));
                    stack.push(Frame { active: cond, taken: cond, enclosing_active: enc });
                }
                "ifndef" => {
                    let enc = all_active(&stack);
                    let cond = enc && !macros.contains_key(first_ident(args));
                    stack.push(Frame { active: cond, taken: cond, enclosing_active: enc });
                }
                "if" => {
                    let enc = all_active(&stack);
                    let cond = enc && eval_const_expr(args, &macros)? != 0;
                    stack.push(Frame { active: cond, taken: cond, enclosing_active: enc });
                }
                "elif" => {
                    let f = stack.last_mut().ok_or_else(|| err("#elif without #if"))?;
                    if !f.taken && f.enclosing_active && eval_const_expr(args, &macros)? != 0 {
                        f.active = true; f.taken = true;
                    } else {
                        f.active = false;
                    }
                }
                "else" => {
                    let f = stack.last_mut().ok_or_else(|| err("#else without #if"))?;
                    f.active = !f.taken && f.enclosing_active;
                    f.taken = true;
                }
                "endif" => { stack.pop().ok_or_else(|| err("#endif without #if"))?; }
                "include" if all_active(&stack) => { had_include = true; }
                // define/undef in an inactive branch, or any other directive
                // (#pragma/#error/#line/…) — ignore.
                _ => {}
            }
            continue;
        }
        if all_active(&stack) {
            out.push_str(&expand(line, &macros, &mut HashSet::new()));
            out.push('\n');
        }
    }
    Ok((out, had_include))
}

fn err(msg: &str) -> EmitError {
    EmitError::Unsupported(format!("preprocessor: {msg}"))
}

/// Split `<word> <rest>` of a directive into the keyword and the remainder.
fn split_directive(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
    (&s[..end], s[end..].trim_start())
}

/// The first identifier token in `s` (for `#ifdef NAME` / `#undef NAME`).
fn first_ident(s: &str) -> &str {
    let s = s.trim_start();
    let end = s.find(|c: char| !(c == '_' || c.is_ascii_alphanumeric())).unwrap_or(s.len());
    &s[..end]
}

/// Process a `#define`: `NAME body` (object-like) or `NAME(p1,p2) body`.
fn define(s: &str, macros: &mut HashMap<String, Macro>) -> Result<(), EmitError> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && is_ident_byte(bytes[i], i == 0) { i += 1; }
    if i == 0 { return Err(err("malformed #define")); }
    let name = s[..i].to_string();
    // Function-like only when `(` immediately follows the name (no space).
    if bytes.get(i) == Some(&b'(') {
        i += 1;
        let mut params = Vec::new();
        let close = s[i..].find(')').ok_or_else(|| err("unterminated macro params"))? + i;
        for p in s[i..close].split(',') {
            let p = p.trim();
            if !p.is_empty() { params.push(p.to_string()); }
        }
        let body = s[close + 1..].trim().to_string();
        macros.insert(name, Macro { params: Some(params), body });
    } else {
        let body = s[i..].trim().to_string();
        macros.insert(name, Macro { params: None, body });
    }
    Ok(())
}

/// Expand all macro invocations in `text`. `hide` holds the names currently
/// being expanded (prevents infinite self-reference).
fn expand(text: &str, macros: &HashMap<String, Macro>, hide: &mut HashSet<String>) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // Copy string / char literals verbatim.
        if b == b'"' || b == b'\'' {
            let quote = b;
            out.push(b as char);
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i] as char);
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    out.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                if bytes[i] == quote { i += 1; break; }
                i += 1;
            }
            continue;
        }
        // Copy comments verbatim.
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() { out.push(bytes[i] as char); i += 1; }
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            out.push_str("/*"); i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                out.push(bytes[i] as char); i += 1;
            }
            if i < bytes.len() { out.push_str("*/"); i += 2; }
            continue;
        }
        if is_ident_byte(b, true) {
            let start = i;
            while i < bytes.len() && is_ident_byte(bytes[i], false) { i += 1; }
            let ident = &text[start..i];
            if !hide.contains(ident)
                && let Some(m) = macros.get(ident)
            {
                match &m.params {
                    None => {
                        hide.insert(ident.to_string());
                        out.push_str(&expand(&m.body, macros, hide));
                        hide.remove(ident);
                        continue;
                    }
                    Some(params) => {
                        // Skip whitespace, then require `(` for a call.
                        let mut j = i;
                        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') { j += 1; }
                        if bytes.get(j) == Some(&b'(') {
                            if let Some((args, after)) = read_args(text, j) {
                                let expanded = substitute(&m.body, params, &args, macros);
                                hide.insert(ident.to_string());
                                out.push_str(&expand(&expanded, macros, hide));
                                hide.remove(ident);
                                i = after;
                                continue;
                            }
                        }
                        out.push_str(ident);
                        continue;
                    }
                }
            }
            out.push_str(ident);
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

/// Read `(arg, arg, ...)` starting at the `(` index. Returns the (raw) args and
/// the index just past the closing `)`. Honors nested parens.
fn read_args(text: &str, lparen: usize) -> Option<(Vec<String>, usize)> {
    let bytes = text.as_bytes();
    let mut i = lparen + 1;
    let mut depth = 1;
    let mut args: Vec<String> = Vec::new();
    let mut cur = String::new();
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'(' => { depth += 1; cur.push('('); }
            b')' => { depth -= 1; if depth == 0 { i += 1; break; } cur.push(')'); }
            b',' if depth == 1 => { args.push(cur.trim().to_string()); cur = String::new(); }
            _ => cur.push(b as char),
        }
        i += 1;
    }
    if depth != 0 { return None; }
    if !(cur.trim().is_empty() && args.is_empty()) { args.push(cur.trim().to_string()); }
    Some((args, i))
}

/// Substitute macro arguments into a function-like macro body, honoring `#`
/// (stringize) and `##` (token paste). Each substituted argument is itself
/// macro-expanded (except operands of `#`/`##`).
fn substitute(body: &str, params: &[String], args: &[String], macros: &HashMap<String, Macro>) -> String {
    let arg_of = |name: &str| -> Option<usize> { params.iter().position(|p| p == name) };
    let bytes = body.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // `#param` → stringized (unexpanded) argument.
        if b == b'#' && bytes.get(i + 1) != Some(&b'#') {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') { j += 1; }
            let s = j;
            while j < bytes.len() && is_ident_byte(bytes[j], j == s) { j += 1; }
            if let Some(idx) = arg_of(&body[s..j]) {
                out.push('"');
                out.push_str(args.get(idx).map(String::as_str).unwrap_or(""));
                out.push('"');
                i = j;
                continue;
            }
        }
        if is_ident_byte(b, true) {
            let start = i;
            while i < bytes.len() && is_ident_byte(bytes[i], false) { i += 1; }
            let tok = &body[start..i];
            // Token paste `tok ## next`: emit operands literally (param →
            // raw arg) with no surrounding space, no expansion.
            let mut k = i;
            while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') { k += 1; }
            let pasted = bytes.get(k) == Some(&b'#') && bytes.get(k + 1) == Some(&b'#');
            let raw = arg_of(tok).map(|idx| args[idx].clone());
            if pasted {
                out.push_str(raw.as_deref().unwrap_or(tok));
                // consume `##` and following whitespace; the next token is
                // appended directly on the following loop iterations.
                i = k + 2;
                while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') { i += 1; }
                continue;
            }
            match raw {
                Some(a) => out.push_str(&expand(&a, macros, &mut HashSet::new())),
                None => out.push_str(tok),
            }
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

// ---- `#if` constant-expression evaluation ------------------------------------

/// Evaluate a `#if` / `#elif` controlling expression to an integer. The
/// `defined` operator is resolved first, then macros expand, then a small
/// recursive-descent evaluator runs. Unknown identifiers evaluate to 0.
fn eval_const_expr(expr: &str, macros: &HashMap<String, Macro>) -> Result<i64, EmitError> {
    let resolved = resolve_defined(expr, macros);
    let expanded = expand(&resolved, macros, &mut HashSet::new());
    let mut ev = Eval { toks: lex_if(&expanded), pos: 0 };
    let v = ev.parse(0)?;
    Ok(v)
}

/// Replace `defined(X)` / `defined X` with `1` or `0` before macro expansion
/// (so the operand name isn't itself expanded).
fn resolve_defined(expr: &str, macros: &HashMap<String, Macro>) -> String {
    let mut out = String::new();
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if expr[i..].starts_with("defined")
            && !bytes.get(i + 7).is_some_and(|&b| is_ident_byte(b, false))
        {
            let mut j = i + 7;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') { j += 1; }
            let paren = bytes.get(j) == Some(&b'(');
            if paren { j += 1; while j < bytes.len() && bytes[j] == b' ' { j += 1; } }
            let s = j;
            while j < bytes.len() && is_ident_byte(bytes[j], j == s) { j += 1; }
            let name = &expr[s..j];
            if paren {
                while j < bytes.len() && bytes[j] != b')' { j += 1; }
                if j < bytes.len() { j += 1; }
            }
            out.push_str(if macros.contains_key(name) { "1" } else { "0" });
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[derive(Clone, PartialEq)]
enum IfTok { Num(i64), Op(String), LParen, RParen }

fn lex_if(s: &str) -> Vec<IfTok> {
    let bytes = s.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b' ' || b == b'\t' { i += 1; continue; }
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'x' || bytes[i] == b'X') { i += 1; }
            let lit = &s[start..i];
            let v = if let Some(h) = lit.strip_prefix("0x").or_else(|| lit.strip_prefix("0X")) {
                i64::from_str_radix(h, 16).unwrap_or(0)
            } else {
                lit.trim_end_matches(|c: char| c.is_ascii_alphabetic()).parse().unwrap_or(0)
            };
            toks.push(IfTok::Num(v));
            continue;
        }
        if is_ident_byte(b, true) {
            // An unexpanded identifier in `#if` is 0 (undefined macro).
            while i < bytes.len() && is_ident_byte(bytes[i], false) { i += 1; }
            toks.push(IfTok::Num(0));
            continue;
        }
        if b == b'(' { toks.push(IfTok::LParen); i += 1; continue; }
        if b == b')' { toks.push(IfTok::RParen); i += 1; continue; }
        // Two-char operators first.
        let two = &s[i..(i + 2).min(s.len())];
        if matches!(two, "&&" | "||" | "==" | "!=" | "<=" | ">=") {
            toks.push(IfTok::Op(two.to_string())); i += 2; continue;
        }
        toks.push(IfTok::Op((b as char).to_string()));
        i += 1;
    }
    toks
}

struct Eval { toks: Vec<IfTok>, pos: usize }

impl Eval {
    fn peek(&self) -> Option<&IfTok> { self.toks.get(self.pos) }

    /// Precedence-climbing parser. `min` is the minimum binding power.
    fn parse(&mut self, min: u8) -> Result<i64, EmitError> {
        let mut lhs = self.unary()?;
        while let Some(IfTok::Op(op)) = self.peek() {
            let op = op.clone();
            let bp = binding_power(&op);
            if bp == 0 || bp < min { break; }
            self.pos += 1;
            let rhs = self.parse(bp + 1)?;
            lhs = apply(&op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<i64, EmitError> {
        match self.peek().cloned() {
            Some(IfTok::Op(op)) if op == "!" || op == "-" || op == "+" => {
                self.pos += 1;
                let v = self.unary()?;
                Ok(match op.as_str() { "!" => (v == 0) as i64, "-" => -v, _ => v })
            }
            Some(IfTok::LParen) => {
                self.pos += 1;
                let v = self.parse(0)?;
                if matches!(self.peek(), Some(IfTok::RParen)) { self.pos += 1; }
                Ok(v)
            }
            Some(IfTok::Num(n)) => { self.pos += 1; Ok(n) }
            _ => Err(err("malformed #if expression")),
        }
    }
}

fn binding_power(op: &str) -> u8 {
    match op {
        "||" => 1, "&&" => 2,
        "==" | "!=" => 3,
        "<" | ">" | "<=" | ">=" => 4,
        "+" | "-" => 5,
        "*" | "/" | "%" => 6,
        _ => 0,
    }
}

fn apply(op: &str, l: i64, r: i64) -> i64 {
    match op {
        "||" => ((l != 0) || (r != 0)) as i64,
        "&&" => ((l != 0) && (r != 0)) as i64,
        "==" => (l == r) as i64,
        "!=" => (l != r) as i64,
        "<" => (l < r) as i64,
        ">" => (l > r) as i64,
        "<=" => (l <= r) as i64,
        ">=" => (l >= r) as i64,
        "+" => l + r,
        "-" => l - r,
        "*" => l * r,
        "/" => if r != 0 { l / r } else { 0 },
        "%" => if r != 0 { l % r } else { 0 },
        _ => 0,
    }
}
