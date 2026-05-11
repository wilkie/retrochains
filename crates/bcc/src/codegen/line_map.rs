//! Map byte offsets within a source string to 1-based line numbers and
//! back to the raw line contents. Used by the codegen to figure out
//! which source line a given AST span belongs to, so the right `;`
//! comment block can be emitted.

#[derive(Debug)]
pub struct LineMap {
    /// `starts[i]` = byte offset where line `i + 1` begins.
    starts: Vec<usize>,
    source_len: usize,
}

impl LineMap {
    pub fn new(source: &str) -> Self {
        let mut starts = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self { starts, source_len: source.len() }
    }

    /// 1-based line containing the byte at `offset`.
    pub fn line_of(&self, offset: u32) -> u32 {
        let off = offset as usize;
        let idx = self.starts.partition_point(|&s| s <= off);
        // partition_point returns the index of the first start *greater*
        // than off; that's also the 1-based line number containing it.
        u32::try_from(idx).unwrap_or(u32::MAX)
    }

    /// Content of the given 1-based line, with trailing `\r\n`/`\n`
    /// stripped. Returns an empty string for out-of-range lines.
    pub fn line_content<'src>(&self, source: &'src str, line: u32) -> &'src str {
        if line == 0 {
            return "";
        }
        let i = (line - 1) as usize;
        let Some(&start) = self.starts.get(i) else {
            return "";
        };
        let end = self.starts.get(i + 1).copied().unwrap_or(self.source_len);
        let raw = &source[start..end];
        raw.trim_end_matches(['\r', '\n'])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_single_line_source() {
        let src = "int main(void) { return 0; }\n";
        let lm = LineMap::new(src);
        assert_eq!(lm.line_of(0), 1);
        assert_eq!(lm.line_of(20), 1);
        assert_eq!(lm.line_content(src, 1), "int main(void) { return 0; }");
    }

    #[test]
    fn maps_multi_line_source() {
        let src = "int main(void) {\n  int x = 5;\n  return x;\n}\n";
        let lm = LineMap::new(src);
        assert_eq!(lm.line_of(0), 1);
        assert_eq!(lm.line_of(17), 2);
        assert_eq!(lm.line_of(30), 3);
        assert_eq!(lm.line_of(43), 4);
        assert_eq!(lm.line_content(src, 1), "int main(void) {");
        assert_eq!(lm.line_content(src, 2), "  int x = 5;");
        assert_eq!(lm.line_content(src, 3), "  return x;");
        assert_eq!(lm.line_content(src, 4), "}");
    }
}
