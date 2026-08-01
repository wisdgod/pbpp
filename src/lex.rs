//! Lossless lexer.
//!
//! Produces the full token stream and, separately, the full comment stream.
//! Every token and comment carries its byte span, line number(s), and whether
//! a blank line precedes it — everything the parser needs to attach trivia
//! and everything the pruner needs to delete by span.
//!
//! Strictness: anything outside the proto3 lexical grammar is an error here,
//! not something to skip over.

// The SWAR helpers and byte-class tests are single-expression leaf functions
// on the hottest loops; `inline(always)` is load-bearing (measured via the
// throughput bench), not decoration.
#![expect(
    clippy::inline_always,
    reason = "leaf helpers of the SWAR hot loops; measured throughput \
              regresses when the compiler outlines them"
)]

use crate::error::Error;
use crate::span::Span;

/// The kind of a lexed token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum TokKind {
    /// An identifier (including keywords).
    Ident,
    /// An integer literal.
    Int,
    /// A floating-point literal.
    Float,
    /// A string literal.
    Str,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBrack,
    /// `]`
    RBrack,
    /// `<`
    LAngle,
    /// `>`
    RAngle,
    /// `=`
    Eq,
    /// `;`
    Semi,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `.`
    Dot,
    /// `-`
    Minus,
    /// `+`
    Plus,
    /// `/`
    Slash,
    /// End of input.
    Eof,
}

/// Keyword classification of an `Ident` token, established once in the
/// lexer while the identifier's bytes are still hot in cache. The parser
/// dispatches on this byte instead of re-comparing token text at every
/// grammar position (a plain field's first token would otherwise go
/// through up to three keyword-match passes).
///
/// `None` marks non-keyword identifiers and non-`Ident` tokens. Contextual
/// keywords stay usable as plain identifiers: the parser only consults
/// `kw` at dispatch positions, exactly where it used to compare text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kw {
    None,
    // Statement / definition keywords.
    Syntax,
    Package,
    Import,
    Option,
    Message,
    Enum,
    Service,
    Oneof,
    Reserved,
    Rpc,
    Returns,
    Stream,
    To,
    Max,
    Public,
    Weak,
    Repeated,
    Optional,
    Map,
    // proto2 constructs, classified for targeted diagnostics.
    Required,
    Extend,
    Extensions,
    Group,
    // Signed-constant identifiers.
    Inf,
    Nan,
    // Scalar type names.
    Double,
    Float,
    Int32,
    Int64,
    Uint32,
    Uint64,
    Sint32,
    Sint64,
    Fixed32,
    Fixed64,
    Sfixed32,
    Sfixed64,
    Bool,
    String,
    Bytes,
}

impl Kw {
    /// True for the fifteen scalar field type names.
    #[must_use]
    pub(crate) const fn is_scalar_type(self) -> bool {
        matches!(
            self,
            Self::Double
                | Self::Float
                | Self::Int32
                | Self::Int64
                | Self::Uint32
                | Self::Uint64
                | Self::Sint32
                | Self::Sint64
                | Self::Fixed32
                | Self::Fixed64
                | Self::Sfixed32
                | Self::Sfixed64
                | Self::Bool
                | Self::String
                | Self::Bytes
        )
    }

    /// True for valid map key types: integer types, `bool`, and `string`
    /// (scalars minus `double`/`float`/`bytes`).
    #[must_use]
    pub(crate) const fn is_map_key_type(self) -> bool {
        self.is_scalar_type() && !matches!(self, Self::Double | Self::Float | Self::Bytes)
    }
}

