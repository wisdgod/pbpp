# Contributing

## Toolchain

Stable Rust ≥ 1.88 (the crate's `rust-version`). No nightly features are
used; nightly is only handy for extras like `-Zprint-type-sizes`.

## Checks

Everything CI runs, runnable locally:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --release          # boundary tests differ under optimization
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

## Lint policy

`clippy::all` + `pedantic` + `nursery` are enabled in `Cargo.toml
[lints]` and the tree stays at **zero unjustified warnings**. Intentional
deviations use `#[expect(lint, reason = "...")]` at the site — never a
bare `#[allow]` — so a lint that stops firing is itself a signal.

## Design ground rules

- **Strict over tolerant**: inputs outside the proto3 grammar are located
  errors; no silent recovery, no guessed defaults. Every configuration
  item must have an effect (a rule matching nothing is an error).
- **Losslessness by keeping the original text**: pruning's output is the
  input minus deleted spans; the formatter is the only printer.
- **The core is pure**: filesystem side effects live in `pbpp::fs` and
  the CLI only. New I/O goes through that boundary (path validation,
  manifest sync, atomic writes), not around it.
- Performance claims need evidence: the allocation-counting benchmark
  (`cargo run --release --example bench`) before/after, and
  `-Zprint-type-sizes` for layout changes.

## Tests

New behavior needs a test in the matching suite: `tests/errors.rs`
(diagnostics), `tests/roundtrip.rs` (format properties),
`tests/select.rs` / `tests/prune.rs` (selection/prune semantics),
`tests/cli.rs` (process-level CLI contract). Fixed bugs get a regression
test pinned to the boundary that broke.

## Commits

Conventional Commits (`feat:`, `fix:`, `chore:`, ...); the subject states
the intent, the body states the why.
