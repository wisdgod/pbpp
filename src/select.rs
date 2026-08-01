//! Selection: evaluates the rule set over the address space and computes
//! the keep set as marks on nodes.
//!
//! Marks (dense array indexed by `SymId`, the design's "parallel array"):
//! - `Explicit`  — the node's effective rule decision is keep (a rule
//!   matched it or an ancestor; later rules override earlier ones);
//! - `Required`  — pulled in by the reachability closure (referenced by a
//!   kept node, or content of a kept definition);
//! - `Container` — kept only as a shell because a descendant is kept
//!   (method-level selection keeps the service shell, not its siblings);
//! - `CascadeDrop` — a kept field/method whose type was excluded with `-!`;
//!   pruning deletes it and reserves its number.
//!
//! Conflicts: a plain `-` exclusion that is still reachable from the kept
//! set is an error carrying the reference chain. `-!` resolves the conflict
//! by cascading — unless a *later* rule keeps the referencing node
//! (directly or through subtree inheritance), which is a contradiction
//! and errors.

use crate::error::Error;
use crate::fileset::FileSet;
use crate::rules::{Polarity, Rule, RuleSet};
use crate::sema::{Sema, SymId, SymKind};
use std::collections::VecDeque;

/// A node's selection mark; the ordering is the "keep strength" the
/// closure raises monotonically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Mark {
    /// Not selected; pruning deletes the node.
    None,
    /// Kept only as a shell because a descendant is kept.
    Container,
    /// Pulled in by the reachability closure (referenced by a kept node,
    /// or content of a kept definition).
    Required,
    /// A rule's effective decision is keep — direct hit or subtree
    /// inheritance.
    Explicit,
}

/// The selection result: parse output and marks, nothing materialized.
///
/// The dense arrays are private: they share the invariant "indexed by
/// `SymId`, same length as the symbol table", which the accessors below
/// rely on and exposed fields could not protect.
#[derive(Debug)]
pub struct Selected {
    marks: Vec<Mark>,
    /// Kept fields/methods that cascade away because their type is `-!`.
    cascade_dropped: Vec<bool>,
    /// For `Required`/`Container` marks: the symbol that pulled this one in.
    introduced_by: Vec<Option<SymId>>,
    /// Index of the rule that decided this symbol, if any.
    decided_by: Vec<Option<usize>>,
}

impl Selected {
    /// True if the symbol survives pruning (marked, and not cascaded away).
    #[must_use]
    pub fn is_kept(&self, s: SymId) -> bool {
        self.marks[s.index()] > Mark::None && !self.cascade_dropped[s.index()]
    }

    /// The symbol's selection mark.
    #[must_use]
    pub fn mark(&self, s: SymId) -> Mark {
        self.marks[s.index()]
    }

    /// True for kept fields/methods that cascade away because their type
    /// was excluded with `-!`; pruning deletes them and reserves their
    /// numbers.
    #[must_use]
    pub fn is_cascade_dropped(&self, s: SymId) -> bool {
        self.cascade_dropped[s.index()]
    }

    /// The symbol whose traversal pulled this one into the keep set
    /// (`Required`/`Container` marks).
    #[must_use]
    pub fn introduced_by(&self, s: SymId) -> Option<SymId> {
        self.introduced_by[s.index()]
    }

    /// Index into the rule set of the rule that decided this symbol.
    #[must_use]
    pub fn deciding_rule(&self, s: SymId) -> Option<usize> {
        self.decided_by[s.index()]
    }
}