/// Classifies an identifier's bytes; compiles to a length switch plus
/// memcmp tree, run once per identifier.
const fn classify_ident(b: &[u8]) -> Kw {
    match b {
        b"to" => Kw::To,
        b"map" => Kw::Map,
        b"max" => Kw::Max,
        b"inf" => Kw::Inf,
        b"nan" => Kw::Nan,
        b"rpc" => Kw::Rpc,
        b"bool" => Kw::Bool,
        b"enum" => Kw::Enum,
        b"weak" => Kw::Weak,
        b"bytes" => Kw::Bytes,
        b"float" => Kw::Float,
        b"group" => Kw::Group,
        b"int32" => Kw::Int32,
        b"int64" => Kw::Int64,
        b"oneof" => Kw::Oneof,
        b"double" => Kw::Double,
        b"extend" => Kw::Extend,
        b"import" => Kw::Import,
        b"option" => Kw::Option,
        b"public" => Kw::Public,
        b"sint32" => Kw::Sint32,
        b"sint64" => Kw::Sint64,
        b"stream" => Kw::Stream,
        b"string" => Kw::String,
        b"syntax" => Kw::Syntax,
        b"uint32" => Kw::Uint32,
        b"uint64" => Kw::Uint64,
        b"fixed32" => Kw::Fixed32,
        b"fixed64" => Kw::Fixed64,
        b"message" => Kw::Message,
        b"package" => Kw::Package,
        b"returns" => Kw::Returns,
        b"service" => Kw::Service,
        b"optional" => Kw::Optional,
        b"repeated" => Kw::Repeated,
        b"required" => Kw::Required,
        b"reserved" => Kw::Reserved,
        b"sfixed32" => Kw::Sfixed32,
        b"sfixed64" => Kw::Sfixed64,
        b"extensions" => Kw::Extensions,
        _ => Kw::None,
    }
}

/// A token: 16 bytes, no text.
///
/// The text slice is derivable from `span` and the source, and tokens are
/// the densest allocation in the pipeline (`-Z print-type-sizes`:
/// 32 → 16 bytes; `kw` sits in former padding).
#[derive(Clone, Copy, Debug)]
pub struct Tok {
    /// Byte span of the token in the source.
    pub span: Span,
    /// 1-based line the token starts on (tokens never span lines).
    pub line: u32,
    /// The token kind.
    pub kind: TokKind,
    /// True if at least one blank line separates this token from the
    /// previous token or comment.
    pub blank_before: bool,
    /// Keyword classification for `Ident` tokens; `Kw::None` otherwise.
    pub(crate) kw: Kw,
}

/// The kind of a comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommentKind {
    /// `// ...` line comment.
    Line,
    /// `/* ... */` block comment.
    Block,
}

/// A source comment.
#[derive(Clone, Copy, Debug)]
pub struct Comment<'a> {
    /// Whether this is a line or a block comment.
    pub kind: CommentKind,
    /// Verbatim text including the `//` or `/* */` markers.
    pub text: &'a str,
    /// Byte span of the comment in the source.
    pub span: Span,
    /// 1-based line the comment starts on.
    pub line_start: u32,
    /// 1-based line the comment ends on.
    pub line_end: u32,
    /// True if at least one blank line separates this comment from the
    /// previous token or comment.
    pub blank_before: bool,
}

/// The output of [`lex`]: the token stream and the comment stream.
pub struct Lexed<'a> {
    /// All tokens in source order, ending with an `Eof` token.
    pub toks: Vec<Tok>,
    /// All comments in source order.
    pub comments: Vec<Comment<'a>>,
}

/// Tokenizes proto3 source text into tokens and comments, both carrying
/// spans and line information.
///
/// # Errors
///
/// Anything outside the proto3 lexical grammar: unexpected characters,
/// malformed numeric literals, invalid escape sequences, unterminated
/// strings or block comments, newlines inside string literals.
pub fn lex(src: &str) -> Result<Lexed<'_>, Error> {
    Lexer {
        src,
        b: src.as_bytes(),
        i: 0,
        line: 1,
        prev_end_line: 0,
        // Pre-sized from typical proto token density, to avoid regrowth
        // copies on large inputs.
        toks: Vec::with_capacity(src.len() / 5 + 8),
        comments: Vec::with_capacity(src.len() / 128 + 4),
    }
    .run()
}

struct Lexer<'a> {
    src: &'a str,
    b: &'a [u8],
    i: usize,
    line: u32,
    /// Line on which the previous token or comment ended; 0 = nothing yet.
    prev_end_line: u32,
    toks: Vec<Tok>,
    comments: Vec<Comment<'a>>,
}

// Byte-class table: one load+test per byte instead of chained range
// compares in the lexer's hottest loops.
const CLS_IDENT_START: u8 = 1;
const CLS_IDENT_CONT: u8 = 2;
const CLS_WS: u8 = 4; // space, \t, \r, \x0b, \x0c — not \n (tracks lines)

#[expect(
    clippy::cast_possible_truncation,
    reason = "the loop bound guarantees b < 256"
)]
static CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut b = 0usize;
    while b < 256 {
        let c = b as u8;
        if c.is_ascii_alphabetic() || c == b'_' {
            t[b] = CLS_IDENT_START | CLS_IDENT_CONT;
        } else if c.is_ascii_digit() {
            t[b] = CLS_IDENT_CONT;
        } else if matches!(c, b' ' | b'\t' | b'\r' | 0x0b | 0x0c) {
            t[b] = CLS_WS;
        }
        b += 1;
    }
    t
};

