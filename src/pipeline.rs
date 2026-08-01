//! The orchestration façade: parse a file set, analyze it, and run
//! selection or pruning against rule sets — the whole
//! `text → CST → select → prune → text` pipeline behind one type.
//!
//! The lower-level stages ([`crate::parse()`], [`crate::sema::analyze`],
//! [`crate::select()`], [`crate::prune()`]) remain public for callers that need
//! to hold intermediate results or drive the stages themselves.

use crate::error::Error;
use crate::fileset::FileSet;
use crate::prune::PruneOutput;
use crate::rules::RuleSet;
use crate::select::Selected;
use crate::sema::{self, Sema};

/// A parsed and semantically analyzed input set, ready for selection and
/// pruning. Borrows the caller's sources — collect them first, then build:
///
/// ```
/// let sources = vec![(
///     "a.proto".to_string(),
///     "syntax = \"proto3\";\npackage a;\nmessage M { int32 x = 1; }\n".to_string(),
/// )];
/// let pipeline = pbpp::Pipeline::new(
///     sources.iter().map(|(p, s)| (p.clone(), s.as_str())).collect(),
/// )?;
///
/// let mut rules = pbpp::RuleSet::new();
/// rules.keep("a.M")?;
/// let pruned = pipeline.prune(&rules)?;
/// assert_eq!(pruned.files.len(), 1);
/// # Ok::<(), pbpp::Error>(())
/// ```
pub struct Pipeline<'a> {
    // Private: `sema` is derived from `set` at construction; exposing the
    // fields mutably would let the two drift apart mid-pipeline.
    set: FileSet<'a>,
    sema: Sema<'a>,
}

impl<'a> Pipeline<'a> {
    /// The parsed input set (read-only; it and the analysis were built
    /// together and stay consistent).
    #[must_use]
    pub const fn file_set(&self) -> &FileSet<'a> {
        &self.set
    }

    /// The semantic analysis over the set (read-only).
    #[must_use]
    pub const fn sema(&self) -> &Sema<'a> {
        &self.sema
    }

    /// Parses and analyzes `(import_path, source)` pairs.
    ///
    /// # Errors
    ///
    /// Parse errors, duplicate definitions, unresolved or invisible type
    /// references, and imports naming no input or well-known file.
    pub fn new(inputs: Vec<(String, &'a str)>) -> Result<Self, Error> {
        let set = FileSet::parse(inputs)?;
        let sema = sema::analyze(&set)?;
        Ok(Self { set, sema })
    }

    /// Computes the keep set for the rules: marks only, nothing modified.
    ///
    /// # Errors
    ///
    /// See [`crate::select()`].
    pub fn select(&self, rules: &RuleSet) -> Result<Selected, Error> {
        crate::select(&self.set, &self.sema, rules)
    }

    /// Selects and materializes: returns the pruned files in raw form
    /// (original text minus deletions). Call [`PruneOutput::format`] on the
    /// result for normalized layout.
    ///
    /// # Errors
    ///
    /// See [`crate::select()`].
    pub fn prune(&self, rules: &RuleSet) -> Result<PruneOutput, Error> {
        let selected = self.select(rules)?;
        Ok(crate::prune(&self.set, &self.sema, &selected))
    }
}
