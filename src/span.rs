//! Byte spans and line/column mapping for diagnostics.

/// A byte range into a source text: `start..end`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Span {
    /// Inclusive start offset.
    pub start: u32,
    /// Exclusive end offset.
    pub end: u32,
}

impl Span {
    /// A span from byte offsets.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the single span construction point; parse() rejects inputs \
                  over 4 GiB at the boundary, so offsets always fit u32"
    )]
    pub const fn new(start: usize, end: usize) -> Self {
        Self {
            start: start as u32,
            end: end as u32,
        }
    }

    /// The spanned slice of `src`.
    #[must_use]
    pub fn slice<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start as usize..self.end as usize]
    }

    /// The smallest span covering both `a` and `b`.
    #[must_use]
    pub fn join(a: Self, b: Self) -> Self {
        Self {
            start: a.start.min(b.start),
            end: a.end.max(b.end),
        }
    }
}

/// Maps byte offsets to 1-based (line, column) pairs.
pub(crate) struct LineIndex {
    starts: Vec<u32>,
}

impl LineIndex {
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "offsets fit u32 for the same reason spans do: parse() \
                  rejects inputs over 4 GiB at the boundary"
    )]
    pub(crate) fn new(src: &str) -> Self {
        let mut starts = vec![0u32];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i as u32 + 1);
            }
        }
        Self { starts }
    }

    /// 1-based (line, column) of a byte offset.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`starts` has one entry per source line and offsets fit u32, \
                  so the line index fits u32 a fortiori"
    )]
    pub(crate) fn line_col(&self, offset: u32) -> (u32, u32) {
        let line = match self.starts.binary_search(&offset) {
            Ok(l) => l,
            Err(l) => l - 1,
        };
        (line as u32 + 1, offset - self.starts[line] + 1)
    }

    /// Returns the source text of a 1-based line, without the trailing newline.
    #[must_use]
    pub(crate) fn line_text<'a>(&self, src: &'a str, line: u32) -> &'a str {
        let i = (line - 1) as usize;
        let start = self.starts[i] as usize;
        let end = self.starts.get(i + 1).map_or(src.len(), |&s| s as usize);
        src[start..end].trim_end_matches(['\n', '\r'])
    }
}
