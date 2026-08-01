//! Pruning: materializes a selection by deleting the complement.
//!
//! The output is the original text minus deleted spans — losslessness by
//! construction: options, comments, `reserved`, everything unaddressed is
//! preserved because it is never rebuilt. The single point of text
//! *insertion* in the whole pipeline is the `reserved` statement generated
//! for deleted field numbers (and, by the same wire-compatibility
//! invariant, deleted enum value numbers): numbers are never reused, never
//! renumbered.
//!
//! Cascade cleanup (all pure deletion):
//! - a definition's leading comments go with it;
//! - a oneof whose fields are all deleted goes entirely;
//! - a file whose definitions are all deleted goes entirely, along with
//!   `import` lines pointing at it;
//! - imports that no longer provide any referenced symbol are deleted
//!   (`import public` lines survive as long as their target file survives:
//!   they are visibility statements for downstream consumers, not
//!   dependencies of this file);
//! - well-known files never participate as inputs; only their import lines
//!   are kept, and only while still referenced.
//!
//! The pruned text is then run through the formatter — the pipeline's only
//! printer — so deletions never need manual whitespace surgery.

use crate::cst::{
    ConstKind, EnumItem, Item, Message, Meta, MsgItem, OneofItem, OptNamePart, OptionStmt, SvcItem,
    Word,
};
use crate::fileset::FileSet;
use crate::select::Selected;
use crate::sema::{FileId, Sema, SymId};
use crate::span::Span;
use rustc_hash::{FxHashMap, FxHashSet};

/// One surviving file of a prune.
pub struct PrunedFile {
    /// Import path, unchanged from the input set.
    pub path: String,
    /// The pruned text — raw (original minus deleted spans) until
    /// [`PruneOutput::format`] normalizes it.
    pub text: String,
}

/// Everything a prune produced.
pub struct PruneOutput {
    /// Surviving files in *raw* form (original text minus deleted spans,
    /// plus `reserved` insertions). Files with nothing kept are absent.
    /// Call [`PruneOutput::format`] to normalize layout.
    pub files: Vec<PrunedFile>,
    /// Paths of files that were dropped entirely.
    pub dropped: Vec<String>,
}

enum Edit {
    Del(Span),
    Ins(u32, String),
}

/// Materializes a selection: deletes everything unselected, reserves the
/// numbers of deleted fields, and cleans up imports and emptied files.
///
/// The output is the *raw* form — original text minus deleted spans, plus
/// the `reserved` insertions — losslessness by construction. Run it through
/// [`PruneOutput::format`] (or [`crate::format()`] per file) for normalized
/// layout; the design's formatter is the cleanup pass for deletion residue.
#[must_use]
pub fn prune(set: &FileSet<'_>, sema: &Sema<'_>, sel: &Selected) -> PruneOutput {
    let has_kept_defs: Vec<bool> = (0..set.files.len())
        .map(|fi| {
            sema.file_top[fi]
                .iter()
                .any(|&t| subtree_kept(sema, sel, t))
        })
        .collect();

    let needed = needed_providers(set, sema, sel);
    // Built once; import decisions and the bridge fixpoint share it.
    let path_to_idx: FxHashMap<&str, FileId> = set
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.path.as_str(), FileId::from_index(i)))
        .collect();
    let kept_files = required_files(set, sema, &path_to_idx, &has_kept_defs, &needed);

    let mut out = PruneOutput {
        files: Vec::new(),
        dropped: Vec::new(),
    };

    for (fi, f) in set.files.iter().enumerate() {
        if !kept_files[fi] {
            out.dropped.push(f.path.clone());
            continue;
        }
        let mut p = Pruner {
            sel,
            node_sym: &sema.node_sym[fi],
            cs: &f.cst.comments,
            segs: &f.cst.segs,
            edits: Vec::new(),
            reserved: Vec::new(),
        };

        for item in &f.cst.top.items {
            match item {
                Item::Message(m) => p.visit_message(m),
                Item::Enum(e) => p.visit_enum(e),
                Item::Service(s) => p.visit_service(s),
                Item::Import(imp) => {
                    let inner = imp.path_inner;
                    if !import_survives(
                        sema,
                        &path_to_idx,
                        &kept_files,
                        &needed[fi],
                        inner,
                        imp.kind,
                    ) {
                        p.delete_meta(&imp.meta);
                    }
                }
                Item::Syntax(_) | Item::Package(_) | Item::Option(_) => {}
            }
        }

        out.files.push(PrunedFile {
            path: f.path.clone(),
            text: apply_edits(f.src, p.edits),
        });
    }
    out
}

