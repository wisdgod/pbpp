# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Composite action: running without a checkout no longer passes
  silently. Default `.proto` discovery ran `git ls-files` inside a
  process substitution, whose failure `set -e` cannot see — with no
  repository the step reported "nothing to do" and exited 0 having
  checked nothing. The action now probes `git rev-parse
  --is-inside-work-tree` first and fails with a clear error (exit 2)
  telling the user to add `actions/checkout` or pass explicit `paths`.

### Changed

- `extend` diagnostics no longer mislabel the construct as proto2-only:
  proto3 permits `extend` for custom options. pbpp still rejects all
  `extend` blocks, but the message now says the construct is
  unsupported (planned for 0.2.0 alongside the breaking CST changes
  it needs).

### Documentation

- README CI recipe: the prune drift check now stages the output
  directory before diffing (`git add -A` + `git diff --cached
  --exit-code`) — `git diff` alone cannot report newly created files,
  so a rules change that adds a file passed the old check silently.
- README: dedicated MSRV section — 1.88 (floor set by let-chains),
  declared in `Cargo.toml` and enforced by CI, with the bump policy
  (changelog-called-out, minor releases only, never a patch).
- Proto3 coverage claims corrected (README and rustdoc): the known
  grammar gaps are documented — custom-option `extend` is rejected,
  and adjacent string-literal concatenation is accepted in option
  values but not in `syntax`/`import`/`reserved` positions — as is the
  preprocessor/checker boundary: protoc-level semantic checks pbpp
  does not duplicate (duplicate field numbers, `reserved` conflicts)
  remain protoc's job.

## [0.1.0] - 2026-08-02

Initial release.

### Added

- Lossless proto3 parser producing a span-carrying CST with comment
  attachment decided at parse time; strict grammar (no unknown-construct
  recovery, proto2 constructs produce targeted diagnostics, a 256-level
  nesting cap keeps every pass on bounded stack).
- Formatter: the pipeline's single printer — idempotent,
  semantics-preserving, stable comment attachment; locked down by corpus
  round-trip tests against an independently implemented semantic digest.
- Selection: ordered gitignore-style rules DSL (`+`/`-`/`-!`, `*`/`**`
  globs, `@kind` qualifiers, scope blocks) evaluated to EXPLICIT/REQUIRED
  marks with a reachability closure; conflicts and dead rules are located
  errors.
- Pruning: deletion of the unselected complement with `reserved`
  insertion for deleted numbers, import/oneof/file cascade cleanup,
  `import public` bridge preservation, and enum-legality guarantees
  (zero value, non-empty, `allow_alias` hygiene).
- `pbpp::fs`, the only side-effecting layer: deterministic
  symlink-refusing discovery, import-path validation (relative, UTF-8,
  no control characters or `..`), `write_atomic` (unpredictable `O_EXCL`
  temp plus rename, so a planted symlink cannot redirect a write), and a
  manifest-tracked `sync` that holds an exclusive directory lock,
  preflights all state, and commits its manifest last.
- `pbtrim` CLI: `fmt` (in-place / `--check` / `--stdout`), `select`
  (keep-set report), `prune` (manifest-synced output directory), plus
  `--help`, `--version`, and a `--` terminator. Stable exit codes
  (0 ok / 1 fmt-check drift / 2 error); a broken output pipe exits 0.
- A composite GitHub Action (`action.yml`) exposing `pbtrim` as
  `uses: wisdgod/pbpp@v…` with a `fmt-check`/`fmt`/`select`/`prune`
  command input.
- Reference `build.rs` consumer under `fixtures/`, compiled and run by
  the test suite.
