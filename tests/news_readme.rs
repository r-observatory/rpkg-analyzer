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

    // Each README has its own word count (5, 4 with the chunk stripped, 2), so the count names the file read.
    let plain = ("README", "five words of plain text\n");
    let rmd = ("README.Rmd", "a b c\n\n```{r}\nx <- 1\n```\n\nd\n");
    let md = ("README.md", "one two\n");
    let all = tree(&[("DESCRIPTION", DESC), plain, rmd, md]);
    assert_eq!(summary(&all, "release")["readme_prose_length"], 2, "README.md is read first");
    let no_md = tree(&[("DESCRIPTION", DESC), plain, rmd]);
    assert_eq!(summary(&no_md, "release")["readme_prose_length"], 4, "then README.Rmd");
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

#[test]
fn the_section_for_the_analysed_version_is_its_own_record() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("NEWS.md", "# fixpkg 1.0.0\n\n* Faster `f()`.\n\n# fixpkg 0.9.0\n\n* Old.\n"),
    ]);
    let recs = records(&t, "release");
    let s = recs.iter().find(|r| r["rec"] == "summary").unwrap();
    assert_eq!(s["release_notes_source"], "news_md");
    assert!(s.get("release_notes").is_none(), "the text never rides the summary");
    let n = recs.iter().find(|r| r["rec"] == "release_notes").expect("a release_notes record");
    assert_eq!(n["package_version"], "1.0.0");
    assert_eq!(n["news_file"], "NEWS.md");
    assert_eq!(n["release_notes"], "* Faster `f()`.");
    assert_eq!(n["release_notes_truncated"], false);

    let other = tree(&[("DESCRIPTION", DESC), ("NEWS.md", "# fixpkg 0.9.0\n\n* Old.\n")]);
    let recs = records(&other, "release");
    assert_eq!(recs.iter().find(|r| r["rec"] == "summary").unwrap()["release_notes_source"], Value::Null);
    assert!(recs.iter().all(|r| r["rec"] != "release_notes"), "no record without a section");
}

#[test]
fn a_description_that_is_not_utf8_gives_no_release_notes() {
    // Its Version cannot be read, so no NEWS section can be the analysed one.
    let t = tree_bytes(&[
        ("DESCRIPTION", b"Package: fixpkg\nVersion: 1.0.0\nAuthor: J\xfcrgen M\xfcller\n"),
        ("NEWS.md", b"# fixpkg (development version)\n\n* Unreleased.\n\n# fixpkg 1.0.0\n\n* Fixed.\n"),
    ]);
    let recs = records(&t, "release");
    let s = recs.iter().find(|r| r["rec"] == "summary").unwrap();
    assert_eq!(s["news_file"], "NEWS.md");
    assert_eq!(s["release_notes_source"], Value::Null);
    assert!(recs.iter().all(|r| r["rec"] != "release_notes"));
}
