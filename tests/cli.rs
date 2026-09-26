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

#[test]
fn explain_lists_each_decision_per_file() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        (".Rbuildignore", "^notes$\n"),
        ("notes/todo.md", "x\n"),
        ("man/f.Rd", "\\name{f}\n\\examples{\n\\donttest{f()}\n}\n"),
        ("vignettes/a.Rmd", "%\\VignetteEngine{knitr::rmarkdown}\n```{r, eval=FALSE}\n1\n```\n"),
    ]);
    let out = run(&["--explain", t.path(), "--input-kind", "git"]);
    assert!(out.status.success());
    let recs: Vec<serde_json::Value> =
        String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let file = |p: &str| recs.iter().find(|r| r["rec"] == "release_file" && r["path"] == p).cloned().unwrap();
    assert_eq!(file("notes/todo.md")["kept"], false);
    assert_eq!(file("man/f.Rd")["kept"], true);
    let page = recs.iter().find(|r| r["rec"] == "rd_page").unwrap();
    assert_eq!(page["file"], "man/f.Rd");
    assert_eq!(page["examples"], "donttest_only");
    let vig = recs.iter().find(|r| r["rec"] == "vignette").unwrap();
    assert_eq!(vig["run"], "static");
}
