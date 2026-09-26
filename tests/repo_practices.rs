mod common;
use common::*;
use serde_json::Value;

const REPO_ONLY: [&str; 7] = [
    "ci_present", "ci_type", "ci_matrix_breadth", "ci_pr_gated",
    "has_pkgdown", "has_code_of_conduct", "has_contributing_guide",
];

#[test]
fn a_release_never_says_no_about_the_repository() {
    let t = tree(&[("DESCRIPTION", DESC), ("R/a.R", "f <- function() 1\n")]);
    let s = summary(&t, "release");
    for k in REPO_ONLY {
        assert_eq!(s[k], Value::Null, "{k} must be NULL on a release that lacks it");
    }
}

#[test]
fn a_release_still_reports_a_file_it_carries() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("inst/_pkgdown.yml", "url: https://example.org\n"),
        ("CONDUCT.md", "Be kind.\n"),
        (".github/workflows/check.yaml", "on: [push, pull_request]\njobs:\n  x:\n    strategy:\n      matrix:\n        os: [ubuntu-latest, macos-latest]\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["has_pkgdown"], true);
    assert_eq!(s["has_code_of_conduct"], true);
    assert_eq!(s["has_contributing_guide"], Value::Null);
    assert_eq!(s["ci_present"], true);
    assert_eq!(s["ci_type"], serde_json::json!(["github-actions"]));
    assert_eq!(s["ci_matrix_breadth"], 2);
    assert_eq!(s["ci_pr_gated"], true);
}

#[test]
fn a_git_branch_answers_yes_or_no() {
    let bare = tree(&[("DESCRIPTION", DESC)]);
    let s = summary(&bare, "git");
    for k in ["ci_present", "ci_pr_gated", "has_pkgdown", "has_code_of_conduct", "has_contributing_guide"] {
        assert_eq!(s[k], false, "{k} is a real 0 on a git branch");
    }
    assert_eq!(s["ci_type"], serde_json::json!([]));
    assert_eq!(s["ci_matrix_breadth"], 0);

    let full = tree(&[
        ("DESCRIPTION", DESC),
        ("pkgdown/_pkgdown.yaml", "template: {}\n"),
        (".github/CODE_OF_CONDUCT.md", "x\n"),
        ("CONTRIBUTING.MD", "x\n"),
        (".travis.yml", "language: r\n"),
    ]);
    let s = summary(&full, "git");
    assert_eq!(s["has_pkgdown"], true);
    assert_eq!(s["has_code_of_conduct"], true);
    assert_eq!(s["has_contributing_guide"], true);
    assert_eq!(s["ci_type"], serde_json::json!(["travis"]));
}
