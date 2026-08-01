//! Lex timing on comment-heavy real-world input (well-known types).

#![expect(
    clippy::cast_precision_loss,
    reason = "throughput reporting; f64 precision is irrelevant at bench sizes"
)]

use std::hint::black_box;
use std::time::{Duration, Instant};

fn main() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/google/protobuf");
    let mut srcs = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "proto") {
            srcs.push(std::fs::read_to_string(p).unwrap());
        }
    }
    let total: usize = srcs.iter().map(String::len).sum();
    println!("wkt corpus: {} files, {total} bytes", srcs.len());

    let mut best = Duration::MAX;
    for _ in 0..2000 {
        let t0 = Instant::now();
        for s in &srcs {
            black_box(pbpp::lex::lex(black_box(s)).unwrap());
        }
        best = best.min(t0.elapsed());
    }
    let mbs = total as f64 / best.as_secs_f64() / 1e6;
    println!("lex min: {best:?}  {mbs:.1} MB/s");
}