/// Evaluates the rules over the address space and computes the keep set.
///
/// # Errors
///
/// A rule that matches nothing; a plain `-` exclusion still reachable from
/// the kept set (the diagnostic carries the reference chain); a keep rule
/// written after a `-!` cascade that excludes its type.
pub fn select(set: &FileSet<'_>, sema: &Sema<'_>, rules: &RuleSet) -> Result<Selected, Error> {
    // An empty rule set is configuration absence, not "select nothing":
    // erroring here keeps the DSL path (`parse_rules` rejects empty files)
    // and the programmatic path (`RuleSet::new()` never fed a rule)
    // consistent, as `RuleSet::new` documents.
    if rules.rules.is_empty() {
        return Err(Error::new("selector configuration contains no rules"));
    }
    let n = sema.sym_count();
    let direct = evaluate_rules(sema, rules)?;

    // decision[s]: last rule matching the symbol or any ancestor — the
    // effective decision with subtree inheritance. Parents precede
    // children in the symbol table, so one forward pass inheriting the
    // parent's (already final) decision replaces the per-symbol ancestor
    // walk: O(n) instead of O(n · depth).
    let mut decision: Vec<Option<(usize, Polarity)>> = vec![None; n];
    for (sid, sym) in sema.syms() {
        let i = sid.index();
        let inherited = sym.parent.and_then(|p| {
            debug_assert!(p.index() < i, "symbols are created parents-first");
            decision[p.index()]
        });
        decision[i] = match (direct[i], inherited) {
            (Some((own_rule, own_pol)), Some((parent_rule, parent_pol))) => {
                // Later rules override earlier ones, regardless of depth.
                if parent_rule > own_rule {
                    Some((parent_rule, parent_pol))
                } else {
                    Some((own_rule, own_pol))
                }
            }
            (d, None) => d,
            (None, p) => p,
        };
    }

    // ---- closure -------------------------------------------------------------
    let mut sel = Closure {
        set,
        sema,
        rules,
        decision: &decision,
        marks: vec![Mark::None; n],
        cascade: vec![false; n],
        introduced: vec![None; n],
        queue: VecDeque::new(),
        processed: vec![false; n],
    };

    // Seed: every symbol whose effective decision is keep. Well-known types
    // are never inputs, so they are never seeded.
    for (sid, sym) in sema.syms() {
        if sym.file.is_none() {
            continue;
        }
        if matches!(decision[sid.index()], Some((_, Polarity::Keep))) {
            sel.keep(sid, Mark::Explicit, None)?;
        }
    }

    while let Some(sid) = sel.queue.pop_front() {
        sel.process(sid)?;
    }

    check_enum_legality(set, sema, &sel.marks)?;

    Ok(Selected {
        marks: sel.marks,
        cascade_dropped: sel.cascade,
        introduced_by: sel.introduced,
        decided_by: decision.iter().map(|d| d.map(|(i, _)| i)).collect(),
    })
}

/// Evaluates every rule over the address space; `direct[s]` is the last
/// rule matching symbol `s` itself.
///
/// # Errors
///
/// A rule matching nothing effect-capable (zero-hit configuration rot),
/// with hints for bare package prefixes and builtin-only keeps.
fn evaluate_rules(
    sema: &Sema<'_>,
    rules: &RuleSet,
) -> Result<Vec<Option<(usize, Polarity)>>, Error> {
    let mut direct: Vec<Option<(usize, Polarity)>> = vec![None; sema.sym_count()];
    let mut hits = vec![0usize; rules.rules.len()];
    let mut wkt_only = vec![false; rules.rules.len()];
    for (sid, sym) in sema.syms() {
        let segs = sema.segs(sid);
        for (ri, rule) in rules.rules.iter().enumerate() {
            if rule_matches(rule, sym.kind, segs) {
                // Keep rules address the input set only: builtins are never
                // materialized, they are pulled in by reference. A keep
                // matching only builtins is dead configuration, and it must
                // not shadow an exclusion in the decision order.
                if sym.file.is_none() && rule.polarity == Polarity::Keep {
                    wkt_only[ri] = true;
                    continue;
                }
                hits[ri] += 1;
                direct[sid.index()] = Some((ri, rule.polarity));
            }
        }
    }

    // Zero-hit rules are configuration rot: error, with a hint when the
    // pattern looks like a bare package prefix.
    let dead: Vec<usize> = hits
        .iter()
        .enumerate()
        .filter(|&(_, &h)| h == 0)
        .map(|(i, _)| i)
        .collect();
    if let Some(&first) = dead.first() {
        let rule = &rules.rules[first];
        let mut err = rule_error(
            rules,
            first,
            format!("rule `{}` matches nothing in the input set", rule.raw),
        );
        if wkt_only[first] {
            err = err.note(
                "the pattern only matches well-known builtins; keep rules cannot address them — builtins are kept automatically while referenced",
            );
        }
        if prefix_of_some_symbol(rule, sema) {
            err = err.note(
                "the pattern matches a package or definition prefix; to select its contents, append `.**`",
            );
        }
        for &d in dead.iter().skip(1) {
            err = match (rules.rules[d].span, rules.src.as_deref()) {
                (Some(sp), Some(src)) => err.note_at(
                    format!("rule `{}` also matches nothing", rules.rules[d].raw),
                    sp,
                    src,
                ),
                _ => err.note(format!(
                    "rule `{}` also matches nothing",
                    rules.rules[d].raw
                )),
            };
        }
        return Err(err);
    }
    Ok(direct)
}

/// An error located at a rule's line in the rules source, when available
/// (rules built programmatically carry no source).
fn rule_error(rules: &RuleSet, ri: usize, msg: String) -> Error {
    match (rules.rules[ri].span, rules.src.as_deref()) {
        (Some(sp), Some(src)) => Error::at(msg, sp, src),
        _ => Error::new(msg),
    }
}

