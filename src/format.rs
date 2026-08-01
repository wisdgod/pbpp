//! The formatter: the pipeline's only printer.
//!
//! Locked-down properties (enforced by the round-trip tests):
//! 1. idempotence: `format(format(x)) == format(x)`;
//! 2. semantic preservation: `parse(format(x))` is semantically equal to
//!    `parse(x)` — everything the printer emits comes verbatim from the CST;
//! 3. stable comment attachment: comments are printed where the parser
//!    attached them, so a second parse attaches them identically.
//!
//! Definitions keep their source order; the printer never reorders.
//!
//! Performance: all hot renderers write into the output buffer directly
//! (no per-node `String`s), the buffer is pre-sized from the source length,
//! and comments/path segments are read from the file's arenas via ranges.

use crate::cst::{
    Block, ConstKind, Constant, Enum, EnumItem, Field, FieldOpt, File, HasMeta, IdxRange,
    ImportKind, Item, Message, MsgItem, OneofItem, OptNamePart, OptionName, OptionStmt, Path,
    ResEnd, Reserved, ReservedKind, Rpc, RpcItem, Service, Sign, StrLit, SvcItem, TextField,
    TextMsg, TextName, TextVal, TypeRef, Word,
};
use crate::lex::Comment;

/// 32 indent levels' worth of spaces; deeper nesting falls back to chunks.
const INDENT_TABLE: &str = "                                                                ";

/// Where the printer stands between lines — the explicit state that used
/// to be reverse-derived from the output buffer's tail (`is_empty()` /
/// `ends_with("\n\n")`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineState {
    /// Nothing emitted yet; separators are meaningless here.
    Start,
    /// A line just closed with `\n`.
    Closed,
    /// A closed line followed by exactly one blank line.
    AfterBlank,
}

/// Formats the file: the pipeline's one printer, idempotent and
/// semantics-preserving (see the module docs for the locked-down
/// properties).
#[must_use]
pub fn format(file: &File<'_>) -> String {
    let mut p = Printer {
        // Formatted output tracks source size closely.
        out: String::with_capacity(file.src.len() + file.src.len() / 8 + 64),
        indent: 0,
        line: LineState::Start,
        cs: &file.comments,
        segs: &file.segs,
    };
    write_items(&mut p, &file.top, &top_force, &mut write_top_item);
    if p.line == LineState::Start {
        // Grammar guarantees non-empty output (a file must at least hold
        // `syntax`), so this only defends the zero-items edge in tests.
        p.out.push('\n');
    }
    p.out
}

struct Printer<'x, 'a> {
    out: String,
    indent: usize,
    line: LineState,
    /// The file's comment stream; nodes attach ranges into it.
    cs: &'x [Comment<'a>],
    /// The file's word arena (path segments, string-literal parts).
    segs: &'x [Word<'a>],
}

impl Printer<'_, '_> {
    fn line_start(&mut self) {
        // Constant strings for the depths that cover essentially all proto
        // (file / message / field / nested message body): the compiler
        // inlines these as fixed-size stores, measurably faster than a
        // runtime-length copy from a shared table.
        match self.indent {
            0 => {}
            1 => self.out.push_str("  "),
            2 => self.out.push_str("    "),
            3 => self.out.push_str("      "),
            _ => {
                let mut n = self.indent * 2;
                while n > 0 {
                    let take = n.min(INDENT_TABLE.len());
                    self.out.push_str(&INDENT_TABLE[..take]);
                    n -= take;
                }
            }
        }
    }

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn nl(&mut self) {
        self.out.push('\n');
        self.line = LineState::Closed;
    }

    /// Ensures exactly one blank line before what comes next (no-op at the
    /// very start of the output).
    fn blank(&mut self) {
        if self.line == LineState::Closed {
            self.out.push('\n');
            self.line = LineState::AfterBlank;
        }
    }

    /// Runs `f` one indent level deeper; the pairing cannot be mismatched.
    fn indented(&mut self, f: impl FnOnce(&mut Self)) {
        self.indent += 1;
        f(self);
        self.indent -= 1;
    }

    fn comment_line(&mut self, c: &Comment<'_>) {
        self.line_start();
        self.push(c.text);
        self.nl();
    }
}

