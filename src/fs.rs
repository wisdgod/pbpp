//! The filesystem boundary: input discovery, import-path validation,
//! manifest-tracked output sync, and atomic writes.
//!
//! Everything else in pbpp is pure and deterministic in memory; this
//! module — and the `pbtrim` shell on top of it — owns the side effects,
//! so CI jobs and build scripts share one audited implementation of the
//! dangerous parts instead of hand-rolling `dest.join(path)`.
//!
//! What this module guarantees:
//! - **paths are validated**: import paths are relative, `/`-separated,
//!   UTF-8, with no control characters, no empty/`.`/`..` segments, and no
//!   component reserved for pbpp's own bookkeeping — a hostile path can
//!   neither escape the output directory, forge a manifest line, nor
//!   overwrite the manifest or lock;
//! - **symlinks are refused**: discovery errors on them (no scan cycles,
//!   no files pulled from outside the root), and sync never writes
//!   through or removes one;
//! - **writes never follow a pre-placed path**: temp files are created
//!   with `O_EXCL` semantics (`create_new`) under an unpredictable name
//!   and renamed into place, so a planted symlink/hardlink cannot
//!   redirect or truncate an unrelated file, and no destination is ever
//!   observed half-written;
//! - **one writer at a time**: sync holds an exclusive lock on the output
//!   directory;
//! - **sync validates before mutating**: every input path and the entire
//!   existing manifest are checked before the first write, and the
//!   manifest is rewritten last, so a manifest on disk only ever names a
//!   completed write phase;
//! - **sync only removes what it recorded**: removal candidates come
//!   solely from the previous manifest.
//!
//! What it does *not* guarantee — deliberately stated, since each is a
//! plausible expectation:
//! - **sync is not atomic as a whole.** Individual file writes are, but a
//!   crash mid-run can leave some files from the new generation and a
//!   manifest still describing the old one. Files written before such a
//!   crash and not produced by a later run are never recorded, and are
//!   therefore treated as foreign from then on (they are not cleaned up).
//!   Recovering from that means inspecting the directory yourself.
//! - **a crashed run leaves its lock file behind.** There is no timeout or
//!   stale-lock takeover: the next sync fails and names the lock path so a
//!   human can remove it. This is the conservative direction — a spurious
//!   takeover could interleave two writers.
//! - **containment is checked at resolve time, not enforced against a
//!   racing filesystem.** Destinations and removal targets are
//!   canonicalized and compared against the output root, but nothing
//!   prevents an *external* process from swapping a parent directory
//!   between that check and the operation. The output directory is
//!   expected to be owned by this tool, which is what the lock encourages.
//! - **foreign files are safe from removal, not from collision.** Sync
//!   only deletes manifest-recorded paths, but it does overwrite whatever
//!   sits at a path it is asked to write.

use crate::error::Error;
use crate::prune::PrunedFile;
use rustc_hash::FxHashSet;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Name of the ownership manifest [`sync`] keeps in the output directory.
pub const MANIFEST: &str = ".pbtrim-manifest";

/// Name of the exclusive lock file [`sync`] holds for the duration of a
/// run.
pub const LOCK: &str = ".pbtrim-lock";

/// First line of a well-formed manifest; a version tag so the format can
/// change without a silent misread.
const MANIFEST_HEADER: &str = "# pbtrim manifest v1";

/// Reserved file-name prefix for pbpp's own bookkeeping inside an output
/// directory ([`MANIFEST`], [`LOCK`], and write temporaries). An output
/// path is refused if any component starts with it, so a pruned file can
/// never land on — and clobber — the manifest or lock.
const RESERVED_PREFIX: &str = ".pbtrim-";

