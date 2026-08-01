//! Process-level CLI tests: exit codes, stdout/stderr shape, and file
//! effects of the three `pbtrim` subcommands, driven through the real
//! binary (`CARGO_BIN_EXE_pbtrim`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A per-test scratch directory, removed on drop (best effort).
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pbpp-cli-{}-{name}", std::process::id()));
        // A stale dir from a crashed previous run must not leak into
        // assertions.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
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

fn pbtrim(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pbtrim"))
        .args(args)
        .output()
        .expect("pbtrim binary runs")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("pbtrim exits normally")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

const MESSY: &str = "syntax=\"proto3\";package a;message   M{int32 x=1;}";
const FORMATTED: &str = "syntax = \"proto3\";\n\npackage a;\n\nmessage M {\n  int32 x = 1;\n}\n";

#[test]
fn fmt_stdout_prints_formatted_output() {
    let s = Scratch::new("fmt-stdout");
    let f = s.write("m.proto", MESSY);
    let out = pbtrim(&["fmt".as_ref(), "--stdout".as_ref(), &f]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), FORMATTED);
    // --stdout must not touch the file.
    assert_eq!(std::fs::read_to_string(&f).unwrap(), MESSY);
}

#[test]
fn fmt_check_reports_and_leaves_file_alone() {
    let s = Scratch::new("fmt-check");
    let f = s.write("m.proto", MESSY);
    let out = pbtrim(&["fmt".as_ref(), "--check".as_ref(), &f]);
    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("would reformat"), "{}", stdout(&out));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), MESSY);
}

#[test]
fn fmt_rewrites_in_place_then_check_passes() {
    let s = Scratch::new("fmt-write");
    let f = s.write("m.proto", MESSY);
    let out = pbtrim(&["fmt".as_ref(), &f]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), FORMATTED);

    let check = pbtrim(&["fmt".as_ref(), "--check".as_ref(), &f]);
    assert_eq!(code(&check), 0, "already formatted must pass --check");
}

#[test]
fn fmt_parse_error_exits_2_with_located_diagnostic() {
    let s = Scratch::new("fmt-err");
    let f = s.write("bad.proto", "syntax = \"proto3\";\nmessage M {\n");
    let out = pbtrim(&["fmt".as_ref(), &f]);
    assert_eq!(code(&out), 2);
    let e = stderr(&out);
    assert!(e.contains("error:"), "{e}");
    assert!(e.contains("bad.proto"), "diagnostic names the file: {e}");
}

const APP: &str = "syntax = \"proto3\";\npackage app;\nimport \"lib.proto\";\nmessage M { lib.T t = 1; }\nmessage Gone { int32 x = 1; }\n";
const LIB: &str = "syntax = \"proto3\";\npackage lib;\nmessage T { int32 y = 1; }\n";