/// Blank-line policy at the top level: force separation whenever the item
/// category changes, and always around definitions.
const fn top_force(prev: &Item<'_>, cur: &Item<'_>) -> bool {
    const fn group(i: &Item<'_>) -> u8 {
        match i {
            Item::Syntax(_) => 0,
            Item::Package(_) => 1,
            Item::Import(_) => 2,
            Item::Option(_) => 3,
            Item::Message(_) | Item::Enum(_) | Item::Service(_) => 4,
        }
    }
    group(prev) != group(cur) || group(cur) == 4
}

const fn no_force<T>(_: &T, _: &T) -> bool {
    false
}

/// The uniform scope writer: intro comments, items with their leading /
/// trailing / detached comments and blank-line flags, then dangling
/// comments.
fn write_items<'x, 'a, T: HasMeta>(
    p: &mut Printer<'x, 'a>,
    blk: &Block<T>,
    force: &dyn Fn(&T, &T) -> bool,
    write: &mut dyn FnMut(&mut Printer<'x, 'a>, &T),
) {
    for (k, c) in blk.intro.slice(p.cs).iter().enumerate() {
        if k > 0 && c.blank_before {
            p.blank();
        }
        p.comment_line(c);
    }
    for (i, item) in blk.items.iter().enumerate() {
        let m = item.meta();
        let want_blank = if i == 0 {
            !blk.intro.is_empty()
        } else {
            m.blank_before || force(&blk.items[i - 1], item)
        };
        if want_blank {
            p.blank();
        }
        for c in m.leading.slice(p.cs) {
            p.comment_line(c);
        }
        write(p, item);
        append_trailing(p, m.trailing);
        for c in m.detached.slice(p.cs) {
            if c.blank_before {
                p.blank();
            }
            p.comment_line(c);
        }
    }
    for c in blk.dangling.slice(p.cs) {
        if c.blank_before {
            p.blank();
        }
        p.comment_line(c);
    }
}

/// Appends same-line trailing comments to the line just written: reopens
/// the closed line (pop the `\n`), appends, closes it again — the line
/// state is `Closed` on entry and `Closed` on exit.
fn append_trailing(p: &mut Printer<'_, '_>, trailing: IdxRange) {
    if trailing.is_empty() {
        return;
    }
    debug_assert!(p.line == LineState::Closed && p.out.ends_with('\n'));
    p.out.pop();
    for c in trailing.slice(p.cs) {
        p.out.push(' ');
        p.out.push_str(c.text);
    }
    p.out.push('\n');
}

// ---- top-level items --------------------------------------------------------

fn write_top_item(p: &mut Printer<'_, '_>, item: &Item<'_>) {
    match item {
        Item::Syntax(s) => {
            p.line_start();
            p.push("syntax = ");
            p.push(s.value.text);
            p.push(";");
            p.nl();
        }
        Item::Package(pkg) => {
            p.line_start();
            p.push("package ");
            push_path(&mut p.out, &pkg.path, p.segs);
            p.push(";");
            p.nl();
        }
        Item::Import(imp) => {
            p.line_start();
            p.push("import ");
            match imp.kind {
                ImportKind::Plain => {}
                ImportKind::Public => p.push("public "),
                ImportKind::Weak => p.push("weak "),
            }
            p.push(imp.path.text);
            p.push(";");
            p.nl();
        }
        Item::Option(o) => write_option_stmt(p, o),
        Item::Message(m) => write_message(p, m),
        Item::Enum(e) => write_enum(p, e),
        Item::Service(s) => write_service(p, s),
    }
}

// ---- definitions --------------------------------------------------------------

/// Writes `<header> {` … `}` (or `<header> {}` for an empty body), where
/// `header` writes the part before the brace.
fn write_braced_block<'x, 'a, T: HasMeta>(
    p: &mut Printer<'x, 'a>,
    header: &mut dyn FnMut(&mut Printer<'x, 'a>),
    blk: &Block<T>,
    write: &mut dyn FnMut(&mut Printer<'x, 'a>, &T),
) {
    p.line_start();
    header(p);
    if blk.is_empty() {
        p.push(" {}");
        p.nl();
        return;
    }
    p.push(" {");
    p.nl();
    p.indented(|p| write_items(p, blk, &no_force, write));
    p.line_start();
    p.push("}");
    p.nl();
}

