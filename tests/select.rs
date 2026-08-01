//! Selection property tests (design: 选择性质测试 + 错误路径).
//!
//! Given selectors and a corpus, assert the selected set — EXPLICIT and
//! REQUIRED asserted separately; rule override order, scope nesting, kind
//! qualifiers, method-level selection each have cases; a blacklisted node
//! inside the kept closure errors with the reference chain.

use pbpp::FileSet;
use pbpp::rules::parse_rules;
use pbpp::select::{Mark, Selected, select};
use pbpp::sema::{Sema, analyze};

/// Parses files + rules and runs selection.
fn run<'a>(files: &[(&str, &'a str)], rules_src: &str) -> Result<(Sema<'a>, Selected), String> {
    let inputs: Vec<(String, &str)> = files.iter().map(|(p, s)| (p.to_string(), *s)).collect();
    let set = FileSet::parse(inputs).map_err(|e| e.message().to_string())?;
    let sema = analyze(&set).map_err(|e| e.message().to_string())?;
    let rules = parse_rules(rules_src).map_err(|d| d.message().to_string())?;
    let selected = select(&set, &sema, &rules).map_err(|e| {
        let notes: Vec<String> = e.notes().iter().map(|n| n.message.clone()).collect();
        format!("{}\n{}", e.message(), notes.join("\n"))
    })?;
    Ok((sema, selected))
}

fn mark(sema: &Sema<'_>, sel: &Selected, fq: &str) -> Mark {
    let id = sema
        .lookup_fq(fq)
        .unwrap_or_else(|| panic!("no symbol `{fq}`"));
    sel.mark(id)
}

fn cascaded(sema: &Sema<'_>, sel: &Selected, fq: &str) -> bool {
    let id = sema.lookup_fq(fq).unwrap();
    sel.is_cascade_dropped(id)
}

const BASE: &str = r#"
syntax = "proto3";
package pkg;

message A {
  B b = 1;
  string name = 2;
}

message B {
  int32 x = 1;
}

message C {
  int32 y = 1;
}
"#;

#[test]
fn explicit_and_required_closure() {
    let (sema, sel) = run(&[("pkg.proto", BASE)], "+ pkg.A\n").unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.A"), Mark::Explicit);
    // Fields of an explicitly kept message inherit the keep.
    assert_eq!(mark(&sema, &sel, "pkg.A.b"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "pkg.A.name"), Mark::Explicit);
    // B is pulled in by the closure, distinctly marked REQUIRED.
    assert_eq!(mark(&sema, &sel, "pkg.B"), Mark::Required);
    assert_eq!(mark(&sema, &sel, "pkg.B.x"), Mark::Required);
    // C is unreferenced and unselected.
    assert_eq!(mark(&sema, &sel, "pkg.C"), Mark::None);

    // Provenance: B was introduced by the field A.b.
    let b = sema.lookup_fq("pkg.B").unwrap();
    let by = sel.introduced_by(b).unwrap();
    assert_eq!(sema.fq(by), "pkg.A.b");
}

#[test]
fn later_rules_override_earlier() {
    // Keep everything, then drop C (unreferenced): C goes.
    let (sema, sel) = run(&[("pkg.proto", BASE)], "+ pkg.**\n- pkg.C\n").unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.A"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "pkg.C"), Mark::None);

    // Reversed order: the keep wins because it is written later.
    let (sema, sel) = run(&[("pkg.proto", BASE)], "- pkg.C\n+ pkg.**\n").unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.C"), Mark::Explicit);
}

#[test]
fn field_level_selection_keeps_container_shell() {
    let (sema, sel) = run(&[("pkg.proto", BASE)], "+ pkg.A.name @field\n").unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.A.name"), Mark::Explicit);
    // The message is kept only as a container; its other fields stay out.
    assert_eq!(mark(&sema, &sel, "pkg.A"), Mark::Container);
    assert_eq!(mark(&sema, &sel, "pkg.A.b"), Mark::None);
    assert_eq!(mark(&sema, &sel, "pkg.B"), Mark::None);
}

#[test]
fn dropped_field_inside_kept_message() {
    let (sema, sel) = run(&[("pkg.proto", BASE)], "+ pkg.A\n- pkg.A.b @field\n").unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.A"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "pkg.A.b"), Mark::None);
    // Since the only reference to B is dropped, B stays out.
    assert_eq!(mark(&sema, &sel, "pkg.B"), Mark::None);
}

