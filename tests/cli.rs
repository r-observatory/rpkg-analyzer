mod common;
use common::*;

#[test]
fn the_summary_names_the_input_kind_it_was_computed_under() {
    let t = tree(&[("DESCRIPTION", DESC)]);
    assert_eq!(summary(&t, "release")["input_kind"], "release");
    assert_eq!(summary(&t, "git")["input_kind"], "git");
}

#[test]
fn an_absent_input_kind_exits_2_with_a_usage_line_and_no_records() {
    let t = tree(&[("DESCRIPTION", DESC)]);
    for args in [vec![t.path()], vec![t.path(), "--input-kind", "tarball"]] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}");
        assert!(out.stdout.is_empty(), "no records on stdout for {args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("--input-kind release|git"));
    }
}

#[test]
fn version_datasets_and_sexp_need_no_input_kind() {
    let t = tree(&[("DESCRIPTION", DESC), ("R/a.R", "f <- function(x) x\n")]);
    let v = run(&["--version"]);
    assert!(v.status.success());
    assert!(String::from_utf8_lossy(&v.stdout).starts_with("rpkg-analyzer 0.5."));
    assert!(run(&["--datasets", t.path()]).status.success());
    let r = t.root.join("R/a.R");
    assert!(run(&["--sexp", r.to_str().unwrap()]).status.success());
}