/// True for a well-formed import path.
///
/// Well-formed means relative, `/`-separated, no empty or `.`/`..`
/// segments, no `\` or `:` (drive letters, alternate separators), and no
/// ASCII control characters (`< 0x20` or `0x7f`). The path is `&str`, so
/// it is already UTF-8. Together these make `out_dir.join(path)`
/// containment-safe and keep a path from forging a line in the
/// newline-delimited manifest or injecting a `cargo::` directive when
/// echoed by a build script.
#[must_use]
pub fn valid_import_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains(':')
        && !p.bytes().any(|b| b < 0x20 || b == 0x7f)
        && p.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                // Never collide with pbpp's own bookkeeping files.
                && !seg.starts_with(RESERVED_PREFIX)
        })
}

pub(crate) fn check_import_path(p: &str) -> Result<(), Error> {
    if valid_import_path(p) {
        Ok(())
    } else {
        Err(Error::new(format!(
            "invalid import path `{}` (paths must be relative, `/`-separated, UTF-8, \
             free of control characters, without empty, `.`, or `..` segments, and \
             without a `{RESERVED_PREFIX}` component reserved for pbpp's bookkeeping)",
            p.escape_default()
        )))
    }
}

/// Discovers every `.proto` under `root` and reads it, returning
/// `(import_path, source)` pairs sorted by path (deterministic input
/// order for reproducible builds).
///
/// # Errors
///
/// An unreadable directory or file; a symlink anywhere under `root`
/// (refused: following them can cycle the scan or pull files from outside
/// the root — restructure or copy instead); a non-UTF-8 path component
/// (import paths must be UTF-8).
pub fn discover(root: &Path) -> Result<Vec<(String, String)>, Error> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| Error::new(format!("cannot read directory `{}`: {e}", dir.display())))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::new(format!("cannot read `{}`: {e}", dir.display())))?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| Error::new(format!("cannot stat `{}`: {e}", path.display())))?;
            if meta.file_type().is_symlink() {
                return Err(Error::new(format!(
                    "symlink in the input tree: `{}` (pbpp does not follow symlinks)",
                    path.display()
                )));
            }
            if meta.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "proto") {
                paths.push(path);
            }
        }
    }
    paths.sort();

    let mut out = Vec::with_capacity(paths.len());
    for p in &paths {
        let rel = rel_import_path(root, p)?;
        // Walked paths are relative and clean by construction, but the
        // validity check is this boundary's contract, not an assumption.
        check_import_path(&rel)?;
        let src = std::fs::read_to_string(p)
            .map_err(|e| Error::new(format!("cannot read `{}`: {e}", p.display())))?;
        out.push((rel, src));
    }
    Ok(out)
}

/// The `/`-joined path of `p` relative to `root`, requiring every
/// component to be UTF-8 (import paths are strings).
fn rel_import_path(root: &Path, p: &Path) -> Result<String, Error> {
    let rel = p
        .strip_prefix(root)
        .map_err(|_| Error::new(format!("`{}` is not under the root", p.display())))?;
    let mut segs = Vec::new();
    for c in rel.components() {
        let s = c.as_os_str().to_str().ok_or_else(|| {
            Error::new(format!(
                "non-UTF-8 path component in `{}` (import paths must be UTF-8)",
                p.display()
            ))
        })?;
        segs.push(s);
    }
    Ok(segs.join("/"))
}

/// What a [`sync`] did: files written this run and manifest-owned
/// leftovers removed, both as import paths in sorted order.
#[derive(Debug)]
pub struct SyncReport {
    /// Import paths written this run.
    pub written: Vec<String>,
    /// Manifest-owned leftovers removed this run.
    pub removed: Vec<String>,
}

