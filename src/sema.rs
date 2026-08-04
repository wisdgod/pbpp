//! Semantic layer over the CST: symbol table, import visibility, and type
//! reference resolution. Lives beside the CST, never replaces it.
//!
//! Strictness (from the design):
//! - every type reference must resolve, into the input set or the
//!   well-known set — unresolved is an error;
//! - every `import` must name an input file or a well-known file;
//! - a type may only be used where its defining file is visible (imported
//!   directly, or through an `import public` chain);
//! - the same fully-qualified path defined twice is an error.
//!
//! Resolution is protoc-faithful (`descriptor.cc`,
//! `LookupSymbolNoPlaceholder`): a namespace tree keyed by
//! `(parent node, segment)`, `.`-prefixed paths absolute, and relative
//! paths anchored on their first segment — the nearest enclosing scope
//! where that segment names a package or definition decides the rest of
//! the lookup (`Builder::resolve_path` has the full rule). Symbol paths
//! live in one
//! shared segment arena (`extend_from_within` from the parent's range), so
//! building and resolving symbols allocates no per-symbol strings.

use crate::cst::{EnumItem, Item, MsgItem, NodeId, OneofItem, Path, SvcItem, TypeRef};
use crate::error::Error;
use crate::fileset::FileSet;
use crate::span::Span;
use crate::wkt;
use core::num::NonZeroU32;
use rustc_hash::{FxHashMap, FxHashSet};

/// Symbol id: an index into the symbol table, stored +1 in a `NonZeroU32`
/// so `Option<SymId>` costs nothing extra in the dense per-symbol arrays
/// (`parent`, `node_sym`, selection's `introduced_by`).
///
/// Ids of the four kinds (`SymId`, `FileId`, `NsId`, `NodeId`) are distinct
/// types: mixing them up is a compile error, and the `usize` conversion
/// lives in exactly one place per type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct SymId(NonZeroU32);

impl SymId {
    /// Wraps a symbol-table index.
    ///
    /// # Panics
    ///
    /// Panics past `u32::MAX - 1`; unreachable for ids of live symbols
    /// because `analyze` bounds the symbol count at entry.
    pub(crate) fn from_index(i: usize) -> Self {
        let v = u32::try_from(i).expect("symbol count bounded at analyze entry");
        Self(NonZeroU32::new(v + 1).expect("v + 1 >= 1 and v < u32::MAX by the analyze bound"))
    }

    pub(crate) const fn index(self) -> usize {
        self.0.get() as usize - 1
    }
}

/// Input file id: an index into `FileSet::files`, +1 in a `NonZeroU32`
/// for the same niche reason as [`SymId`].
///
/// Well-known builtins have no file: `Symbol::file` is `None` for them
/// (the typed replacement of the old `u32::MAX` sentinel).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct FileId(NonZeroU32);

impl FileId {
    /// Wraps a file index.
    ///
    /// # Panics
    ///
    /// See [`SymId::from_index`]; every file holds at least one node, so
    /// the analyze bound covers file counts too.
    pub(crate) fn from_index(i: usize) -> Self {
        let v = u32::try_from(i).expect("file count bounded at analyze entry");
        Self(NonZeroU32::new(v + 1).expect("v + 1 >= 1 and v < u32::MAX by the analyze bound"))
    }

    pub(crate) const fn index(self) -> usize {
        self.0.get() as usize - 1
    }
}

/// Namespace tree node id; `NS_ROOT` is the root.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub(crate) struct NsId(u32);

pub(crate) const NS_ROOT: NsId = NsId(0);

impl NsId {
    /// Wraps a namespace-tree index; same bound argument as
    /// [`SymId::from_index`] (namespace nodes are at most one per symbol
    /// plus package segments, covered by the analyze bound).
    fn from_index(i: usize) -> Self {
        Self(u32::try_from(i).expect("namespace count bounded at analyze entry"))
    }

    const fn index(self) -> usize {
        self.0 as usize
    }
}

/// The kind of a symbol — every addressable node has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SymKind {
    /// A `message` definition.
    Message,
    /// An `enum` definition.
    Enum,
    /// A `service` definition.
    Service,
    /// A message (or oneof) field.
    Field,
    /// An enum value.
    EnumValue,
    /// An `rpc` method.
    Method,
}

impl SymKind {
    /// True for definition kinds (message, enum, service) — the nodes
    /// that own scopes and can be deleted whole.
    #[must_use]
    pub const fn is_def(self) -> bool {
        matches!(self, Self::Message | Self::Enum | Self::Service)
    }