impl PruneOutput {
    /// Reformats every surviving file in place — the usual final step.
    ///
    /// # Panics
    ///
    /// Panics if a pruned text fails to re-parse — an internal invariant
    /// (pruning only ever removes well-formed regions), not an input
    /// condition.
    pub fn format(&mut self) {
        for f in &mut self.files {
            let cst = crate::parse(&f.text).unwrap_or_else(|e| {
                panic!(
                    "internal error: pruned output of {} does not parse:\n{e}\n--- text ---\n{}",
                    f.path, f.text
                )
            });
            f.text = crate::format(&cst);
        }
    }
}

/// Files that must survive pruning: those with kept definitions, plus
/// `import public` bridge files that still carry a needed provider to a
/// surviving importer.
///
/// A bridge is definition-free, so "has kept definitions" alone would drop
/// it and leave the importer's reference unresolvable. The fixpoint pushes
/// *demand* — the set of providers each file must keep reachable — from
/// definition-kept files into their imports, and onward along `import
/// public` chains, marking every file the demand passes through.
fn required_files<'a>(
    set: &FileSet<'a>,
    sema: &Sema<'a>,
    path_to_idx: &FxHashMap<&str, FileId>,
    has_kept_defs: &[bool],
    needed: &[Needed<'a>],
) -> Vec<bool> {
    // Pushes the part of `demand` that `t` can provide into `t`.
    fn push_demand<'a>(
        sema: &Sema<'a>,
        t: FileId,
        demand: &Needed<'a>,
        required: &mut [bool],
        flow: &mut [Needed<'a>],
        queue: &mut Vec<usize>,
    ) {
        let (pf, pw) = &sema.provides[t.index()];
        let mut grew = false;
        for f in demand.0.intersection(pf) {
            grew |= flow[t.index()].0.insert(*f);
        }
        for w in demand.1.intersection(pw) {
            grew |= flow[t.index()].1.insert(*w);
        }
        if grew {
            required[t.index()] = true;
            queue.push(t.index());
        }
    }

    let n = set.files.len();
    let mut required = has_kept_defs.to_vec();
    // flow[t]: providers that must stay reachable through t's public chain.
    let mut flow: Vec<Needed<'a>> = vec![(FxHashSet::default(), FxHashSet::default()); n];
    let mut queue: Vec<usize> = Vec::new();

    // Seed: the import lines written in definition-kept files.
    for (fi, f) in set.files.iter().enumerate() {
        if !has_kept_defs[fi] {
            continue;
        }
        for item in &f.cst.top.items {
            let Item::Import(imp) = item else { continue };
            let Some(&t) = path_to_idx.get(imp.path_inner) else {
                continue; // well-known import: no file to keep
            };
            push_demand(sema, t, &needed[fi], &mut required, &mut flow, &mut queue);
        }
    }

    // Propagate along `import public` chains. Flow sets only grow, so the
    // loop reaches a fixpoint.
    while let Some(fi) = queue.pop() {
        let demand = flow[fi].clone(); // small; freed each round
        for item in &set.files[fi].cst.top.items {
            let Item::Import(imp) = item else { continue };
            if imp.kind != crate::cst::ImportKind::Public {
                continue;
            }
            let Some(&t) = path_to_idx.get(imp.path_inner) else {
                continue;
            };
            if t.index() == fi {
                continue; // degenerate self-import
            }
            push_demand(sema, t, &demand, &mut required, &mut flow, &mut queue);
        }
    }
    required
}

fn subtree_kept(sema: &Sema<'_>, sel: &Selected, s: SymId) -> bool {
    sel.is_kept(s) || sema.children(s).iter().any(|&c| subtree_kept(sema, sel, c))
}

/// Provider sets still referenced from kept symbols, per file:
/// input file indices and well-known paths.
type Needed<'a> = (FxHashSet<FileId>, FxHashSet<&'a str>);

fn needed_providers<'a>(
    set: &FileSet<'a>,
    sema: &Sema<'a>,
    sel: &Selected,
) -> Vec<Needed<'static>> {
    let mut needed: Vec<Needed> =
        vec![(FxHashSet::default(), FxHashSet::default()); set.files.len()];
    for (sid, sym) in sema.syms() {
        let Some(sf) = sym.file else { continue };
        if !sel.is_kept(sid) {
            continue;
        }
        for r in sema.refs(sid) {
            let t = sema.sym(r.target);
            match t.file {
                None => {
                    needed[sf.index()].1.insert(t.wkt_path.unwrap());
                }
                Some(tf) if tf != sf => {
                    needed[sf.index()].0.insert(tf);
                }
                Some(_) => {}
            }
        }
    }
    needed
}

