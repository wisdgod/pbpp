//! Lossless CST.
//!
//! Two hard requirements from the design:
//! 1. every node carries its byte span — token-level and definition-level;
//! 2. trivia (comments, blank-line separation) is attached to nodes, never
//!    dropped. Attachment is decided once, at parse time.
//!
//! Every addressable node (definitions, fields, enum values, methods, …)
//! carries a `NodeId`; selection marks are stored in dense arrays indexed by
//! it (the layout equivalent of a bitset field on the node).
//!
//! Layout: the file owns two arenas — the comment stream and the word
//! arena (path segments, string-literal parts). Nodes refer into them with
//! `IdxRange` (8 bytes, two u32) instead of owning vectors: comment
//! attachment is always a contiguous run of the comment stream, and a
//! path's segments are written to the arena once during parsing. This
//! keeps nodes small (`-Z print-type-sizes` guided) and makes attachment
//! allocation-free.
//!
//! Public surface (0.1): the CST is the input to pbpp's own passes —
//! `format`, `digest`, `prune` — which is how
//! downstream code observes its full content (comments included). The
//! arena ranges (`IdxRange`) and the backing arenas (`File`'s comment and
//! word vectors) are crate-internal on purpose: there is no public
//! trivia-traversal API in 0.1, so those internals can evolve without a
//! breaking change. Node types and spans are public for inspection.

use crate::lex::Comment;
use crate::span::Span;

/// Node id: dense per-file index assigned by the parser. The typed
/// counterpart of "index into the per-file `NodeId -> SymId` array"; not
/// interchangeable with the other id kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct NodeId(pub(crate) u32);

impl NodeId {
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A range into one of the file-owned arenas.
///
/// `len` is a full `u32`: a `u16` would be free space-wise (alignment
/// padding) but silently truncates in release for degenerate inputs such
/// as 65,536 adjacent string literals — a correctness cliff, not a
/// worthwhile trade.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdxRange {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

impl IdxRange {
    pub(crate) const EMPTY: Self = Self { start: 0, len: 0 };

    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the single construction point for arena ranges; arena \
                  indices (and hence lengths) fit u32 because parse() \
                  rejects inputs over 4 GiB at the boundary"
    )]
    pub(crate) const fn new(start: usize, len: usize) -> Self {
        Self {
            start: start as u32,
            len: len as u32,
        }
    }

    #[must_use]
    pub(crate) const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub(crate) fn slice<T>(self, arena: &[T]) -> &[T] {
        &arena[self.start as usize..self.start as usize + self.len as usize]
    }
}

/// A source word: identifier, number, or string literal — verbatim slice
/// plus span.
#[derive(Debug, Clone, Copy)]
pub struct Word<'a> {
    /// Verbatim source text of the word.
    pub text: &'a str,
    /// Byte span of the word in the source.
    pub span: Span,
}

/// Per-node bookkeeping shared by all item-like nodes. Comment attachment
/// is stored as ranges into the file's comment stream:
/// - `leading`: block directly above, contiguous with the node;
/// - `trailing`: same-line comments after the node ends;
/// - `detached`: blocks below the node separated from the *next* node by a
///   blank line — they belong to this node, not the next.
#[derive(Debug)]
pub struct Meta {
    /// Contract: unique within the file and `< File::node_count` — the
    /// dense per-node arrays in `sema` index by it. Crate-internal so
    /// nothing outside parsing can break that.
    pub(crate) id: NodeId,
    /// Code span of the node (excluding attached comments; comments carry
    /// their own spans).
    pub span: Span,
    /// A blank line separated this node (or its first leading comment) from
    /// what precedes it.
    pub blank_before: bool,
    /// Leading comment block, as a range into the file's comment stream.
    pub leading: IdxRange,
    /// Same-line trailing comments, as a range into the file's comment
    /// stream.
    pub trailing: IdxRange,
    /// Detached comment blocks below the node, as a range into the file's
    /// comment stream.
    pub detached: IdxRange,
}

impl Meta {
    #[must_use]
    pub(crate) const fn has_comments(&self) -> bool {
        !(self.leading.is_empty() && self.trailing.is_empty() && self.detached.is_empty())
    }
}

