//! Throughput and allocation benchmark over a synthetic corpus.
//!
//! Run with: `cargo run --release --example bench`

#![expect(
    clippy::cast_precision_loss,
    reason = "throughput reporting; f64 precision is irrelevant at bench sizes"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct Counting;

static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_BYTES.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn gen_file(pkg_idx: usize, msgs: usize) -> String {
    let mut s = String::new();
    writeln!(s, "syntax = \"proto3\";").unwrap();
    writeln!(s, "package bench.p{pkg_idx};").unwrap();
    if pkg_idx > 0 {
        writeln!(s, "import \"bench/p{}.proto\";", pkg_idx - 1).unwrap();
    }
    writeln!(s, "option java_package = \"com.bench.p{pkg_idx}\";").unwrap();
    for m in 0..msgs {
        writeln!(s, "// Message M{m} carries benchmark payload {m}.").unwrap();
        writeln!(s, "message M{m} {{").unwrap();
        writeln!(s, "  string name = 1; // display name").unwrap();
        writeln!(s, "  int64 id = 2;").unwrap();
        writeln!(s, "  repeated double values = 3 [packed = true];").unwrap();
        writeln!(s, "  map<string, int32> counts = 4;").unwrap();
        if m > 0 {
            writeln!(s, "  M{} prev = 5;", m - 1).unwrap();
        } else if pkg_idx > 0 {
            writeln!(s, "  bench.p{}.M0 other = 5;", pkg_idx - 1).unwrap();
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
    writeln!(s, "service S{pkg_idx} {{").unwrap();
    for m in 0..msgs.min(50) {
        writeln!(s, "  rpc Call{m}(M{m}) returns (M{m});").unwrap();
    }
    writeln!(s, "}}").unwrap();
    s
}

/// Reports the *minimum* iteration time: robust against preemption noise
/// from other load on the machine.
fn time<T>(name: &str, bytes: usize, iters: u32, mut f: impl FnMut() -> T) {
    for _ in 0..3 {
        black_box(f());
    }
    let b0 = ALLOC_BYTES.load(Ordering::Relaxed);
    let c0 = ALLOC_COUNT.load(Ordering::Relaxed);
    let mut best = std::time::Duration::MAX;
    for _ in 0..iters {
        let t0 = Instant::now();
        black_box(f());
        best = best.min(t0.elapsed());
    }
    let ab = (ALLOC_BYTES.load(Ordering::Relaxed) - b0) / iters as usize;
    let ac = (ALLOC_COUNT.load(Ordering::Relaxed) - c0) / iters as usize;
    let mbs = bytes as f64 / best.as_secs_f64() / 1e6;
    println!("{name:10} {best:>10.3?}  {mbs:>8.1} MB/s  {ab:>10} B/iter  {ac:>7} allocs/iter");
}

fn main() {
    let files: Vec<(String, String)> = (0..8)
        .map(|i| (format!("bench/p{i}.proto"), gen_file(i, 250)))
        .collect();
    let total: usize = files.iter().map(|(_, s)| s.len()).sum();
    println!("corpus: {} files, {} bytes total", files.len(), total);

    let one = &files[0].1;
    println!("single file: {} bytes", one.len());

    time("lex", one.len(), 100, || pbpp::lex::lex(one).unwrap());
    time("parse", one.len(), 100, || pbpp::parse(one).unwrap());
    let cst = pbpp::parse(one).unwrap();
    time("format", one.len(), 100, || pbpp::format(&cst));

    let rules_src = "+ bench.p7.**\n";
    let inputs =
        || -> Vec<(String, &str)> { files.iter().map(|(p, s)| (p.clone(), s.as_str())).collect() };
    time("pipeline", total, 20, || {
        let set = pbpp::FileSet::parse(inputs()).unwrap();
        let sema = pbpp::sema::analyze(&set).unwrap();
        let rules = pbpp::rules::parse_rules(rules_src).unwrap();
        let sel = pbpp::select(&set, &sema, &rules).unwrap();
        pbpp::prune(&set, &sema, &sel)
    });

    // Stage breakdown of the pipeline.
    let set = pbpp::FileSet::parse(inputs()).unwrap();
    time("  set-parse", total, 20, || {
        pbpp::FileSet::parse(inputs()).unwrap()
    });
    time("  analyze", total, 20, || {
        pbpp::sema::analyze(&set).unwrap()
    });
    let sema = pbpp::sema::analyze(&set).unwrap();
    let rules = pbpp::rules::parse_rules(rules_src).unwrap();
    time("  select", total, 20, || {
        pbpp::select(&set, &sema, &rules).unwrap()
    });
    let sel = pbpp::select(&set, &sema, &rules).unwrap();
    time("  prune", total, 20, || pbpp::prune(&set, &sema, &sel));
}