/// Decides whether an import line survives pruning.
fn import_survives(
    sema: &Sema<'_>,
    path_to_idx: &FxHashMap<&str, FileId>,
    kept_files: &[bool],
    needed: &Needed<'_>,
    path: &str,
    kind: crate::cst::ImportKind,
) -> bool {
    if let Some(&t) = path_to_idx.get(path) {
        if !kept_files[t.index()] {
            return false;
        }
        if kind == crate::cst::ImportKind::Public {
            // Visibility statement for downstream consumers; keep while the
            // target survives.
            return true;
        }
        // Needed if the import (or its public chain) provides a referenced
        // file or well-known path. `Sema` computed the provide sets during
        // import checking; no graph walk happens here.
        let (files, wkts) = &sema.provides[t.index()];
        files.iter().any(|f| needed.0.contains(f)) || wkts.iter().any(|w| needed.1.contains(w))
    } else {
        // Well-known import: kept while still referenced.
        kind == crate::cst::ImportKind::Public || needed.1.contains(path)
    }
}

struct Pruner<'r, 'a> {
    sel: &'r Selected,
    node_sym: &'r [Option<SymId>],
    /// The file's comment stream, for resolving attachment ranges.
    cs: &'r [crate::lex::Comment<'a>],
    /// The file's word arena, for resolving option-name paths.
    segs: &'r [Word<'a>],
    edits: Vec<Edit>,
    /// Scratch stack of deleted numbers, shared across the whole visit:
    /// each scope works on the tail above its base mark and truncates back,
    /// so nesting needs no per-scope vector. Field numbers fit i64 (the
    /// parser bounds them to 2^29 - 1); enum values are i64 natively.
    reserved: Vec<i64>,
}

impl Pruner<'_, '_> {
    fn kept_node(&self, node: crate::cst::NodeId) -> bool {
        // Nodes without symbols (options, reserved, …) follow their parent,
        // which decided before asking.
        self.node_sym[node.index()].is_none_or(|s| self.sel.is_kept(s))
    }

    /// Deletes a node together with its attached comments.
    fn delete_meta(&mut self, meta: &Meta) {
        let mut start = meta.span.start;
        if let Some(first) = meta.leading.slice(self.cs).first() {
            start = start.min(first.span.start);
        }
        let mut end = meta.span.end;
        if let Some(last) = meta.trailing.slice(self.cs).last() {
            end = end.max(last.span.end);
        }
        self.edits.push(Edit::Del(Span { start, end }));
        for c in meta.detached.slice(self.cs) {
            self.edits.push(Edit::Del(c.span));
        }
    }

    fn visit_message(&mut self, m: &Message<'_>) {
        if !self.kept_node(m.meta.id) {
            self.delete_meta(&m.meta);
            return;
        }
        let base = self.reserved.len();
        for item in &m.body.items {
            match item {
                MsgItem::Field(f) => {
                    if !self.kept_node(f.meta.id) {
                        self.delete_meta(&f.meta);
                        self.reserved.push(f.number_val.cast_signed());
                    }
                }
                MsgItem::Oneof(o) => self.visit_oneof(o),
                MsgItem::Message(nested) => self.visit_message(nested),
                MsgItem::Enum(e) => self.visit_enum(e),
                MsgItem::Option(_) | MsgItem::Reserved(_) => {}
            }
        }
        self.insert_reserved(&m.meta, base);
    }

    /// Counts dropped fields in one pass; no intermediate field vectors.
    fn visit_oneof(&mut self, o: &crate::cst::Oneof<'_>) {
        let mut fields = 0usize;
        let mut dropped = 0usize;
        for it in &o.body.items {
            if let OneofItem::Field(f) = it {
                fields += 1;
                if !self.kept_node(f.meta.id) {
                    dropped += 1;
                }
            }
        }
        if fields > 0 && dropped == fields {
            // Nothing left inside: the whole oneof goes, all numbers
            // reserved. (A degenerate option-only oneof stays: pruning was
            // not asked to touch it.)
            self.delete_meta(&o.meta);
            for it in &o.body.items {
                if let OneofItem::Field(f) = it {
                    self.reserved.push(f.number_val.cast_signed());
                }
            }
        } else {
            for it in &o.body.items {
                if let OneofItem::Field(f) = it
                    && !self.kept_node(f.meta.id)
                {
                    self.delete_meta(&f.meta);
                    self.reserved.push(f.number_val.cast_signed());
                }
            }
        }
    }

