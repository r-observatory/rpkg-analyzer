mod common;
use common::*;
use serde_json::Value;

#[test]
fn a_plain_readme_counts_and_its_prose_is_measured() {
    let t = tree(&[("DESCRIPTION", DESC), ("README", "Three words here.\n")]);
    let s = summary(&t, "release");
    assert_eq!(s["has_readme"], true);
    assert_eq!(s["readme_prose_length"], 3);
    let lower = tree(&[("DESCRIPTION", DESC), ("readme.markdown", "one two\n")]);
    assert_eq!(summary(&lower, "release")["has_readme"], true, "names match without regard to case");
    let deep = tree(&[("DESCRIPTION", DESC), ("inst/README.md", "x\n")]);
    assert_eq!(summary(&deep, "release")["has_readme"], false, "only the root counts");
}

#[test]
fn news_under_inst_is_found_and_read() {
    let rd = tree(&[
        ("DESCRIPTION", DESC),
        ("inst/NEWS.Rd", "\\name{NEWS}\n\\section{Changes in version 1.0.0}{\n\\itemize{\\item First.}\n}\n"),
    ]);
    let s = summary(&rd, "release");
    assert_eq!(s["news_file"], "inst/NEWS.Rd");
    assert_eq!(s["news_present"], true);
    assert_eq!(s["news_up_to_date"], true);
    assert_eq!(s["news_structure_quality"], Value::Null, "Rd is not scored as Markdown");

    let md = tree(&[("DESCRIPTION", DESC), ("inst/NEWS.md", "# fixpkg 0.9.0\n\n- Old.\n"), ("ChangeLog", "x\n")]);
    let s = summary(&md, "release");
    assert_eq!(s["news_file"], "inst/NEWS.md");
    assert_eq!(s["news_up_to_date"], false);
    assert!(s["news_structure_quality"].is_number());
    assert_eq!(s["changelog_file"], "ChangeLog");
}

#[test]
fn no_news_file_reads_null_and_false() {
    let t = tree(&[("DESCRIPTION", DESC)]);
    let s = summary(&t, "release");
    assert_eq!(s["news_file"], Value::Null);
    assert_eq!(s["news_present"], false);
    assert_eq!(s["news_up_to_date"], Value::Null);
    assert_eq!(s["changelog_file"], Value::Null);
}

#[test]
fn a_news_file_the_build_leaves_out_is_not_in_the_release() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("NEWS.md", "# fixpkg 1.0.0\n"),
        ("README.md", "hello\n"),
        (".Rbuildignore", "^NEWS\\.md$\n^README\\.md$\n"),
    ]);
    let s = summary(&t, "git");
    assert_eq!(s["news_present"], false);
    assert_eq!(s["news_up_to_date"], Value::Null);
    assert_eq!(s["has_readme"], false);
    assert_eq!(s["build_ignored"], serde_json::json!(["README.md", "NEWS.md"]));
}