pub(crate) trait HasMeta {
    fn meta(&self) -> &Meta;
    fn meta_mut(&mut self) -> &mut Meta;
}

macro_rules! impl_has_meta {
    ($($t:ident),* $(,)?) => {
        $(impl<'a> HasMeta for $t<'a> {
            fn meta(&self) -> &Meta { &self.meta }
            fn meta_mut(&mut self) -> &mut Meta { &mut self.meta }
        })*
    };
}

macro_rules! impl_has_meta_enum {
    ($t:ident { $($v:ident),* $(,)? }) => {
        impl<'a> HasMeta for $t<'a> {
            fn meta(&self) -> &Meta {
                match self { $($t::$v(x) => x.meta(),)* }
            }
            fn meta_mut(&mut self) -> &mut Meta {
                match self { $($t::$v(x) => x.meta_mut(),)* }
            }
        }
    };
}

/// The contents of a braced scope (or of the file itself). `intro` are
/// comments at the start of the scope leading no item; `dangling` are
/// comments after the last item.
#[derive(Debug)]
pub struct Block<T> {
    /// Comments at the start of the scope leading no item.
    pub intro: IdxRange,
    /// The items of the scope, in source order.
    pub items: Vec<T>,
    /// Comments after the last item.
    pub dangling: IdxRange,
}

impl<T> Block<T> {
    #[must_use]
    pub(crate) const fn is_empty(&self) -> bool {
        self.intro.is_empty() && self.items.is_empty() && self.dangling.is_empty()
    }
}

/// A parsed proto file: the root of the lossless CST.
#[derive(Debug)]
pub struct File<'a> {
    /// The full source text the file was parsed from.
    pub src: &'a str,
    /// The file's full comment stream, in source order. Nodes attach
    /// comments as ranges into it; crate-internal so the ranges can never
    /// dangle under an external `mem::take`.
    pub(crate) comments: Vec<Comment<'a>>,
    /// Word arena: path segments (`Path::segs`) and string-literal parts
    /// (`StrLit::parts`) point here. Crate-internal for the same
    /// dangling-range reason as `comments`.
    pub(crate) segs: Vec<Word<'a>>,
    /// The file's top-level items.
    pub top: Block<Item<'a>>,
    /// Number of `NodeId`s allocated while parsing this file. Contract:
    /// every `Meta::id` in the file is below it.
    pub(crate) node_count: u32,
}

/// A top-level item of a proto file.
#[derive(Debug)]
#[non_exhaustive]
pub enum Item<'a> {
    /// A `syntax = "...";` declaration.
    Syntax(Syntax<'a>),
    /// A `package ...;` declaration.
    Package(Package),
    /// An `import ...;` statement.
    Import(Import<'a>),
    /// A file-level `option ...;` statement.
    Option(OptionStmt<'a>),
    /// A `message` definition.
    Message(Message<'a>),
    /// An `enum` definition.
    Enum(Enum<'a>),
    /// A `service` definition.
    Service(Service<'a>),
}

impl_has_meta_enum!(Item {
    Syntax,
    Package,
    Import,
    Option,
    Message,
    Enum,
    Service
});

/// A `syntax = "...";` declaration.
#[derive(Debug)]
pub struct Syntax<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// Raw string literal including quotes, e.g. `"proto3"`.
    pub value: Word<'a>,
}

/// A `package ...;` declaration.
#[derive(Debug)]
pub struct Package {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The declared package path.
    pub path: Path,
}

impl HasMeta for Package {
    fn meta(&self) -> &Meta {
        &self.meta
    }
    fn meta_mut(&mut self) -> &mut Meta {
        &mut self.meta
    }
}

/// A dotted name, optionally rooted with a leading `.`. Segments live in
/// the file's segment arena.
#[derive(Debug, Clone, Copy)]
pub struct Path {
    /// True if the path is written with a leading `.` (fully qualified).
    pub leading_dot: bool,
    /// The path's segments: a range into the file's word arena.
    pub segs: IdxRange,
    /// Byte span of the whole path.
    pub span: Span,
}

/// The modifier of an `import` statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImportKind {
    /// Unmodified `import`.
    Plain,
    /// `import public`.
    Public,
    /// `import weak`.
    Weak,
}

/// An `import ...;` statement.
#[derive(Debug)]
pub struct Import<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The import modifier: plain, `public`, or `weak`.
    pub kind: ImportKind,
    /// Raw string literal including quotes.
    pub path: Word<'a>,
    /// The import path with the quotes stripped — established once at
    /// parse time instead of re-sliced by every consumer.
    pub(crate) path_inner: &'a str,
}

/// An `option name = value;` statement.
#[derive(Debug)]
pub struct OptionStmt<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The option name.
    pub name: OptionName<'a>,
    /// The option value.
    pub value: Constant<'a>,
}

/// An option name: `ident`, `(full.ident)`, or a `.`-joined sequence.
/// The first part is inline — nearly every option name has exactly one
/// part, and the inline layout makes that case allocation-free.
#[derive(Debug, Clone)]
pub struct OptionName<'a> {
    /// The first (and usually only) part of the name.
    pub first: OptNamePart<'a>,
    /// `.`-joined continuation parts; empty for the common one-part name.
    pub rest: Vec<OptNamePart<'a>>,
    /// Byte span of the whole name.
    pub span: Span,
}

