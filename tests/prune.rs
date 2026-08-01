//! Prune property tests (design: 减枝性质测试).
//!
//! - pruned output parses;
//! - the kept reachability set is complete: re-analyzing the pruned set
//!   succeeds, so there are no dangling references and the import set
//!   matches actual cross-file references;
//! - deleted field numbers appear in `reserved`;
//! - files with nothing kept vanish, along with imports of them;
//! - numbers are never renumbered.

use pbpp::FileSet;
use pbpp::prune::PruneOutput;
use pbpp::rules::parse_rules;
use pbpp::sema::analyze;

fn run(files: &[(&str, &str)], rules_src: &str) -> PruneOutput {
    let inputs: Vec<(String, &str)> = files.iter().map(|(p, s)| (p.to_string(), *s)).collect();
    let set = FileSet::parse(inputs).unwrap();
    let sema = analyze(&set).unwrap();
    let rules = parse_rules(rules_src).unwrap();
    let selected = pbpp::select(&set, &sema, &rules).unwrap();
    let out = pbpp::prune(&set, &sema, &selected);

    // Property: the pruned output re-parses and re-analyzes as a closed,
    // consistent set (no dangling references, imports match usage).
    let re_inputs: Vec<(String, &str)> = out
        .files
        .iter()
        .map(|f| (f.path.clone(), f.text.as_str()))
        .collect();
    if !re_inputs.is_empty() {
        let re_set = FileSet::parse(re_inputs).unwrap_or_else(|e| {
            panic!("pruned output does not parse: {}", e.message());
        });
        analyze(&re_set).unwrap_or_else(|e| {
            panic!("pruned output is not a closed set: {}", e.message());
        });
    }
    out
}

fn text_of<'o>(out: &'o PruneOutput, path: &str) -> &'o str {
    &out.files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} missing from output"))
        .text
}

const API: &str = r#"
syntax = "proto3";
package acme.v1;

import "acme/v1/types.proto";
import "google/protobuf/any.proto";

service Search {
  rpc Lookup(LookupRequest) returns (LookupResponse);
  rpc Purge(PurgeRequest) returns (PurgeResponse);
}

message LookupRequest {
  string q = 1;
  google.protobuf.Any hint = 2;
  int32 limit = 3;
}

message LookupResponse {
  repeated Item items = 1;
}

message PurgeRequest { string id = 1; }
message PurgeResponse { bool ok = 1; }
"#;

const TYPES: &str = r#"
syntax = "proto3";
package acme.v1;

message Item {
  string id = 1;
  Meta meta = 2;
}

message Meta { string etag = 1; }
message Unused { int32 x = 1; }
"#;

#[test]
fn end_to_end_prune() {
    let out = run(
        &[("acme/v1/api.proto", API), ("acme/v1/types.proto", TYPES)],
        "+ acme.v1.Search.Lookup @method\n-! google.protobuf.Any\n",
    );

    let api = text_of(&out, "acme/v1/api.proto");
    // Kept: the Lookup rpc and its request/response.
    assert!(api.contains("rpc Lookup"), "{api}");
    assert!(!api.contains("rpc Purge"), "{api}");
    assert!(!api.contains("PurgeRequest"), "{api}");
    // The cascaded Any field is gone; its number is reserved.
    assert!(!api.contains("google.protobuf.Any hint"), "{api}");
    assert!(api.contains("reserved 2;"), "{api}");
    // The Any import is no longer referenced and is dropped.
    assert!(!api.contains("google/protobuf/any.proto"), "{api}");
    // The types import survives (Item is still referenced).
    assert!(api.contains("acme/v1/types.proto"), "{api}");
    // Numbers are never renumbered.
    assert!(api.contains("int32 limit = 3;"), "{api}");

    let types = text_of(&out, "acme/v1/types.proto");
    assert!(types.contains("message Item"), "{types}");
    assert!(types.contains("message Meta"), "{types}");
    assert!(!types.contains("Unused"), "{types}");
}

#[test]
fn dropped_file_and_its_imports_vanish() {
    let a = r#"
syntax = "proto3";
package a;
import "b.proto";
import "c.proto";
message KeepMe { c.Used u = 1; }
message DropMe { b.OnlyHere x = 1; }
"#;
    let b = "syntax = \"proto3\";\npackage b;\nmessage OnlyHere { int32 x = 1; }\n";
    let c = "syntax = \"proto3\";\npackage c;\nmessage Used { int32 y = 1; }\n";

    let out = run(
        &[("a.proto", a), ("b.proto", b), ("c.proto", c)],
        "+ a.KeepMe\n",
    );
    assert_eq!(out.dropped, vec!["b.proto".to_string()]);
    let a_text = text_of(&out, "a.proto");
    assert!(!a_text.contains("b.proto"), "{a_text}");
    assert!(a_text.contains("c.proto"), "{a_text}");
    assert!(!a_text.contains("DropMe"), "{a_text}");
}