/// Pruning an enum must leave it legal proto3: at least one value, and the
/// first surviving value (source order is preserved) must be zero. Enum
/// values are only dropped by explicit rules — they carry no references,
/// so cascade never touches them — which makes this a deterministic
/// configuration error, reported here rather than as illegal output.
fn check_enum_legality(set: &FileSet<'_>, sema: &Sema<'_>, marks: &[Mark]) -> Result<(), Error> {
    for (sid, sym) in sema.syms() {
        if sym.kind != SymKind::Enum || marks[sid.index()] == Mark::None {
            continue;
        }
        let Some(fid) = sym.file else { continue };
        let first_kept = sema
            .children(sid)
            .iter()
            .find(|&&c| marks[c.index()] > Mark::None);
        let f = &set.files[fid.index()];
        match first_kept {
            None => {
                return Err(Error::at(
                    format!(
                        "selection keeps enum `{}` but drops all of its values",
                        sema.fq(sid)
                    ),
                    sym.name_span,
                    f.src,
                )
                .with_file(&f.path)
                .note("an enum must have at least one value; drop the enum itself instead"));
            }
            Some(&c) if sema.sym(c).number != Some(0) => {
                return Err(Error::at(
                    format!(
                        "selection drops the zero value of enum `{}`",
                        sema.fq(sid)
                    ),
                    sym.name_span,
                    f.src,
                )
                .with_file(&f.path)
                .note_at_file(
                    format!(
                        "the first surviving value would be `{}`, but proto3 requires the first enum value to be zero",
                        sema.fq(c)
                    ),
                    sema.sym(c).name_span,
                    f.src,
                    &f.path,
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn rule_matches(rule: &Rule, kind: SymKind, segs: &[&str]) -> bool {
    if let Some(k) = rule.kind
        && k != kind
    {
        return false;
    }
    rule.matches_path(segs)
}

/// True if the rule's literal pattern is a strict prefix of some symbol's
/// path — the "you addressed a package, not a node" case.
fn prefix_of_some_symbol(rule: &Rule, sema: &Sema<'_>) -> bool {
    let lits: Option<Vec<&str>> = rule
        .pattern
        .iter()
        .map(|s| match s {
            crate::rules::PatSeg::Lit(l) => Some(l.as_str()),
            _ => None,
        })
        .collect();
    let Some(lits) = lits else { return false };
    sema.syms().any(|(sid, _)| {
        let segs = sema.segs(sid);
        segs.len() > lits.len() && segs[..lits.len()] == lits[..]
    })
}

struct Closure<'r, 'a> {
    set: &'r FileSet<'a>,
    sema: &'r Sema<'a>,
    rules: &'r RuleSet,
    decision: &'r [Option<(usize, Polarity)>],
    marks: Vec<Mark>,
    cascade: Vec<bool>,
    introduced: Vec<Option<SymId>>,
    queue: VecDeque<SymId>,
    processed: Vec<bool>,
}

impl Closure<'_, '_> {
    /// Marks a symbol kept at the given level, running the cascade/conflict
    /// checks for reference-carrying symbols before committing.
    fn keep(&mut self, sid: SymId, level: Mark, by: Option<SymId>) -> Result<(), Error> {
        debug_assert!(level >= Mark::Required);
        let i = sid.index();
        if self.cascade[i] {
            return Ok(());
        }
        if self.marks[i] >= level {
            return Ok(());
        }

        // Fields and methods carry type references: check them against
        // exclusions before keeping.
        let sym = self.sema.sym(sid);
        if matches!(sym.kind, SymKind::Field | SymKind::Method) {
            for r in self.sema.refs(sid) {
                match self.decision[r.target.index()] {
                    Some((drop_rule, Polarity::DropCascade)) => {
                        // A keep decided by a *later* rule — directly or by
                        // subtree inheritance — contradicts the cascade;
                        // keeps from earlier rules are overridden by it.
                        if let Some((keep_rule, Polarity::Keep)) = self.decision[i]
                            && keep_rule > drop_rule
                        {
                            return Err(
                                self.contradiction_error(sid, r.target, keep_rule, drop_rule)
                            );
                        }
                        self.cascade[i] = true;
                        self.marks[i] = Mark::None;
                        return Ok(());
                    }
                    Some((drop_rule, Polarity::Drop)) => {
                        return Err(self.conflict_error(sid, r.target, drop_rule, by));
                    }
                    _ => {}
                }
            }
        }

        let was = self.marks[i];
        self.marks[i] = level;
        if self.introduced[i].is_none() {
            self.introduced[i] = by;
        }

        // Keep the ancestor chain at least as containers.
        let mut cur = sym.parent;
        let mut child = sid;
        while let Some(p) = cur {
            let pi = p.index();
            if self.marks[pi] >= Mark::Container {
                break;
            }
            self.marks[pi] = Mark::Container;
            if self.introduced[pi].is_none() {
                self.introduced[pi] = Some(child);
            }
            child = p;
            cur = self.sema.sym(p).parent;
        }

        // Contents are traversed for full keeps only (Required/Explicit);
        // containers hold nothing but the kept descendants.
        if was < Mark::Required && !self.processed[i] {
            self.processed[i] = true;
            self.queue.push_back(sid);
        }
        Ok(())
    }

    fn process(&mut self, sid: SymId) -> Result<(), Error> {
        // Copy the `&Sema` out of `self` so the symbol borrow doesn't tie up
        // `self` while `keep` mutates marks (also avoids cloning the
        // children/refs vectors).
        let sema = self.sema;
        let sym = sema.sym(sid);
        if sym.file.is_none() {
            // Builtins have no contents to traverse; keeping them only
            // preserves the import line.
            return Ok(());
        }
        match sym.kind {
            SymKind::Message | SymKind::Enum | SymKind::Service => {
                for &child in sema.children(sid) {
                    let ci = child.index();
                    // Dropped content of a kept definition stays out
                    // (pruning reserves field numbers); nested definitions
                    // come in only through references or their own rules,
                    // not by virtue of the parent.
                    if matches!(
                        self.decision[ci],
                        Some((_, Polarity::Drop | Polarity::DropCascade))
                    ) || sema.sym(child).kind.is_def()
                    {
                        continue;
                    }
                    let level = match self.decision[ci] {
                        Some((_, Polarity::Keep)) => Mark::Explicit,
                        _ => Mark::Required,
                    };
                    self.keep(child, level, Some(sid))?;
                }
            }
            SymKind::Field | SymKind::Method => {
                for r in sema.refs(sid) {
                    self.keep(r.target, Mark::Required, Some(sid))?;
                }
            }
            SymKind::EnumValue => {}
        }
        Ok(())
    }

    /// `- X` while X is still reachable: error with the reference chain.
    fn conflict_error(
        &self,
        from: SymId,
        target: SymId,
        drop_rule: usize,
        by: Option<SymId>,
    ) -> Error {
        let from_sym = self.sema.sym(from);
        let ref_span = self
            .sema
            .refs(from)
            .iter()
            .find(|r| r.target == target)
            .map_or(from_sym.name_span, |r| r.span);

        let ff = from_sym
            .file
            .expect("reference-carrying symbols (fields/methods) live in input files");
        let f = &self.set.files[ff.index()];
        let mut err = Error::at(
            format!(
                "cannot exclude `{}`: it is still referenced by kept {} `{}`",
                self.sema.fq(target),
                from_sym.kind.label(),
                self.sema.fq(from)
            ),
            ref_span,
            f.src,
        )
        .with_file(&f.path);

        // Why is the referencing node kept? Walk the introduction chain up
        // to an explicitly selected root.
        let mut cur = by;
        let mut last = from;
        while let Some(c) = cur {
            err = err.note(format!(
                "`{}` is kept because of `{}`",
                self.sema.fq(last),
                self.sema.fq(c)
            ));
            last = c;
            cur = self.introduced[c.index()];
        }
        if let Some((ri, Polarity::Keep)) = self.decision[last.index()] {
            err = err.note(format!(
                "`{}` is selected by rule: `{}`",
                self.sema.fq(last),
                self.rules.rules[ri].raw
            ));
        }
        err.note(format!(
            "excluded by rule: `{}`",
            self.rules.rules[drop_rule].raw
        ))
        .note(
            "changing message semantics must be explicit: use `-!` to cascade-delete the referencing fields and reserve their numbers",
        )
    }

    fn contradiction_error(
        &self,
        kept: SymId,
        target: SymId,
        keep_rule: usize,
        drop_rule: usize,
    ) -> Error {
        let mut err = rule_error(
            self.rules,
            keep_rule,
            format!(
                "rule `{}` keeps `{}`, but its type `{}` is cascade-excluded by an earlier `{}`",
                self.rules.rules[keep_rule].raw,
                self.sema.fq(kept),
                self.sema.fq(target),
                self.rules.rules[drop_rule].raw
            ),
        );
        if let (Some(sp), Some(src)) = (self.rules.rules[drop_rule].span, self.rules.src.as_deref())
        {
            err = err.note_at("the cascade rule is here", sp, src);
        }
        err.note(
            "the keep rule is written after the cascade rule, so neither can override the other; drop one of them",
        )
    }
}
