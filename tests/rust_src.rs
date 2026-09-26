mod common;
use common::*;

#[test]
fn rust_in_src_is_counted_and_vendored_crates_are_not() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("src/entrypoint.c", "void R_init_fixpkg(void *dll) {}\n"),
        ("src/rust/src/lib.rs", "fn one() {}\nfn two() { one(); }\n"),
        ("src/rust/vendor/dep/src/lib.rs", "fn vendored() {}\nfn more() {}\n"),
        ("src/rust/vendor-patched/other/src/lib.rs", "fn patched() {}\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["has_src"], true);
    assert_eq!(s["lang_breakdown"]["rs"], 2, "the package's own Rust lines");
    assert_eq!(s["n_fns_rust"], 2);
    assert_eq!(s["rnet_n_nodes"], 2);
    assert_eq!(s["rnet_n_edges"], 1);
    assert_eq!(s["files_src"], 4, "the src/ file count never consulted the source filter");
}
