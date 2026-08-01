//! Recursive-descent parser producing the lossless CST.
//!
//! Strictness rules (from the design):
//! - full proto3 grammar coverage, including the complete option grammar
//!   (custom option paths, aggregate literals); anything outside the grammar
//!   is a parse error — there is no "skip unknown construct" recovery;
//! - proto2 constructs (`required`, `group`, `extensions`, `extend`,
//!   `syntax = "proto2"`) produce explicit, targeted diagnostics;
//! - comment attachment is decided here, once, per the rules in the design
//!   (same-line trailing / contiguous leading / blank-separated detached).

use crate::cst::{
    Block, ConstKind, Constant, Enum, EnumItem, EnumValue, Field, FieldOpt, File, HasMeta,
    IdxRange, Import, ImportKind, Item, Label, LabelKind, MapType, Message, Meta, MsgItem, NodeId,
    Oneof, OneofItem, OptNamePart, OptionName, OptionStmt, Package, Path, ResEnd, ResRange,
    Reserved, ReservedKind, Rpc, RpcItem, Service, Sign, StrLit, SvcItem, Syntax, TextField,
    TextMsg, TextName, TextVal, TypeRef, Word,
};
use crate::error::Error;
use crate::lex::{Comment, Kw, Tok, TokKind, lex};
use crate::span::Span;

/// Parses one proto3 source text into its lossless CST.
///
/// # Errors
///
/// Anything outside the proto3 grammar is an error with a source location:
/// lexical errors, grammar violations, proto2 constructs (`required`,
/// `group`, `extend`, `extensions`, `syntax = "proto2"`), a missing or
/// misplaced `syntax` statement, and out-of-range numeric literals.
pub fn parse(src: &str) -> Result<File<'_>, Error> {
    // Spans and arena indices are u32: reject pathological inputs once,
    // here at the boundary, so every later `as u32` is provably lossless.
    if src.len() > u32::MAX as usize {
        return Err(Error::new("input exceeds 4 GiB; spans are 32-bit"));
    }
    let lexed = lex(src)?;
    let mut p = Parser {
        src,
        toks: lexed.toks,
        comments: lexed.comments,
        // Pre-sized from typical path-segment/string-part density, like the
        // lexer's token buffer, so large files don't regrow it repeatedly.
        seg_arena: Vec::with_capacity(src.len() / 24 + 8),
        ti: 0,
        ci: 0,
        last_span: Span::default(),
        last_line: 0,
        next_id: 0,
        depth: 0,
    };
    p.file()
}

/// Comment bookkeeping for one item at a scope boundary — the named
/// (formerly anonymous-tuple) state of the begin → parse → finish item
/// protocol in `block_items`.
#[must_use]
struct ItemStart {
    /// Comment blocks that belong to the *previous* node (detached) or to
    /// the scope (intro).
    to_prev: IdxRange,
    /// Leading comment block of the item about to be parsed.
    leading: IdxRange,
    /// Blank line separated the item from what precedes it.
    blank: bool,
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Tok>,
    /// Becomes `File::comments`; nodes hold ranges into it.
    comments: Vec<Comment<'a>>,
    /// Becomes `File::segs`; paths hold ranges into it.
    seg_arena: Vec<Word<'a>>,
    ti: usize,
    ci: usize,
    last_span: Span,
    last_line: u32,
    next_id: u32,
    /// Current scope nesting depth; see [`MAX_NESTING`].
    depth: u32,
}

/// Scope nesting cap. Parsing and every downstream pass recurse along CST
/// nesting; capping it here (fail-fast, with a located diagnostic) keeps
/// them all off unbounded stacks. 256 is far beyond any real proto and
/// far below stack limits (parser frames are the largest, ~2 KiB each).
const MAX_NESTING: u32 = 256;

impl<'a> Parser<'a> {
    // ---- token primitives -------------------------------------------------

    fn peek(&self) -> Tok {
        self.toks[self.ti]
    }

    fn peek2(&self) -> Tok {
        self.toks[(self.ti + 1).min(self.toks.len() - 1)]
    }