/// Mirrors `files` into `out_dir` under an exclusive directory lock.
///
/// Phases: validate every input path and the entire existing manifest;
/// write each file atomically; remove previous-generation files the
/// manifest recorded and this run did not produce; rewrite the manifest.
/// The manifest goes last, so what is on disk always describes a
/// completed write phase.
///
/// This is not a transaction. A crash between phases leaves part of the
/// new generation on disk under the old manifest; files written but never
/// recorded are indistinguishable from foreign files afterwards and will
/// not be cleaned up automatically. A crashed run also leaves its lock
/// file behind, which the next call reports rather than overriding. See
/// the module documentation for the full boundary.
///
/// # Errors
///
/// A concurrent sync already holds the lock; invalid file or manifest
/// paths; I/O failures; a destination or manifest entry that resolves
/// (through a symlinked directory) outside `out_dir` — refused rather
/// than acted on.
///
/// # Panics
///
/// Never on the normal path: the `expect`s assert that a validated,
/// non-empty relative path joined onto the output root has a parent and a
/// file name — established by `check_import_path` in the preflight above.
pub fn sync(out_dir: &Path, files: &[PrunedFile]) -> Result<SyncReport, Error> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| Error::new(format!("cannot create `{}`: {e}", out_dir.display())))?;
    let out_root = out_dir
        .canonicalize()
        .map_err(|e| Error::new(format!("cannot resolve `{}`: {e}", out_dir.display())))?;

    // One writer at a time: the lock guards the manifest and the temp
    // namespace against interleaved runs.
    let _lock = DirLock::acquire(&out_root)?;

    // ---- preflight: validate everything before mutating anything -------------
    for f in files {
        check_import_path(&f.path)?;
    }
    let old_owned = read_manifest(&out_root)?;

    // ---- write phase ---------------------------------------------------------
    let mut written: Vec<String> = Vec::with_capacity(files.len());
    for f in files {
        let dest = out_root.join(&f.path);
        let parent = dest.parent().expect("joined path has a parent");
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::new(format!("cannot create `{}`: {e}", parent.display())))?;
        // A symlinked subdirectory inside the tree must not redirect the
        // write outside it.
        let canon_parent = parent
            .canonicalize()
            .map_err(|e| Error::new(format!("cannot resolve `{}`: {e}", parent.display())))?;
        if !canon_parent.starts_with(&out_root) {
            return Err(Error::new(format!(
                "refusing to write `{}`: it resolves outside the output directory",
                f.path
            )));
        }
        let file_name = dest.file_name().expect("validated path has a file name");
        write_atomic(&canon_parent.join(file_name), &f.text)?;
        written.push(f.path.clone());
    }
    written.sort_unstable();
    let current: FxHashSet<&str> = written.iter().map(String::as_str).collect();

    // ---- removal phase: only previous-generation files this sync owned -------
    let mut removed: Vec<String> = Vec::new();
    for rel in &old_owned {
        if current.contains(rel.as_str()) {
            continue;
        }
        let p = out_root.join(rel);
        let meta = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::new(format!("cannot stat `{}`: {e}", p.display()))),
        };
        if meta.file_type().is_symlink() {
            return Err(Error::new(format!(
                "refusing to remove `{}`: it is a symlink",
                p.display()
            )));
        }
        let canon = p
            .canonicalize()
            .map_err(|e| Error::new(format!("cannot resolve `{}`: {e}", p.display())))?;
        if !canon.starts_with(&out_root) {
            return Err(Error::new(format!(
                "refusing to remove `{}`: it resolves outside the output directory",
                p.display()
            )));
        }
        std::fs::remove_file(&canon)
            .map_err(|e| Error::new(format!("cannot remove `{}`: {e}", canon.display())))?;
        removed.push(rel.clone());
    }
    removed.sort_unstable();

    // ---- commit: manifest last -----------------------------------------------
    let mut manifest =
        String::with_capacity(written.iter().map(|w| w.len() + 1).sum::<usize>() + 32);
    manifest.push_str(MANIFEST_HEADER);
    manifest.push('\n');
    for w in &written {
        manifest.push_str(w);
        manifest.push('\n');
    }
    write_atomic(&out_root.join(MANIFEST), &manifest)?;

    Ok(SyncReport { written, removed })
}