    /// Human-readable kind name for diagnostics and reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Enum => "enum",
            Self::Service => "service",
            Self::Field => "field",
            Self::EnumValue => "enum value",
            Self::Method => "method",
        }
    }
}

/// An outgoing type reference (field type, rpc input/output).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ref {
    pub(crate) span: Span,
    pub(crate) target: SymId,
}

/// One symbol-table entry.
///
/// Children and outgoing references live in shared arenas on [`Sema`]
/// (CSR layout, addressed by [`Sema::children`] and the crate-internal
/// `Sema::refs`), not
/// in per-symbol vectors: symbols are the densest semantic allocation, and
/// the two inline `Vec`s used to cost up to two heap allocations per
/// symbol and half its footprint.
#[derive(Debug)]
pub struct Symbol {
    /// What the symbol is.
    pub kind: SymKind,
    /// Fully-qualified path segments: a range into `Sema::seg_arena`.
    /// `seg_len` is u32 for the same no-silent-truncation reason as
    /// `IdxRange::len` (padding absorbs the widening).
    seg_start: u32,
    seg_len: u32,
    /// The symbol's node in the namespace tree.
    ns: NsId,
    /// Defining input file; `None` for well-known builtins (which carry
    /// `wkt_path` instead).
    pub file: Option<FileId>,
    /// The enclosing definition, if any (fields/values/methods and nested
    /// definitions have one; top-level definitions do not).
    pub parent: Option<SymId>,
    /// Span of the name token, for diagnostics.
    pub name_span: Span,
    /// Field number / enum value number, where applicable.
    pub number: Option<i64>,
    /// For well-known types: the file that provides them.
    pub wkt_path: Option<&'static str>,
}

/// A namespace tree node: package segments and symbols share it.
#[derive(Debug)]
struct NsNode<'a> {
    parent: NsId,
    name: &'a str,
    /// The symbol defined at this exact path, if any (package prefixes
    /// have none).
    sym: Option<SymId>,
}

