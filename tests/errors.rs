//! Error-path tests: every strictness rule in the design has a case that
//! proves it produces a targeted, located diagnostic.

fn err(src: &str) -> pbpp::Error {
    match pbpp::parse(src) {
        Ok(_) => panic!("expected a parse error, but parsing succeeded:\n{src}"),
        Err(d) => d,
    }
}

fn assert_err_contains(src: &str, needle: &str) -> pbpp::Error {
    let d = err(src);
    assert!(
        d.message().contains(needle),
        "expected error containing `{needle}`, got: {}",
        d.message()
    );
    d
}

fn line_of(_src: &str, d: &pbpp::Error) -> u32 {
    d.line().expect("error should carry a location")
}

#[test]
fn proto2_syntax() {
    let src = "syntax = \"proto2\";\n";
    let d = assert_err_contains(src, "proto2");
    assert_eq!(line_of(src, &d), 1);
}

#[test]
fn unknown_syntax() {
    assert_err_contains("syntax = \"editions\";\n", "only `proto3` is supported");
}

#[test]
fn missing_syntax() {
    assert_err_contains(
        "package a.b;\nmessage M {}\n",
        "file must start with `syntax",
    );
    assert_err_contains("", "empty file");
}

#[test]
fn misplaced_syntax() {
    assert_err_contains(
        "package a.b;\nsyntax = \"proto3\";\n",
        "must be the first statement",
    );
}

#[test]
fn duplicate_package() {
    assert_err_contains(
        "syntax = \"proto3\";\npackage a;\npackage b;\n",
        "duplicate `package`",
    );
}

#[test]
fn proto2_required_field() {
    let src = "syntax = \"proto3\";\nmessage M {\n  required int32 a = 1;\n}\n";
    let d = assert_err_contains(src, "`required`");
    assert_eq!(line_of(src, &d), 3);
}

#[test]
fn proto2_group() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  group G = 1 {}\n}\n",
        "`group`",
    );
}

// Not lumped with the proto2 tests: proto3 permits `extend` for custom
// options. pbpp rejects it as unsupported (tracked for 0.2.0), and the
// diagnostic must say so rather than mislabel it a proto2 construct.
#[test]
fn extend_unsupported_top_level() {
    let d = assert_err_contains(
        "syntax = \"proto3\";\nextend google.protobuf.FieldOptions {}\n",
        "`extend`",
    );
    assert!(
        !d.message().contains("proto2"),
        "proto3 permits `extend` for custom options; the diagnostic must \
         not call it a proto2 construct: {}",
        d.message()
    );
}

#[test]
fn extend_unsupported_in_message() {
    let d = assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  extend google.protobuf.FieldOptions {}\n}\n",
        "`extend`",
    );
    assert!(!d.message().contains("proto2"));
}

#[test]
fn proto2_extensions() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  extensions 100 to 199;\n}\n",
        "`extensions`",
    );
}

#[test]
fn unknown_top_level() {
    let src = "syntax = \"proto3\";\nwidget M {}\n";
    let d = assert_err_contains(src, "top-level definition");
    assert_eq!(line_of(src, &d), 2);
}

#[test]
fn nesting_deeper_than_cap_errors() {
    // The parser (and every recursive pass after it) runs on bounded
    // stack: brace nesting is capped at exactly 256 (the file root does
    // not count), with a located diagnostic instead of a stack overflow.
    use std::fmt::Write as _;
    fn nested(depth: usize) -> String {
        let mut src = String::from("syntax = \"proto3\";\n");
        for i in 0..depth {
            let _ = writeln!(src, "message M{i} {{");
        }
        src.push_str("int32 a = 1;\n");
        src.push_str(&"}\n".repeat(depth));
        src
    }

    let d = err(&nested(257));
    assert!(
        d.message().contains("nest deeper than 256 levels"),
        "got: {}",
        d.message()
    );

    // Exactly 256 brace levels parse, and the recursive consumers stay on
    // bounded stack too.
    let src = nested(256);
    let file = pbpp::parse(&src).expect("depth 256 parses");
    let _ = pbpp::format(&file);
}

#[test]
fn map_field_rejected_in_oneof() {
    let src =
        "syntax = \"proto3\";\nmessage M {\n  oneof o {\n    map<string, int32> m = 1;\n  }\n}\n";
    let d = assert_err_contains(src, "map field is not allowed in a oneof");
    assert_eq!(line_of(src, &d), 4);
}