/// One part of an option name.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OptNamePart<'a> {
    /// Plain identifier part.
    Ident(Word<'a>),
    /// `(full.ident)` extension part; span covers the parentheses.
    Ext(Path, Span),
}

/// The sign written before a numeric constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Sign {
    /// No sign written.
    None,
    /// `+`
    Pos,
    /// `-`
    Neg,
}

/// An option value constant.
#[derive(Debug)]
pub struct Constant<'a> {
    /// Byte span of the constant.
    pub span: Span,
    /// What kind of constant this is.
    pub kind: ConstKind<'a>,
}

/// The kinds of option value constants.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConstKind<'a> {
    /// Bare identifier path: `true`, `false`, enum constants, `SPEED`, …
    Path(Path),
    /// Numeric literal, optionally signed; also `inf`/`nan` after a sign.
    Num {
        /// The written sign, if any.
        sign: Sign,
        /// The numeric word as written.
        word: Word<'a>,
    },
    /// One or more adjacent string literals (implicit concatenation).
    Str(StrLit),
    /// Text-format aggregate `{ ... }`; boxed to keep `Constant` small
    /// (aggregates are rare, `Constant` is embedded in every option).
    Aggregate(Box<TextMsg<'a>>),
}

/// One or more adjacent string literals (implicit concatenation).
///
/// The literal words live in the file's word arena (`File::segs`), so a
/// string constant costs no allocation of its own; `parts` is never empty.
#[derive(Debug, Clone, Copy)]
pub struct StrLit {
    /// Raw literals including quotes, in source order: a range into the
    /// file's word arena.
    pub parts: IdxRange,
    /// Byte span covering all parts.
    pub span: Span,
}

/// Text-format message value. Fields are items in a scope so comments
/// between them attach exactly like elsewhere.
#[derive(Debug)]
pub struct TextMsg<'a> {
    /// Byte span of the aggregate including the braces.
    pub span: Span,
    /// The fields of the aggregate.
    pub body: Block<TextField<'a>>,
    /// True if any comment is attached anywhere inside this aggregate —
    /// computed once at parse time (nested aggregates contribute their own
    /// precomputed bit), so the formatter's single-line/multi-line choice
    /// never re-walks the tree.
    pub(crate) has_comments: bool,
}

/// A field inside a text-format aggregate.
#[derive(Debug)]
pub struct TextField<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The field name.
    pub name: TextName<'a>,
    /// The field value.
    pub value: TextVal<'a>,
}

/// A text-format field name.
#[derive(Debug)]
#[non_exhaustive]
pub enum TextName<'a> {
    /// Plain identifier name.
    Ident(Word<'a>),
    /// `[type.googleapis.com/full.Name]` or `[full.ext.name]`; text is the
    /// bracket content with whitespace normalized away.
    Bracket {
        /// The bracket content with whitespace normalized away.
        text: String,
        /// Byte span including the brackets.
        span: Span,
    },
}