const SVC: &str = r#"
syntax = "proto3";
package pkg;

message In { int32 a = 1; }
message Out { int32 b = 1; }
message OtherIn { int32 c = 1; }
message OtherOut { int32 d = 1; }

service Search {
  rpc Foo(In) returns (Out);
  rpc Bar(OtherIn) returns (OtherOut);
}
"#;

#[test]
fn method_level_selection_is_first_class() {
    let (sema, sel) = run(&[("svc.proto", SVC)], "+ pkg.Search.Foo @method\n").unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.Search.Foo"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "pkg.Search"), Mark::Container);
    assert_eq!(mark(&sema, &sel, "pkg.Search.Bar"), Mark::None);
    // Foo's input/output are REQUIRED; Bar's are untouched.
    assert_eq!(mark(&sema, &sel, "pkg.In"), Mark::Required);
    assert_eq!(mark(&sema, &sel, "pkg.Out"), Mark::Required);
    assert_eq!(mark(&sema, &sel, "pkg.OtherIn"), Mark::None);
    assert_eq!(mark(&sema, &sel, "pkg.OtherOut"), Mark::None);
}

#[test]
fn scope_nesting_and_kind_qualifier() {
    let rules = "pkg {\n  + Search.Foo @method\n}\n";
    let (sema, sel) = run(&[("svc.proto", SVC)], rules).unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.Search.Foo"), Mark::Explicit);

    // Kind mismatch: `@enum` on a message-only corpus hits nothing.
    let err = run(&[("pkg.proto", BASE)], "+ pkg.A @enum\n").unwrap_err();
    assert!(err.contains("matches nothing"), "{err}");
}

#[test]
fn zero_hit_rule_errors_with_package_hint() {
    let err = run(&[("pkg.proto", BASE)], "+ pkg\n").unwrap_err();
    assert!(err.contains("matches nothing"), "{err}");
    assert!(err.contains("append `.**`"), "{err}");
}

#[test]
fn empty_rules_error() {
    let err = run(&[("pkg.proto", BASE)], "# nothing\n").unwrap_err();
    assert!(err.contains("no rules"), "{err}");
}

#[test]
fn empty_programmatic_rule_set_errors() {
    // `RuleSet::new()` without a single `push` must error in selection,
    // not silently select nothing (which pruning would turn into
    // "delete everything").
    let inputs = vec![("pkg.proto".to_string(), BASE)];
    let set = FileSet::parse(inputs).unwrap();
    let sema = analyze(&set).unwrap();
    let err = select(&set, &sema, &pbpp::RuleSet::new()).unwrap_err();
    assert!(err.message().contains("no rules"), "{}", err.message());
}

#[test]
fn blacklist_hitting_closure_errors_with_chain() {
    let err = run(&[("pkg.proto", BASE)], "+ pkg.A\n- pkg.B\n").unwrap_err();
    assert!(err.contains("cannot exclude `pkg.B`"), "{err}");
    assert!(err.contains("pkg.A.b"), "{err}");
    // The chain names the explicit root and suggests the cascade marker.
    assert!(err.contains("+ pkg.A"), "{err}");
    assert!(err.contains("-!"), "{err}");
}

#[test]
fn cascade_drops_referencing_field_and_keeps_the_rest() {
    let (sema, sel) = run(&[("pkg.proto", BASE)], "+ pkg.A\n-! pkg.B\n").unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.A"), Mark::Explicit);
    // The field referencing B cascades away; its sibling survives.
    assert!(cascaded(&sema, &sel, "pkg.A.b"));
    assert!(!sel.is_kept(sema.lookup_fq("pkg.A.b").unwrap()));
    assert_eq!(mark(&sema, &sel, "pkg.A.name"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "pkg.B"), Mark::None);
}

#[test]
fn later_explicit_keep_contradicts_cascade() {
    let err = run(
        &[("pkg.proto", BASE)],
        "-! pkg.B\n+ pkg.A.b @field\n+ pkg.A.name @field\n",
    )
    .unwrap_err();
    assert!(err.contains("cascade-excluded"), "{err}");
}