#[test]
fn oneof_emptied_is_deleted_whole() {
    let src = r#"
syntax = "proto3";
package pkg;

message Payload { bytes raw = 1; }

message M {
  string name = 1;
  oneof body {
    string text = 2;
    Payload heavy = 3;
  }
}
"#;
    // Drop both oneof fields via cascade on Payload plus a direct drop.
    let out = run(
        &[("pkg.proto", src)],
        "+ pkg.M\n- pkg.M.text @field\n-! pkg.Payload\n",
    );
    let text = text_of(&out, "pkg.proto");
    assert!(!text.contains("oneof"), "{text}");
    assert!(text.contains("reserved 2, 3;"), "{text}");
    assert!(text.contains("string name = 1;"), "{text}");
    assert!(!text.contains("Payload"), "{text}");
}

#[test]
fn partially_kept_oneof_keeps_shell() {
    let src = r#"
syntax = "proto3";
package pkg;

message M {
  oneof body {
    string text = 1;
    bytes blob = 2;
  }
}
"#;
    let out = run(&[("pkg.proto", src)], "+ pkg.M\n- pkg.M.blob @field\n");
    let text = text_of(&out, "pkg.proto");
    assert!(text.contains("oneof body"), "{text}");
    assert!(text.contains("string text = 1;"), "{text}");
    assert!(!text.contains("blob"), "{text}");
    assert!(text.contains("reserved 2;"), "{text}");
}

#[test]
fn enum_value_deletion_reserves_number() {
    let src = r#"
syntax = "proto3";
package pkg;

enum E {
  E_UNSPECIFIED = 0;
  E_OLD = 1;
  E_NEGATIVE = -5;
}
"#;
    let out = run(
        &[("pkg.proto", src)],
        "+ pkg.E\n- pkg.E.E_OLD @value\n- pkg.E.E_NEGATIVE @value\n",
    );
    let text = text_of(&out, "pkg.proto");
    assert!(!text.contains("E_OLD"), "{text}");
    assert!(text.contains("reserved -5, 1;"), "{text}");
}

#[test]
fn comments_follow_their_nodes() {
    let src = r#"
syntax = "proto3";
package pkg;

// Leading comment on Kept.
message Kept { int32 a = 1; }

// Leading comment on Dropped: goes with it.
message Dropped { int32 b = 1; }
"#;
    let out = run(&[("pkg.proto", src)], "+ pkg.Kept\n");
    let text = text_of(&out, "pkg.proto");
    assert!(text.contains("// Leading comment on Kept."), "{text}");
    assert!(!text.contains("Dropped"), "{text}");
}

#[test]
fn container_service_keeps_only_selected_methods() {
    let out = run(
        &[("acme/v1/api.proto", API), ("acme/v1/types.proto", TYPES)],
        "acme.v1 {\n  + Search.Purge @method\n}\n",
    );
    let api = text_of(&out, "acme/v1/api.proto");
    assert!(api.contains("rpc Purge"), "{api}");
    assert!(!api.contains("rpc Lookup"), "{api}");
    // Lookup's types are not pulled in; the types import dies with them.
    assert!(!api.contains("LookupRequest"), "{api}");
    assert!(!api.contains("acme/v1/types.proto"), "{api}");
    // types.proto has nothing kept.
    assert_eq!(out.dropped, vec!["acme/v1/types.proto".to_string()]);
}

#[test]
fn definition_free_public_bridge_survives() {
    // Regression: a re-export file with no definitions was treated as
    // "nothing kept" and dropped, leaving the importer's reference
    // unresolvable (run() re-analyzes the output and would panic).
    let app = r#"
syntax = "proto3";
package app;
import "bridge1.proto";
message M { leaf.User u = 1; }
"#;
    let bridge1 = "syntax = \"proto3\";\npackage b1;\nimport public \"bridge2.proto\";\n";
    let bridge2 = "syntax = \"proto3\";\npackage b2;\nimport public \"leaf.proto\";\n";
    let leaf = "syntax = \"proto3\";\npackage leaf;\nmessage User { string name = 1; }\n";
    let out = run(
        &[
            ("app.proto", app),
            ("bridge1.proto", bridge1),
            ("bridge2.proto", bridge2),
            ("leaf.proto", leaf),
        ],
        "+ app.M\n",
    );
    assert!(out.dropped.is_empty(), "dropped: {:?}", out.dropped);
    let b1 = text_of(&out, "bridge1.proto");
    assert!(b1.contains("import public \"bridge2.proto\""), "{b1}");
    let b2 = text_of(&out, "bridge2.proto");
    assert!(b2.contains("import public \"leaf.proto\""), "{b2}");
    let a = text_of(&out, "app.proto");
    assert!(a.contains("import \"bridge1.proto\""), "{a}");
}