#[inline(always)]
fn is_ident_start(b: u8) -> bool {
    CLASS[b as usize] & CLS_IDENT_START != 0
}

#[inline(always)]
fn is_ident_continue(b: u8) -> bool {
    CLASS[b as usize] & CLS_IDENT_CONT != 0
}

#[inline(always)]
fn is_ws(b: u8) -> bool {
    CLASS[b as usize] & CLS_WS != 0
}

// ---- SWAR scanning -----------------------------------------------------------
//
// The lexer's hot loops (identifier runs, whitespace runs, comment and
// string bodies) process 8 bytes per step using exact per-byte bit tricks
// (the glibc-strlen family). Scalar tails handle the last <8 bytes.

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = 0x8080_8080_8080_8080;
const LOW7: u64 = 0x7f7f_7f7f_7f7f_7f7f;

// NOTE: an earlier revision kept these helpers non-`const`, citing a
// measured 34% lexing-throughput regression from the `const` marker. On
// the current toolchain (rustc 1.99.0-nightly 2026-07-31) the claim does
// not reproduce: both variants compile to byte-identical assembly
// (`--emit asm` diff) and benchmark identically (lexwkt, 1655 MB/s each),
// so they are `const` — the strictly more capable form at zero cost.

/// Per-byte mask (high bit set) of bytes equal to `n`. Exact for all bytes.
#[inline(always)]
const fn swar_eq(x: u64, n: u8) -> u64 {
    let v = x ^ (ONES * n as u64);
    HIGH & !(v | ((v | HIGH) - ONES))
}

/// Per-byte mask of 7-bit values `>= n` (callers pass `x & LOW7`).
/// Per-byte addition cannot carry: both summands are below 0x80/0x81.
#[inline(always)]
const fn swar_ge7(x7: u64, n: u8) -> u64 {
    (x7 + ONES * (0x80 - n as u64)) & HIGH
}

