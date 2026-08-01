# pbpp

[![crates.io](https://img.shields.io/crates/v/pbpp.svg)](https://crates.io/crates/pbpp)
[![docs.rs](https://img.shields.io/docsrs/pbpp)](https://docs.rs/pbpp)
[![CI](https://github.com/wisdgod/pbpp/actions/workflows/ci.yml/badge.svg)](https://github.com/wisdgod/pbpp/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](Cargo.toml)

A protobuf preprocessor: `.proto` in, `.proto` out. pbpp takes a set of
proto3 files and trims and normalizes them — selection-driven pruning
plus a deterministic formatter. It never generates target-language code;
that is a job for whatever consumes its output.

**pbpp is a library first**; `pbtrim` is a thin CLI shell over it (the
repo/command name split follows the ripgrep/`rg` precedent).

## Why

Vendored or upstream-dumped proto trees usually carry far more surface
than a consumer wants to expose or maintain. pbpp lets you keep exactly
the messages, fields, and RPCs you name — everything referenced follows
automatically, everything else is deleted with wire compatibility
preserved (`reserved` is inserted for every deleted number; numbers are
never reused or renumbered).

Design stance, in one line: **strict over tolerant**. Input outside the
proto3 grammar is a located error, a selector rule that matches nothing
is an error, and semantic-changing operations (cascade deletion) must be
opted into explicitly.

## Library usage

```rust
// Single file: parse + format.
let cst = pbpp::parse(src)?;
let formatted = pbpp::format(&cst);

// File set: select + prune.
let pipeline = pbpp::Pipeline::new(inputs)?; // (import_path, source) pairs
let rules = pbpp::rules::parse_rules("+ acme.v1.**\n-! google.protobuf.Any\n")?;
// Rules can also be built programmatically:
// let mut rules = pbpp::RuleSet::new();
// rules.keep("acme.v1.**")?.drop_cascade("google.protobuf.Any")?;
let selected = pipeline.select(&rules)?;  // marks only, nothing modified
let mut pruned = pipeline.prune(&rules)?; // original text minus deletions
pruned.format();                          // optional normalization pass
```

- Layered API: `lex`/`parse`/`cst` (lossless CST), `format` (the one
  printer), `fileset`/`sema` (symbol table, visibility, reference
  resolution), `rules`/`select`, `prune`, `digest` (semantic-equality
  oracle), `fs` (the only side-effecting layer), with `Pipeline` as the
  orchestration façade — every stage stays public.
- Self-contained errors: `pbpp::Error` implements `std::error::Error`,
  carries file/line/column, the offending source line, and notes;
  `Display` renders caret diagnostics.
- Selection answers "why": `Selected::mark`/`is_kept`/`introduced_by`
  (the reference chain that pulled a node in) / `deciding_rule`.

## CLI usage

```text
pbtrim fmt <files...>            # reformat in place
pbtrim fmt --check <files...>    # exit 1 if anything would change
pbtrim fmt --stdout <file>       # print instead of writing

pbtrim select --rules <file> --root <dir>              # report the keep set
pbtrim prune  --rules <file> --root <dir> --out <dir>  # materialize
```

`prune --out` has **manifest sync semantics**: the output directory
mirrors the pruned set. Writes are atomic (temp file + rename); files a
previous run wrote but this run did not produce are removed — removal is
strictly scoped to the `.pbtrim-manifest` ownership record, so foreign
files are never touched. Re-runs are idempotent. Safety boundaries:
`--out` and `--root` must be disjoint; symlinks in the input tree are
refused; import paths must be relative, `/`-separated, with no
`..`/`.`/empty segments.

## Selector DSL

Line-oriented, later rules override earlier ones (gitignore precedence);
`#` comments:

```text
# keep the whole package, minus one message
+ acme.api.v1.**
- acme.api.v1.LegacyThing

# cascade: delete every field typed Any, reserving its number
-! google.protobuf.Any

# scope block: the prefix applies to nested rules
acme.api.v1 {
  + Search.Lookup @method
  - User.email @field
}
```

- Pattern segments: literal, `*` (exactly one segment), `**` (≥0 in the
  middle, ≥1 at the end); a rule matching a node also covers its subtree.
- `@kind` qualifiers: `message` / `enum` / `service` / `field` /
  `method` / `value`.
- A rule that matches nothing is an error (typo / upstream-rename
  signal); an exclusion still reachable from the kept set is an error
  with the reference chain, unless `-!` explicitly opts into cascading.

## Use in CI

Exit codes are stable: `0` success, `1` only for `fmt --check` drift,
`2` any error (usage, parse, rules). Diagnostics go to stderr with file,
line/column, and a caret.

```yaml
# Formatting gate (NUL-delimited: safe for paths with spaces)
- run: git ls-files -z '*.proto' | xargs -0 --no-run-if-empty pbtrim fmt --check

# Keep pruned artifacts in sync with the rules (manifest sync is
# idempotent, so drift shows up as a plain git diff)
- run: pbtrim prune --rules trim.rules --root proto/ --out gen/proto/
- run: git diff --exit-code gen/proto/
```

The repository also ships a composite action (`action.yml`), so you can
skip the manual install (a Rust toolchain must already be on the runner):

```yaml
- uses: dtolnay/rust-toolchain@stable
# Formatting gate over every tracked .proto:
- uses: wisdgod/pbpp@v0.1.0
  with:
    command: fmt-check
# Or prune into a directory:
- uses: wisdgod/pbpp@v0.1.0
  with:
    command: prune
    rules: trim.rules
    root: proto
    out: gen/proto
```

`command` is one of `fmt-check` / `fmt` / `select` / `prune`; set
`version:` to install a pinned release from crates.io instead of building
from the action's source.

## Use in build.rs

Discovery, path validation, manifest sync, and atomic writes all go
through `pbpp::fs` — do not hand-roll `dest.join(path)`:

```rust
// [build-dependencies] pbpp = "0.1"
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("proto");
    let rules_path = manifest.join("trim.rules");

    // Rerun on rules changes, tree changes, and per-file edits.
    println!("cargo::rerun-if-changed={}", rules_path.display());
    println!("cargo::rerun-if-changed={}", root.display());
    let inputs = pbpp::fs::discover(&root).unwrap(); // sorted, symlink-refusing
    for (rel, _) in &inputs {
        println!("cargo::rerun-if-changed={}", root.join(rel).display());
    }

    let pipeline = pbpp::Pipeline::new(
        inputs.iter().map(|(p, s)| (p.clone(), s.as_str())).collect(),
    ).unwrap();
    let rules = pbpp::rules::parse_rules(&std::fs::read_to_string(&rules_path).unwrap()).unwrap();
    let mut out = pipeline.prune(&rules).unwrap();
    out.format();

    let dest = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("proto");
    pbpp::fs::sync(&dest, &out.files).unwrap(); // atomic writes + manifest sync
}
```

Consume with
`include_str!(concat!(env!("OUT_DIR"), "/proto/acme/v1/api.proto"))`.
A complete, compiled reference lives in the repository under
[`fixtures/buildrs-consumer`](https://github.com/wisdgod/pbpp/tree/main/fixtures/buildrs-consumer),
exercised end to end by `tests/buildrs.rs` (the fixture is not part of
the published crate, so that test skips when run from a packaged
source).

## Design highlights

- **Lossless by keeping the original text**: every addressable node and
  semantic word carries its byte span; comments are fully preserved with
  parse-time attachment. Pruned output is the input minus deleted spans —
  options, comments, unknown-to-selection constructs survive because
  they are never rebuilt.
- **Strict proto3 coverage**: the full grammar including custom option
  paths and aggregate (text-format) literals; proto2 constructs
  (`required`/`group`/`extend`/`extensions`) produce targeted errors.
- **Formatter properties locked by tests**: idempotence, semantic
  preservation (checked against an independently implemented canonical
  digest), stable comment attachment; definition order is never changed.
- **Pruning correctness**: reachability closure over field types, RPC
  signatures, and `import public` visibility chains; deleted numbers are
  reserved; definition-free re-export bridge files survive while they
  still carry a needed provider; enum output stays legal (zero value,
  non-emptiness, `allow_alias` hygiene).
- **Two-layer architecture**: the core is pure and deterministic in
  memory; all filesystem effects live in `pbpp::fs` and the CLI.

## Platform support

CI runs the test suite on Linux, macOS, and Windows (MSRV 1.88 and
stable). The filesystem boundary (`pbpp::fs`) is written to be
cross-platform, but note that atomic replacement and case-sensitivity are
filesystem-dependent; report any platform-specific surprises.

## Building and testing

The only third-party dependency is `rustc-hash`. MSRV is 1.88. The
published crate carries just the library, binary, and docs; tests,
examples, and CI live in the
[repository](https://github.com/wisdgod/pbpp):

```sh
cargo build --release
cargo test
cargo run --release --example bench   # throughput / allocation benchmark
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
