//! Corpus round-trip tests: the three locked-down formatter properties.
//!
//! For every corpus file:
//! 1. `parse` succeeds;
//! 2. `format` output reparses (formatter never emits invalid proto);
//! 3. semantic preservation: digest(parse(src)) == digest(parse(format(src)));
//! 4. idempotence: format(parse(format(src))) == format(parse(src)) —
//!    byte-equal, which also pins comment attachment stability.

use std::path::PathBuf;

fn collect_protos(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_protos(&path, out);
        } else if path.extension().is_some_and(|e| e == "proto") {
            out.push(path);
        }
    }
}

fn corpus_files() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut files = Vec::new();
    collect_protos(&dir, &mut files);
    files.sort();
    assert!(!files.is_empty(), "corpus directory is empty");
    files
}

fn diff_context(name: &str, a: &str, b: &str) -> String {
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            return format!(
                "{name}: first difference at line {}:\n  first : {la}\n  second: {lb}",
                i + 1
            );
        }
    }
    format!(
        "{name}: line counts differ ({} vs {})",
        a.lines().count(),
        b.lines().count()
    )
}

#[test]
fn corpus_roundtrip() {
    for path in corpus_files() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let src = std::fs::read_to_string(&path).unwrap();

        let first = pbpp::parse(&src)
            .unwrap_or_else(|d| panic!("{name}: parse failed:\n{}", d.with_file(&name)));
        let formatted = pbpp::format(&first);

        let second = pbpp::parse(&formatted).unwrap_or_else(|d| {
            panic!(
                "{name}: formatted output does not reparse:\n{}\n--- formatted ---\n{formatted}",
                d.with_file(&name)
            )
        });

        // Semantic preservation.
        let d1 = pbpp::digest::digest(&first);
        let d2 = pbpp::digest::digest(&second);
        assert_eq!(
            d1,
            d2,
            "{name}: semantics changed by formatting\n{}",
            diff_context(&name, &d1, &d2)
        );

        // Idempotence (byte-level), which also pins comment attachment.
        let reformatted = pbpp::format(&second);
        assert_eq!(
            formatted,
            reformatted,
            "{name}: format is not idempotent\n{}",
            diff_context(&name, &formatted, &reformatted)
        );
    }
}

#[test]
fn comment_count_is_preserved() {
    // No comment may be dropped by parse+format: count `//` and `/*` markers
    // in source and in formatted output.
    for path in corpus_files() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let src = std::fs::read_to_string(&path).unwrap();
        let file = pbpp::parse(&src).unwrap();
        let formatted = pbpp::format(&file);
        let count = |s: &str| pbpp::lex::lex(s).unwrap().comments.len();
        assert_eq!(
            count(&src),
            count(&formatted),
            "{name}: comment count changed\n--- formatted ---\n{formatted}"
        );
    }
}

#[test]
fn formatted_corpus_snapshot_sanity() {
    // A couple of targeted expectations on the formatted shape, so that the
    // formatter's basic style is pinned by more than idempotence.
    let src = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/minimal.proto"),
    )
    .unwrap();
    let out = pbpp::format(&pbpp::parse(&src).unwrap());
    assert_eq!(
        out,
        "syntax = \"proto3\";\n\nmessage Only {\n  int32 a = 1;\n}\n"
    );
}

#[test]
fn normalizes_messy_whitespace() {
    let messy = "syntax=\"proto3\";package a.b;message   M{int32 a=1;;;repeated string b=2 [deprecated=true];}";
    let file = pbpp::parse(messy).unwrap();
    let out = pbpp::format(&file);
    assert_eq!(
        out,
        "syntax = \"proto3\";\n\npackage a.b;\n\nmessage M {\n  int32 a = 1;\n  repeated string b = 2 [deprecated = true];\n}\n"
    );
    // And the normalized output round-trips semantically.
    assert_eq!(
        pbpp::digest::digest(&file),
        pbpp::digest::digest(&pbpp::parse(&out).unwrap())
    );
}

#[test]
fn arena_ranges_survive_65536_adjacent_strings() {
    // Regression: `IdxRange::len` was u16 and silently truncated in
    // release — 65,536 adjacent string-literal parts formatted as
    // `option x = ;`. The range is now u32-wide.
    let parts = 65_536 + 3;
    let mut src = String::with_capacity(parts * 4 + 64);
    src.push_str("syntax = \"proto3\";\noption x = ");
    for _ in 0..parts {
        src.push_str("\"s\" ");
    }
    src.push_str(";\n");

    let first = pbpp::parse(&src).unwrap();
    let formatted = pbpp::format(&first);
    assert!(
        !formatted.contains("option x = ;"),
        "string parts were lost"
    );
    let second = pbpp::parse(&formatted).unwrap();
    assert_eq!(
        pbpp::digest::digest(&first),
        pbpp::digest::digest(&second),
        "semantics changed by formatting"
    );
}

#[test]
fn trailing_comment_stays_on_its_line() {
    let src =
        "syntax = \"proto3\";\n\nmessage M {\n  int32 a = 1; // keep me here\n  int32 b = 2;\n}\n";
    let out = pbpp::format(&pbpp::parse(src).unwrap());
    assert!(
        out.contains("int32 a = 1; // keep me here\n"),
        "trailing comment moved:\n{out}"
    );
}

#[test]
fn detached_comment_stays_with_previous_node() {
    let src = "syntax = \"proto3\";\n\nmessage M {\n  int32 a = 1;\n  // belongs to a's neighborhood\n\n  int32 b = 2;\n}\n";
    let out = pbpp::format(&pbpp::parse(src).unwrap());
    let a_pos = out.find("int32 a").unwrap();
    let c_pos = out.find("// belongs").unwrap();
    let b_pos = out.find("int32 b").unwrap();
    assert!(
        a_pos < c_pos && c_pos < b_pos,
        "comment order broken:\n{out}"
    );
    // Blank line must separate the detached comment from b, not from a.
    let between = &out[c_pos..b_pos];
    assert!(between.contains("\n\n"), "blank separation lost:\n{out}");
}