#[test]
fn later_inherited_keep_contradicts_cascade() {
    // The keep decision reaching the referencing field through subtree
    // inheritance is still *later* than the cascade rule — the same
    // contradiction as a direct keep, not a silent cascade. (Write the
    // cascade after the keep to get cascading behavior.)
    let src = r#"
syntax = "proto3";
package pkg;
import "google/protobuf/any.proto";
message Event { google.protobuf.Any payload = 1; }
"#;
    let err = run(&[("pkg.proto", src)], "-! google.protobuf.Any\n+ pkg.**\n").unwrap_err();
    assert!(err.contains("cascade-excluded"), "{err}");

    // Reversed order: the cascade is later and applies.
    let (sema, sel) = run(&[("pkg.proto", src)], "+ pkg.**\n-! google.protobuf.Any\n").unwrap();
    assert!(cascaded(&sema, &sel, "pkg.Event.payload"));
}

#[test]
fn malformed_input_paths_error() {
    // Import paths flow into `out_dir.join(path)` in build scripts; the
    // set boundary rejects anything that could escape or alias.
    for bad in [
        "../evil.proto",
        "/abs.proto",
        "a//b.proto",
        "a/./b.proto",
        "a/../b.proto",
        "a\\b.proto",
        "c:evil.proto",
        "",
    ] {
        let result = FileSet::parse(vec![(bad.to_string(), FILE_B)]);
        let Err(err) = result else {
            panic!("path `{bad}` must be rejected");
        };
        assert!(
            err.message().contains("invalid import path"),
            "`{bad}`: {}",
            err.message()
        );
    }
}

#[test]
fn duplicate_input_paths_error() {
    let result = FileSet::parse(vec![
        ("b.proto".to_string(), FILE_B),
        ("b.proto".to_string(), FILE_B),
    ]);
    let Err(err) = result else {
        panic!("duplicate paths must be rejected");
    };
    assert!(
        err.message().contains("duplicate input file"),
        "{}",
        err.message()
    );
}

#[test]
fn nested_defs_come_in_by_reference_not_by_parent() {
    let src = r#"
syntax = "proto3";
package pkg;

message Outer {
  message UsedInner { int32 a = 1; }
  message UnusedInner { int32 b = 1; }
  UsedInner u = 1;
}
"#;
    let (sema, sel) = run(&[("pkg.proto", src)], "+ pkg.Outer.u @field\n").unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.Outer"), Mark::Container);
    assert_eq!(mark(&sema, &sel, "pkg.Outer.UsedInner"), Mark::Required);
    assert_eq!(mark(&sema, &sel, "pkg.Outer.UnusedInner"), Mark::None);
}

#[test]
fn subtree_selection_includes_nested_defs() {
    let src = r#"
syntax = "proto3";
package pkg;

message Outer {
  message Inner { int32 a = 1; }
  int32 x = 1;
}
"#;
    let (sema, sel) = run(&[("pkg.proto", src)], "+ pkg.Outer\n").unwrap();
    // Selecting a subtree selects everything in it, used or not.
    assert_eq!(mark(&sema, &sel, "pkg.Outer.Inner"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "pkg.Outer.Inner.a"), Mark::Explicit);
}

#[test]
fn enum_values_and_kind_value() {
    let src = r#"
syntax = "proto3";
package pkg;

enum E {
  E_UNSPECIFIED = 0;
  E_OLD = 1;
  E_NEW = 2;
}
"#;
    let (sema, sel) = run(&[("pkg.proto", src)], "+ pkg.E\n- pkg.E.E_OLD @value\n").unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.E"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "pkg.E.E_UNSPECIFIED"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "pkg.E.E_OLD"), Mark::None);
    assert_eq!(mark(&sema, &sel, "pkg.E.E_NEW"), Mark::Explicit);
}

const ENUM_SRC: &str = r#"
syntax = "proto3";
package pkg;

enum E {
  E_UNSPECIFIED = 0;
  E_A = 1;
  E_B = 2;
}
"#;

#[test]
fn dropping_enum_zero_value_errors() {
    // The pruned enum's first surviving value must be zero, or the output
    // would be illegal proto3 — a deterministic configuration error.
    let err = run(
        &[("pkg.proto", ENUM_SRC)],
        "+ pkg.E\n- pkg.E.E_UNSPECIFIED @value\n",
    )
    .unwrap_err();
    assert!(err.contains("drops the zero value"), "{err}");
    assert!(err.contains("first enum value to be zero"), "{err}");
}

#[test]
fn dropping_all_enum_values_errors() {
    let err = run(&[("pkg.proto", ENUM_SRC)], "+ pkg.E\n- pkg.E.* @value\n").unwrap_err();
    assert!(err.contains("drops all of its values"), "{err}");
}