#[test]
fn select_reports_keeps_and_drops() {
    let s = Scratch::new("select");
    s.write("root/app.proto", APP);
    s.write("root/lib.proto", LIB);
    let rules = s.write("rules.txt", "+ app.M\n");
    let out = pbtrim(&[
        "select".as_ref(),
        "--rules".as_ref(),
        &rules,
        "--root".as_ref(),
        &s.path("root"),
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let report = stdout(&out);
    assert!(report.contains("keep message app.M"), "{report}");
    assert!(report.contains("required via"), "{report}");
    // Dropped top-level subtrees are reported by omission.
    assert!(!report.contains("app.Gone"), "{report}");
}

#[test]
fn select_zero_hit_rule_exits_2() {
    let s = Scratch::new("select-miss");
    s.write("root/app.proto", APP);
    s.write("root/lib.proto", LIB);
    let rules = s.write("rules.txt", "+ app.M\n+ nope.Thing\n");
    let out = pbtrim(&[
        "select".as_ref(),
        "--rules".as_ref(),
        &rules,
        "--root".as_ref(),
        &s.path("root"),
    ]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("matches nothing"), "{}", stderr(&out));
}

#[test]
fn prune_writes_kept_files_and_reports_dropped() {
    let s = Scratch::new("prune");
    s.write("root/app.proto", APP);
    s.write("root/lib.proto", LIB);
    s.write(
        "root/dead.proto",
        "syntax = \"proto3\";\npackage dead;\nmessage D { int32 z = 1; }\n",
    );
    let rules = s.write("rules.txt", "+ app.M\n");
    let out = pbtrim(&[
        "prune".as_ref(),
        "--rules".as_ref(),
        &rules,
        "--root".as_ref(),
        &s.path("root"),
        "--out".as_ref(),
        &s.path("out"),
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("dropped dead.proto"),
        "{}",
        stdout(&out)
    );

    let app = std::fs::read_to_string(s.path("out/app.proto")).unwrap();
    assert!(app.contains("message M"), "{app}");
    assert!(!app.contains("Gone"), "{app}");
    let lib = std::fs::read_to_string(s.path("out/lib.proto")).unwrap();
    assert!(lib.contains("message T"), "{lib}");
    assert!(!s.path("out/dead.proto").exists(), "dropped file written");
}

#[test]
fn prune_out_dir_is_synced_via_manifest() {
    // Re-runs must not leave stale outputs behind for CI consumers — but
    // removal is manifest-scoped: only files a previous pbtrim run wrote
    // are candidates. Foreign files (even .proto) are never touched.
    let s = Scratch::new("prune-sync");
    s.write("root/app.proto", APP);
    s.write("root/lib.proto", LIB);
    s.write(
        "root/dead.proto",
        "syntax = \"proto3\";\npackage dead;\nmessage D { int32 z = 1; }\n",
    );
    s.write(
        "out/foreign.proto",
        "syntax = \"proto3\";\nmessage NotOurs {}\n",
    );
    s.write("out/notes.txt", "not a proto; must survive");

    // First run keeps dead.D too: pbtrim now owns out/dead.proto.
    let rules_all = s.write("rules-all.txt", "+ app.M\n+ dead.D\n");
    let out = pbtrim(&[
        "prune".as_ref(),
        "--rules".as_ref(),
        &rules_all,
        "--root".as_ref(),
        &s.path("root"),
        "--out".as_ref(),
        &s.path("out"),
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(s.path("out/dead.proto").exists());

    // Second run drops dead.D: the owned leftover goes, foreign files stay.
    let rules_app = s.write("rules-app.txt", "+ app.M\n");
    let out = pbtrim(&[
        "prune".as_ref(),
        "--rules".as_ref(),
        &rules_app,
        "--root".as_ref(),
        &s.path("root"),
        "--out".as_ref(),
        &s.path("out"),
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("removed stale"), "{}", stdout(&out));
    assert!(
        !s.path("out/dead.proto").exists(),
        "owned stale not removed"
    );
    assert!(s.path("out/foreign.proto").exists(), "foreign file removed");
    assert!(s.path("out/notes.txt").exists());
    assert!(s.path("out/app.proto").exists());
}

#[test]
fn prune_rejects_overlapping_out_and_root() {
    let s = Scratch::new("prune-overlap");
    s.write(
        "root/app.proto",
        "syntax = \"proto3\";\nmessage M { int32 x = 1; }\n",
    );
    let rules = s.write("rules.txt", "+ M\n");
    for out_rel in ["root", "root/out", "."] {
        let out = pbtrim(&[
            "prune".as_ref(),
            "--rules".as_ref(),
            &rules,
            "--root".as_ref(),
            &s.path("root"),
            "--out".as_ref(),
            &s.path(out_rel),
        ]);
        assert_eq!(code(&out), 2, "out={out_rel} must be rejected");
        assert!(
            stderr(&out).contains("must be disjoint"),
            "out={out_rel}: {}",
            stderr(&out)
        );
    }
}

#[cfg(unix)]
#[test]
fn discovery_refuses_symlinks() {
    let s = Scratch::new("symlink");
    s.write(
        "root/app.proto",
        "syntax = \"proto3\";\nmessage M { int32 x = 1; }\n",
    );
    s.write(
        "elsewhere/evil.proto",
        "syntax = \"proto3\";\nmessage E {}\n",
    );
    std::os::unix::fs::symlink(s.path("elsewhere"), s.path("root/link")).unwrap();
    let rules = s.write("rules.txt", "+ M\n");
    let out = pbtrim(&[
        "select".as_ref(),
        "--rules".as_ref(),
        &rules,
        "--root".as_ref(),
        &s.path("root"),
    ]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("symlink"), "{}", stderr(&out));
}

#[test]
fn help_and_version_go_to_stdout_and_exit_0() {
    let h = pbtrim(&["--help".as_ref()]);
    assert_eq!(code(&h), 0);
    assert!(
        stdout(&h).contains("pbtrim: proto3 preprocessor"),
        "{}",
        stdout(&h)
    );

    let v = pbtrim(&["--version".as_ref()]);
    assert_eq!(code(&v), 0);
    assert!(stdout(&v).starts_with("pbtrim "), "{}", stdout(&v));
    assert!(
        stdout(&v).trim().ends_with(env!("CARGO_PKG_VERSION")),
        "{}",
        stdout(&v)
    );
}

#[test]
fn double_dash_terminates_fmt_flags() {
    // A file literally named `--check` is reachable after `--`.
    let s = Scratch::new("dashdash");
    let f = s.write("--check", MESSY);
    let out = pbtrim(&["fmt".as_ref(), "--stdout".as_ref(), "--".as_ref(), &f]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), FORMATTED);
}

#[cfg(unix)]
#[test]
fn broken_pipe_exits_zero_not_101() {
    // `pbtrim fmt --stdout big | head -c1` closes the reader early; the
    // tool must finish with 0, not panic to 101.
    use std::fmt::Write as _;
    use std::process::{Command, Stdio};
    let s = Scratch::new("pipe");
    let mut big = String::from("syntax = \"proto3\";\n");
    for i in 0..5000 {
        let _ = writeln!(big, "message M{i} {{ int32 a = 1; }}");
    }
    let f = s.write("big.proto", &big);

    let mut child = Command::new(env!("CARGO_BIN_EXE_pbtrim"))
        .args(["fmt".as_ref(), "--stdout".as_ref(), f.as_os_str()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read one byte, then drop the read end to force EPIPE on the writer.
    {
        use std::io::Read as _;
        let mut out = child.stdout.take().unwrap();
        let mut one = [0u8; 1];
        let _ = out.read(&mut one);
    }
    let status = child.wait().unwrap();
    let code = status.code().unwrap_or(-1);
    assert_eq!(code, 0, "expected clean exit on broken pipe, got {code}");
}

#[test]
fn fmt_check_and_stdout_are_mutually_exclusive() {
    let s = Scratch::new("fmt-excl");
    let f = s.write("m.proto", FORMATTED);
    let out = pbtrim(&["fmt".as_ref(), "--check".as_ref(), "--stdout".as_ref(), &f]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("mutually exclusive"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn usage_errors_exit_2() {
    let out = pbtrim(&["frobnicate".as_ref()]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("unknown subcommand"),
        "{}",
        stderr(&out)
    );

    let out = pbtrim(&["select".as_ref()]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("requires --rules and --root"),
        "{}",
        stderr(&out)
    );

    let out = pbtrim(&["fmt".as_ref()]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("no input files"), "{}", stderr(&out));
}