fn write_message(p: &mut Printer<'_, '_>, m: &Message<'_>) {
    write_braced_block(
        p,
        &mut |p| {
            p.push("message ");
            p.push(m.name.text);
        },
        &m.body,
        &mut write_msg_item,
    );
}

fn write_msg_item(p: &mut Printer<'_, '_>, item: &MsgItem<'_>) {
    match item {
        MsgItem::Field(f) => write_field(p, f),
        MsgItem::Oneof(o) => {
            write_braced_block(
                p,
                &mut |p| {
                    p.push("oneof ");
                    p.push(o.name.text);
                },
                &o.body,
                &mut |p, it: &OneofItem<'_>| match it {
                    OneofItem::Field(f) => write_field(p, f),
                    OneofItem::Option(o) => write_option_stmt(p, o),
                },
            );
        }
        MsgItem::Message(m) => write_message(p, m),
        MsgItem::Enum(e) => write_enum(p, e),
        MsgItem::Option(o) => write_option_stmt(p, o),
        MsgItem::Reserved(r) => write_reserved(p, r),
    }
}

fn write_enum(p: &mut Printer<'_, '_>, e: &Enum<'_>) {
    write_braced_block(
        p,
        &mut |p| {
            p.push("enum ");
            p.push(e.name.text);
        },
        &e.body,
        &mut |p, it: &EnumItem<'_>| match it {
            EnumItem::Value(v) => {
                p.line_start();
                p.push(v.name.text);
                p.push(" = ");
                if v.negative {
                    p.push("-");
                }
                p.push(v.number.text);
                write_field_options(p, &v.options);
                p.push(";");
                p.nl();
            }
            EnumItem::Option(o) => write_option_stmt(p, o),
            EnumItem::Reserved(r) => write_reserved(p, r),
        },
    );
}

fn write_service(p: &mut Printer<'_, '_>, s: &Service<'_>) {
    write_braced_block(
        p,
        &mut |p| {
            p.push("service ");
            p.push(s.name.text);
        },
        &s.body,
        &mut |p, it: &SvcItem<'_>| match it {
            SvcItem::Rpc(r) => write_rpc(p, r),
            SvcItem::Option(o) => write_option_stmt(p, o),
        },
    );
}

fn push_rpc_header(out: &mut String, r: &Rpc<'_>, segs: &[Word<'_>]) {
    out.push_str("rpc ");
    out.push_str(r.name.text);
    out.push('(');
    if r.client_stream {
        out.push_str("stream ");
    }
    push_path(out, &r.input, segs);
    out.push_str(") returns (");
    if r.server_stream {
        out.push_str("stream ");
    }
    push_path(out, &r.output, segs);
    out.push(')');
}

fn write_rpc(p: &mut Printer<'_, '_>, r: &Rpc<'_>) {
    match &r.body {
        None => {
            p.line_start();
            push_rpc_header(&mut p.out, r, p.segs);
            p.push(";");
            p.nl();
        }
        Some(body) => {
            write_braced_block(
                p,
                &mut |p| push_rpc_header(&mut p.out, r, p.segs),
                body,
                &mut |p, it: &RpcItem<'_>| match it {
                    RpcItem::Option(o) => write_option_stmt(p, o),
                },
            );
        }
    }
}

fn write_field(p: &mut Printer<'_, '_>, f: &Field<'_>) {
    p.line_start();
    if let Some(label) = &f.label {
        p.push(label.kind.text());
        p.push(" ");
    }
    push_type(&mut p.out, &f.ty, p.segs);
    p.push(" ");
    p.push(f.name.text);
    p.push(" = ");
    p.push(f.number.text);
    write_field_options(p, &f.options);
    p.push(";");
    p.nl();
}