#[test]
fn zero_alias_may_replace_dropped_zero_value() {
    // With allow_alias, the zero value may be dropped as long as a zero
    // alias survives to lead the enum.
    let src = r#"
syntax = "proto3";
package pkg;

enum E {
  option allow_alias = true;
  E_UNSPECIFIED = 0;
  E_ZERO = 0;
  E_A = 1;
}
"#;
    let (sema, sel) = run(
        &[("pkg.proto", src)],
        "+ pkg.E\n- pkg.E.E_UNSPECIFIED @value\n",
    )
    .unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.E.E_ZERO"), Mark::Explicit);
}

#[test]
fn keep_rule_cannot_address_builtins() {
    // Builtins are never materialized; a keep matching only them is dead
    // configuration, reported like any zero-hit rule.
    let src = r#"
syntax = "proto3";
package pkg;
import "google/protobuf/any.proto";
message M { google.protobuf.Any a = 1; }
"#;
    let err = run(&[("pkg.proto", src)], "+ pkg.**\n+ google.protobuf.Any\n").unwrap_err();
    assert!(err.contains("matches nothing"), "{err}");
    assert!(err.contains("cannot address them"), "{err}");
}

#[test]
fn rule_errors_point_at_the_rules_source() {
    // Zero-hit and contradiction diagnostics carry the rule's line in the
    // rules file (programmatic rule sets have no source and stay
    // unlocated).
    let inputs = vec![("pkg.proto".to_string(), BASE)];
    let set = FileSet::parse(inputs).unwrap();
    let sema = analyze(&set).unwrap();

    let rules = parse_rules("+ pkg.A\n+ nope.Thing\n").unwrap();
    let err = select(&set, &sema, &rules).unwrap_err();
    assert_eq!(err.line(), Some(2), "{err}");

    let rules = parse_rules("-! pkg.B\n+ pkg.A.b @field\n").unwrap();
    let err = select(&set, &sema, &rules).unwrap_err();
    assert_eq!(err.line(), Some(2), "{err}");
}

// ---- multi-file: imports, visibility, wkt ------------------------------------

const FILE_A: &str = r#"
syntax = "proto3";
package a;
import "b.proto";
message UserList { b.User user = 1; }
"#;

const FILE_B: &str = r#"
syntax = "proto3";
package b;
message User { string name = 1; }
"#;

#[test]
fn cross_file_closure() {
    let (sema, sel) = run(
        &[("a.proto", FILE_A), ("b.proto", FILE_B)],
        "+ a.UserList\n",
    )
    .unwrap();
    assert_eq!(mark(&sema, &sel, "a.UserList"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "b.User"), Mark::Required);
}

#[test]
fn import_must_hit_input_set() {
    let src = "syntax = \"proto3\";\npackage a;\nimport \"missing.proto\";\n";
    let err = run(&[("a.proto", src)], "+ **\n").unwrap_err();
    assert!(err.contains("does not match any input file"), "{err}");
}

#[test]
fn reference_needs_visibility() {
    // a.proto references b.User without importing b.proto.
    let src = "syntax = \"proto3\";\npackage a;\nmessage M { b.User u = 1; }\n";
    let err = run(&[("a.proto", src), ("b.proto", FILE_B)], "+ **\n").unwrap_err();
    assert!(err.contains("not imported here"), "{err}");
}

#[test]
fn import_public_is_transitive() {
    let mid = "syntax = \"proto3\";\npackage mid;\nimport public \"b.proto\";\n";
    let a = r#"
syntax = "proto3";
package a;
import "mid.proto";
message M { b.User u = 1; }
"#;
    // Visible through the public chain.
    let (sema, sel) = run(
        &[("a.proto", a), ("mid.proto", mid), ("b.proto", FILE_B)],
        "+ a.M\n",
    )
    .unwrap();
    assert_eq!(mark(&sema, &sel, "b.User"), Mark::Required);

    // Same shape with a plain (non-public) middle import: not visible.
    let mid_plain = "syntax = \"proto3\";\npackage mid;\nimport \"b.proto\";\n";
    let err = run(
        &[
            ("a.proto", a),
            ("mid.proto", mid_plain),
            ("b.proto", FILE_B),
        ],
        "+ a.M\n",
    )
    .unwrap_err();
    assert!(err.contains("not imported here"), "{err}");
}