/// The semantic layer over a [`FileSet`]: symbol table, namespace tree,
/// resolved references, and import visibility. Built by [`analyze`].
#[derive(Debug)]
pub struct Sema<'a> {
    pub(crate) symbols: Vec<Symbol>,
    seg_arena: Vec<&'a str>,
    ns_nodes: Vec<NsNode<'a>>,
    ns_index: FxHashMap<(NsId, &'a str), NsId>,
    /// Top-level definition symbols of each file, in source order.
    pub file_top: Vec<Vec<SymId>>,
    /// Dense `NodeId -> SymId` map per file (the design's "parallel array
    /// indexed by node index").
    pub(crate) node_sym: Vec<Vec<Option<SymId>>>,
    /// Children arena, CSR layout: symbol `i`'s children (source order)
    /// are `children[children_off[i]..children_off[i+1]]`.
    children: Vec<SymId>,
    children_off: Vec<u32>,
    /// Reference arena, CSR layout like `children`.
    refs: Vec<Ref>,
    refs_off: Vec<u32>,
    /// What importing file `f` provides: `{f}` plus its transitive
    /// `import public` closure (input files, well-known paths). Computed
    /// once by import checking; pruning's import-survival test reads it
    /// instead of re-walking the public-import graph.
    pub(crate) provides: Vec<(FxHashSet<FileId>, FxHashSet<&'a str>)>,
}

impl<'a> Sema<'a> {
    /// The symbol with the given id.
    #[must_use]
    pub fn sym(&self, id: SymId) -> &Symbol {
        &self.symbols[id.index()]
    }

    /// Number of symbols; ids index a dense `0..sym_count()` space.
    #[must_use]
    pub(crate) const fn sym_count(&self) -> usize {
        self.symbols.len()
    }

    /// All symbols with their ids — the typed replacement for
    /// `enumerate()` + `as SymId`.
    pub(crate) fn syms(&self) -> impl Iterator<Item = (SymId, &Symbol)> {
        self.symbols
            .iter()
            .enumerate()
            .map(|(i, s)| (SymId::from_index(i), s))
    }

    /// Fully-qualified path segments of a symbol.
    #[must_use]
    pub fn segs(&self, id: SymId) -> &[&'a str] {
        let s = &self.symbols[id.index()];
        &self.seg_arena[s.seg_start as usize..s.seg_start as usize + s.seg_len as usize]
    }

    /// The symbol's direct children, in source order.
    #[must_use]
    pub fn children(&self, id: SymId) -> &[SymId] {
        let i = id.index();
        &self.children[self.children_off[i] as usize..self.children_off[i + 1] as usize]
    }

    /// The symbol's outgoing type references (field types, rpc signatures).
    #[must_use]
    pub(crate) fn refs(&self, id: SymId) -> &[Ref] {
        let i = id.index();
        &self.refs[self.refs_off[i] as usize..self.refs_off[i + 1] as usize]
    }

    /// Dotted fully-qualified name (allocates; diagnostics and reports only).
    #[must_use]
    pub fn fq(&self, id: SymId) -> String {
        self.segs(id).join(".")
    }

    /// Looks a symbol up by dotted fully-qualified name. Cold path (tests,
    /// tooling): scans the tree linearly instead of borrowing the index,
    /// whose keys are tied to the source lifetime.
    #[must_use]
    pub fn lookup_fq(&self, fq: &str) -> Option<SymId> {
        let mut ns = NS_ROOT;
        for seg in fq.split('.') {
            ns = self
                .ns_nodes
                .iter()
                .enumerate()
                .skip(1) // the root itself is not addressable
                .find(|(_, n)| n.parent == ns && n.name == seg)
                .map(|(i, _)| NsId::from_index(i))?;
        }
        self.ns_nodes[ns.index()].sym
    }

    /// Hot-path child lookup; probe names come from the CST and share its
    /// lifetime.
    fn ns_child(&self, parent: NsId, name: &'a str) -> Option<NsId> {
        self.ns_index.get(&(parent, name)).copied()
    }

    /// Descends `segs` from `base`; hits only if the final node is an
    /// actual symbol (package prefixes don't count as resolution targets).
    fn descend_syms(&self, base: NsId, segs: &[crate::cst::Word<'a>]) -> Option<SymId> {
        let mut ns = base;
        for w in segs {
            ns = self.ns_child(ns, w.text)?;
        }
        self.ns_nodes[ns.index()].sym
    }
}

/// A reference waiting for resolution. The scope it was written in is
/// derived from the owner symbol's parent (its enclosing definition), so
/// nothing is cloned per reference.
struct PendingRef<'c> {
    owner: SymId,
    file: FileId,
    path: &'c Path,
    /// True when the reference must resolve to a message (rpc in/out).
    message_only: bool,
}

/// Where new symbols get attached: a namespace node plus the segment-arena
/// range holding the scope's fully-qualified path.
#[derive(Clone, Copy)]
struct Scope {
    ns: NsId,
    seg_start: u32,
    seg_len: u32,
}

/// Builds the semantic layer for a file set: symbol table, import
/// visibility, and resolved type references.
///
/// # Errors
///
/// Duplicate fully-qualified definitions, imports naming no input or
/// well-known file, unresolved type references, references to types whose
/// defining file is not visible, fields typed by non-types, and rpc
/// signatures using non-message types.
pub fn analyze<'a>(set: &FileSet<'a>) -> Result<Sema<'a>, Error> {
    // Node counts bound the symbol count; pre-size everything so the walk
    // neither reallocates nor rehashes.
    let cap: usize = set
        .files
        .iter()
        .map(|f| f.cst.node_count as usize)
        .sum::<usize>()
        + wkt::TYPES.len();
    // The id boundary: symbol, file, and namespace counts are all bounded
    // by `cap` (every symbol/file/namespace node maps to at least one CST
    // node or builtin), so the +1-offset `NonZeroU32` ids below never
    // overflow once this single check passes.
    if cap >= u32::MAX as usize {
        return Err(Error::new(
            "input set too large: symbol ids are 32-bit (more than u32::MAX nodes)",
        ));
    }
    let mut b = Builder {
        set,
        sema: Sema {
            symbols: Vec::with_capacity(cap),
            seg_arena: Vec::with_capacity(cap * 4),
            ns_nodes: Vec::with_capacity(cap + 16),
            ns_index: FxHashMap::with_capacity_and_hasher(cap + 16, rustc_hash::FxBuildHasher),
            file_top: Vec::with_capacity(set.files.len()),
            node_sym: Vec::with_capacity(set.files.len()),
            children: Vec::new(),
            children_off: Vec::new(),
            refs: Vec::new(),
            refs_off: Vec::new(),
            provides: Vec::with_capacity(set.files.len()),
        },
        pending: Vec::with_capacity(cap / 4),
    };
    b.sema.ns_nodes.push(NsNode {
        parent: NS_ROOT,
        name: "",
        sym: None,
    });
    b.add_wkt_builtins();
    b.collect_symbols()?;
    b.build_children();
    let (visible_files, visible_wkt) = b.check_imports()?;
    b.resolve_refs(&visible_files, &visible_wkt)?;
    Ok(b.sema)
}

