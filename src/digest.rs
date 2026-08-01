//! Semantic digest: a canonical, comment-free, layout-free rendering of a
//! file's semantic content.
//!
//! Two parses are semantically equal iff their digests are byte-equal —
//! this is the round-trip test's equality oracle.
//!
//! The renderers here are deliberately independent of the formatter's
//! (`src/format.rs`): the digest is the *check* on what the formatter
//! emits, and a shared rendering helper would let a bug pass both sides
//! unnoticed. Its canonical text does not need to look like formatted
//! proto — it only needs to be deterministic and semantically complete.

use crate::cst::{
    ConstKind, Constant, Enum, EnumItem, Field, FieldOpt, File, ImportKind, Item, LabelKind,
    Message, MsgItem, OneofItem, OptNamePart, OptionName, OptionStmt, Path, ResEnd, Reserved,
    ReservedKind, RpcItem, Service, Sign, StrLit, SvcItem, TextMsg, TextName, TextVal, TypeRef,
    Word,
};
use std::fmt::Write as _;

/// Renders the file's canonical semantic digest.
///
/// Byte-equal digests mean semantically equal files; nothing else about
/// the text (layout, comments) participates.
#[must_use]
pub fn digest(file: &File<'_>) -> String {
    let mut out = String::with_capacity(file.src.len() / 2 + 64);
    for item in &file.top.items {
        top_item(&mut out, item, &file.segs);
    }
    out
}

fn top_item(out: &mut String, item: &Item<'_>, segs: &[Word<'_>]) {
    match item {
        Item::Syntax(s) => {
            let _ = writeln!(out, "syntax {}", s.value.text);
        }
        Item::Package(p) => {
            out.push_str("package ");
            path(out, &p.path, segs);
            out.push('\n');
        }
        Item::Import(i) => {
            let kind = match i.kind {
                ImportKind::Plain => "",
                ImportKind::Public => "public ",
                ImportKind::Weak => "weak ",
            };
            let _ = writeln!(out, "import {kind}{}", i.path.text);
        }
        Item::Option(o) => option_stmt(out, o, segs),
        Item::Message(m) => message(out, m, segs),
        Item::Enum(e) => enum_def(out, e, segs),
        Item::Service(s) => service(out, s, segs),
    }
}

// ---- independent renderers ---------------------------------------------------

fn path(out: &mut String, p: &Path, segs: &[Word<'_>]) {
    if p.leading_dot {
        out.push('.');
    }
    let mut first = true;
    for w in p.segs.slice(segs) {
        if !first {
            out.push('.');
        }
        first = false;
        out.push_str(w.text);
    }
}

fn type_ref(out: &mut String, ty: &TypeRef<'_>, segs: &[Word<'_>]) {
    match ty {
        TypeRef::Scalar(w) => out.push_str(w.text),
        TypeRef::Named(p) => path(out, p, segs),
        TypeRef::Map(mt) => {
            out.push_str("map<");
            out.push_str(mt.key.text);
            out.push(',');
            type_ref(out, &mt.value, segs);
            out.push('>');
        }
    }
}

fn option_name(out: &mut String, name: &OptionName<'_>, segs: &[Word<'_>]) {
    option_name_part(out, &name.first, segs);
    for part in &name.rest {
        out.push('.');
        option_name_part(out, part, segs);
    }
}

fn option_name_part(out: &mut String, part: &OptNamePart<'_>, segs: &[Word<'_>]) {
    match part {
        OptNamePart::Ident(w) => out.push_str(w.text),
        OptNamePart::Ext(p, _) => {
            out.push('(');
            path(out, p, segs);
            out.push(')');
        }
    }
}

fn sign(out: &mut String, s: Sign) {
    match s {
        Sign::None => {}
        Sign::Pos => out.push('+'),
        Sign::Neg => out.push('-'),
    }
}

fn strlit(out: &mut String, s: &StrLit, segs: &[Word<'_>]) {
    let mut first = true;
    for w in s.parts.slice(segs) {
        if !first {
            out.push(' ');
        }
        first = false;
        out.push_str(w.text);
    }
}

fn constant(out: &mut String, c: &Constant<'_>, segs: &[Word<'_>]) {
    match &c.kind {
        ConstKind::Path(p) => path(out, p, segs),
        ConstKind::Num { sign: s, word } => {
            sign(out, *s);
            out.push_str(word.text);
        }
        ConstKind::Str(s) => strlit(out, s, segs),
        ConstKind::Aggregate(m) => text_msg(out, m, segs),
    }
}

fn text_msg(out: &mut String, m: &TextMsg<'_>, segs: &[Word<'_>]) {
    out.push('{');
    let mut first = true;
    for f in &m.body.items {
        if !first {
            out.push(';');
        }
        first = false;
        match &f.name {
            TextName::Ident(w) => out.push_str(w.text),
            TextName::Bracket { text, .. } => {
                out.push('[');
                out.push_str(text);
                out.push(']');
            }
        }
        out.push(':');
        text_val(out, &f.value, segs);
    }
    out.push('}');
}