/// A text-format field value.
#[derive(Debug)]
#[non_exhaustive]
pub enum TextVal<'a> {
    /// Number, `true`/`false`, enum value, `inf`, `nan` — optionally signed.
    Scalar {
        /// The written sign, if any.
        sign: Sign,
        /// The scalar word as written.
        word: Word<'a>,
    },
    /// One or more adjacent string literals.
    Str(StrLit),
    /// Boxed: the message variant would otherwise double `TextVal`'s size.
    Msg(Box<TextMsg<'a>>),
    /// `[ ... ]` list value.
    List {
        /// The list elements, in source order.
        items: Vec<Self>,
        /// Byte span including the brackets.
        span: Span,
    },
}

/// A `message` definition.
#[derive(Debug)]
pub struct Message<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The message name.
    pub name: Word<'a>,
    /// The message body items.
    pub body: Block<MsgItem<'a>>,
}

/// An item inside a `message` body.
#[derive(Debug)]
#[non_exhaustive]
pub enum MsgItem<'a> {
    /// A field.
    Field(Field<'a>),
    /// A `oneof` group.
    Oneof(Oneof<'a>),
    /// A nested message.
    Message(Message<'a>),
    /// A nested enum.
    Enum(Enum<'a>),
    /// An `option ...;` statement.
    Option(OptionStmt<'a>),
    /// A `reserved` statement.
    Reserved(Reserved<'a>),
}

impl_has_meta_enum!(MsgItem {
    Field,
    Oneof,
    Message,
    Enum,
    Option,
    Reserved
});

/// The kind of a field label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LabelKind {
    /// `repeated`
    Repeated,
    /// `optional`
    Optional,
}

impl LabelKind {
    /// The label as written — derivable from the kind because the parser
    /// only accepts these two exact words.
    #[must_use]
    pub(crate) const fn text(self) -> &'static str {
        match self {
            Self::Repeated => "repeated",
            Self::Optional => "optional",
        }
    }
}

/// A field label. Carries no text: the word is derivable from `kind`
/// (see the crate-internal `LabelKind::text`), which keeps
/// `Option<Label>` at 12 bytes in
/// every `Field` instead of 32.
#[derive(Debug, Clone, Copy)]
pub struct Label {
    /// Which label was written.
    pub kind: LabelKind,
    /// Span of the label token, for diagnostics.
    pub span: Span,
}

/// A `map<key, value>` field type.
#[derive(Debug)]
pub struct MapType<'a> {
    /// The key type (always a scalar type name).
    pub key: Word<'a>,
    /// The value type.
    pub value: TypeRef<'a>,
    /// Byte span of the whole `map<...>` type.
    pub span: Span,
}

/// A field type reference.
#[derive(Debug)]
#[non_exhaustive]
pub enum TypeRef<'a> {
    /// Built-in scalar type name.
    Scalar(Word<'a>),
    /// Named message or enum type.
    Named(Path),
    /// Boxed: maps are rare and the inline key/value would dominate the
    /// enum's size.
    Map(Box<MapType<'a>>),
}

/// A single `name = value` entry in a bracketed field option list.
#[derive(Debug)]
pub struct FieldOpt<'a> {
    /// The option name.
    pub name: OptionName<'a>,
    /// The option value.
    pub value: Constant<'a>,
    /// Byte span of the `name = value` pair.
    pub span: Span,
}

/// A message (or oneof) field.
#[derive(Debug)]
pub struct Field<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The `repeated`/`optional` label, if written.
    pub label: Option<Label>,
    /// The field type.
    pub ty: TypeRef<'a>,
    /// The field name.
    pub name: Word<'a>,
    /// The field number as written.
    pub number: Word<'a>,
    /// The parsed field number.
    pub number_val: u64,
    /// Bracketed field options, in source order.
    pub options: Vec<FieldOpt<'a>>,
}

/// A `oneof` group.
#[derive(Debug)]
pub struct Oneof<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The oneof name.
    pub name: Word<'a>,
    /// The oneof body items.
    pub body: Block<OneofItem<'a>>,
}