#[test]
fn unneeded_bridge_is_still_dropped() {
    // The bridge fixpoint keeps only demanded bridges: one that carries
    // nothing referenced goes away with its importer's import line.
    let app = r#"
syntax = "proto3";
package app;
import "bridge.proto";
message M { string s = 1; }
"#;
    let bridge = "syntax = \"proto3\";\npackage b1;\nimport public \"leaf.proto\";\n";
    let leaf = "syntax = \"proto3\";\npackage leaf;\nmessage Unused { int32 x = 1; }\n";
    let out = run(
        &[
            ("app.proto", app),
            ("bridge.proto", bridge),
            ("leaf.proto", leaf),
        ],
        "+ app.M\n",
    );
    assert_eq!(
        out.dropped,
        vec!["bridge.proto".to_string(), "leaf.proto".to_string()]
    );
    let a = text_of(&out, "app.proto");
    assert!(!a.contains("bridge.proto"), "{a}");
}

#[test]
fn enum_alias_reserved_respects_survivors() {
    // Regression: deleting an alias reserved its number even when a
    // surviving alias still used it (illegal proto), and two dropped
    // aliases reserved the shared number twice.
    let src = r#"
syntax = "proto3";
package pkg;

enum E {
  option allow_alias = true;
  E_UNSPECIFIED = 0;
  E_A = 1;
  E_A_ALIAS = 1;
  E_B = 2;
  E_B_ALIAS = 2;
}
"#;
    let out = run(
        &[("pkg.proto", src)],
        "+ pkg.E\n- pkg.E.E_A_ALIAS @value\n- pkg.E.E_B @value\n- pkg.E.E_B_ALIAS @value\n",
    );
    let text = text_of(&out, "pkg.proto");
    // 1 is still in use by the surviving alias: not reserved.
    assert!(text.contains("E_A = 1;"), "{text}");
    assert!(!text.contains("E_A_ALIAS"), "{text}");
    // Both aliases of 2 dropped: reserved exactly once.
    assert!(text.contains("reserved 2;"), "{text}");
    // No kept values alias each other any more: `allow_alias = true`
    // would be rejected by protoc, so it is deleted with them.
    assert!(!text.contains("allow_alias"), "{text}");
}

#[test]
fn allow_alias_survives_while_aliasing_does() {
    let src = r#"
syntax = "proto3";
package pkg;

enum E {
  option allow_alias = true;
  E_UNSPECIFIED = 0;
  E_A = 1;
  E_A_ALIAS = 1;
  E_B = 2;
}
"#;
    // Only the non-aliased value goes: aliasing is still in use.
    let out = run(&[("pkg.proto", src)], "+ pkg.E\n- pkg.E.E_B @value\n");
    let text = text_of(&out, "pkg.proto");
    assert!(text.contains("allow_alias = true"), "{text}");
    assert!(text.contains("reserved 2;"), "{text}");
}

#[test]
fn option_only_oneof_is_not_deleted() {
    // Degenerate but parseable: a oneof holding only options. Pruning was
    // not asked to touch it, so it must survive a kept parent.
    let src = r#"
syntax = "proto3";
package pkg;

message M {
  string name = 1;
  oneof o {
    option deprecated = true;
  }
}
"#;
    let out = run(&[("pkg.proto", src)], "+ pkg.M\n");
    let text = text_of(&out, "pkg.proto");
    assert!(text.contains("oneof o"), "{text}");
    assert!(text.contains("deprecated"), "{text}");
}

#[test]
fn import_public_line_survives_while_target_does() {
    let a = r#"
syntax = "proto3";
package a;
import "mid.proto";
message M { b.User u = 1; }
"#;
    let mid = "syntax = \"proto3\";\npackage mid;\nimport public \"b.proto\";\nmessage Marker { int32 x = 1; }\n";
    let b = "syntax = \"proto3\";\npackage b;\nmessage User { string name = 1; }\n";
    let out = run(
        &[("a.proto", a), ("mid.proto", mid), ("b.proto", b)],
        "+ a.M\n+ mid.Marker\n",
    );
    let mid_text = text_of(&out, "mid.proto");
    // The public import is a visibility statement: a.proto reaches b.User
    // through it, so it must survive.
    assert!(mid_text.contains("import public \"b.proto\""), "{mid_text}");
}
