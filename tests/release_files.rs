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

#[test]
fn a_kept_extdata_file_is_seen_behind_more_left_out_files_than_the_walk_takes() {
    // More left-out files than the extdata walk's cap, sorted ahead of the one the release keeps.
    let mut owned: Vec<(String, &str)> =
        (0..201).map(|i| (format!("inst/extdata/a_big/f{i:03}.csv"), "a,b\n1,2\n")).collect();
    owned.push(("inst/extdata/z.csv".to_string(), "a,b\n1,2\n"));
    owned.push((".Rbuildignore".to_string(), "^inst/extdata/a_big$\n"));
    owned.push(("DESCRIPTION".to_string(), DESC));
    let files: Vec<(&str, &str)> = owned.iter().map(|(p, t)| (p.as_str(), *t)).collect();
    let t = tree(&files);
    let recs = records(&t, "git");
    let s = recs.iter().find(|r| r["rec"] == "summary").unwrap();
    assert_eq!(s["extdata"]["files"], 1, "only z.csv is in the release");
    assert_eq!(s["extdata"]["read"]["csv"]["n"], 1);
    let ext: Vec<&str> = recs
        .iter()
        .filter(|r| r["rec"] == "dataset" && r["origin_dir"] == "extdata")
        .filter_map(|r| r["file"].as_str())
        .collect();
    assert_eq!(ext, vec!["inst/extdata/z.csv"]);
}

#[test]
fn a_wholly_left_out_extdata_is_absent_on_git_and_present_on_release() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("inst/extdata/x.csv", "a,b\n1,2\n"),
        ("inst/extdata/sub/y.txt", "hello\n"),
        (".Rbuildignore", "^inst/extdata$\n"),
    ]);
    let recs = records(&t, "git");
    let s = recs.iter().find(|r| r["rec"] == "summary").unwrap();
    assert_eq!(s["extdata"], Value::Null, "the release has no inst/extdata");
    assert!(recs.iter().all(|r| r["origin_dir"] != "extdata"));
    let rel = summary(&t, "release");
    assert_eq!(rel["extdata"]["files"], 2, "the tree as given has both files");

    // A file the release keeps below the walk's depth still means the directory is there.
    let deep = tree(&[
        ("DESCRIPTION", DESC),
        ("inst/extdata/top.csv", "a,b\n1,2\n"),
        ("inst/extdata/a/b/c/deep.csv", "a,b\n1,2\n"),
        (".Rbuildignore", "^inst/extdata/top\\.csv$\n"),
    ]);
    assert_eq!(summary(&deep, "git")["extdata"]["files"], 0);
}

#[test]
fn build_ignored_names_the_left_out_items_on_git_input_only() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("README.Rmd", "x\n"),
        ("README.md", "x\n"),
        ("NEWS.md", "# fixpkg 1.0.0\n"),
        ("inst/NEWS.Rd", "\\name{NEWS}\n"),
        ("man/f.Rd", "\\name{f}\n"),
        ("data-raw/make.R", "1\n"),
        (".Rbuildignore", "^README\\.Rmd$\n^data-raw$\n^inst/NEWS\\.Rd$\n"),
    ]);
    assert_eq!(summary(&t, "git")["build_ignored"], serde_json::json!(["README.Rmd", "data-raw", "inst/NEWS.Rd"]));
    assert_eq!(summary(&t, "release")["build_ignored"], Value::Null);
    let none = tree(&[("DESCRIPTION", DESC)]);
    assert_eq!(summary(&none, "git")["build_ignored"], serde_json::json!([]));
}
