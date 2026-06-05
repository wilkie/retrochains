use crate::*;

/// One-pass scan for `typedef <type-tokens> <alias>;` declarations.
/// Records each alias as the full token sequence of its type spec
/// (already expanded through prior aliases) and splices that sequence
/// in at every later use site. This covers scalar aliases (`typedef
/// int X`), pointer aliases (`typedef int *PI` — the `*` is kept),
/// alias chains (`typedef X Y` resolves `X` first), and named-struct
/// aliases (`typedef struct Tag T`). Anonymous-struct, array, and
/// function-pointer typedefs contain `{`/`[`/`(` and are left for the
/// parser to handle (or remain unsupported); they are skipped here.
/// The typedef declaration tokens themselves are kept; `parse_typedef`
/// consumes them. Fixture 1000.
pub(crate) fn apply_typedef_substitutions(toks: &mut Vec<Tok>) {
    let mut aliases: std::collections::HashMap<String, Vec<Tok>> =
        std::collections::HashMap::new();
    let mut i = 0;
    while i < toks.len() {
        // Substitute a known alias used at a non-typedef site. Expansions
        // are already fully resolved, so the spliced tokens never contain
        // another unresolved alias — advance past them.
        if let Tok::Ident(name) = &toks[i] {
            if let Some(expansion) = aliases.get(name).cloned() {
                let n = expansion.len();
                toks.splice(i..=i, expansion);
                i += n;
                continue;
            }
        }
        if matches!(&toks[i], Tok::Kw("typedef")) {
            let start = i + 1;
            // Find the terminating `;` at brace depth 0 (a struct-body
            // typedef has inner field semicolons we must skip past).
            let mut depth = 0i32;
            let mut semi = start;
            while semi < toks.len() {
                match &toks[semi] {
                    Tok::LBrace => depth += 1,
                    Tok::RBrace => depth -= 1,
                    Tok::Semi if depth == 0 => break,
                    _ => {}
                }
                semi += 1;
            }
            // `typedef struct|union [Tag] { ... } Alias;` — rewrite to a plain
            // `struct <name> { ... };` so the existing struct-definition path
            // registers it, and record `Alias -> struct <name>` so later uses
            // (`Alias v;`) splice to `struct <name> v;`. An anonymous struct
            // borrows the alias itself as the tag. Fixtures 489, 2334, 2545.
            let is_struct_kw =
                matches!(&toks[start], Tok::Kw("struct") | Tok::Kw("union"));
            let brace_open = toks[start..semi]
                .iter()
                .position(|t| matches!(t, Tok::LBrace))
                .map(|p| start + p);
            if is_struct_kw {
                if let Some(bopen) = brace_open {
                    // Matching `}` at depth 0.
                    let mut d = 0i32;
                    let mut bclose = bopen;
                    while bclose < semi {
                        match &toks[bclose] {
                            Tok::LBrace => d += 1,
                            Tok::RBrace => {
                                d -= 1;
                                if d == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        bclose += 1;
                    }
                    // Alias name = last Ident between `}` and `;`.
                    let alias = toks[bclose + 1..semi].iter().rev().find_map(|t| {
                        if let Tok::Ident(s) = t { Some(s.clone()) } else { None }
                    });
                    // Tag = Ident between the struct/union kw and `{`, if any.
                    let tag = toks[start + 1..bopen].iter().find_map(|t| {
                        if let Tok::Ident(s) = t { Some(s.clone()) } else { None }
                    });
                    if let Some(alias) = alias {
                        let struct_kw = toks[start].clone();
                        let tag_name = tag.clone().unwrap_or_else(|| alias.clone());
                        aliases.insert(
                            alias.clone(),
                            vec![struct_kw.clone(), Tok::Ident(tag_name.clone())],
                        );
                        // Build `struct <tag> { body } ;`.
                        let mut rewrite = vec![struct_kw, Tok::Ident(tag_name)];
                        rewrite.extend(toks[bopen..=bclose].iter().cloned());
                        rewrite.push(Tok::Semi);
                        let n = rewrite.len();
                        toks.splice(i..=semi, rewrite);
                        i += n;
                        continue;
                    }
                }
            }
            // Function-pointer (`(`) typedefs are left for the parser. An array
            // typedef (`[`) is handled lossily: the dimension is dropped and
            // only the element type is recorded, so `IARR a = {...}` becomes
            // `int a = {...}` and the initializer sizes the array (matches the
            // prior behavior; fixture 2099).
            let complex = toks[start..semi]
                .iter()
                .any(|t| matches!(t, Tok::LBrace | Tok::LParen));
            if !complex {
                // Type tokens stop at the first `[` (the array suffix, if any).
                let cut = toks[start..semi]
                    .iter()
                    .position(|t| matches!(t, Tok::LBrack))
                    .unwrap_or(semi - start);
                let body = &toks[start..start + cut];
                if let Some(rel_name) =
                    body.iter().rposition(|t| matches!(t, Tok::Ident(_)))
                {
                    let name = match &body[rel_name] {
                        Tok::Ident(s) => s.clone(),
                        _ => unreachable!(),
                    };
                    // Expansion = the type tokens (everything but the alias
                    // name), resolving any nested alias to its expansion.
                    let mut expansion: Vec<Tok> = Vec::new();
                    for (k, t) in body.iter().enumerate() {
                        if k == rel_name {
                            continue;
                        }
                        match t {
                            Tok::Ident(s) => match aliases.get(s) {
                                Some(e) => expansion.extend(e.iter().cloned()),
                                None => expansion.push(t.clone()),
                            },
                            _ => expansion.push(t.clone()),
                        }
                    }
                    let has_base = expansion.iter().any(|t| {
                        matches!(
                            t,
                            Tok::Kw("int")
                                | Tok::Kw("char")
                                | Tok::Kw("long")
                                | Tok::Kw("unsigned")
                                | Tok::Kw("signed")
                                | Tok::Kw("short")
                                | Tok::Kw("struct")
                                | Tok::Kw("union")
                                | Tok::Kw("void")
                        )
                    });
                    if has_base {
                        aliases.insert(name, expansion);
                    }
                }
            }
            i = semi + 1;
            continue;
        }
        i += 1;
    }
}
/// One-pass scan that resolves `enum` declarations at the token level,
/// so the parser never needs to model enums as a type. An `enum [tag]
/// { A, B = K, ... }` definition registers each constant's integer
/// value (auto-incrementing, honoring explicit `= K`), and every later
/// use of the constant is replaced with its `Int` literal. A pure
/// definition (`enum ... ;`) is removed; a definition used as a type
/// (`enum {..} x;`) and a brace-less `enum tag` collapse to `int`.
/// Fixtures 470, 472, 473, 474, 2353, 3195, 3631, 2684.
pub(crate) fn apply_enum_substitutions(toks: &mut Vec<Tok>) {
    let mut vals: std::collections::HashMap<String, i32> =
        std::collections::HashMap::new();
    let mut i = 0;
    while i < toks.len() {
        if let Tok::Ident(name) = &toks[i] {
            if let Some(&v) = vals.get(name) {
                toks[i] = Tok::Int(v);
                i += 1;
                continue;
            }
        }
        if matches!(&toks[i], Tok::Kw("enum")) {
            // Past an optional tag.
            let mut j = i + 1;
            if matches!(toks.get(j), Some(Tok::Ident(_))) {
                j += 1;
            }
            if matches!(toks.get(j), Some(Tok::LBrace)) {
                // Find the matching `}`.
                let body_start = j + 1;
                let mut depth = 1i32;
                let mut bclose = body_start;
                while bclose < toks.len() {
                    match &toks[bclose] {
                        Tok::LBrace => depth += 1,
                        Tok::RBrace => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    bclose += 1;
                }
                // Register constants `NAME [= [-]K]`, auto-incrementing.
                let mut next_val = 0i32;
                let mut m = body_start;
                while m < bclose {
                    if let Tok::Ident(name) = toks[m].clone() {
                        m += 1;
                        if matches!(toks.get(m), Some(Tok::Assign)) {
                            m += 1;
                            let neg = matches!(toks.get(m), Some(Tok::Minus));
                            if neg {
                                m += 1;
                            }
                            if let Some(Tok::Int(v)) = toks.get(m).cloned() {
                                next_val = if neg { -v } else { v };
                                m += 1;
                            }
                        }
                        vals.insert(name, next_val);
                        next_val += 1;
                    } else {
                        m += 1;
                    }
                }
                // A `;` right after `}` is a pure definition (drop it); any
                // declarator means the enum was a type spec (→ `int`).
                if matches!(toks.get(bclose + 1), Some(Tok::Semi)) {
                    toks.drain(i..=bclose + 1);
                } else {
                    toks.splice(i..=bclose, [Tok::Kw("int")]);
                    i += 1;
                }
                continue;
            } else {
                // Brace-less `enum tag` used as a type → `int`.
                toks.splice(i..j, [Tok::Kw("int")]);
                i += 1;
                continue;
            }
        }
        i += 1;
    }
}
pub(crate) fn tokenize(source: &str) -> Result<Vec<Tok>, EmitError> {
    let bytes = source.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'(' => { toks.push(Tok::LParen); i += 1; }
            b')' => { toks.push(Tok::RParen); i += 1; }
            b'{' => { toks.push(Tok::LBrace); i += 1; }
            b'}' => { toks.push(Tok::RBrace); i += 1; }
            b'[' => { toks.push(Tok::LBrack); i += 1; }
            b']' => { toks.push(Tok::RBrack); i += 1; }
            b'&' => {
                if bytes.get(i + 1) == Some(&b'&') {
                    toks.push(Tok::AndAnd); i += 2;
                } else if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::AndEq); i += 2;
                } else {
                    toks.push(Tok::Amp); i += 1;
                }
            }
            b';' => { toks.push(Tok::Semi); i += 1; }
            b',' => { toks.push(Tok::Comma); i += 1; }
            b'+' => {
                if bytes.get(i + 1) == Some(&b'+') {
                    toks.push(Tok::PlusPlus); i += 2;
                } else if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::PlusEq); i += 2;
                } else {
                    toks.push(Tok::Plus); i += 1;
                }
            }
            b'*' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::StarEq); i += 2;
                } else {
                    toks.push(Tok::Star); i += 1;
                }
            }
            b'<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::Le); i += 2;
                } else if bytes.get(i + 1) == Some(&b'<') {
                    if bytes.get(i + 2) == Some(&b'=') {
                        toks.push(Tok::ShlEq); i += 3;
                    } else {
                        toks.push(Tok::Shl); i += 2;
                    }
                } else {
                    toks.push(Tok::Lt); i += 1;
                }
            }
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::Ge); i += 2;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    if bytes.get(i + 2) == Some(&b'=') {
                        toks.push(Tok::ShrEq); i += 3;
                    } else {
                        toks.push(Tok::Shr); i += 2;
                    }
                } else {
                    toks.push(Tok::Gt); i += 1;
                }
            }
            b'/' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::SlashEq); i += 2;
                } else if bytes.get(i + 1) == Some(&b'/') {
                    while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
                } else if bytes.get(i + 1) == Some(&b'*') {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    if i + 1 >= bytes.len() {
                        return Err(EmitError::Unsupported(
                            "unterminated `/* ... */` comment".to_owned(),
                        ));
                    }
                    i += 2;
                } else {
                    toks.push(Tok::Slash); i += 1;
                }
            }
            b'%' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::PercentEq); i += 2;
                } else {
                    toks.push(Tok::Percent); i += 1;
                }
            }
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    toks.push(Tok::OrOr); i += 2;
                } else if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::PipeEq); i += 2;
                } else {
                    toks.push(Tok::Pipe); i += 1;
                }
            }
            b'^' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::CaretEq); i += 2;
                } else {
                    toks.push(Tok::Caret); i += 1;
                }
            }
            b'~' => { toks.push(Tok::Tilde); i += 1; }
            b'?' => { toks.push(Tok::Quest); i += 1; }
            b':' => { toks.push(Tok::Colon); i += 1; }
            b'.' => { toks.push(Tok::Dot); i += 1; }
            b'=' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::EqEq);
                    i += 2;
                } else {
                    toks.push(Tok::Assign);
                    i += 1;
                }
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::NotEq);
                    i += 2;
                } else {
                    toks.push(Tok::Bang);
                    i += 1;
                }
            }
            b'-' => {
                if bytes.get(i + 1) == Some(&b'-') {
                    toks.push(Tok::MinusMinus); i += 2;
                } else if bytes.get(i + 1) == Some(&b'=') {
                    toks.push(Tok::MinusEq); i += 2;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    toks.push(Tok::Arrow); i += 2;
                } else {
                    toks.push(Tok::Minus); i += 1;
                }
            }
            b'#' => {
                // Treat the entire line as a preprocessor directive
                // (consume up to but not including the newline).
                // Phase 1 doesn't actually process any directive;
                // <stdio.h> definitions for printf are implicit.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                toks.push(Tok::PreprocLine);
            }
            b'\'' => {
                // Char literal — single byte (with escape support)
                // bracketed by `'`. Becomes a `Tok::Int(byte)`; the
                // C semantics widen char to int for free.
                i += 1;
                if i >= bytes.len() {
                    return Err(EmitError::Unsupported("unterminated char literal".to_owned()));
                }
                let value: i32 = if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    let esc = bytes[i + 1];
                    // Octal escape `\NNN` (1–3 digits). C says `\0` is
                    // NUL even when followed by another digit unless
                    // that digit is also octal — we match the latter.
                    if esc.is_ascii_digit() && esc <= b'7' {
                        let mut v: i32 = 0;
                        i += 1;
                        let mut digits = 0;
                        while digits < 3 && i < bytes.len()
                            && bytes[i].is_ascii_digit() && bytes[i] <= b'7'
                        {
                            v = v * 8 + (bytes[i] - b'0') as i32;
                            i += 1;
                            digits += 1;
                        }
                        v
                    } else {
                        let v = match esc {
                            b'n' => 0x0A,
                            b't' => 0x09,
                            b'r' => 0x0D,
                            b'\\' => b'\\' as i32,
                            b'\'' => b'\'' as i32,
                            b'"' => b'"' as i32,
                            b'a' => 0x07,
                            b'b' => 0x08,
                            b'f' => 0x0C,
                            b'v' => 0x0B,
                            _ => {
                                return Err(EmitError::Unsupported(format!(
                                    "unknown escape `\\{}` in char literal",
                                    esc as char
                                )));
                            }
                        };
                        i += 2;
                        v as i32
                    }
                } else {
                    let v = bytes[i] as i32;
                    i += 1;
                    v
                };
                if i >= bytes.len() || bytes[i] != b'\'' {
                    return Err(EmitError::Unsupported("unterminated char literal".to_owned()));
                }
                i += 1;
                toks.push(Tok::Int(value));
            }
            b'"' => {
                // String literal — collect bytes until the closing
                // quote. Handles common C escapes (`\n`, `\t`, `\\`,
                // `\"`, `\0`).
                i += 1;
                let mut buf = Vec::new();
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        let esc = bytes[i + 1];
                        let translated = match esc {
                            b'n' => 0x0A,
                            b't' => 0x09,
                            b'r' => 0x0D,
                            b'0' => 0x00,
                            b'\\' => b'\\',
                            b'"' => b'"',
                            _ => {
                                return Err(EmitError::Unsupported(format!(
                                    "unknown escape `\\{}`",
                                    esc as char
                                )));
                            }
                        };
                        buf.push(translated);
                        i += 2;
                    } else {
                        buf.push(bytes[i]);
                        i += 1;
                    }
                }
                if i >= bytes.len() {
                    return Err(EmitError::Unsupported("unterminated string literal".to_owned()));
                }
                i += 1; // consume closing "
                toks.push(Tok::StrLit(buf));
            }
            b'0'..=b'9' => {
                // Floating-point literal: decimal digits followed by a `.`,
                // an exponent, or an `f` suffix (hex floats unsupported).
                if !(bytes.get(i) == Some(&b'0')
                    && matches!(bytes.get(i + 1), Some(&b'x') | Some(&b'X')))
                {
                    let mut j = i;
                    while j < bytes.len() && bytes[j].is_ascii_digit() { j += 1; }
                    if matches!(bytes.get(j), Some(b'.') | Some(b'e') | Some(b'E') | Some(b'f') | Some(b'F')) {
                        let start = i;
                        while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
                        if bytes.get(i) == Some(&b'.') {
                            i += 1;
                            while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
                        }
                        if matches!(bytes.get(i), Some(b'e') | Some(b'E')) {
                            i += 1;
                            if matches!(bytes.get(i), Some(b'+') | Some(b'-')) { i += 1; }
                            while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
                        }
                        let text = std::str::from_utf8(&bytes[start..i])
                            .map_err(|_| EmitError::Unsupported("non-ASCII in float".to_owned()))?;
                        let val: f64 = text.parse()
                            .map_err(|_| EmitError::Unsupported(format!("bad float `{text}`")))?;
                        let mut is_double = true;
                        match bytes.get(i) {
                            Some(b'f') | Some(b'F') => { is_double = false; i += 1; }
                            Some(b'l') | Some(b'L') => { i += 1; }
                            _ => {}
                        }
                        toks.push(Tok::Float(val.to_bits(), is_double));
                        continue;
                    }
                }
                // Hex (`0x` / `0X`), octal (`0` followed by digits),
                // and decimal forms. Trailing L/U/UL suffixes ignored.
                let n: i32 = if bytes.get(i) == Some(&b'0')
                    && matches!(bytes.get(i + 1), Some(&b'x') | Some(&b'X'))
                {
                    i += 2;
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                        i += 1;
                    }
                    let text = std::str::from_utf8(&bytes[start..i])
                        .map_err(|_| EmitError::Unsupported("non-ASCII in hex int".to_owned()))?;
                    i32::from_str_radix(text, 16)
                        .or_else(|_| u32::from_str_radix(text, 16).map(|u| u as i32))
                        .map_err(|_| EmitError::Unsupported(format!("bad hex `0x{text}`")))?
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    let text = std::str::from_utf8(&bytes[start..i])
                        .map_err(|_| EmitError::Unsupported("non-ASCII in integer".to_owned()))?;
                    if let Some(rest) = text.strip_prefix('0').filter(|s| !s.is_empty() && s.bytes().all(|b| (b'0'..=b'7').contains(&b))) {
                        i32::from_str_radix(rest, 8)
                            .map_err(|_| EmitError::Unsupported(format!("bad octal `0{rest}`")))?
                    } else {
                        text.parse()
                            .map_err(|_| EmitError::Unsupported(format!("bad integer `{text}`")))?
                    }
                };
                // Skip trailing L/U/l/u suffix bytes; we promote
                // everything to int in Phase 1.
                while matches!(bytes.get(i), Some(b'L') | Some(b'l') | Some(b'U') | Some(b'u')) {
                    i += 1;
                }
                toks.push(Tok::Int(n));
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                let text = std::str::from_utf8(&bytes[start..i])
                    .map_err(|_| EmitError::Unsupported("non-ASCII in identifier".to_owned()))?;
                let tok = match text {
                    "int" => Tok::Kw("int"),
                    "char" => Tok::Kw("char"),
                    "short" => Tok::Kw("short"),
                    "main" => Tok::Kw("main"),
                    "void" => Tok::Kw("void"),
                    "return" => Tok::Kw("return"),
                    "if" => Tok::Kw("if"),
                    "else" => Tok::Kw("else"),
                    "while" => Tok::Kw("while"),
                    "do" => Tok::Kw("do"),
                    "for" => Tok::Kw("for"),
                    "switch" => Tok::Kw("switch"),
                    "case" => Tok::Kw("case"),
                    "default" => Tok::Kw("default"),
                    "break" => Tok::Kw("break"),
                    "continue" => Tok::Kw("continue"),
                    "struct" => Tok::Kw("struct"),
                    "union" => Tok::Kw("union"),
                    "sizeof" => Tok::Kw("sizeof"),
                    "long" => Tok::Kw("long"),
                    "float" => Tok::Kw("float"),
                    "double" => Tok::Kw("double"),
                    "enum" => Tok::Kw("enum"),
                    "typedef" => Tok::Kw("typedef"),
                    "cdecl" => Tok::Kw("cdecl"),
                    "pascal" => Tok::Kw("pascal"),
                    "far" => Tok::Kw("far"),
                    "near" => Tok::Kw("near"),
                    "huge" => Tok::Kw("huge"),
                    "interrupt" => Tok::Kw("interrupt"),
                    // Storage-class / qualifier modifiers we currently
                    // treat as no-ops in declarator parsing.
                    "unsigned" => Tok::Kw("unsigned"),
                    "signed" => Tok::Kw("signed"),
                    "static" => Tok::Kw("static"),
                    "extern" => Tok::Kw("extern"),
                    "register" => Tok::Kw("register"),
                    "auto" => Tok::Kw("auto"),
                    "volatile" => Tok::Kw("volatile"),
                    "const" => Tok::Kw("const"),
                    _ => Tok::Ident(text.to_owned()),
                };
                toks.push(tok);
            }
            _ => {
                return Err(EmitError::Unsupported(format!(
                    "unexpected character `{}` in source",
                    c as char
                )));
            }
        }
    }
    // Adjacent string-literal concatenation (`"ab" "cd"` → `"abcd"`): C joins
    // consecutive string literal tokens into one before parsing.
    let mut merged: Vec<Tok> = Vec::with_capacity(toks.len());
    for t in toks {
        if let Tok::StrLit(cur) = &t
            && let Some(Tok::StrLit(prev)) = merged.last_mut()
        {
            prev.extend_from_slice(cur);
        } else {
            merged.push(t);
        }
    }
    Ok(merged)
}