struct Builder<'s, 'a> {
    set: &'s FileSet<'a>,
    sema: Sema<'a>,
    pending: Vec<PendingRef<'s>>,
}

impl<'s, 'a> Builder<'s, 'a> {
    /// An error at a span inside one of the set's files.
    fn error_in(&self, file: FileId, msg: impl Into<String>, span: Span) -> Error {
        let f = &self.set.files[file.index()];
        Error::at(msg, span, f.src).with_file(&f.path)
    }

    /// Current end of the segment arena, as the `u32` symbol ranges store.
    ///
    /// # Errors
    ///
    /// Total segment count (path depth summed over all symbols) exceeding
    /// the u32 range — the one id-adjacent quantity the `analyze` entry
    /// bound does not cover.
    fn seg_mark(&self) -> Result<u32, Error> {
        u32::try_from(self.sema.seg_arena.len()).map_err(|_| {
            Error::new("input set too large: symbol path segments exceed the u32 range")
        })
    }

    /// Namespace node for `name` under `parent`, created if absent.
    fn ns_intern(&mut self, parent: NsId, name: &'a str) -> NsId {
        if let Some(&id) = self.sema.ns_index.get(&(parent, name)) {
            return id;
        }
        let id = NsId::from_index(self.sema.ns_nodes.len());
        self.sema.ns_nodes.push(NsNode {
            parent,
            name,
            sym: None,
        });
        self.sema.ns_index.insert((parent, name), id);
        id
    }

    fn add_wkt_builtins(&mut self) {
        let google = self.ns_intern(NS_ROOT, "google");
        let protobuf = self.ns_intern(google, "protobuf");
        for t in wkt::TYPES {
            let ns = self.ns_intern(protobuf, t.name);
            let id = SymId::from_index(self.sema.symbols.len());
            let seg_start = self
                .seg_mark()
                .expect("builtin segments are far below the u32 range");
            self.sema.seg_arena.extend(["google", "protobuf", t.name]);
            self.sema.ns_nodes[ns.index()].sym = Some(id);
            self.sema.symbols.push(Symbol {
                kind: if t.is_enum {
                    SymKind::Enum
                } else {
                    SymKind::Message
                },
                seg_start,
                seg_len: 3,
                ns,
                file: None,
                parent: None,
                name_span: Span::default(),
                number: None,
                wkt_path: Some(t.file),
            });
        }
    }

