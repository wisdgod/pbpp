//! Compiles and runs the fixture crate whose `build.rs` drives pbpp —
//! the end-to-end proof that the build-script story (discovery, prune,
//! `OUT_DIR` sync, `include_str!` consumption) actually works.
//!
//! The fixture is a standalone crate with its own target directory, so
//! the nested cargo invocation does not contend with this test run's
//! locks; repeat runs are incremental.

#[test]
fn buildrs_fixture_compiles_and_runs() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/buildrs-consumer");
    if !dir.exists() {
        // fixtures/ is excluded from the published package; running the
        // suite from a crates.io tarball must not fail on it.
        eprintln!("fixture not present (packaged source); skipping");
        return;
    }
    let out = std::process::Command::new(env!("CARGO"))
        .args(["run", "--quiet"])
        .current_dir(&dir)
        .output()
        .expect("cargo is runnable");
    assert!(
        out.status.success(),
        "fixture build/run failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
}