    fn visit_enum(&mut self, e: &crate::cst::Enum<'_>) {
        if !self.kept_node(e.meta.id) {
            self.delete_meta(&e.meta);
            return;
        }
        let base = self.reserved.len();
        let mut deleted_any = false;
        for item in &e.body.items {
            if let EnumItem::Value(v) = item
                && !self.kept_node(v.meta.id)
            {
                self.delete_meta(&v.meta);
                self.reserved.push(v.number_val);
                deleted_any = true;
            }
        }
        if deleted_any && !self.kept_values_alias(e) {
            // Deletions removed the last alias pair: `allow_alias = true`
            // with no aliases is illegal proto, so the now-ineffective
            // option goes too (a pure deletion, like everything else here).
            for item in &e.body.items {
                if let EnumItem::Option(o) = item
                    && is_allow_alias_true(o, self.segs)
                {
                    self.delete_meta(&o.meta);
                }
            }
        }
        if self.reserved.len() > base {
            // Enum aliases (`allow_alias`) share numbers: reserving a
            // number still used by a surviving value would make the output
            // illegal, and two dropped aliases must reserve their shared
            // number once. Keep a dropped number only if it is neither a
            // duplicate nor alive on a kept value.
            self.reserved[base..].sort_unstable();
            let mut w = base;
            for i in base..self.reserved.len() {
                let nv = self.reserved[i];
                if w > base && self.reserved[w - 1] == nv {
                    continue;
                }
                let alive = e.body.items.iter().any(|it| {
                    matches!(it, EnumItem::Value(v)
                        if v.number_val == nv && self.kept_node(v.meta.id))
                });
                if !alive {
                    self.reserved[w] = nv;
                    w += 1;
                }
            }
            self.reserved.truncate(w);
        }
        self.insert_reserved(&e.meta, base);
    }

    fn visit_service(&mut self, s: &crate::cst::Service<'_>) {
        if !self.kept_node(s.meta.id) {
            self.delete_meta(&s.meta);
            return;
        }
        for item in &s.body.items {
            if let SvcItem::Rpc(r) = item
                && !self.kept_node(r.meta.id)
            {
                self.delete_meta(&r.meta);
            }
        }
    }

    /// True if two *kept* values of the enum share a number (aliasing is
    /// still in use). O(values²), but enums are small.
    fn kept_values_alias(&self, e: &crate::cst::Enum<'_>) -> bool {
        for (i, a) in e.body.items.iter().enumerate() {
            let EnumItem::Value(va) = a else { continue };
            if !self.kept_node(va.meta.id) {
                continue;
            }
            for b in &e.body.items[i + 1..] {
                let EnumItem::Value(vb) = b else { continue };
                if vb.number_val == va.number_val && self.kept_node(vb.meta.id) {
                    return true;
                }
            }
        }
        false
    }

    /// Emits a `reserved` insertion for the numbers this scope pushed
    /// above `base` (none = no edit), then hands the scratch tail back.
    fn insert_reserved(&mut self, meta: &Meta, base: usize) {
        use std::fmt::Write as _;
        if self.reserved.len() == base {
            return;
        }
        self.reserved[base..].sort_unstable();
        // One owned string per edit (the edit stores it); built directly,
        // no per-number strings or join.
        let mut text = String::with_capacity(16 + (self.reserved.len() - base) * 8);
        text.push_str("\nreserved ");
        for (i, n) in self.reserved[base..].iter().enumerate() {
            if i > 0 {
                text.push_str(", ");
            }
            let _ = write!(text, "{n}");
        }
        text.push_str(";\n");
        // Insert just before the closing `}`; the formatter normalizes the
        // layout afterwards.
        self.edits.push(Edit::Ins(meta.span.end - 1, text));
        self.reserved.truncate(base);
    }
}

/// Recognizes `option allow_alias = true;` (the exact spelling protoc
/// accepts for the aliasing switch: one bare name part, bare `true`).
fn is_allow_alias_true(o: &OptionStmt<'_>, segs: &[Word<'_>]) -> bool {
    if !o.name.rest.is_empty() {
        return false;
    }
    let OptNamePart::Ident(w) = &o.name.first else {
        return false;
    };
    if w.text != "allow_alias" {
        return false;
    }
    match &o.value.kind {
        ConstKind::Path(p) if !p.leading_dot => {
            let s = p.segs.slice(segs);
            s.len() == 1 && s[0].text == "true"
        }
        _ => false,
    }
}

fn apply_edits(src: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| match e {
        Edit::Del(s) => (s.start, 0u8),
        Edit::Ins(at, _) => (*at, 1u8),
    });
    let mut out = String::with_capacity(src.len());
    let mut pos = 0usize;
    for e in edits {
        match e {
            Edit::Del(s) => {
                let (a, b) = (s.start as usize, s.end as usize);
                if a >= pos {
                    out.push_str(&src[pos..a]);
                    pos = b;
                } else {
                    // Overlapping deletion (contained in a previous one).
                    pos = pos.max(b);
                }
            }
            Edit::Ins(at, text) => {
                let at = at as usize;
                if at >= pos {
                    out.push_str(&src[pos..at]);
                    pos = at;
                }
                out.push_str(&text);
            }
        }
    }
    out.push_str(&src[pos..]);
    out
}