#[test]
fn unresolved_reference_errors() {
    let src = "syntax = \"proto3\";\npackage a;\nmessage M { Ghost g = 1; }\n";
    let err = run(&[("a.proto", src)], "+ **\n").unwrap_err();
    assert!(err.contains("cannot resolve type `Ghost`"), "{err}");
}

#[test]
fn duplicate_fq_across_files_errors() {
    let dup = "syntax = \"proto3\";\npackage b;\nmessage User { int32 id = 1; }\n";
    let err = run(&[("b.proto", FILE_B), ("dup.proto", dup)], "+ **\n").unwrap_err();
    assert!(err.contains("duplicate definition of `b.User`"), "{err}");
}

#[test]
fn nearest_scope_resolution_shadows_outer() {
    let src = r#"
syntax = "proto3";
package pkg;

message Shadow { int32 outer = 1; }

message Holder {
  message Shadow { int32 inner = 1; }
  Shadow s = 1;
}
"#;
    let (sema, sel) = run(&[("pkg.proto", src)], "+ pkg.Holder.s @field\n").unwrap();
    // The nested Shadow wins over the package-level one.
    assert_eq!(mark(&sema, &sel, "pkg.Holder.Shadow"), Mark::Required);
    assert_eq!(mark(&sema, &sel, "pkg.Shadow"), Mark::None);
}

#[test]
fn wkt_resolution_and_cascade() {
    let src = r#"
syntax = "proto3";
package pkg;
import "google/protobuf/any.proto";
import "google/protobuf/timestamp.proto";

message Event {
  google.protobuf.Timestamp at = 1;
  google.protobuf.Any payload = 2;
  string name = 3;
}
"#;
    // Design's motivating example: never Any, cascade it away.
    let (sema, sel) = run(
        &[("pkg.proto", src)],
        "+ pkg.Event\n-! google.protobuf.Any\n",
    )
    .unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.Event"), Mark::Explicit);
    assert_eq!(mark(&sema, &sel, "pkg.Event.at"), Mark::Explicit);
    assert!(cascaded(&sema, &sel, "pkg.Event.payload"));
    // Timestamp is a builtin pulled in as required (keeps the import).
    assert_eq!(
        mark(&sema, &sel, "google.protobuf.Timestamp"),
        Mark::Required
    );
}

#[test]
fn google_protobuf_inputs_are_excluded_automatically() {
    // The toolchain provides google/protobuf/*: feeding such files as
    // inputs must not clash with the builtin table — they are skipped, and
    // references still resolve against the builtins.
    let fake_wkt = r#"
syntax = "proto3";
package google.protobuf;
message Timestamp { int64 seconds = 1; int32 nanos = 2; }
"#;
    let src = r#"
syntax = "proto3";
package pkg;
import "google/protobuf/timestamp.proto";
message Event { google.protobuf.Timestamp at = 1; }
"#;
    let (sema, sel) = run(
        &[
            ("google/protobuf/timestamp.proto", fake_wkt),
            ("pkg.proto", src),
        ],
        "+ pkg.Event\n",
    )
    .unwrap();
    assert_eq!(mark(&sema, &sel, "pkg.Event"), Mark::Explicit);
    assert_eq!(
        mark(&sema, &sel, "google.protobuf.Timestamp"),
        Mark::Required
    );
}

#[test]
fn wkt_reference_without_import_errors() {
    let src = r#"
syntax = "proto3";
package pkg;
message Event { google.protobuf.Timestamp at = 1; }
"#;
    let err = run(&[("pkg.proto", src)], "+ **\n").unwrap_err();
    assert!(err.contains("not imported here"), "{err}");
    assert!(err.contains("google/protobuf/timestamp.proto"), "{err}");
}

#[test]
fn rpc_must_use_message_types() {
    let src = r#"
syntax = "proto3";
package pkg;
enum E { E_UNSPECIFIED = 0; }
message M { int32 a = 1; }
service S { rpc F(E) returns (M); }
"#;
    let err = run(&[("pkg.proto", src)], "+ **\n").unwrap_err();
    assert!(
        err.contains("rpc input and output must be message types"),
        "{err}"
    );
}

#[test]
fn no_package_file() {
    let src = "syntax = \"proto3\";\nmessage Root { int32 a = 1; }\n";
    let (sema, sel) = run(&[("root.proto", src)], "+ Root\n").unwrap();
    assert_eq!(mark(&sema, &sel, "Root"), Mark::Explicit);
}