fn text_val(out: &mut String, v: &TextVal<'_>, segs: &[Word<'_>]) {
    match v {
        TextVal::Scalar { sign: s, word } => {
            sign(out, *s);
            out.push_str(word.text);
        }
        TextVal::Str(s) => strlit(out, s, segs),
        TextVal::Msg(m) => text_msg(out, m, segs),
        TextVal::List { items, .. } => {
            out.push('[');
            let mut first = true;
            for item in items {
                if !first {
                    out.push(';');
                }
                first = false;
                text_val(out, item, segs);
            }
            out.push(']');
        }
    }
}

// ---- statements ----------------------------------------------------------------

fn option_stmt(out: &mut String, o: &OptionStmt<'_>, segs: &[Word<'_>]) {
    out.push_str("option ");
    option_name(out, &o.name, segs);
    out.push_str(" = ");
    constant(out, &o.value, segs);
    out.push('\n');
}

fn field_opts(out: &mut String, opts: &[FieldOpt<'_>], segs: &[Word<'_>]) {
    if opts.is_empty() {
        return;
    }
    out.push_str(" [");
    let mut first = true;
    for o in opts {
        if !first {
            out.push_str(", ");
        }
        first = false;
        option_name(out, &o.name, segs);
        out.push_str(" = ");
        constant(out, &o.value, segs);
    }
    out.push(']');
}

fn field(out: &mut String, f: &Field<'_>, segs: &[Word<'_>]) {
    let label = match f.label.map(|l| l.kind) {
        None => "",
        Some(LabelKind::Repeated) => "repeated ",
        Some(LabelKind::Optional) => "optional ",
    };
    out.push_str("field ");
    out.push_str(label);
    type_ref(out, &f.ty, segs);
    let _ = write!(out, " {} = {}", f.name.text, f.number_val);
    field_opts(out, &f.options, segs);
    out.push('\n');
}

fn reserved(out: &mut String, r: &Reserved<'_>) {
    out.push_str("reserved ");
    match &r.kind {
        ReservedKind::Ranges(ranges) => {
            let mut first = true;
            for range in ranges {
                if !first {
                    out.push(',');
                }
                first = false;
                let _ = write!(out, "{}", range.start_val);
                match &range.end {
                    None => {}
                    Some(ResEnd::Num { value, .. }) => {
                        let _ = write!(out, "-{value}");
                    }
                    Some(ResEnd::Max(_)) => out.push_str("-max"),
                }
            }
        }
        ReservedKind::Names(names) => {
            let mut first = true;
            for n in names {
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(n.text);
            }
        }
    }
    out.push('\n');
}

fn message(out: &mut String, m: &Message<'_>, segs: &[Word<'_>]) {
    let _ = writeln!(out, "message {} {{", m.name.text);
    for item in &m.body.items {
        match item {
            MsgItem::Field(f) => field(out, f, segs),
            MsgItem::Oneof(o) => {
                let _ = writeln!(out, "oneof {} {{", o.name.text);
                for it in &o.body.items {
                    match it {
                        OneofItem::Field(f) => field(out, f, segs),
                        OneofItem::Option(o) => option_stmt(out, o, segs),
                    }
                }
                let _ = writeln!(out, "}}");
            }
            MsgItem::Message(nested) => message(out, nested, segs),
            MsgItem::Enum(e) => enum_def(out, e, segs),
            MsgItem::Option(o) => option_stmt(out, o, segs),
            MsgItem::Reserved(r) => reserved(out, r),
        }
    }
    let _ = writeln!(out, "}}");
}

fn enum_def(out: &mut String, e: &Enum<'_>, segs: &[Word<'_>]) {
    let _ = writeln!(out, "enum {} {{", e.name.text);
    for item in &e.body.items {
        match item {
            EnumItem::Value(v) => {
                let _ = write!(out, "value {} = {}", v.name.text, v.number_val);
                field_opts(out, &v.options, segs);
                out.push('\n');
            }
            EnumItem::Option(o) => option_stmt(out, o, segs),
            EnumItem::Reserved(r) => reserved(out, r),
        }
    }
    let _ = writeln!(out, "}}");
}

fn service(out: &mut String, s: &Service<'_>, segs: &[Word<'_>]) {
    let _ = writeln!(out, "service {} {{", s.name.text);
    for item in &s.body.items {
        match item {
            SvcItem::Rpc(r) => {
                let cs = if r.client_stream { "stream " } else { "" };
                let ss = if r.server_stream { "stream " } else { "" };
                let _ = write!(out, "rpc {} ({cs}", r.name.text);
                path(out, &r.input, segs);
                let _ = write!(out, ") returns ({ss}");
                path(out, &r.output, segs);
                let _ = writeln!(out, ") {{");
                if let Some(body) = &r.body {
                    for it in &body.items {
                        match it {
                            RpcItem::Option(o) => option_stmt(out, o, segs),
                        }
                    }
                }
                let _ = writeln!(out, "}}");
            }
            SvcItem::Option(o) => option_stmt(out, o, segs),
        }
    }
    let _ = writeln!(out, "}}");
}