fn write_field_options(p: &mut Printer<'_, '_>, opts: &[FieldOpt<'_>]) {
    if opts.is_empty() {
        return;
    }
    let commented = opts.iter().any(|o| const_has_comments(&o.value));
    if !commented {
        p.push(" [");
        for (i, o) in opts.iter().enumerate() {
            if i > 0 {
                p.push(", ");
            }
            push_option_name(&mut p.out, &o.name, p.segs);
            p.push(" = ");
            push_const_inline(&mut p.out, &o.value, p.segs);
        }
        p.push("]");
        return;
    }
    // Aggregates carrying comments must go multi-line so the comments have
    // a line to live on.
    p.push(" [");
    p.nl();
    p.indented(|p| {
        for (i, o) in opts.iter().enumerate() {
            p.line_start();
            push_option_name(&mut p.out, &o.name, p.segs);
            p.push(" = ");
            write_const(p, &o.value);
            if i + 1 < opts.len() {
                p.push(",");
            }
            p.nl();
        }
    });
    p.line_start();
    p.push("]");
}

fn write_reserved(p: &mut Printer<'_, '_>, r: &Reserved<'_>) {
    p.line_start();
    p.push("reserved ");
    match &r.kind {
        ReservedKind::Ranges(ranges) => {
            for (i, range) in ranges.iter().enumerate() {
                if i > 0 {
                    p.push(", ");
                }
                if range.start_neg {
                    p.push("-");
                }
                p.push(range.start.text);
                match &range.end {
                    None => {}
                    Some(ResEnd::Num { neg, word, .. }) => {
                        p.push(" to ");
                        if *neg {
                            p.push("-");
                        }
                        p.push(word.text);
                    }
                    Some(ResEnd::Max(_)) => p.push(" to max"),
                }
            }
        }
        ReservedKind::Names(names) => {
            for (i, n) in names.iter().enumerate() {
                if i > 0 {
                    p.push(", ");
                }
                p.push(n.text);
            }
        }
    }
    p.push(";");
    p.nl();
}

// ---- options and constants --------------------------------------------------

fn write_option_stmt(p: &mut Printer<'_, '_>, o: &OptionStmt<'_>) {
    p.line_start();
    p.push("option ");
    push_option_name(&mut p.out, &o.name, p.segs);
    p.push(" = ");
    match &o.value.kind {
        // Aggregates in `option` statements always print multi-line: they
        // are the values that grow and get commented.
        ConstKind::Aggregate(m) if !m.body.is_empty() => {
            write_text_msg_block(p, m);
        }
        _ => push_const_inline(&mut p.out, &o.value, p.segs),
    }
    p.push(";");
    p.nl();
}

/// Writes `{` … `}` multi-line, starting at the current position (the line
/// is already open).
fn write_text_msg_block(p: &mut Printer<'_, '_>, m: &TextMsg<'_>) {
    if m.body.is_empty() {
        p.push("{}");
        return;
    }
    p.push("{");
    p.nl();
    p.indented(|p| write_items(p, &m.body, &no_force, &mut write_text_field));
    p.line_start();
    p.push("}");
}

fn write_text_field(p: &mut Printer<'_, '_>, f: &TextField<'_>) {
    p.line_start();
    push_text_name(&mut p.out, &f.name);
    match &f.value {
        TextVal::Msg(m) => {
            p.push(" ");
            write_text_msg_block(p, m);
        }
        TextVal::List { items, .. } if items.iter().any(text_val_has_comments) => {
            p.push(": [");
            p.nl();
            p.indented(|p| {
                for (i, v) in items.iter().enumerate() {
                    p.line_start();
                    match v {
                        TextVal::Msg(m) => write_text_msg_block(p, m),
                        other => push_text_val_inline(&mut p.out, other, p.segs),
                    }
                    if i + 1 < items.len() {
                        p.push(",");
                    }
                    p.nl();
                }
            });
            p.line_start();
            p.push("]");
        }
        v => {
            p.push(": ");
            push_text_val_inline(&mut p.out, v, p.segs);
        }
    }
    p.nl();
}

fn write_const(p: &mut Printer<'_, '_>, c: &Constant<'_>) {
    match &c.kind {
        ConstKind::Aggregate(m) if const_has_comments(c) => write_text_msg_block(p, m),
        _ => push_const_inline(&mut p.out, c, p.segs),
    }
}

// ---- inline (comment-free) renderers -----------------------------------------
//
// All renderers write straight into the output buffer; the semantic digest
// (`src/digest.rs`) deliberately does NOT share them — it is the oracle
// checking this module's output, so its rendering is written independently.

fn push_path(out: &mut String, path: &Path, segs: &[Word<'_>]) {
    if path.leading_dot {
        out.push('.');
    }
    for (i, seg) in path.segs.slice(segs).iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push_str(seg.text);
    }
}

fn push_type(out: &mut String, ty: &TypeRef<'_>, segs: &[Word<'_>]) {
    match ty {
        TypeRef::Scalar(w) => out.push_str(w.text),
        TypeRef::Named(p) => push_path(out, p, segs),
        TypeRef::Map(mt) => {
            out.push_str("map<");
            out.push_str(mt.key.text);
            out.push_str(", ");
            push_type(out, &mt.value, segs);
            out.push('>');
        }
    }
}

