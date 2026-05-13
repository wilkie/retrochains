//! Line-oriented tokenizer for the BCC `-S` dialect. We aren't trying
//! to be a general TASM front-end; the only inputs we have to handle
//! are exactly what BCC writes, which is highly regular: TAB-indented,
//! one statement per line, comments are `;` to end-of-line.
//!
//! Output: one `Line` per non-empty, non-comment source line, with the
//! input split into a leading label (if any), a directive/mnemonic
//! keyword, and a list of remaining operand tokens.

use crate::ir::{AsmError, AsmResult};

/// One parsed source line.
#[derive(Debug)]
pub struct Line<'a> {
    pub line_no: usize,
    /// e.g. `_main` from `_main\tproc\tnear`, or `@1@50` from `@1@50:`.
    /// `None` for ordinary indented statements.
    pub label: Option<&'a str>,
    /// The keyword after any label: `proc`, `mov`, `endm`, `segment`,
    /// `?debug`, `public`, etc. Lowercased for comparison; the
    /// original casing is preserved if the caller cares.
    pub keyword: &'a str,
    /// Remaining text after the keyword, with leading whitespace
    /// trimmed. The parser decides how to split this — some
    /// directives take comma-separated arguments, some take a single
    /// string, etc.
    pub rest: &'a str,
}

/// Split the input into lines. Returns one `Line` per "interesting"
/// source line:
/// - comment lines (start with `;` after optional whitespace, or are
///   `   ;\t...` source-trace lines) are skipped
/// - blank lines are skipped
/// - the DOS `0x1A` EOF byte and any trailing CRLF are tolerated
pub fn tokenize(src: &str) -> AsmResult<Vec<Line<'_>>> {
    let mut out = Vec::new();
    for (idx, raw) in src.split('\n').enumerate() {
        let line_no = idx + 1;
        // Strip CR (CRLF) and any trailing 0x1A on the last line.
        let line = raw.trim_end_matches('\r').trim_end_matches('\x1A');
        if line.is_empty() {
            continue;
        }
        // BCC source-trace comments: `   ;\t<text>` (three spaces + `;`).
        // Treat any line whose first non-whitespace char is `;` as a
        // pure comment.
        let trimmed = line.trim_start();
        if trimmed.starts_with(';') || trimmed.is_empty() {
            continue;
        }
        let parsed = parse_line(line, line_no)?;
        out.push(parsed);
    }
    Ok(out)
}

/// Parse a single non-empty line into `(label, keyword, rest)`.
fn parse_line(line: &str, line_no: usize) -> AsmResult<Line<'_>> {
    // Two shapes:
    //   1) `<label>:<TAB-or-nothing><rest...>` — label on its own line or with
    //      trailing instruction. Example: `@1@50:`
    //   2) `[<lhs-label>]<TAB><keyword>[<TAB><rest>]` — TAB-indented
    //      statements. `<lhs-label>` is non-empty for procs and segment
    //      decls (e.g. `_main\tproc\tnear`); empty for ordinary
    //      instructions (`\tmov\tax,1`).
    //
    // Note: comments-only lines were filtered upstream.

    // Shape 1 first: any colon outside a string indicates a label.
    if let Some(colon_idx) = line.find(':') {
        // Make sure the colon comes before any operand (which BCC never
        // produces with a colon outside `assume cs:_TEXT` and addressing
        // expressions like `[bp-2]:word ptr`).
        // For BCC: a label line is just `@N@M:` possibly followed by
        // CRLF (already trimmed) — nothing else.
        let head = &line[..colon_idx];
        let tail = line[colon_idx + 1..].trim_start_matches('\t').trim_start();
        // `assume cs:_TEXT,ds:DGROUP` has a colon but isn't a label;
        // detect by checking head contains a TAB (then it's really
        // `<keyword>\t<operand-with-colon>`).
        if !head.contains('\t') && !head.contains(' ') && tail.is_empty() {
            return Ok(Line {
                line_no,
                label: Some(head),
                keyword: "",
                rest: "",
            });
        }
    }

    // Shape 2: TAB-delimited fields.
    let bytes = line.as_bytes();
    if !bytes.is_empty() && bytes[0] != b'\t' {
        // There's a leading identifier — a label-on-statement form.
        // Split at the first TAB.
        let tab_idx = line.find('\t').ok_or_else(|| {
            AsmError::new(line_no, format!("expected TAB after label: {line:?}"))
        })?;
        let label = &line[..tab_idx];
        let rest_after_label = &line[tab_idx + 1..];
        let (keyword, rest) = split_keyword(rest_after_label);
        return Ok(Line {
            line_no,
            label: Some(label),
            keyword,
            rest,
        });
    }

    // Pure TAB-indented statement.
    let stripped = line.trim_start_matches('\t');
    let (keyword, rest) = split_keyword(stripped);
    Ok(Line {
        line_no,
        label: None,
        keyword,
        rest,
    })
}

/// Split off the first word (the keyword) at the first TAB or space.
/// `segment` is followed by a single space in BCC's output
/// (`_TEXT\tsegment byte public 'CODE'`), while `mov` is followed by a
/// TAB (`\tmov\tax,1`); we accept both.
fn split_keyword(s: &str) -> (&str, &str) {
    if let Some(sep) = s.find(|c: char| c == '\t' || c == ' ') {
        (
            &s[..sep],
            s[sep..].trim_start_matches(|c: char| c == '\t' || c == ' '),
        )
    } else {
        (s, "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_comments_and_blanks() {
        let src = "\tmov\tax,1\r\n   ;\thello\r\n\r\n\tret\t\r\n";
        let lines = tokenize(src).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].keyword, "mov");
        assert_eq!(lines[1].keyword, "ret");
    }

    #[test]
    fn label_only_line() {
        let src = "@1@50:\r\n";
        let lines = tokenize(src).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].label, Some("@1@50"));
        assert_eq!(lines[0].keyword, "");
    }

    #[test]
    fn label_on_proc() {
        let src = "_main\tproc\tnear\r\n";
        let lines = tokenize(src).unwrap();
        assert_eq!(lines[0].label, Some("_main"));
        assert_eq!(lines[0].keyword, "proc");
        assert_eq!(lines[0].rest, "near");
    }

    #[test]
    fn segment_directive() {
        let src = "_TEXT\tsegment byte public 'CODE'\r\n";
        let lines = tokenize(src).unwrap();
        assert_eq!(lines[0].label, Some("_TEXT"));
        assert_eq!(lines[0].keyword, "segment");
        assert_eq!(lines[0].rest, "byte public 'CODE'");
    }

    #[test]
    fn tolerates_eof_byte() {
        let src = "\tret\r\n\x1A";
        let lines = tokenize(src).unwrap();
        assert_eq!(lines[0].keyword, "ret");
    }
}