#[test]
fn message_reserved_must_be_field_numbers() {
    // Negative reserved is enum-only grammar.
    let d = assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  reserved -5;\n}\n",
        "reserved numbers in a message must be field numbers",
    );
    assert_eq!(d.line().unwrap(), 3);
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  reserved 0;\n}\n",
        "reserved numbers in a message must be field numbers",
    );
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  reserved 1 to 536870912;\n}\n",
        "reserved numbers in a message must be field numbers",
    );
    // The same spellings stay legal in an enum (values are int32).
    pbpp::parse(
        "syntax = \"proto3\";\nenum E {\n  E_UNSPECIFIED = 0;\n  reserved -5, 7 to max;\n}\n",
    )
    .expect("enum reserved may be negative");
}

#[test]
fn enum_numbers_must_fit_int32() {
    assert_err_contains(
        "syntax = \"proto3\";\nenum E {\n  E_UNSPECIFIED = 0;\n  E_BIG = 3000000000;\n}\n",
        "out of range (enum values are 32-bit integers)",
    );
    assert_err_contains(
        "syntax = \"proto3\";\nenum E {\n  E_UNSPECIFIED = 0;\n  reserved 3000000000;\n}\n",
        "enum reserved numbers must fit a 32-bit integer",
    );
}

#[test]
fn unclosed_message() {
    let src = "syntax = \"proto3\";\nmessage M {\n  int32 a = 1;\n";
    let d = err(src);
    assert!(d.message().contains("expected `}`"), "got: {}", d.message());
    assert!(
        d.notes()
            .iter()
            .any(|n| n.message.contains("unclosed block")),
        "missing unclosed-block note"
    );
}

#[test]
fn bad_map_key() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  map<float, string> m = 1;\n}\n",
        "invalid map key type `float`",
    );
}

#[test]
fn nested_map() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  map<string, map<string, int32>> m = 1;\n}\n",
        "map value cannot be another map",
    );
}

#[test]
fn label_in_oneof() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  oneof o {\n    repeated int32 a = 1;\n  }\n}\n",
        "not allowed on a oneof field",
    );
}

#[test]
fn map_with_label() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  repeated map<string, int32> m = 1;\n}\n",
        "map field cannot have a label",
    );
}

#[test]
fn float_field_number() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  int32 a = 1.5;\n}\n",
        "field number must be an integer",
    );
}

#[test]
fn field_number_range() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  int32 a = 0;\n}\n",
        "out of range",
    );
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  int32 a = 536870912;\n}\n",
        "out of range",
    );
    // The maximum valid number is fine.
    assert!(pbpp::parse("syntax = \"proto3\";\nmessage M {\n  int32 a = 536870911;\n}\n").is_ok());
}

#[test]
fn lexical_errors() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M { int32 a = 09; }\n",
        "octal",
    );
    assert_err_contains("syntax = \"proto3\";\n@\n", "unexpected character");
    assert_err_contains(
        "syntax = \"proto3\";\n/* never closed\n",
        "unterminated block comment",
    );
    assert_err_contains(
        "syntax = \"proto3\";\noption a = \"bad \\q escape\";\n",
        "invalid escape sequence",
    );
    assert_err_contains(
        "syntax = \"proto3\";\noption a = \"unterminated;\n",
        "string literal",
    );
}

#[test]
fn rpc_grammar() {
    assert_err_contains(
        "syntax = \"proto3\";\nservice S {\n  rpc M(A) gives (B);\n}\n",
        "expected `returns`",
    );
    assert_err_contains(
        "syntax = \"proto3\";\nservice S {\n  rpc M(A) returns (B) {\n    int32 a = 1;\n  }\n}\n",
        "only options may appear in an rpc body",
    );
}

#[test]
fn reserved_grammar() {
    assert_err_contains(
        "syntax = \"proto3\";\nmessage M {\n  reserved true;\n}\n",
        "field number range or a quoted field name",
    );
}

#[test]
fn stray_token_in_message() {
    let src = "syntax = \"proto3\";\nmessage M {\n  = 1;\n}\n";
    let d = err(src);
    assert_eq!(line_of(src, &d), 3);
}

#[test]
fn diagnostics_render_with_caret() {
    let src = "syntax = \"proto3\";\nmessage M {\n  required int32 a = 1;\n}\n";
    let rendered = err(src).with_file("test.proto").to_string();
    assert!(rendered.contains("test.proto:3:3"), "{rendered}");
    assert!(rendered.contains('^'), "{rendered}");
    assert!(rendered.contains("required int32 a = 1;"), "{rendered}");
}
