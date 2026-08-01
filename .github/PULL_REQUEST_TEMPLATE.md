<!-- Describe what changes and why. Link any issue it closes. -->

## Checks

<!-- CI runs all of these; ticking them locally first saves a round trip. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings` (deviations use
      `#[expect(..., reason = "...")]`, never a bare `allow`)
- [ ] `cargo test` and `cargo test --release`
- [ ] New behavior has a test; a fixed bug has a regression test pinned
      to the boundary that broke
- [ ] `CHANGELOG.md` `[Unreleased]` updated if the change is user-visible

## Notes

<!--
Performance claims need evidence: `cargo run --release --example bench`
before/after, and `-Zprint-type-sizes` for layout changes.
New filesystem side effects belong in `pbpp::fs`, not around it.
-->
