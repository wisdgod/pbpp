# Security Policy

## Supported versions

pbpp is pre-1.0. Security fixes land on the latest published `0.x`
release; older `0.x` versions are not maintained.

## Reporting a vulnerability

Please report privately rather than in a public issue, via GitHub's
[private vulnerability reporting](https://github.com/wisdgod/pbpp/security/advisories/new)
(Security → Report a vulnerability). If that form is unavailable to you,
open a public issue containing only a request for a private channel — no
details — and a maintainer will follow up.

Include a description, the affected version(s), and a reproduction if you
have one. Expect an acknowledgement within a few days.

## Threat model

pbpp's core is pure and in-memory. The security-relevant surface is the
filesystem boundary (`pbpp::fs`) and the `pbtrim` CLI, which read a
source tree and write pruned output:

- **Discovery** refuses symlinks and rejects non-UTF-8 or control-bearing
  paths, so a hostile input tree cannot escape the root or cause a scan
  cycle.
- **Import paths** are validated (relative, `/`-separated, no
  `.`/`..`/empty segments, no control characters), so
  `out_dir.join(path)` stays contained.
- **Output writes** create temp files with `O_EXCL` semantics under
  unpredictable names and rename into place, so a symlink or hardlink
  pre-planted *at a write target* cannot redirect or truncate an
  unrelated file.
- **Output sync** holds an exclusive directory lock, validates the
  existing manifest before acting, and removes only files it previously
  recorded — never files it did not record.

**In scope.** With the output directory owned by this tool, pbpp must not
let a crafted import path escape it, let a symlink or hardlink planted at
a write target redirect a write, or let sync delete a file it never
recorded. Reports about those are welcome.

**Out of scope.** pbpp is not a sandbox for adversarial proto *content*
(inputs are sources you already build with), and — as
[`pbpp::fs`](src/fs.rs) documents — it does not defend the output
directory against a hostile *external process* mutating the tree
concurrently: containment is checked at resolve time, not enforced
against a racing filesystem (parent-directory swap / TOCTOU), and a path
pbpp is legitimately asked to write is overwritten even if a foreign file
sits there. The directory lock discourages concurrent `pbtrim` runs but
is advisory, not a security control.
