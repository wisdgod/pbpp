//! The reference build.rs for driving pbpp from a build script: discover
//! inputs deterministically, prune by rules, sync into `OUT_DIR` — all
//! through `pbpp::fs`, which owns path validation and atomic writes.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let root = manifest.join("proto");
    let rules_path = manifest.join("trim.rules");

    // Rerun on rules changes, on tree changes (file added/removed), and on
    // every discovered input (file edited).
    println!("cargo::rerun-if-changed={}", rules_path.display());
    println!("cargo::rerun-if-changed={}", root.display());
    let inputs = pbpp::fs::discover(&root).expect("discover .proto inputs");
    for (rel, _) in &inputs {
        println!("cargo::rerun-if-changed={}", root.join(rel).display());
    }

    let pipeline = pbpp::Pipeline::new(
        inputs.iter().map(|(p, s)| (p.clone(), s.as_str())).collect(),
    )
    .expect("parse + analyze the proto set");
    let rules_src = std::fs::read_to_string(&rules_path).expect("read trim.rules");
    let rules = pbpp::rules::parse_rules(&rules_src).expect("parse trim.rules");
    let mut out = pipeline.prune(&rules).expect("selection");
    out.format();

    let dest = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets it")).join("proto");
    let report = pbpp::fs::sync(&dest, &out.files).expect("sync outputs into OUT_DIR");
    assert!(!report.written.is_empty(), "nothing was written");
}
