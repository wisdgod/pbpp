//! Multi-file input set.

use crate::cst;
use crate::error::Error;
use rustc_hash::FxHashSet;

/// One parsed input file.
pub struct SetFile<'a> {
    /// Import path of the file, e.g. `acme/api/v1/user.proto`.
    pub path: String,
    /// The file's source text.
    pub src: &'a str,
    /// The file's lossless CST.
    pub cst: cst::File<'a>,
}

/// The parsed multi-file input set, in input order.
pub struct FileSet<'a> {
    /// The set's files; indices are stable and shared with `sema`'s
    /// `FileId` space.
    pub files: Vec<SetFile<'a>>,
}

impl<'a> FileSet<'a> {
    /// Parses every input into the set. `inputs` are `(import_path, source)`
    /// pairs; import paths use `/` separators.
    ///
    /// `google/protobuf/*` inputs are excluded automatically: that namespace
    /// is considered provided by the toolchain (the ten well-known files
    /// resolve from the builtin table; siblings like `descriptor.proto` are
    /// proto2 and never participate). When proto2 support lands, inputs
    /// will take precedence over the builtin table instead.
    ///
    /// # Errors
    ///
    /// The first parse error among the inputs, attributed to its file; a
    /// duplicate import path (two inputs with the same path would silently
    /// shadow each other in every path-keyed lookup); a malformed import
    /// path (absolute, `..`/`.`/empty segments, `\` or `:`) — paths flow
    /// into `out_dir.join(path)` downstream, so containment is enforced
    /// here at the set boundary.
    pub fn parse(inputs: Vec<(String, &'a str)>) -> Result<Self, Error> {
        let mut files: Vec<SetFile<'a>> = Vec::with_capacity(inputs.len());
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        for (path, src) in inputs {
            crate::fs::check_import_path(&path)?;
            if crate::wkt::is_google_protobuf_path(&path) {
                continue;
            }
            let cst = crate::parse(src).map_err(|e| e.with_file(&path))?;
            files.push(SetFile { path, src, cst });
        }
        for f in &files {
            if !seen.insert(&f.path) {
                return Err(Error::new(format!(
                    "duplicate input file `{}` (import paths must be unique)",
                    f.path
                )));
            }
        }
        Ok(FileSet { files })
    }
}
