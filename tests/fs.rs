//! Filesystem-boundary tests: path validation, atomic writes that refuse
//! pre-planted targets, manifest-scoped sync, and concurrency locking.

use pbpp::fs::{self, MANIFEST};
use pbpp::prune::PrunedFile;
use std::path::{Path, PathBuf};

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pbpp-fs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn file(path: &str, text: &str) -> PrunedFile {
    PrunedFile {
        path: path.to_string(),
        text: text.to_string(),
    }
}

#[test]
fn import_path_validation() {
    for good in ["a.proto", "a/b.proto", "acme/v1/user.proto"] {
        assert!(fs::valid_import_path(good), "{good} should be valid");
    }
    for bad in [
        "",
        "/abs.proto",
        "../escape.proto",
        "a/../b.proto",
        "a/./b.proto",
        "a//b.proto",
        "a\\b.proto",
        "c:evil.proto",
        "line\nbreak.proto",
        "tab\tname.proto",
        "bell\x07.proto",
        "del\x7f.proto",
        // Reserved for pbpp's own bookkeeping inside an output directory.
        ".pbtrim-manifest",
        ".pbtrim-lock",
        "a/.pbtrim-manifest",
        ".pbtrim-tmp.1234.deadbeef.0",
    ] {
        assert!(!fs::valid_import_path(bad), "{bad:?} should be rejected");
    }
}

#[test]
fn sync_writes_and_manifest_records_ownership() {
    let s = Scratch::new("sync-basic");
    let out = s.path("out");
    let report = fs::sync(&out, &[file("a/x.proto", "one"), file("b.proto", "two")]).unwrap();
    assert_eq!(report.written, vec!["a/x.proto", "b.proto"]);
    assert!(report.removed.is_empty());
    assert_eq!(
        std::fs::read_to_string(out.join("a/x.proto")).unwrap(),
        "one"
    );

    let manifest = std::fs::read_to_string(out.join(MANIFEST)).unwrap();
    assert!(manifest.starts_with("# pbtrim manifest v1\n"), "{manifest}");
    assert!(manifest.contains("a/x.proto"));

    // A second sync dropping b.proto removes only the owned leftover.
    std::fs::write(out.join("foreign.proto"), "not ours").unwrap();
    let report = fs::sync(&out, &[file("a/x.proto", "one")]).unwrap();
    assert_eq!(report.removed, vec!["b.proto"]);
    assert!(!out.join("b.proto").exists());
    assert!(out.join("foreign.proto").exists(), "foreign file removed");
    // The lock file is released after each sync.
    assert!(!out.join(pbpp::fs::LOCK).exists());
}

#[test]
fn atomic_write_replaces_without_following_symlink_at_dest() {
    // Reference the platform symlink API only where it exists.
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(unix)]
    {
        let s = Scratch::new("atomic-symlink");
        let victim = s.path("victim.txt");
        std::fs::write(&victim, "SECRET — must not be touched").unwrap();
        let dest = s.path("dest.proto");
        symlink(&victim, &dest).unwrap();

        fs::write_atomic(&dest, "new content").unwrap();

        // The victim is untouched; the symlink was replaced by a real file.
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "SECRET — must not be touched"
        );
        assert!(
            !std::fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new content");
    }
    let _ = Path::new(".");
}

#[test]
fn sync_lock_blocks_concurrent_writer() {
    let s = Scratch::new("lock");
    let out = s.path("out");
    std::fs::create_dir_all(&out).unwrap();
    let canon = out.canonicalize().unwrap();
    // Simulate a held lock from another run.
    std::fs::write(canon.join(pbpp::fs::LOCK), "999999").unwrap();

    let err = fs::sync(&out, &[file("a.proto", "x")]).unwrap_err();
    assert!(err.message().contains("locked"), "{}", err.message());
    // The pre-existing lock is left in place (not ours to remove).
    assert!(canon.join(pbpp::fs::LOCK).exists());
}

#[test]
fn sync_refuses_to_write_over_its_own_metadata() {
    // A pruned path colliding with the manifest or lock would let pruning
    // clobber its own ownership record.
    let s = Scratch::new("reserved");
    let out = s.path("out");
    for reserved in [".pbtrim-manifest", ".pbtrim-lock", "nested/.pbtrim-lock"] {
        let err = fs::sync(&out, &[file(reserved, "hijack")]).unwrap_err();
        assert!(
            err.message().contains("invalid import path"),
            "{reserved}: {}",
            err.message()
        );
    }
    // The real manifest from a legitimate sync is intact and parseable.
    fs::sync(&out, &[file("ok.proto", "x")]).unwrap();
    let manifest = std::fs::read_to_string(out.join(MANIFEST)).unwrap();
    assert!(manifest.contains("ok.proto"), "{manifest}");
}

#[test]
fn sync_rejects_foreign_manifest() {
    let s = Scratch::new("bad-manifest");
    let out = s.path("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join(MANIFEST), "not a pbtrim manifest\nwhatever\n").unwrap();

    let err = fs::sync(&out, &[file("a.proto", "x")]).unwrap_err();
    assert!(
        err.message().contains("not a pbtrim manifest"),
        "{}",
        err.message()
    );
}
