# Releasing

Releases are published from a `v*` tag by
[`.github/workflows/release.yml`](.github/workflows/release.yml) using
crates.io trusted publishing, so no API token is stored anywhere.

## Setup (one-time)

Trusted publishing cannot create a crate, so bootstrap with a **manual
prerelease**, then let the workflow ship every real version — including
the first stable one. Do not manually publish a version you also intend
to tag: the tag would re-run the workflow and fail on the duplicate.

1. Manually publish a prerelease placeholder to create the crate — a
   short-lived token scoped `publish-new` for `pbpp`, then
   `cargo publish --locked` at version `0.1.0-alpha.0`, then revoke the
   token. Version requirements ignore prereleases, so nothing depends on
   it, and the real `0.1.0` still goes out through the tagged workflow.
2. On the crate's crates.io page add a trusted publisher: this
   repository, workflow `release.yml`, environment `release`.
3. Create the `release` environment in the GitHub repository settings
   (optionally with required reviewers) so publishing is gated.
4. Protect `main` (require CI to pass): the release workflow refuses to
   publish a tag whose commit is not contained in `origin/main`, which
   only means something if `main` itself is protected.
5. Enable **private vulnerability reporting** (Settings → Security) so
   the channel `SECURITY.md` points at actually exists.

## Per release

1. `main` is green in CI (OS matrix, MSRV + stable, lint, docs, package,
   deny).
2. Bump `version` in `Cargo.toml`; move `CHANGELOG.md`'s `[Unreleased]`
   entries under a new dated heading.
3. From a clean tree:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --all-targets --locked -- -D warnings
   cargo test --locked && cargo test --locked --release
   RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
   cargo publish --dry-run --locked
   ```

4. Commit, tag, push:

   ```sh
   git tag -a vX.Y.Z -m "pbpp X.Y.Z"
   git push origin main --follow-tags
   ```

5. The workflow re-runs the gates against the tagged tree and publishes.
   Verify [crates.io](https://crates.io/crates/pbpp) and
   [docs.rs](https://docs.rs/pbpp), then cut the GitHub release from the
   tag using that changelog section as its notes.

A bad release is yanked (`cargo yank --version X.Y.Z`), which removes it
from new dependency resolution but does not delete it; fix forward with a
patch version.