/// An item inside a `oneof` body.
#[derive(Debug)]
#[non_exhaustive]
pub enum OneofItem<'a> {
    /// A field.
    Field(Field<'a>),
    /// An `option ...;` statement.
    Option(OptionStmt<'a>),
}

impl_has_meta_enum!(OneofItem { Field, Option });

/// An `enum` definition.
#[derive(Debug)]
pub struct Enum<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The enum name.
    pub name: Word<'a>,
    /// The enum body items.
    pub body: Block<EnumItem<'a>>,
}

/// An item inside an `enum` body.
#[derive(Debug)]
#[non_exhaustive]
pub enum EnumItem<'a> {
    /// An enum value.
    Value(EnumValue<'a>),
    /// An `option ...;` statement.
    Option(OptionStmt<'a>),
    /// A `reserved` statement.
    Reserved(Reserved<'a>),
}

impl_has_meta_enum!(EnumItem {
    Value,
    Option,
    Reserved
});

/// A single enum value definition.
#[derive(Debug)]
pub struct EnumValue<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The value name.
    pub name: Word<'a>,
    /// True if the number is written with a `-`.
    pub negative: bool,
    /// The value number as written (without the sign).
    pub number: Word<'a>,
    /// The parsed value number, sign applied.
    pub number_val: i64,
    /// Bracketed value options, in source order.
    pub options: Vec<FieldOpt<'a>>,
}

/// A `service` definition.
#[derive(Debug)]
pub struct Service<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The service name.
    pub name: Word<'a>,
    /// The service body items.
    pub body: Block<SvcItem<'a>>,
}

/// An item inside a `service` body.
#[derive(Debug)]
#[non_exhaustive]
pub enum SvcItem<'a> {
    /// An `rpc` method.
    Rpc(Rpc<'a>),
    /// An `option ...;` statement.
    Option(OptionStmt<'a>),
}

impl_has_meta_enum!(SvcItem { Rpc, Option });

/// An `rpc` method definition.
#[derive(Debug)]
pub struct Rpc<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// The method name.
    pub name: Word<'a>,
    /// True if the input type is marked `stream`.
    pub client_stream: bool,
    /// The input (request) type.
    pub input: Path,
    /// True if the output type is marked `stream`.
    pub server_stream: bool,
    /// The output (response) type.
    pub output: Path,
    /// `None` = declared with `;`. `Some` = braced body holding options.
    /// Boxed: bodies are rare and the inline block would dominate `Rpc`.
    pub body: Option<Box<Block<RpcItem<'a>>>>,
}

/// An item inside an `rpc` body.
#[derive(Debug)]
#[non_exhaustive]
pub enum RpcItem<'a> {
    /// An `option ...;` statement.
    Option(OptionStmt<'a>),
}

impl_has_meta_enum!(RpcItem { Option });

/// A `reserved` statement.
#[derive(Debug)]
pub struct Reserved<'a> {
    /// Node bookkeeping: span and comment attachment.
    pub meta: Meta,
    /// Whether the statement reserves ranges or names.
    pub kind: ReservedKind<'a>,
}

/// The payload of a `reserved` statement.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReservedKind<'a> {
    /// Reserved number ranges.
    Ranges(Vec<ResRange<'a>>),
    /// Raw string literals including quotes.
    Names(Vec<Word<'a>>),
}

/// A single reserved range: a number, or `start to end`.
#[derive(Debug)]
pub struct ResRange<'a> {
    /// Enum reserved ranges may be negative.
    pub start_neg: bool,
    /// The range start as written (without the sign).
    pub start: Word<'a>,
    /// The parsed range start, sign applied.
    pub start_val: i64,
    /// The range end, if written as `start to end`.
    pub end: Option<ResEnd<'a>>,
}

/// The end of a reserved range.
#[derive(Debug)]
#[non_exhaustive]
pub enum ResEnd<'a> {
    /// Numeric end.
    Num {
        /// True if written with a `-`.
        neg: bool,
        /// The end number as written (without the sign).
        word: Word<'a>,
        /// The parsed end value, sign applied.
        value: i64,
    },
    /// The keyword `max`.
    Max(Span),
}

impl_has_meta!(
    Syntax, Import, OptionStmt, Message, Enum, Service, Field, Oneof, EnumValue, Rpc, Reserved,
    TextField,
);
