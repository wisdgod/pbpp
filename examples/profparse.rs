//! Profiling target: parse the synthetic bench file in a tight loop.

#![expect(
    clippy::cast_precision_loss,
    reason = "throughput reporting; f64 precision is irrelevant at bench sizes"
)]

use std::fmt::Write as _;
use std::hint::black_box;

fn gen_file(msgs: usize) -> String {
    let mut s = String::new();
    writeln!(s, "syntax = \"proto3\";").unwrap();
    writeln!(s, "package bench.p0;").unwrap();
    writeln!(s, "option java_package = \"com.bench.p0\";").unwrap();
    for m in 0..msgs {
        writeln!(s, "// Message M{m} carries benchmark payload {m}.").unwrap();
        writeln!(s, "message M{m} {{").unwrap();
        writeln!(s, "  string name = 1; // display name").unwrap();
        writeln!(s, "  int64 id = 2;").unwrap();
        writeln!(s, "  repeated double values = 3 [packed = true];").unwrap();
        writeln!(s, "  map<string, int32> counts = 4;").unwrap();
        if m > 0 {
            writeln!(s, "  M{} prev = 5;", m - 1).unwrap();
        }
        writeln!(s, "  oneof body {{").unwrap();
        writeln!(s, "    string text = 6;").unwrap();
        writeln!(s, "    bytes blob = 7;").unwrap();
        writeln!(s, "  }}").unwrap();
        writeln!(s, "  enum Kind {{").unwrap();
        writeln!(s, "    KIND_UNSPECIFIED = 0;").unwrap();
        writeln!(s, "    KIND_A = 1;").unwrap();
        writeln!(s, "  }}").unwrap();
        writeln!(s, "  Kind kind = 8;").unwrap();
        writeln!(s, "  reserved 100 to 110;").unwrap();
        writeln!(s, "}}").unwrap();
    }
    writeln!(s, "service S0 {{").unwrap();
    for m in 0..msgs.min(50) {
        writeln!(s, "  rpc Call{m}(M{m}) returns (M{m});").unwrap();
    }
    writeln!(s, "}}").unwrap();
    s
}

fn main() {
    let src = gen_file(250);
    println!("src: {} bytes", src.len());
    for _ in 0..100 {
        black_box(pbpp::parse(black_box(&src)).unwrap());
    }
    let mut best = std::time::Duration::MAX;
    for _ in 0..2000 {
        let t0 = std::time::Instant::now();
        black_box(pbpp::parse(black_box(&src)).unwrap());
        best = best.min(t0.elapsed());
    }
    let mbs = src.len() as f64 / best.as_secs_f64() / 1e6;
    println!("parse min: {best:?}  {mbs:.1} MB/s");

    // Split construction from teardown: parse timed without drop (leaked),
    // then drop timed separately. Bounded iterations keep the leak sane.
    let mut best_build = std::time::Duration::MAX;
    let mut best_drop = std::time::Duration::MAX;
    for _ in 0..300 {
        let t0 = std::time::Instant::now();
        let cst = black_box(pbpp::parse(black_box(&src)).unwrap());
        best_build = best_build.min(t0.elapsed());
        let t1 = std::time::Instant::now();
        drop(cst);
        best_drop = best_drop.min(t1.elapsed());
    }
    println!("build min: {best_build:?}   drop min: {best_drop:?}");
}