    /// Token text, sliced from the source (tokens don't carry it).
    fn text(&self, t: Tok) -> &'a str {
        // SAFETY: token spans are produced by our lexer, which cuts only at
        // ASCII delimiters — always valid char boundaries within `src`.
        debug_assert!(self.src.is_char_boundary(t.span.start as usize));
        debug_assert!(self.src.is_char_boundary(t.span.end as usize));
        unsafe {
            self.src
                .get_unchecked(t.span.start as usize..t.span.end as usize)
        }
    }

    /// Consumes the current token unconditionally (never called at EOF —
    /// callers have already peeked).
    fn advance(&mut self, t: Tok) {
        if t.kind != TokKind::Eof {
            self.ti += 1;
        }
        self.last_span = t.span;
        self.last_line = t.line;
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.ti];
        self.advance(t);
        t
    }

    fn at(&self, k: TokKind) -> bool {
        self.peek().kind == k
    }

    /// True when the current token is the given keyword. Dispatches on the
    /// lexer's classification byte; no text comparison.
    fn at_kw(&self, kw: Kw) -> bool {
        debug_assert_ne!(kw, Kw::None);
        self.peek().kw == kw
    }

    fn eat(&mut self, k: TokKind) -> Option<Tok> {
        // Single indexed load: peek once, advance on the same token.
        let t = self.peek();
        if t.kind == k {
            self.advance(t);
            Some(t)
        } else {
            None
        }
    }

    /// An error at a span, resolved against this parser's source.
    fn error(&self, msg: impl Into<String>, span: Span) -> Error {
        Error::at(msg, span, self.src)
    }

    fn err_here(&self, what: &str) -> Error {
        let t = self.peek();
        let shown = if t.kind == TokKind::Eof {
            "end of file".to_string()
        } else {
            format!("`{}`", self.text(t))
        };
        self.error(format!("expected {what}, found {shown}"), t.span)
    }

    fn expect(&mut self, k: TokKind, what: &str) -> Result<Tok, Error> {
        let t = self.peek();
        if t.kind == k {
            self.advance(t);
            Ok(t)
        } else {
            Err(self.err_here(what))
        }
    }

    fn expect_ident(&mut self, what: &str) -> Result<Word<'a>, Error> {
        let t = self.expect(TokKind::Ident, what)?;
        Ok(self.word(t))
    }

    fn word(&self, t: Tok) -> Word<'a> {
        Word {
            text: self.text(t),
            span: t.span,
        }
    }

    // ---- comment attachment -----------------------------------------------
    //
    // Attachment never copies a comment: every attachment is a contiguous
    // run of the comment stream (comments are consumed strictly in order),
    // so nodes store `IdxRange`s into `File::comments`.

    /// All comments positioned before the current token.
    fn take_pending(&mut self) -> IdxRange {
        let limit = self.peek().span.start;
        let start = self.ci;
        while self.ci < self.comments.len() && self.comments[self.ci].span.start < limit {
            self.ci += 1;
        }
        IdxRange::new(start, self.ci - start)
    }

    /// Called at an item boundary. Splits pending comments into
    /// (blocks-for-previous-node, leading-block-of-this-item) and computes
    /// the item's blank-line flag.
    ///
    /// Part of the item protocol: `begin_item` advances the comment cursor
    /// (monotonically — attachments are contiguous runs of the stream) and
    /// hands back an [`ItemStart`] that the scope loop must consume; the
    /// `#[must_use]` on the type keeps a begun item from being dropped
    /// silently.
    fn begin_item(&mut self) -> ItemStart {
        let pending = self.take_pending();
        let cur = self.peek();
        let mut to_prev = pending;
        let mut leading = IdxRange::EMPTY;
        if !pending.is_empty() {
            let cs = pending.slice(&self.comments);
            let last = cs.last().unwrap();
            // Contiguous with the item (no blank line in between)?
            if cur.line <= last.line_end + 1 {
                let mut j = cs.len() - 1;
                while j > 0 && !cs[j].blank_before {
                    j -= 1;
                }
                to_prev = IdxRange::new(pending.start as usize, j);
                leading = IdxRange::new(pending.start as usize + j, pending.len as usize - j);
            }
        }
        let blank = if leading.is_empty() {
            cur.blank_before
        } else {
            self.comments[leading.start as usize].blank_before
        };
        ItemStart {
            to_prev,
            leading,
            blank,
        }
    }

    /// Comments on the same line the item ended on become its trailing
    /// comments.
    fn finish_trailing(&mut self, end_line: u32) -> IdxRange {
        let limit = self.peek().span.start;
        let start = self.ci;
        while self.ci < self.comments.len() {
            let c = &self.comments[self.ci];
            if c.span.start < limit && c.line_start == end_line {
                self.ci += 1;
            } else {
                break;
            }
        }
        IdxRange::new(start, self.ci - start)
    }

    const fn meta(&mut self, start: u32, blank: bool, leading: IdxRange) -> Meta {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        Meta {
            id,
            span: Span {
                start,
                end: self.last_span.end,
            },
            blank_before: blank,
            leading,
            trailing: IdxRange::EMPTY,
            detached: IdxRange::EMPTY,
        }
    }

    // ---- generic scope loop -----------------------------------------------

    /// Parses items until `closer`, handling empty statements, comment
    /// attachment, and trailing comments uniformly.
    ///
    /// Every nested scope (message/oneof/enum/service/rpc bodies, text
    /// aggregates) enters through here, so this is also the recursion-depth
    /// gate: parsing, and every later recursive pass over the CST
    /// (formatting, symbol collection, pruning), stays on bounded stack.
    fn block_items<T: HasMeta>(
        &mut self,
        closer: TokKind,
        opener: Option<Span>,
        parse_item: impl FnMut(&mut Self, bool, IdxRange) -> Result<T, Error>,
    ) -> Result<Block<T>, Error> {
        // The file root (opener = None) is not a nesting level: the cap
        // counts braced blocks exactly.
        if opener.is_none() {
            return self.block_items_inner(closer, opener, parse_item);
        }
        self.depth += 1;
        if self.depth > MAX_NESTING {
            let t = self.peek();
            return Err(self.error(
                format!("blocks nest deeper than {MAX_NESTING} levels"),
                t.span,
            ));
        }
        let r = self.block_items_inner(closer, opener, parse_item);
        self.depth -= 1;
        r
    }

    fn block_items_inner<T: HasMeta>(
        &mut self,
        closer: TokKind,
        opener: Option<Span>,
        mut parse_item: impl FnMut(&mut Self, bool, IdxRange) -> Result<T, Error>,
    ) -> Result<Block<T>, Error> {
        let mut intro = IdxRange::EMPTY;
        let mut items: Vec<T> = Vec::new();
        let dangling;
        loop {
            while self.at(TokKind::Semi) {
                self.bump();
            }
            if self.at(closer) {
                dangling = self.take_pending();
                self.bump();
                break;
            }
            if self.at(TokKind::Eof) {
                let mut d = self.err_here("`}`");
                if let Some(sp) = opener {
                    d = d.note_at("unclosed block starts here", sp, self.src);
                }
                return Err(d);
            }
            let start = self.begin_item();
            if !start.to_prev.is_empty() {
                if let Some(it) = items.last_mut() {
                    debug_assert!(it.meta().detached.is_empty());
                    it.meta_mut().detached = start.to_prev;
                } else {
                    debug_assert!(intro.is_empty());
                    intro = start.to_prev;
                }
            }
            // Items are large (a field is ~200 bytes); growing from zero
            // reallocs and memcpys repeatedly. Reserve once, lazily, so
            // empty blocks stay allocation-free.
            if items.capacity() == 0 {
                items.reserve(8);
            }
            let mut item = parse_item(self, start.blank, start.leading)?;
            let end_line = self.last_line;
            item.meta_mut().trailing = self.finish_trailing(end_line);
            items.push(item);
        }
        Ok(Block {
            intro,
            items,
            dangling,
        })
    }

    // ---- file --------------------------------------------------------------

    fn file(&mut self) -> Result<File<'a>, Error> {
        let top = self.block_items(TokKind::Eof, None, Self::top_item)?;

        if top.items.is_empty() {
            return Err(self.error(
                "empty file: expected `syntax = \"proto3\";`",
                Span::default(),
            ));
        }
        let mut saw_package = false;
        for (i, item) in top.items.iter().enumerate() {
            match item {
                Item::Syntax(s) => {
                    if i != 0 {
                        return Err(self.error(
                            "`syntax` must be the first statement in the file",
                            s.meta.span,
                        ));
                    }
                }
                Item::Package(p) => {
                    if saw_package {
                        return Err(self.error("duplicate `package` statement", p.meta.span));
                    }
                    saw_package = true;
                }
                _ => {}
            }
        }
        if !matches!(top.items[0], Item::Syntax(_)) {
            return Err(self.error(
                "file must start with `syntax = \"proto3\";`",
                top.items[0].meta().span,
            ));
        }

        Ok(File {
            src: self.src,
            comments: std::mem::take(&mut self.comments),
            segs: std::mem::take(&mut self.seg_arena),
            top,
            node_count: self.next_id,
        })
    }

    fn top_item(p: &mut Self, blank: bool, leading: IdxRange) -> Result<Item<'a>, Error> {
        let t = p.peek();
        if t.kind != TokKind::Ident {
            return Err(p.err_here("a top-level definition"));
        }
        match t.kw {
            Kw::Syntax => p.syntax(blank, leading).map(Item::Syntax),
            Kw::Package => p.package(blank, leading).map(Item::Package),
            Kw::Import => p.import(blank, leading).map(Item::Import),
            Kw::Option => p.option_stmt(blank, leading).map(Item::Option),
            Kw::Message => p.message(blank, leading).map(Item::Message),
            Kw::Enum => p.enum_def(blank, leading).map(Item::Enum),
            Kw::Service => p.service(blank, leading).map(Item::Service),
            Kw::Extend => Err(p.error(
                "proto2 construct `extend` is not supported (pbpp is proto3-only)",
                t.span,
            )),
            Kw::Extensions => Err(p.error(
                "proto2 construct `extensions` is not supported (pbpp is proto3-only)",
                t.span,
            )),
            _ => Err(p.err_here(
                "a top-level definition (`message`, `enum`, `service`, `import`, `option`, or `package`)",
            )),
        }
    }

    fn syntax(&mut self, blank: bool, leading: IdxRange) -> Result<Syntax<'a>, Error> {
        let start = self.peek().span.start;
        self.bump(); // syntax
        self.expect(TokKind::Eq, "`=`")?;
        let t = self.expect(TokKind::Str, "a quoted syntax string")?;
        let value = self.word(t);
        let inner = &value.text[1..value.text.len() - 1];
        if inner != "proto3" {
            let msg = if inner == "proto2" {
                "proto2 files are not supported (pbpp is proto3-only)".to_string()
            } else {
                format!("unsupported syntax `{inner}`; only `proto3` is supported")
            };
            return Err(self.error(msg, value.span));
        }
        self.expect(TokKind::Semi, "`;`")?;
        Ok(Syntax {
            meta: self.meta(start, blank, leading),
            value,
        })
    }

    fn package(&mut self, blank: bool, leading: IdxRange) -> Result<Package, Error> {
        let start = self.peek().span.start;
        self.bump(); // package
        let path = self.path()?;
        if path.leading_dot {
            return Err(self.error("package name must not start with `.`", path.span));
        }
        self.expect(TokKind::Semi, "`;`")?;
        Ok(Package {
            meta: self.meta(start, blank, leading),
            path,
        })
    }

    fn import(&mut self, blank: bool, leading: IdxRange) -> Result<Import<'a>, Error> {
        let start = self.peek().span.start;
        self.bump(); // import
        let kind = if self.at_kw(Kw::Public) {
            self.bump();
            ImportKind::Public
        } else if self.at_kw(Kw::Weak) {
            self.bump();
            ImportKind::Weak
        } else {
            ImportKind::Plain
        };
        let t = self.expect(TokKind::Str, "a quoted import path")?;
        let path = self.word(t);
        self.expect(TokKind::Semi, "`;`")?;
        Ok(Import {
            meta: self.meta(start, blank, leading),
            kind,
            path,
            // Strip the quotes once, here; the lexer guarantees the
            // literal starts and ends with its quote character.
            path_inner: &path.text[1..path.text.len() - 1],
        })
    }

    fn option_stmt(&mut self, blank: bool, leading: IdxRange) -> Result<OptionStmt<'a>, Error> {
        let start = self.peek().span.start;
        self.bump(); // option
        let name = self.option_name()?;
        self.expect(TokKind::Eq, "`=`")?;
        let value = self.constant()?;
        self.expect(TokKind::Semi, "`;`")?;
        Ok(OptionStmt {
            meta: self.meta(start, blank, leading),
            name,
            value,
        })
    }

    // ---- names, paths, constants -------------------------------------------

    /// `[.]ident(.ident)*` — segments are appended to the shared arena, so
    /// a path costs no allocation of its own.
    fn path(&mut self) -> Result<Path, Error> {
        let start = self.peek().span.start;
        let leading_dot = self.eat(TokKind::Dot).is_some();
        let seg_start = self.seg_arena.len();
        let first = self.expect_ident("an identifier")?;
        self.seg_arena.push(first);
        while self.at(TokKind::Dot) {
            self.bump();
            let seg = self.expect_ident("an identifier after `.`")?;
            self.seg_arena.push(seg);
        }
        Ok(Path {
            leading_dot,
            segs: IdxRange::new(seg_start, self.seg_arena.len() - seg_start),
            span: Span {
                start,
                end: self.last_span.end,
            },
        })
    }

    /// `(ident | "(" [.]full.ident ")") ("." (ident | "(" ... ")"))*`
    fn option_name(&mut self) -> Result<OptionName<'a>, Error> {
        let start = self.peek().span.start;
        let first = self.option_name_part()?;
        // The common case is a one-part name; `rest` stays unallocated.
        let mut rest = Vec::new();
        while self.at(TokKind::Dot) {
            self.bump();
            rest.push(self.option_name_part()?);
        }
        Ok(OptionName {
            first,
            rest,
            span: Span {
                start,
                end: self.last_span.end,
            },
        })
    }

    fn option_name_part(&mut self) -> Result<OptNamePart<'a>, Error> {
        if self.at(TokKind::LParen) {
            let lp = self.bump();
            let path = self.path()?;
            let rp = self.expect(TokKind::RParen, "`)`")?;
            Ok(OptNamePart::Ext(path, Span::join(lp.span, rp.span)))
        } else {
            Ok(OptNamePart::Ident(self.expect_ident("an option name")?))
        }
    }

    fn constant(&mut self) -> Result<Constant<'a>, Error> {
        let start = self.peek().span.start;
        let kind = match self.peek().kind {
            TokKind::Str => ConstKind::Str(self.strlit()?),
            TokKind::Int | TokKind::Float => {
                let t = self.bump();
                ConstKind::Num {
                    sign: Sign::None,
                    word: self.word(t),
                }
            }
            TokKind::Minus | TokKind::Plus => {
                let sign = if self.bump().kind == TokKind::Minus {
                    Sign::Neg
                } else {
                    Sign::Pos
                };
                let t = self.peek();
                let ok = matches!(t.kind, TokKind::Int | TokKind::Float)
                    || matches!(t.kw, Kw::Inf | Kw::Nan);
                if !ok {
                    return Err(self.err_here("a numeric literal after the sign"));
                }
                let t = self.bump();
                ConstKind::Num {
                    sign,
                    word: self.word(t),
                }
            }
            TokKind::Ident => ConstKind::Path(self.path()?),
            TokKind::LBrace => ConstKind::Aggregate(Box::new(self.text_msg()?)),
            _ => return Err(self.err_here("a constant value")),
        };
        Ok(Constant {
            span: Span {
                start,
                end: self.last_span.end,
            },
            kind,
        })
    }

    /// One or more adjacent string literals; the words go into the shared
    /// arena, so a string constant allocates nothing of its own.
    fn strlit(&mut self) -> Result<StrLit, Error> {
        let first = self.expect(TokKind::Str, "a string literal")?;
        let seg_start = self.seg_arena.len();
        let first_w = self.word(first);
        self.seg_arena.push(first_w);
        let mut last_span = first_w.span;
        while self.at(TokKind::Str) {
            let t = self.bump();
            let w = self.word(t);
            self.seg_arena.push(w);
            last_span = w.span;
        }
        Ok(StrLit {
            parts: IdxRange::new(seg_start, self.seg_arena.len() - seg_start),
            span: Span::join(first_w.span, last_span),
        })
    }

    fn int_value(&self, w: Word<'a>) -> Result<u64, Error> {
        parse_int(w.text).ok_or_else(|| {
            self.error(
                format!("integer literal `{}` is out of range", w.text),
                w.span,
            )
        })
    }

    fn signed_value(&self, w: Word<'a>, neg: bool) -> Result<i64, Error> {
        let magnitude = self.int_value(w)?;
        let limit = i64::MAX.cast_unsigned() + u64::from(neg);
        if magnitude > limit {
            return Err(self.error(
                format!("integer literal `{}` is out of range", w.text),
                w.span,
            ));
        }
        // Guarded above: the reinterpretation is exact (including i64::MIN
        // for the negative extreme).
        Ok(if neg {
            magnitude.cast_signed().wrapping_neg()
        } else {
            magnitude.cast_signed()
        })
    }

    // ---- message -----------------------------------------------------------

    fn message(&mut self, blank: bool, leading: IdxRange) -> Result<Message<'a>, Error> {
        let start = self.peek().span.start;
        self.bump(); // message
        let name = self.expect_ident("a message name")?;
        let lb = self.expect(TokKind::LBrace, "`{`")?;
        let body = self.block_items(TokKind::RBrace, Some(lb.span), Self::msg_item)?;
        Ok(Message {
            meta: self.meta(start, blank, leading),
            name,
            body,
        })
    }

    fn msg_item(p: &mut Self, blank: bool, leading: IdxRange) -> Result<MsgItem<'a>, Error> {
        let t = p.peek();
        match (t.kind, t.kw) {
            (TokKind::Ident, Kw::Message) => p.message(blank, leading).map(MsgItem::Message),
            (TokKind::Ident, Kw::Enum) => p.enum_def(blank, leading).map(MsgItem::Enum),
            (TokKind::Ident, Kw::Oneof) => p.oneof(blank, leading).map(MsgItem::Oneof),
            (TokKind::Ident, Kw::Option) => p.option_stmt(blank, leading).map(MsgItem::Option),
            (TokKind::Ident, Kw::Reserved) => {
                p.reserved(blank, leading, false).map(MsgItem::Reserved)
            }
            (TokKind::Ident, Kw::Extensions | Kw::Extend) => Err(p.error(
                format!(
                    "proto2 construct `{}` is not supported (pbpp is proto3-only)",
                    p.text(t)
                ),
                t.span,
            )),
            (TokKind::Ident, Kw::Group) => Err(p.error(
                "proto2 construct `group` is not supported (pbpp is proto3-only)",
                t.span,
            )),
            (TokKind::Ident, Kw::Required) => Err(p.error(
                "proto2 label `required` is not supported (pbpp is proto3-only)",
                t.span,
            )),
            (TokKind::Ident | TokKind::Dot, _) => {
                p.field(blank, leading, false).map(MsgItem::Field)
            }
            _ => Err(p.err_here("a field or definition")),
        }
    }

    fn field(
        &mut self,
        blank: bool,
        leading: IdxRange,
        in_oneof: bool,
    ) -> Result<Field<'a>, Error> {
        let start = self.peek().span.start;
        let mut label = None;
        let t = self.peek();
        // Dispatch on the lexer's keyword byte; token text is only touched
        // on error paths.
        let kind = match t.kw {
            Kw::Repeated => Some(LabelKind::Repeated),
            Kw::Optional => Some(LabelKind::Optional),
            Kw::Required => {
                if in_oneof {
                    return Err(
                        self.error("label `required` is not allowed on a oneof field", t.span)
                    );
                }
                return Err(self.error(
                    "proto2 label `required` is not supported (pbpp is proto3-only)",
                    t.span,
                ));
            }
            _ => None,
        };
        if let Some(kind) = kind {
            if in_oneof {
                return Err(self.error(
                    format!("label `{}` is not allowed on a oneof field", self.text(t)),
                    t.span,
                ));
            }
            self.advance(t);
            label = Some(Label { kind, span: t.span });
        }
        let ty = self.type_ref()?;
        if let TypeRef::Map(mt) = &ty {
            if let Some(l) = &label {
                return Err(self.error(
                    "a map field cannot have a label (maps are implicitly repeated)",
                    l.span,
                ));
            }
            if in_oneof {
                return Err(self.error("a map field is not allowed in a oneof", mt.span));
            }
        }
        let name = self.expect_ident("a field name")?;
        self.expect(TokKind::Eq, "`=`")?;
        let number = self.field_number()?;
        let number_val = self.int_value(number)?;
        // protoc rejects these at parse time too: field numbers live in
        // [1, 536_870_911] (2^29 - 1).
        if number_val == 0 || number_val > 536_870_911 {
            return Err(self.error(
                format!(
                    "field number {number_val} is out of range (must be between 1 and 536870911)"
                ),
                number.span,
            ));
        }
        let options = self.field_options()?;
        self.expect(TokKind::Semi, "`;`")?;
        Ok(Field {
            meta: self.meta(start, blank, leading),
            label,
            ty,
            name,
            number,
            number_val,
            options,
        })
    }

    fn field_number(&mut self) -> Result<Word<'a>, Error> {
        let t = self.peek();
        if t.kind == TokKind::Float {
            return Err(self.error("field number must be an integer", t.span));
        }
        let t = self.expect(TokKind::Int, "a field number")?;
        Ok(self.word(t))
    }

    fn type_ref(&mut self) -> Result<TypeRef<'a>, Error> {
        let t = self.peek();
        match t.kind {
            TokKind::Ident => {
                // Scalars and `map` dispatch on the lexer's keyword byte;
                // the identifier's bytes were classified exactly once.
                if t.kw.is_scalar_type() {
                    self.advance(t);
                    Ok(TypeRef::Scalar(self.word(t)))
                } else if t.kw == Kw::Map && self.peek2().kind == TokKind::LAngle {
                    self.map_type(t.span.start)
                } else {
                    Ok(TypeRef::Named(self.path()?))
                }
            }
            TokKind::Dot => Ok(TypeRef::Named(self.path()?)),
            _ => Err(self.err_here("a field type")),
        }
    }

    fn map_type(&mut self, start: u32) -> Result<TypeRef<'a>, Error> {
        self.bump(); // map
        self.bump(); // <
        let key_tok = self.peek();
        let key = self.expect_ident("a map key type")?;
        if !key_tok.kw.is_map_key_type() {
            return Err(self.error(
                format!(
                    "invalid map key type `{}` (must be an integer type, `bool`, or `string`)",
                    key.text
                ),
                key.span,
            ));
        }
        self.expect(TokKind::Comma, "`,`")?;
        let value = self.type_ref()?;
        if matches!(value, TypeRef::Map(_)) {
            return Err(self.error("a map value cannot be another map", self.last_span));
        }
        self.expect(TokKind::RAngle, "`>`")?;
        Ok(TypeRef::Map(Box::new(MapType {
            key,
            value,
            span: Span {
                start,
                end: self.last_span.end,
            },
        })))
    }

    fn field_options(&mut self) -> Result<Vec<FieldOpt<'a>>, Error> {
        if self.eat(TokKind::LBrack).is_none() {
            return Ok(Vec::new());
        }
        // `[` guarantees at least one option; size for that dominant case
        // so the first push never goes through growth logic.
        let mut out = Vec::with_capacity(1);
        loop {
            let start = self.peek().span.start;
            let name = self.option_name()?;
            self.expect(TokKind::Eq, "`=`")?;
            let value = self.constant()?;
            out.push(FieldOpt {
                name,
                value,
                span: Span {
                    start,
                    end: self.last_span.end,
                },
            });
            if self.eat(TokKind::Comma).is_some() {
                continue;
            }
            self.expect(TokKind::RBrack, "`]` or `,`")?;
            break;
        }
        Ok(out)
    }

    // ---- oneof, enum, service ----------------------------------------------

    fn oneof(&mut self, blank: bool, leading: IdxRange) -> Result<Oneof<'a>, Error> {
        let start = self.peek().span.start;
        self.bump(); // oneof
        let name = self.expect_ident("a oneof name")?;
        let lb = self.expect(TokKind::LBrace, "`{`")?;
        let body = self.block_items(TokKind::RBrace, Some(lb.span), |p, blank, leading| {
            let t = p.peek();
            match (t.kind, t.kw) {
                (TokKind::Ident, Kw::Option) => {
                    p.option_stmt(blank, leading).map(OneofItem::Option)
                }
                (TokKind::Ident | TokKind::Dot, _) => {
                    p.field(blank, leading, true).map(OneofItem::Field)
                }
                _ => Err(p.err_here("a oneof field or `option`")),
            }
        })?;
        Ok(Oneof {
            meta: self.meta(start, blank, leading),
            name,
            body,
        })
    }

    fn enum_def(&mut self, blank: bool, leading: IdxRange) -> Result<Enum<'a>, Error> {
        let start = self.peek().span.start;
        self.bump(); // enum
        let name = self.expect_ident("an enum name")?;
        let lb = self.expect(TokKind::LBrace, "`{`")?;
        let body = self.block_items(TokKind::RBrace, Some(lb.span), |p, blank, leading| {
            let t = p.peek();
            match (t.kind, t.kw) {
                (TokKind::Ident, Kw::Option) => p.option_stmt(blank, leading).map(EnumItem::Option),
                (TokKind::Ident, Kw::Reserved) => {
                    p.reserved(blank, leading, true).map(EnumItem::Reserved)
                }
                (TokKind::Ident, _) => p.enum_value(blank, leading).map(EnumItem::Value),
                _ => Err(p.err_here("an enum value")),
            }
        })?;
        Ok(Enum {
            meta: self.meta(start, blank, leading),
            name,
            body,
        })
    }

    fn enum_value(&mut self, blank: bool, leading: IdxRange) -> Result<EnumValue<'a>, Error> {
        let start = self.peek().span.start;
        let name = self.expect_ident("an enum value name")?;
        self.expect(TokKind::Eq, "`=`")?;
        let negative = self.eat(TokKind::Minus).is_some();
        let t = self.expect(TokKind::Int, "an enum value number")?;
        let number = self.word(t);
        let number_val = self.signed_value(number, negative)?;
        // protoc rejects these at parse time too: enum values are int32.
        if i32::try_from(number_val).is_err() {
            return Err(self.error(
                format!("enum value number {number_val} is out of range (enum values are 32-bit integers)"),
                number.span,
            ));
        }
        let options = self.field_options()?;
        self.expect(TokKind::Semi, "`;`")?;
        Ok(EnumValue {
            meta: self.meta(start, blank, leading),
            name,
            negative,
            number,
            number_val,
            options,
        })
    }

    fn service(&mut self, blank: bool, leading: IdxRange) -> Result<Service<'a>, Error> {
        let start = self.peek().span.start;
        self.bump(); // service
        let name = self.expect_ident("a service name")?;
        let lb = self.expect(TokKind::LBrace, "`{`")?;
        let body = self.block_items(TokKind::RBrace, Some(lb.span), |p, blank, leading| {
            let t = p.peek();
            match (t.kind, t.kw) {
                (TokKind::Ident, Kw::Option) => p.option_stmt(blank, leading).map(SvcItem::Option),
                (TokKind::Ident, Kw::Rpc) => p.rpc(blank, leading).map(SvcItem::Rpc),
                _ => Err(p.err_here("`rpc` or `option`")),
            }
        })?;
        Ok(Service {
            meta: self.meta(start, blank, leading),
            name,
            body,
        })
    }

    fn rpc(&mut self, blank: bool, leading: IdxRange) -> Result<Rpc<'a>, Error> {
        let start = self.peek().span.start;
        self.bump(); // rpc
        let name = self.expect_ident("an rpc name")?;
        self.expect(TokKind::LParen, "`(`")?;
        let client_stream = self.eat_stream();
        let input = self.path()?;
        self.expect(TokKind::RParen, "`)`")?;
        let ret = self.peek();
        if ret.kw != Kw::Returns {
            return Err(self.err_here("`returns`"));
        }
        self.advance(ret);
        self.expect(TokKind::LParen, "`(`")?;
        let server_stream = self.eat_stream();
        let output = self.path()?;
        self.expect(TokKind::RParen, "`)`")?;
        let body = if self.eat(TokKind::Semi).is_some() {
            None
        } else if self.at(TokKind::LBrace) {
            let lb = self.bump();
            Some(Box::new(self.block_items(
                TokKind::RBrace,
                Some(lb.span),
                |p, blank, leading| {
                    if p.at_kw(Kw::Option) {
                        p.option_stmt(blank, leading).map(RpcItem::Option)
                    } else {
                        Err(p.err_here("`option` (only options may appear in an rpc body)"))
                    }
                },
            )?))
        } else {
            return Err(self.err_here("`;` or `{`"));
        };
        Ok(Rpc {
            meta: self.meta(start, blank, leading),
            name,
            client_stream,
            input,
            server_stream,
            output,
            body,
        })
    }

    /// Eats a `stream` keyword, but only when it is a modifier (followed by
    /// a type), not a message type named `stream`.
    fn eat_stream(&mut self) -> bool {
        if self.at_kw(Kw::Stream) && matches!(self.peek2().kind, TokKind::Ident | TokKind::Dot) {
            self.bump();
            true
        } else {
            false
        }
    }

    // ---- reserved ------------------------------------------------------------

    /// `in_enum` fixes the number grammar: enum reserved ranges are signed
    /// int32 values, message reserved ranges are field numbers (>= 1).
    fn reserved(
        &mut self,
        blank: bool,
        leading: IdxRange,
        in_enum: bool,
    ) -> Result<Reserved<'a>, Error> {
        let start = self.peek().span.start;
        self.bump(); // reserved
        let kind = if self.at(TokKind::Str) {
            let t = self.bump();
            let mut names = vec![self.word(t)];
            while self.eat(TokKind::Comma).is_some() {
                let t = self.expect(TokKind::Str, "a quoted field name")?;
                names.push(self.word(t));
            }
            ReservedKind::Names(names)
        } else if self.at(TokKind::Int) || self.at(TokKind::Minus) {
            let mut ranges = Vec::new();
            loop {
                // Only enum reserved ranges may be negative (enum values
                // are int32); message ranges are field numbers, >= 1.
                let start_neg = self.eat(TokKind::Minus).is_some();
                let t = self.expect(TokKind::Int, "a number")?;
                let start_w = self.word(t);
                let start_val = self.signed_value(start_w, start_neg)?;
                self.check_reserved_bound(in_enum, start_neg, start_val, start_w.span)?;
                let end = if self.at_kw(Kw::To) {
                    self.bump();
                    if self.at_kw(Kw::Max) {
                        Some(ResEnd::Max(self.bump().span))
                    } else {
                        let neg = self.eat(TokKind::Minus).is_some();
                        let t = self.expect(TokKind::Int, "a number or `max`")?;
                        let w = self.word(t);
                        let value = self.signed_value(w, neg)?;
                        self.check_reserved_bound(in_enum, neg, value, w.span)?;
                        Some(ResEnd::Num {
                            neg,
                            word: w,
                            value,
                        })
                    }
                } else {
                    None
                };
                ranges.push(ResRange {
                    start_neg,
                    start: start_w,
                    start_val,
                    end,
                });
                if self.eat(TokKind::Comma).is_none() {
                    break;
                }
            }
            ReservedKind::Ranges(ranges)
        } else {
            return Err(self.err_here("a field number range or a quoted field name"));
        };
        self.expect(TokKind::Semi, "`;`")?;
        Ok(Reserved {
            meta: self.meta(start, blank, leading),
            kind,
        })
    }

    fn check_reserved_bound(
        &self,
        in_enum: bool,
        neg: bool,
        value: i64,
        span: Span,
    ) -> Result<(), Error> {
        if in_enum {
            // Enum values are int32; so are their reserved ranges.
            if i32::try_from(value).is_err() {
                return Err(self.error("enum reserved numbers must fit a 32-bit integer", span));
            }
        } else if neg || !(1..=536_870_911).contains(&value) {
            return Err(self.error(
                "reserved numbers in a message must be field numbers (between 1 and 536870911)",
                span,
            ));
        }
        Ok(())
    }

    // ---- text-format aggregates ------------------------------------------------

    /// Parses `{ ... }` or `< ... >`; current token must be the opener.
    fn text_msg(&mut self) -> Result<TextMsg<'a>, Error> {
        let opener = self.bump();
        let closer = match opener.kind {
            TokKind::LBrace => TokKind::RBrace,
            TokKind::LAngle => TokKind::RAngle,
            _ => return Err(self.error("expected `{`", opener.span)),
        };
        let start = opener.span.start;
        let body = self.block_items(closer, Some(opener.span), Self::text_field)?;
        // The comment bit the formatter dispatches on. Nested aggregates
        // already carry theirs, so this is one shallow pass per level —
        // O(nodes) over the whole parse, never recomputed.
        let has_comments = !body.intro.is_empty()
            || !body.dangling.is_empty()
            || body
                .items
                .iter()
                .any(|f| f.meta.has_comments() || text_val_has_comments(&f.value));
        Ok(TextMsg {
            span: Span {
                start,
                end: self.last_span.end,
            },
            body,
            has_comments,
        })
    }

    fn text_field(p: &mut Self, blank: bool, leading: IdxRange) -> Result<TextField<'a>, Error> {
        let start = p.peek().span.start;
        let name = if p.at(TokKind::LBrack) {
            let lb = p.bump();
            let mut text = String::new();
            loop {
                let t = p.peek();
                match t.kind {
                    TokKind::Ident | TokKind::Dot | TokKind::Slash => {
                        text.push_str(p.text(t));
                        p.bump();
                    }
                    TokKind::RBrack => break,
                    _ => return Err(p.err_here("an extension or type name inside `[...]`")),
                }
            }
            if text.is_empty() {
                return Err(p.err_here("an extension or type name inside `[...]`"));
            }
            let rb = p.bump();
            TextName::Bracket {
                text,
                span: Span::join(lb.span, rb.span),
            }
        } else {
            TextName::Ident(p.expect_ident("a field name")?)
        };
        let value = if p.eat(TokKind::Colon).is_some() {
            p.text_val()?
        } else if matches!(p.peek().kind, TokKind::LBrace | TokKind::LAngle) {
            TextVal::Msg(Box::new(p.text_msg()?))
        } else {
            return Err(p.err_here("`:` or `{`"));
        };
        // Optional separator.
        if p.at(TokKind::Comma) || p.at(TokKind::Semi) {
            p.bump();
        }
        Ok(TextField {
            meta: p.meta(start, blank, leading),
            name,
            value,
        })
    }

    fn text_val(&mut self) -> Result<TextVal<'a>, Error> {
        let t = self.peek();
        match t.kind {
            TokKind::Str => Ok(TextVal::Str(self.strlit()?)),
            TokKind::Int | TokKind::Float | TokKind::Ident => {
                let t = self.bump();
                Ok(TextVal::Scalar {
                    sign: Sign::None,
                    word: self.word(t),
                })
            }
            TokKind::Minus | TokKind::Plus => {
                let sign = if self.bump().kind == TokKind::Minus {
                    Sign::Neg
                } else {
                    Sign::Pos
                };
                let t = self.peek();
                let ok = matches!(t.kind, TokKind::Int | TokKind::Float)
                    || matches!(t.kw, Kw::Inf | Kw::Nan);
                if !ok {
                    return Err(self.err_here("a numeric literal after the sign"));
                }
                let t = self.bump();
                Ok(TextVal::Scalar {
                    sign,
                    word: self.word(t),
                })
            }
            TokKind::LBrace | TokKind::LAngle => Ok(TextVal::Msg(Box::new(self.text_msg()?))),
            TokKind::LBrack => {
                let lb = self.bump();
                let mut items = Vec::new();
                if !self.at(TokKind::RBrack) {
                    loop {
                        if self.at(TokKind::LBrack) {
                            return Err(self.err_here(
                                "a value (nested lists are not allowed in text format)",
                            ));
                        }
                        items.push(self.text_val()?);
                        if self.eat(TokKind::Comma).is_some() {
                            continue;
                        }
                        break;
                    }
                }
                let rb = self.expect(TokKind::RBrack, "`]` or `,`")?;
                Ok(TextVal::List {
                    items,
                    span: Span::join(lb.span, rb.span),
                })
            }
            _ => Err(self.err_here("a value")),
        }
    }
}

/// True if any comment is attached inside a text-format value; nested
/// aggregates answer from their precomputed bit.
fn text_val_has_comments(v: &TextVal<'_>) -> bool {
    match v {
        TextVal::Msg(m) => m.has_comments,
        TextVal::List { items, .. } => items.iter().any(text_val_has_comments),
        TextVal::Scalar { .. } | TextVal::Str(_) => false,
    }
}

/// Parses a proto integer literal: decimal, `0x` hex, or leading-zero octal.
fn parse_int(text: &str) -> Option<u64> {
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None if text.len() > 1 && text.starts_with('0') => u64::from_str_radix(&text[1..], 8).ok(),
        None => text.parse().ok(),
    }
}
