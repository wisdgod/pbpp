//! Asserts the build script's output landed in `OUT_DIR` as pruned.

static API: &str = include_str!(concat!(env!("OUT_DIR"), "/proto/acme/v1/api.proto"));
static TYPES: &str = include_str!(concat!(env!("OUT_DIR"), "/proto/acme/v1/types.proto"));

fn main() {
    assert!(API.contains("message Api"), "kept message missing:\n{API}");
    assert!(!API.contains("Gone"), "dropped message leaked:\n{API}");
    assert!(
        TYPES.contains("message Item"),
        "closure-required type missing:\n{TYPES}"
    );
    println!("ok");
}
