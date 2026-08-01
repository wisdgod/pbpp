# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