#[inline(always)]
fn load8(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// Index of the first byte flagged in `mask` (little-endian = string order).
#[inline(always)]
const fn first_flag(mask: u64) -> usize {
    (mask.trailing_zeros() as usize) >> 3
}

/// Advances past `[A-Za-z0-9_]*` starting at `i`.
fn scan_ident(b: &[u8], mut i: usize) -> usize {
    while i + 8 <= b.len() {
        let x = load8(b, i);
        let x7 = x & LOW7;
        let digit = swar_ge7(x7, b'0') & !swar_ge7(x7, b'9' + 1);
        let upper = swar_ge7(x7, b'A') & !swar_ge7(x7, b'Z' + 1);
        let lower = swar_ge7(x7, b'a') & !swar_ge7(x7, b'z' + 1);
        let underscore = swar_eq(x, b'_');
        // Bytes with the high bit set (UTF-8 tails) are never identifiers.
        let ident = (digit | upper | lower | underscore) & !(x & HIGH);
        let non = !ident & HIGH;
        if non == 0 {
            i += 8;
        } else {
            return i + first_flag(non);
        }
    }
    while i < b.len() && is_ident_continue(b[i]) {
        i += 1;
    }
    i
}

/// Advances past whitespace (including newlines) starting at `i`, adding
/// the newlines encountered to `line`.
fn scan_ws(b: &[u8], mut i: usize, line: &mut u32) -> usize {
    while i + 8 <= b.len() {
        let x = load8(b, i);
        let x7 = x & LOW7;
        // \t..\r (0x09..=0x0d) plus space; high-bit bytes excluded.
        let ctl = swar_ge7(x7, 0x09) & !swar_ge7(x7, 0x0e);
        let ws = (ctl | swar_eq(x, b' ')) & !(x & HIGH);
        let non = !ws & HIGH;
        let nl = swar_eq(x, b'\n');
        if non == 0 {
            *line += nl.count_ones();
            i += 8;
        } else {
            let k = first_flag(non); // 0..=7, so the shift below is in range
            let before_stop = (1u64 << (8 * k)) - 1;
            *line += (nl & before_stop).count_ones();
            return i + k;
        }
    }
    while i < b.len() {
        match b[i] {
            b'\n' => {
                *line += 1;
                i += 1;
            }
            w if is_ws(w) => i += 1,
            _ => break,
        }
    }
    i
}

/// Advances to the next `\n` (or end of input) starting at `i`.
fn scan_to_newline(b: &[u8], mut i: usize) -> usize {
    while i + 8 <= b.len() {
        let nl = swar_eq(load8(b, i), b'\n');
        if nl == 0 {
            i += 8;
        } else {
            return i + first_flag(nl);
        }
    }
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    i
}

/// Advances to the next string-terminating byte (the quote, a backslash,
/// or a newline) starting at `i`.
fn scan_str_body(b: &[u8], quote: u8, mut i: usize) -> usize {
    while i + 8 <= b.len() {
        let x = load8(b, i);
        let stop = swar_eq(x, quote) | swar_eq(x, b'\\') | swar_eq(x, b'\n');
        if stop == 0 {
            i += 8;
        } else {
            return i + first_flag(stop);
        }
    }
    while i < b.len() && b[i] != quote && b[i] != b'\\' && b[i] != b'\n' {
        i += 1;
    }
    i
}

impl<'a> Lexer<'a> {
    fn err(&self, msg: impl Into<String>, start: usize, end: usize) -> Error {
        Error::at(msg, Span::new(start, end.min(self.b.len())), self.src)
    }

    const fn blank_before(&self, start_line: u32) -> bool {
        self.prev_end_line != 0 && start_line > self.prev_end_line + 1
    }

    fn push_tok(&mut self, kind: TokKind, start: usize, start_line: u32) {
        self.push_tok_kw(kind, start, start_line, Kw::None);
    }

    fn push_tok_kw(&mut self, kind: TokKind, start: usize, start_line: u32, kw: Kw) {
        self.toks.push(Tok {
            span: Span::new(start, self.i),
            line: start_line,
            kind,
            blank_before: self.blank_before(start_line),
            kw,
        });
        self.prev_end_line = start_line;
    }

    fn run(mut self) -> Result<Lexed<'a>, Error> {
        while self.i < self.b.len() {
            let c = self.b[self.i];
            match c {
                // Whitespace runs (indentation is ~a quarter of typical
                // proto text): SWAR-consume the whole run, counting the
                // newlines it contains.
                b'\n' | b' ' | b'\t' | b'\r' | 0x0b | 0x0c => {
                    self.i = scan_ws(self.b, self.i, &mut self.line);
                }
                b'/' => self.slash()?,
                b'"' | b'\'' => self.string()?,
                b'0'..=b'9' => self.number(false)?,
                b'.' => {
                    if self.i + 1 < self.b.len() && self.b[self.i + 1].is_ascii_digit() {
                        self.number(true)?;
                    } else {
                        let s = self.i;
                        self.i += 1;
                        self.push_tok(TokKind::Dot, s, self.line);
                    }
                }
                _ if is_ident_start(c) => {
                    let s = self.i;
                    self.i = scan_ident(self.b, self.i + 1);
                    let kw = classify_ident(&self.b[s..self.i]);
                    self.push_tok_kw(TokKind::Ident, s, self.line, kw);
                }
                _ => {
                    let kind = match c {
                        b'{' => TokKind::LBrace,
                        b'}' => TokKind::RBrace,
                        b'(' => TokKind::LParen,
                        b')' => TokKind::RParen,
                        b'[' => TokKind::LBrack,
                        b']' => TokKind::RBrack,
                        b'<' => TokKind::LAngle,
                        b'>' => TokKind::RAngle,
                        b'=' => TokKind::Eq,
                        b';' => TokKind::Semi,
                        b',' => TokKind::Comma,
                        b':' => TokKind::Colon,
                        b'-' => TokKind::Minus,
                        b'+' => TokKind::Plus,
                        _ => {
                            let ch = self.src[self.i..].chars().next().unwrap();
                            return Err(self.err(
                                format!("unexpected character `{}`", ch.escape_default()),
                                self.i,
                                self.i + ch.len_utf8(),
                            ));
                        }
                    };
                    let s = self.i;
                    self.i += 1;
                    self.push_tok(kind, s, self.line);
                }
            }
        }
        let line = self.line;
        let s = self.i;
        self.push_tok(TokKind::Eof, s, line);
        Ok(Lexed {
            toks: self.toks,
            comments: self.comments,
        })
    }

    fn slash(&mut self) -> Result<(), Error> {
        let start = self.i;
        let start_line = self.line;
        let next = self.b.get(self.i + 1).copied();
        match next {
            Some(b'/') => {
                self.i = scan_to_newline(self.b, self.i + 2);
                let raw = &self.src[start..self.i];
                let text = raw.trim_end();
                self.comments.push(Comment {
                    kind: CommentKind::Line,
                    text,
                    span: Span::new(start, start + text.len()),
                    line_start: start_line,
                    line_end: start_line,
                    blank_before: self.blank_before(start_line),
                });
                self.prev_end_line = start_line;
                Ok(())
            }
            Some(b'*') => {
                self.i += 2;
                loop {
                    if self.i + 1 >= self.b.len() {
                        return Err(self
                            .err("unterminated block comment", start, start + 2)
                            .note("block comment starts here and never closes"));
                    }
                    if self.b[self.i] == b'\n' {
                        self.line += 1;
                        self.i += 1;
                    } else if self.b[self.i] == b'*' && self.b[self.i + 1] == b'/' {
                        self.i += 2;
                        break;
                    } else {
                        self.i += 1;
                    }
                }
                let span = Span::new(start, self.i);
                self.comments.push(Comment {
                    kind: CommentKind::Block,
                    text: span.slice(self.src),
                    span,
                    line_start: start_line,
                    line_end: self.line,
                    blank_before: self.blank_before(start_line),
                });
                self.prev_end_line = self.line;
                Ok(())
            }
            _ => {
                let s = self.i;
                self.i += 1;
                self.push_tok(TokKind::Slash, s, self.line);
                Ok(())
            }
        }
    }

    fn string(&mut self) -> Result<(), Error> {
        let start = self.i;
        let start_line = self.line;
        let quote = self.b[self.i];
        self.i += 1;
        loop {
            // Skip the plain body in bulk; only the quote, a backslash, or
            // a newline needs byte-level handling.
            self.i = scan_str_body(self.b, quote, self.i);
            if self.i >= self.b.len() {
                return Err(self.err("unterminated string literal", start, start + 1));
            }
            let c = self.b[self.i];
            if c == quote {
                self.i += 1;
                break;
            }
            match c {
                b'\n' => {
                    return Err(self.err(
                        "newline in string literal (string literals cannot span lines)",
                        start,
                        self.i,
                    ));
                }
                b'\\' => {
                    let esc_start = self.i;
                    self.i += 1;
                    let e = *self
                        .b
                        .get(self.i)
                        .ok_or_else(|| self.err("unterminated string literal", start, start + 1))?;
                    self.i += 1;
                    match e {
                        b'a' | b'b' | b'f' | b'n' | b'r' | b't' | b'v' | b'\\' | b'\'' | b'"'
                        | b'?' => {}
                        b'0'..=b'7' => {
                            let mut n = 1;
                            while n < 3
                                && self.i < self.b.len()
                                && (b'0'..=b'7').contains(&self.b[self.i])
                            {
                                self.i += 1;
                                n += 1;
                            }
                        }
                        b'x' | b'X' => {
                            let mut n = 0;
                            while n < 2
                                && self.i < self.b.len()
                                && self.b[self.i].is_ascii_hexdigit()
                            {
                                self.i += 1;
                                n += 1;
                            }
                            if n == 0 {
                                return Err(self.err(
                                    "invalid hex escape in string literal (expected hex digits after `\\x`)",
                                    esc_start,
                                    self.i,
                                ));
                            }
                        }
                        b'u' | b'U' => {
                            let want = if e == b'u' { 4 } else { 8 };
                            for _ in 0..want {
                                if self.i >= self.b.len() || !self.b[self.i].is_ascii_hexdigit() {
                                    return Err(self.err(
                                        format!(
                                            "invalid unicode escape in string literal (expected {want} hex digits after `\\{}`)",
                                            e as char
                                        ),
                                        esc_start,
                                        self.i,
                                    ));
                                }
                                self.i += 1;
                            }
                        }
                        _ => {
                            return Err(self.err(
                                format!(
                                    "invalid escape sequence `\\{}` in string literal",
                                    (e as char).escape_default()
                                ),
                                esc_start,
                                self.i,
                            ));
                        }
                    }
                }
                _ => self.i += 1,
            }
        }
        self.push_tok(TokKind::Str, start, start_line);
        Ok(())
    }

    /// Lexes a numeric literal. `leading_dot` means the current byte is `.`
    /// followed by a digit.
    fn number(&mut self, leading_dot: bool) -> Result<(), Error> {
        let start = self.i;
        let start_line = self.line;
        let mut is_float = false;

        if leading_dot {
            is_float = true;
            self.i += 1; // '.'
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        } else if self.b[self.i] == b'0' && matches!(self.b.get(self.i + 1), Some(b'x' | b'X')) {
            self.i += 2;
            let digits = self.i;
            while self.i < self.b.len() && self.b[self.i].is_ascii_hexdigit() {
                self.i += 1;
            }
            if self.i == digits {
                return Err(self.err("malformed hex literal (expected hex digits)", start, self.i));
            }
            self.check_number_end(start)?;
            self.push_tok(TokKind::Int, start, start_line);
            return Ok(());
        } else {
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
            if self.i < self.b.len() && self.b[self.i] == b'.' {
                is_float = true;
                self.i += 1;
                while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                    self.i += 1;
                }
            }
        }

        // Exponent.
        if self.i < self.b.len() && matches!(self.b[self.i], b'e' | b'E') {
            let save = self.i;
            self.i += 1;
            if self.i < self.b.len() && matches!(self.b[self.i], b'+' | b'-') {
                self.i += 1;
            }
            let digits = self.i;
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
            if self.i == digits {
                // Not an exponent after all (e.g. `1eger`? that's an error anyway,
                // caught by check_number_end). Roll back and let the check fail.
                self.i = save;
            } else {
                is_float = true;
            }
        }

        // Optional text-format float suffix `f`/`F`.
        if is_float
            && self.i < self.b.len()
            && matches!(self.b[self.i], b'f' | b'F')
            && !self
                .b
                .get(self.i + 1)
                .copied()
                .is_some_and(is_ident_continue)
        {
            self.i += 1;
        }

        // Octal validation: leading 0 with more digits means octal.
        if !is_float {
            let text = &self.src[start..self.i];
            if text.len() > 1
                && text.starts_with('0')
                && !text.bytes().all(|b| (b'0'..=b'7').contains(&b))
            {
                return Err(self.err(
                    format!("malformed octal literal `{text}` (a leading zero implies octal; digits must be 0-7)"),
                    start,
                    self.i,
                ));
            }
        }

        self.check_number_end(start)?;
        self.push_tok(
            if is_float {
                TokKind::Float
            } else {
                TokKind::Int
            },
            start,
            start_line,
        );
        Ok(())
    }

    fn check_number_end(&self, start: usize) -> Result<(), Error> {
        if self.i < self.b.len() && is_ident_continue(self.b[self.i]) {
            let mut end = self.i;
            while end < self.b.len() && is_ident_continue(self.b[end]) {
                end += 1;
            }
            return Err(self.err(
                format!("malformed numeric literal `{}`", &self.src[start..end]),
                start,
                end,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokKind> {
        lex(src).unwrap().toks.iter().map(|t| t.kind).collect()
    }

    #[test]
    fn basic_tokens() {
        assert_eq!(
            kinds("message Foo { int32 a = 1; }"),
            vec![
                TokKind::Ident,
                TokKind::Ident,
                TokKind::LBrace,
                TokKind::Ident,
                TokKind::Ident,
                TokKind::Eq,
                TokKind::Int,
                TokKind::Semi,
                TokKind::RBrace,
                TokKind::Eof,
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds("0 1 0x1F 077 1.5 .5 1e9 1.5e-3 2.5f")[..9].to_vec(),
            {
                use TokKind::*;
                vec![Int, Int, Int, Int, Float, Float, Float, Float, Float]
            }
        );
        assert!(lex("09").is_err());
        assert!(lex("0x").is_err());
        assert!(lex("1abc").is_err());
    }

    #[test]
    fn strings() {
        assert_eq!(
            kinds(r#""hello \n \x41 \101 \u0041""#),
            vec![TokKind::Str, TokKind::Eof]
        );
        assert!(lex(r#""bad \q""#).is_err());
        assert!(lex("\"unterminated").is_err());
        assert!(lex("\"line\nbreak\"").is_err());
    }

    #[test]
    fn comments_and_blanks() {
        let out = lex("// a\n\n// b\nint32").unwrap();
        assert_eq!(out.comments.len(), 2);
        assert!(!out.comments[0].blank_before);
        assert!(out.comments[1].blank_before);
        assert!(!out.toks[0].blank_before);
        assert!(lex("/* unterminated").is_err());
    }
}