/// Reads and validates the previous manifest's owned paths (empty if
/// absent). Rejects a manifest with the wrong header or an invalid entry
/// rather than acting on garbage.
fn read_manifest(out_root: &Path) -> Result<Vec<String>, Error> {
    let path = out_root.join(MANIFEST);
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::new(format!("cannot read `{}`: {e}", path.display()))),
    };
    let mut lines = text.lines();
    match lines.next() {
        Some(MANIFEST_HEADER) => {}
        _ => {
            return Err(Error::new(format!(
                "`{}` is not a pbtrim manifest (missing `{MANIFEST_HEADER}` header); \
                 refusing to act on it",
                path.display()
            )));
        }
    }
    let mut owned = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if !valid_import_path(line) {
            return Err(Error::new(format!(
                "`{}` contains an invalid entry `{}`; refusing to act on it",
                path.display(),
                line.escape_default()
            )));
        }
        owned.push(line.to_string());
    }
    Ok(owned)
}

/// Writes `contents` to `dest` atomically, without ever following a
/// pre-placed path at `dest` or the temp name.
///
/// The temp file is created in the destination's directory with
/// `create_new` (`O_EXCL`: fails on any existing entry, including a
/// symlink) under an unpredictable name, written through the returned
/// handle, then renamed over `dest`. The rename replaces `dest` (or the
/// symlink *at* `dest`) atomically on the same filesystem. A drop guard
/// removes the temp file on any early return.
///
/// # Errors
///
/// I/O failures creating, writing, or renaming; exhausting temp-name
/// attempts (implies something is actively racing the same directory).
///
/// # Panics
///
/// Never on the normal path: the `expect` asserts the temp path is still
/// held at commit time, which the code above guarantees (it is only taken
/// once, here).
pub fn write_atomic(dest: &Path, contents: &str) -> Result<(), Error> {
    let parent = dest
        .parent()
        .ok_or_else(|| Error::new(format!("`{}` has no parent directory", dest.display())))?;
    let (mut file, tmp) = create_temp(parent)?;
    let mut guard = TempGuard { path: Some(tmp) };
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| {
            Error::new(format!(
                "cannot write `{}`: {e}",
                guard.path.as_deref().unwrap_or(dest).display()
            ))
        })?;
    drop(file);
    let tmp = guard.path.take().expect("temp path present until commit");
    std::fs::rename(&tmp, dest).map_err(|e| {
        // Rename failed: the temp file is orphaned, so clean it up here
        // (the guard already released it).
        let _ = std::fs::remove_file(&tmp);
        Error::new(format!(
            "cannot move `{}` into place at `{}`: {e}",
            tmp.display(),
            dest.display()
        ))
    })
}

/// Creates a fresh temp file in `dir` under an unpredictable name,
/// retrying on the (rare) name collision. `create_new` guarantees we
/// never open an existing entry — a planted symlink at the guessed name
/// fails rather than redirecting the write.
fn create_temp(dir: &Path) -> Result<(File, PathBuf), Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut last_err = None;
    for _ in 0..64 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".pbtrim-tmp.{}.{nanos:x}.{n:x}", std::process::id());
        let tmp = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
            }
            Err(e) => {
                return Err(Error::new(format!(
                    "cannot create a temp file in `{}`: {e}",
                    dir.display()
                )));
            }
        }
    }
    Err(Error::new(format!(
        "cannot create a temp file in `{}` after many attempts: {}",
        dir.display(),
        last_err.map_or_else(|| "name collisions".to_string(), |e| e.to_string())
    )))
}

/// Removes a temp file on drop unless the path has been taken (committed).
struct TempGuard {
    path: Option<PathBuf>,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(p) = &self.path {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// An exclusive, advisory lock on an output directory, held for the life
/// of a [`sync`] via a `create_new` lock file removed on drop.
struct DirLock {
    path: PathBuf,
}

impl DirLock {
    fn acquire(out_root: &Path) -> Result<Self, Error> {
        let path = out_root.join(LOCK);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Error::new(format!(
                "output directory `{}` is locked by another pbtrim run (`{}`); \
                 remove it if no run is active",
                out_root.display(),
                path.display()
            ))),
            Err(e) => Err(Error::new(format!(
                "cannot lock `{}`: {e}",
                out_root.display()
            ))),
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