    /// Registers a symbol under `scope`. Its path segments are the scope's
    /// plus the name, appended to the shared arena without allocation.
    #[expect(
        clippy::too_many_arguments,
        reason = "the seven arguments are the symbol's definition, passed \
                  once from each walk_* site; a params struct would be \
                  built and destructured immediately"
    )]
    fn add_symbol(
        &mut self,
        file: FileId,
        kind: SymKind,
        scope: Scope,
        name: crate::cst::Word<'a>,
        node: NodeId,
        parent: Option<SymId>,
        number: Option<i64>,
    ) -> Result<(SymId, Scope), Error> {
        let ns = self.ns_intern(scope.ns, name.text);
        if let Some(prev) = self.sema.ns_nodes[ns.index()].sym {
            let prev_sym = self.sema.sym(prev);
            let fq = self.sema.fq(prev);
            let err = self.error_in(file, format!("duplicate definition of `{fq}`"), name.span);
            return Err(match prev_sym.file {
                None => err.note(format!(
                    "`{fq}` is a well-known type provided by `{}`",
                    prev_sym.wkt_path.unwrap()
                )),
                Some(pf) => {
                    let f = &self.set.files[pf.index()];
                    err.note_at_file("first defined here", prev_sym.name_span, f.src, &f.path)
                }
            });
        }

        let id = SymId::from_index(self.sema.symbols.len());
        let seg_start = self.seg_mark()?;
        self.sema.seg_arena.extend_from_within(
            scope.seg_start as usize..scope.seg_start as usize + scope.seg_len as usize,
        );
        self.sema.seg_arena.push(name.text);
        let seg_len = scope.seg_len + 1;

        self.sema.ns_nodes[ns.index()].sym = Some(id);
        self.sema.symbols.push(Symbol {
            kind,
            seg_start,
            seg_len,
            ns,
            file: Some(file),
            parent,
            name_span: name.span,
            number,
            wkt_path: None,
        });
        self.sema.node_sym[file.index()][node.index()] = Some(id);
        Ok((
            id,
            Scope {
                ns,
                seg_start,
                seg_len,
            },
        ))
    }

    fn collect_symbols(&mut self) -> Result<(), Error> {
        for (idx, f) in self.set.files.iter().enumerate() {
            let file = FileId::from_index(idx);
            self.sema
                .node_sym
                .push(vec![None; f.cst.node_count as usize]);
            self.sema.file_top.push(Vec::new());

            // Package scope: intern the chain, park the segments in the
            // arena once per file.
            let mut scope = Scope {
                ns: NS_ROOT,
                seg_start: self.seg_mark()?,
                seg_len: 0,
            };
            if let Some(pkg) = f.cst.top.items.iter().find_map(|it| match it {
                Item::Package(p) => Some(&p.path),
                _ => None,
            }) {
                for w in pkg.segs.slice(&f.cst.segs) {
                    scope.ns = self.ns_intern(scope.ns, w.text);
                    self.sema.seg_arena.push(w.text);
                    scope.seg_len += 1;
                }
            }

            for item in &f.cst.top.items {
                match item {
                    Item::Message(m) => {
                        let id = self.walk_message(file, m, scope, None)?;
                        self.sema.file_top[idx].push(id);
                    }
                    Item::Enum(e) => {
                        let id = self.walk_enum(file, e, scope, None)?;
                        self.sema.file_top[idx].push(id);
                    }
                    Item::Service(s) => {
                        let id = self.walk_service(file, s, scope)?;
                        self.sema.file_top[idx].push(id);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn walk_message(
        &mut self,
        file: FileId,
        m: &'s crate::cst::Message<'a>,
        scope: Scope,
        parent: Option<SymId>,
    ) -> Result<SymId, Error> {
        let (id, inner) = self.add_symbol(
            file,
            SymKind::Message,
            scope,
            m.name,
            m.meta.id,
            parent,
            None,
        )?;
        for item in &m.body.items {
            match item {
                MsgItem::Field(fld) => self.walk_field(file, fld, inner, id)?,
                MsgItem::Oneof(o) => {
                    for it in &o.body.items {
                        match it {
                            OneofItem::Field(fld) => self.walk_field(file, fld, inner, id)?,
                            OneofItem::Option(_) => {}
                        }
                    }
                }
                MsgItem::Message(nested) => {
                    self.walk_message(file, nested, inner, Some(id))?;
                }
                MsgItem::Enum(e) => {
                    self.walk_enum(file, e, inner, Some(id))?;
                }
                MsgItem::Option(_) | MsgItem::Reserved(_) => {}
            }
        }
        Ok(id)
    }

    fn walk_field(
        &mut self,
        file: FileId,
        fld: &'s crate::cst::Field<'a>,
        scope: Scope,
        parent: SymId,
    ) -> Result<(), Error> {
        let (id, _) = self.add_symbol(
            file,
            SymKind::Field,
            scope,
            fld.name,
            fld.meta.id,
            Some(parent),
            // Guarded by the parser: field numbers are at most 2^29 - 1.
            Some(fld.number_val.cast_signed()),
        )?;
        let ty = match &fld.ty {
            TypeRef::Map(mt) => &mt.value,
            other => other,
        };
        if let TypeRef::Named(path) = ty {
            self.pending.push(PendingRef {
                owner: id,
                file,
                path,
                message_only: false,
            });
        }
        Ok(())
    }

    fn walk_enum(
        &mut self,
        file: FileId,
        e: &'s crate::cst::Enum<'a>,
        scope: Scope,
        parent: Option<SymId>,
    ) -> Result<SymId, Error> {
        let (id, inner) =
            self.add_symbol(file, SymKind::Enum, scope, e.name, e.meta.id, parent, None)?;
        for item in &e.body.items {
            if let EnumItem::Value(v) = item {
                self.add_symbol(
                    file,
                    SymKind::EnumValue,
                    inner,
                    v.name,
                    v.meta.id,
                    Some(id),
                    Some(v.number_val),
                )?;
            }
        }
        Ok(id)
    }

    fn walk_service(
        &mut self,
        file: FileId,
        s: &'s crate::cst::Service<'a>,
        scope: Scope,
    ) -> Result<SymId, Error> {
        let (id, inner) =
            self.add_symbol(file, SymKind::Service, scope, s.name, s.meta.id, None, None)?;
        for item in &s.body.items {
            if let SvcItem::Rpc(r) = item {
                let (mid, _) = self.add_symbol(
                    file,
                    SymKind::Method,
                    inner,
                    r.name,
                    r.meta.id,
                    Some(id),
                    None,
                )?;
                for path in [&r.input, &r.output] {
                    self.pending.push(PendingRef {
                        owner: mid,
                        file,
                        path,
                        message_only: true,
                    });
                }
            }
        }
        Ok(id)
    }

    /// Builds the children arena (CSR over the symbol table): count per
    /// parent, prefix-sum, scatter. Three linear passes, two allocations —
    /// replacing one `Vec` per symbol.
    fn build_children(&mut self) {
        let n = self.sema.symbols.len();
        // counts[i + 1] = number of children of symbol i.
        let mut off = vec![0u32; n + 1];
        for s in &self.sema.symbols {
            if let Some(p) = s.parent {
                off[p.index() + 1] += 1;
            }
        }
        for i in 1..=n {
            off[i] += off[i - 1];
        }
        let total = off[n] as usize;
        // Placeholder id; every slot is written exactly once below (the
        // scatter writes one slot per counted child).
        let mut children = vec![SymId::from_index(0); total];
        let mut cursor: Vec<u32> = off.clone();
        for (sid, s) in self
            .sema
            .symbols
            .iter()
            .enumerate()
            .map(|(i, s)| (SymId::from_index(i), s))
        {
            if let Some(p) = s.parent {
                let c = &mut cursor[p.index()];
                children[*c as usize] = sid;
                *c += 1;
            }
        }
        // Symbols are created parents-first in source order, so each
        // parent's slice ends up in source order — the property the
        // report and selection traversals rely on.
        self.sema.children = children;
        self.sema.children_off = off;
    }

    /// Resolution scope of a pending reference: the owner's enclosing
    /// definition — the message for a field, the *service* for a method.
    /// protoc probes the service scope too (`relative_to` is the method's
    /// full name), so a sibling rpc's name shadows a message name in
    /// signatures; `rpc M(M) returns (M)` is protoc's "\"M\" is not a
    /// message type" error, not a lookup that skips past the service.
    fn start_ns(&self, p: &PendingRef<'s>) -> NsId {
        let owner = self.sema.sym(p.owner);
        self.sema
            .sym(owner.parent.expect("ref owner has a parent"))
            .ns
    }

    /// Checks that every import names an input file or a well-known file,
    /// computes per-file visibility (direct imports plus `import public`
    /// chains), and stores each file's provide set (`Sema::provides`) for
    /// pruning's import-survival test.
    #[expect(
        clippy::type_complexity,
        reason = "the two visibility sets are consumed once by resolve_refs \
                  and dropped; naming the pair would add a type for one \
                  call site"
    )]
    fn check_imports(
        &mut self,
    ) -> Result<(Vec<FxHashSet<FileId>>, Vec<FxHashSet<&'a str>>), Error> {
        // reach_pub(f) = {f} ∪ reach_pub(public imports of f), plus wkt paths.
        fn reach_pub<'a>(
            f: FileId,
            public: &[Vec<FileId>],
            public_wkt: &[Vec<&'a str>],
            files: &mut FxHashSet<FileId>,
            wkts: &mut FxHashSet<&'a str>,
        ) {
            if !files.insert(f) {
                return;
            }
            for w in &public_wkt[f.index()] {
                wkts.insert(w);
            }
            for &p in &public[f.index()] {
                reach_pub(p, public, public_wkt, files, wkts);
            }
        }

        let n = self.set.files.len();
        let path_to_idx: FxHashMap<&str, FileId> = self
            .set
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.path.as_str(), FileId::from_index(i)))
            .collect();

        // direct[f] / public[f]: imported input files; *_wkt: imported
        // well-known paths.
        let mut direct: Vec<Vec<FileId>> = vec![Vec::new(); n];
        let mut public: Vec<Vec<FileId>> = vec![Vec::new(); n];
        let mut direct_wkt: Vec<Vec<&'a str>> = vec![Vec::new(); n];
        let mut public_wkt: Vec<Vec<&'a str>> = vec![Vec::new(); n];

        for (idx, f) in self.set.files.iter().enumerate() {
            for item in &f.cst.top.items {
                let Item::Import(imp) = item else { continue };
                let inner = imp.path_inner;
                let is_public = imp.kind == crate::cst::ImportKind::Public;
                if let Some(&t) = path_to_idx.get(inner) {
                    direct[idx].push(t);
                    if is_public {
                        public[idx].push(t);
                    }
                } else if wkt::is_wkt_file(inner) {
                    direct_wkt[idx].push(inner);
                    if is_public {
                        public_wkt[idx].push(inner);
                    }
                } else {
                    return Err(self.error_in(
                        FileId::from_index(idx),
                        format!(
                            "import `{inner}` does not match any input file or well-known file"
                        ),
                        imp.path.span,
                    ));
                }
            }
        }

        // Provide sets, computed once and kept on `Sema`.
        for idx in 0..n {
            let mut pf = FxHashSet::default();
            let mut pw: FxHashSet<&'a str> = FxHashSet::default();
            reach_pub(
                FileId::from_index(idx),
                &public,
                &public_wkt,
                &mut pf,
                &mut pw,
            );
            self.sema.provides.push((pf, pw));
        }

        // Visibility = self + direct wkt imports + what each direct import
        // provides.
        let mut visible_files: Vec<FxHashSet<FileId>> = Vec::with_capacity(n);
        let mut visible_wkt: Vec<FxHashSet<&'a str>> = Vec::with_capacity(n);
        for idx in 0..n {
            let mut vf = FxHashSet::default();
            let mut vw: FxHashSet<&'a str> = FxHashSet::default();
            vf.insert(FileId::from_index(idx));
            for w in &direct_wkt[idx] {
                vw.insert(w);
            }
            for &d in &direct[idx] {
                let (pf, pw) = &self.sema.provides[d.index()];
                vf.extend(pf.iter().copied());
                vw.extend(pw.iter().copied());
            }
            visible_files.push(vf);
            visible_wkt.push(vw);
        }
        Ok((visible_files, visible_wkt))
    }

    /// Resolves every pending reference into the refs arena.
    ///
    /// `pending` is in symbol-creation order (each field/method pushes its
    /// references right after its own `add_symbol`), so each owner's
    /// references are contiguous — the CSR offsets fall out of a single
    /// merge-style pass with no per-symbol allocation.
    fn resolve_refs(
        &mut self,
        visible_files: &[FxHashSet<FileId>],
        visible_wkt: &[FxHashSet<&'a str>],
    ) -> Result<(), Error> {
        let pending = std::mem::take(&mut self.pending);
        let n = self.sema.symbols.len();
        let mut refs: Vec<Ref> = Vec::with_capacity(pending.len());
        let mut off: Vec<u32> = Vec::with_capacity(n + 1);
        let mut pi = 0usize;
        for i in 0..n {
            // Bounded by 2 * the analyze-entry cap (methods carry two
            // references), still well under u32.
            let mark = u32::try_from(refs.len())
                .map_err(|_| Error::new("input set too large: references exceed the u32 range"))?;
            off.push(mark);
            while pi < pending.len() && pending[pi].owner.index() == i {
                let p = &pending[pi];
                pi += 1;
                let target = self.resolve_one(p, visible_files, visible_wkt)?;
                refs.push(Ref {
                    span: p.path.span,
                    target,
                });
            }
        }
        debug_assert_eq!(pi, pending.len(), "pending refs sorted by owner");
        off.push(u32::try_from(refs.len()).expect("checked by the loop above"));
        self.sema.refs = refs;
        self.sema.refs_off = off;
        Ok(())
    }

    /// Resolves one pending reference and runs its kind and visibility
    /// checks.
    fn resolve_one(
        &self,
        p: &PendingRef<'s>,
        visible_files: &[FxHashSet<FileId>],
        visible_wkt: &[FxHashSet<&'a str>],
    ) -> Result<SymId, Error> {
        let target = self.resolve_path(p)?;
        let tsym = self.sema.sym(target);

        // Kind check.
        let ok_kind = if p.message_only {
            tsym.kind == SymKind::Message
        } else {
            matches!(tsym.kind, SymKind::Message | SymKind::Enum)
        };
        if !ok_kind {
            let what = if p.message_only {
                "rpc input and output must be message types"
            } else {
                "a field type must be a message or an enum"
            };
            return Err(self.error_in(
                p.file,
                format!(
                    "`{}` is a {}; {what}",
                    self.sema.fq(target),
                    tsym.kind.label()
                ),
                p.path.span,
            ));
        }

        // Visibility check.
        let visible = tsym.file.map_or_else(
            || visible_wkt[p.file.index()].contains(tsym.wkt_path.unwrap()),
            |tf| visible_files[p.file.index()].contains(&tf),
        );
        if !visible {
            let provider = tsym.file.map_or_else(
                || tsym.wkt_path.unwrap().to_string(),
                |tf| self.set.files[tf.index()].path.clone(),
            );
            return Err(self
                .error_in(
                    p.file,
                    format!(
                        "`{}` is defined in `{provider}`, which is not imported here",
                        self.sema.fq(target)
                    ),
                    p.path.span,
                )
                .note(format!("add `import \"{provider}\";`")));
        }

        Ok(target)
    }

    /// Reference resolution, protoc-faithful (`descriptor.cc`,
    /// `LookupSymbolNoPlaceholder`); allocation-free until an error.
    ///
    /// `.`-prefixed paths resolve from the root. A relative path anchors
    /// on its *first* segment: walking outward from the innermost scope,
    /// the first scope where that segment names a package or definition
    /// decides the whole lookup — if the remaining segments don't resolve
    /// inside it, the reference is unresolved (an inner `A` without `A.B`
    /// shadows an outer `A.B`; protoc: "the innermost scope is searched
    /// first"), never a cue to keep walking outward. Scopes where the
    /// first segment names a field, enum value, or method are passed
    /// over, and so is a single-segment match on anything but a message
    /// or enum when a field type is wanted (protoc's `LOOKUP_TYPES`);
    /// rpc signatures (`LOOKUP_ALL`) take the nearest symbol of any kind
    /// and leave the rejection to the kind check.
    fn resolve_path(&self, p: &PendingRef<'s>) -> Result<SymId, Error> {
        let segs = p.path.segs.slice(&self.set.files[p.file.index()].cst.segs);
        if p.path.leading_dot {
            return self
                .sema
                .descend_syms(NS_ROOT, segs)
                .ok_or_else(|| self.unresolved(p, segs, None));
        }
        let (first, rest) = segs.split_first().expect("the grammar has no empty paths");
        let mut base = self.start_ns(p);
        loop {
            if let Some(ns) = self.sema.ns_child(base, first.text) {
                let sym = self.sema.ns_nodes[ns.index()].sym;
                if !rest.is_empty() {
                    // Compound path: a first segment naming a package
                    // prefix or a definition anchors the lookup (protoc:
                    // `IsAggregate`); the rest must resolve inside it and
                    // a miss is final. Anything else (field, enum value,
                    // method) is passed over.
                    if sym.is_none_or(|id| self.sema.sym(id).kind.is_def()) {
                        return self
                            .sema
                            .descend_syms(ns, rest)
                            .ok_or_else(|| self.unresolved(p, segs, Some(ns)));
                    }
                } else if let Some(id) = sym {
                    if p.message_only
                        || matches!(self.sema.sym(id).kind, SymKind::Message | SymKind::Enum)
                    {
                        return Ok(id);
                    }
                    // A field type over a non-type name: keep walking.
                } else if p.message_only {
                    // A bare package name: protoc (`LOOKUP_ALL`) takes
                    // the package symbol here and fails the message-type
                    // check; pbpp has no symbol for a package, so the
                    // same hard stop reports as unresolved.
                    return Err(self.unresolved(p, segs, None));
                }
                // A bare package name as a field type: keep walking
                // (protoc: a package is not a type).
            }
            if base == NS_ROOT {
                return Err(self.unresolved(p, segs, None));
            }
            base = self.sema.ns_nodes[base.index()].parent;
        }
    }

    /// The unresolved-reference error. When the first segment anchored
    /// somewhere (`anchor` is its namespace node) and the rest of the
    /// path missed, the note spells out the shadowing the way protoc
    /// does and points at the leading-dot escape hatch.
    fn unresolved(
        &self,
        p: &PendingRef<'s>,
        segs: &[crate::cst::Word<'a>],
        anchor: Option<NsId>,
    ) -> Error {
        let mut written = String::with_capacity(32);
        if p.path.leading_dot {
            written.push('.');
        }
        for (i, seg) in segs.iter().enumerate() {
            if i > 0 {
                written.push('.');
            }
            written.push_str(seg.text);
        }
        let err = self.error_in(
            p.file,
            format!("cannot resolve type `{written}`"),
            p.path.span,
        );
        let Some(ns) = anchor else {
            return err.note(
                "every type reference must resolve within the input set or the well-known types",
            );
        };
        // Anchored miss: rebuild the fully-qualified path the reference
        // resolved to (error path; allocation is fine here).
        let mut parts: Vec<&str> = Vec::new();
        let mut at = ns;
        while at != NS_ROOT {
            let node = &self.sema.ns_nodes[at.index()];
            parts.push(node.name);
            at = node.parent;
        }
        parts.reverse();
        let mut resolved = parts.join(".");
        for seg in &segs[1..] {
            resolved.push('.');
            resolved.push_str(seg.text);
        }
        err.note(format!(
            "`{written}` resolves to `{resolved}`, which is not defined; the innermost \
             scope is searched first — use a leading dot (`.{written}`) to start from \
             the outermost scope"
        ))
    }
}
