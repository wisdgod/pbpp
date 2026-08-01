# Test corpus provenance

The round-trip test (`tests/roundtrip.rs`) runs `parse → format → parse`
over every `.proto` here, asserting semantic equality and formatting
idempotence.

| Source | Files | Notes |
|---|---|---|
| protobuf upstream (vendored) | `google/protobuf/*.proto` (10 well-known types) | Copied from [protocolbuffers/protobuf](https://github.com/protocolbuffers/protobuf) `src/google/protobuf/`. The exact upstream tag/commit was **not recorded**; pin it here when refreshing. **Excluded from the published crate** (`Cargo.toml` `exclude`) so no third-party code ships without pinned provenance — present only for local tests. |
| this repository | `minimal.proto`, `basic.proto`, `comments.proto`, `options.proto` | Targeted samples: a minimal file, a mix of common constructs, comment-attachment edges, and option/aggregate grammar. These ship with the crate. |

The round-trip test iterates whatever `.proto` files are present, so it
still runs against the repository samples when the vendored corpus is
absent (as in the packaged crate).

## Known gap

The test strategy calls for real proto corpora (an upstream dump plus the
well-known types). The well-known set is covered; a **real upstream dump
is not yet wired in**. When adding one:

1. record its source, version/commit, and fetch date;
2. if it contains proto2 files, put them in a separate directory and use
   them in the error-path tests (pbpp is proto3-only; proto2 input must
   produce a located diagnostic);
3. add an independent oracle (protoc `--descriptor_set_out` comparison)
   as a second evidence source beside the digest.
