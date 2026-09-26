mod common;
use common::*;
use serde_json::Value;

fn git_tree(rbi: &[u8]) -> Tree {
    tree_bytes(&[
        ("DESCRIPTION", DESC.as_bytes()),
        ("NAMESPACE", b"export(f)\n"),
        ("R/a.R", b"f <- function(x) x\n"),
        ("R/dev.R", b"g <- function() 2\n"),
        ("data-raw/make.R", b"x <- 1\n"),
        (".Rbuildignore", rbi),
    ])
}

#[test]
fn git_input_counts_only_what_the_build_keeps() {
    for rbi in [&b"^data-raw$\n^R/dev\\.R$\n"[..], b"^data-raw$\r\n^R/dev\\.R$\r\n", b"^data-raw$\r^R/dev\\.R$\r"] {
        let t = git_tree(rbi);
        let s = summary(&t, "git");
        assert_eq!(s["files_r"], 1, "R/dev.R is left out for {:?}", String::from_utf8_lossy(rbi));
        assert_eq!(s["n_files"], 3, "DESCRIPTION, NAMESPACE and R/a.R remain");
        assert_eq!(s["build_ignore_bad_lines"], 0);
    }
}

#[test]
fn release_input_reads_the_tree_as_given() {
    let t = git_tree(b"^data-raw$\n^R/dev\\.R$\n");
    let s = summary(&t, "release");
    assert_eq!(s["files_r"], 2);
    assert_eq!(s["build_ignore_bad_lines"], Value::Null);
}

#[test]
fn bad_lines_are_counted_and_null_without_an_rbuildignore() {
    let t = git_tree(b"(unclosed\n^R/dev\\.R$\n");
    assert_eq!(summary(&t, "git")["build_ignore_bad_lines"], 1);
    let none = tree(&[("DESCRIPTION", DESC)]);
    assert_eq!(summary(&none, "git")["build_ignore_bad_lines"], Value::Null);
}

#[test]
fn files_read_directly_follow_the_release_boundary() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("NAMESPACE", "export(f)\n"),
        ("R/a.R", "f <- function(x) x\n"),
        ("data/datalist", "hidden\n"),
        ("data/kept.csv", "a;b\n1;2\n"),
        ("data/dropped.csv", "a;b\n1;2\n"),
        (".Rbuildignore", "^NAMESPACE$\n^data/datalist$\n^data/dropped\\.csv$\n"),
    ]);
    let recs = records(&t, "git");
    let s = recs.iter().find(|r| r["rec"] == "summary").unwrap();
    assert_eq!(s["n_exports"], Value::Null, "an excluded NAMESPACE is not read");
    // The datalist is left out, so names come from the files the release keeps.
    assert_eq!(s["datasets"], serde_json::json!(["kept"]));
    let names: Vec<&str> = recs.iter().filter(|r| r["rec"] == "dataset").filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(names, vec!["kept"]);
}

#[test]
fn an_unreadable_rbuildignore_nulls_every_release_content_value() {
    // A directory named .Rbuildignore exists and cannot be read as a file.
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("R/a.R", "f <- function(x) x\n"),
        (".Rbuildignore/x", "y\n"),
        (".travis.yml", "language: r\n"),
    ]);
    let recs = records(&t, "git");
    let s = recs.iter().find(|r| r["rec"] == "summary").unwrap();
    assert_eq!(s["input_kind"], "git");
    assert_eq!(s["package"], "fixpkg");
    assert_eq!(s["ci_present"], true, "repository-only values are still computed");
    for k in ["files_r", "n_files", "loc_r", "has_readme", "license", "build_ignore_bad_lines"] {
        assert_eq!(s[k], Value::Null, "{k} is undetermined");
    }
    assert!(recs.iter().all(|r| r["rec"] != "function"), "no release-content detail records");
    assert!(recs.iter().any(|r| r["rec"] == "dcf"), "the DESCRIPTION record is kept");
}