fn push_option_name(out: &mut String, name: &OptionName<'_>, segs: &[Word<'_>]) {
    push_option_name_part(out, &name.first, segs);
    for part in &name.rest {
        out.push('.');
        push_option_name_part(out, part, segs);
    }
}

fn push_option_name_part(out: &mut String, part: &OptNamePart<'_>, segs: &[Word<'_>]) {
    match part {
        OptNamePart::Ident(w) => out.push_str(w.text),
        OptNamePart::Ext(p, _) => {
            out.push('(');
            push_path(out, p, segs);
            out.push(')');
        }
    }
}

fn push_strlit(out: &mut String, s: &StrLit, segs: &[Word<'_>]) {
    for (i, w) in s.parts.slice(segs).iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(w.text);
    }
}

fn push_const_inline(out: &mut String, c: &Constant<'_>, segs: &[Word<'_>]) {
    match &c.kind {
        ConstKind::Path(p) => push_path(out, p, segs),
        ConstKind::Num { sign, word } => push_signed(out, *sign, word.text),
        ConstKind::Str(s) => push_strlit(out, s, segs),
        ConstKind::Aggregate(m) => push_text_msg_inline(out, m, segs),
    }
}

fn push_signed(out: &mut String, sign: Sign, text: &str) {
    match sign {
        Sign::None => {}
        Sign::Pos => out.push('+'),
        Sign::Neg => out.push('-'),
    }
    out.push_str(text);
}

fn push_text_name(out: &mut String, n: &TextName<'_>) {
    match n {
        TextName::Ident(w) => out.push_str(w.text),
        TextName::Bracket { text, .. } => {
            out.push('[');
            out.push_str(text);
            out.push(']');
        }
    }
}

fn push_text_msg_inline(out: &mut String, m: &TextMsg<'_>, segs: &[Word<'_>]) {
    if m.body.items.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push_str("{ ");
    for (i, f) in m.body.items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        push_text_name(out, &f.name);
        match &f.value {
            TextVal::Msg(inner) => {
                out.push(' ');
                push_text_msg_inline(out, inner, segs);
            }
            v => {
                out.push_str(": ");
                push_text_val_inline(out, v, segs);
            }
        }
    }
    out.push_str(" }");
}

fn push_text_val_inline(out: &mut String, v: &TextVal<'_>, segs: &[Word<'_>]) {
    match v {
        TextVal::Scalar { sign, word } => push_signed(out, *sign, word.text),
        TextVal::Str(s) => push_strlit(out, s, segs),
        TextVal::Msg(m) => push_text_msg_inline(out, m, segs),
        TextVal::List { items, .. } => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                push_text_val_inline(out, item, segs);
            }
            out.push(']');
        }
    }
}

// ---- comment detection ---------------------------------------------------------
//
// The parser computed the aggregate comment bit once (`TextMsg::
// has_comments`); these are constant-time reads, not tree walks.

const fn const_has_comments(c: &Constant<'_>) -> bool {
    match &c.kind {
        ConstKind::Aggregate(m) => m.has_comments,
        _ => false,
    }
}

fn text_val_has_comments(v: &TextVal<'_>) -> bool {
    match v {
        TextVal::Msg(m) => m.has_comments,
        TextVal::List { items, .. } => items.iter().any(text_val_has_comments),
        _ => false,
    }
}
