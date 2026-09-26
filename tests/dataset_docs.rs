mod common;
use common::*;
use serde_json::Value;

#[test]
fn a_dataset_page_gives_its_source_and_whether_it_has_a_format() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("data/sourced.csv", "a;b\n1;2\n"),
        ("data/unsourced.csv", "a;b\n1;2\n"),
        ("data/orphan.csv", "a;b\n1;2\n"),
        (
            "man/sourced.Rd",
            "\\name{sourced}\n\\alias{sourced}\n\\docType{data}\n\\title{Sourced}\n\\format{A frame.}\n\\source{Collected by \\emph{the authors}, see \\url{https://example.org}.}\n",
        ),
        ("man/unsourced.Rd", "\\name{unsourced}\n\\alias{unsourced}\n\\docType{data}\n\\title{Unsourced}\n%% \\source{\n%% ~~ reference to a publication or URL ~~\n%% }\n"),
    ]);
    let recs = records(&t, "release");
    let ds = |n: &str| recs.iter().find(|r| r["rec"] == "dataset" && r["name"] == n).cloned().unwrap();
    let s = ds("sourced");
    assert_eq!(s["dataset_doc_format"], 1);
    assert_eq!(s["dataset_doc_source"], "Collected by *the authors*, see https://example.org.");
    let u = ds("unsourced");
    assert_eq!(u["dataset_doc_format"], 0);
    assert_eq!(u["dataset_doc_source"], Value::Null, "the page does not state a source");
    let o = ds("orphan");
    assert_eq!(o["title"], Value::Null);
    assert_eq!(o["dataset_doc_format"], Value::Null, "no page documents it");
    assert_eq!(o["dataset_doc_source"], Value::Null);
}

#[test]
fn a_long_source_is_capped_at_a_character_boundary() {
    let long = format!("\\name{{big}}\n\\alias{{big}}\n\\source{{{}}}\n", "é".repeat(3000));
    let t = tree(&[("DESCRIPTION", DESC), ("data/big.csv", "a;b\n1;2\n"), ("man/big.Rd", &long)]);
    let recs = records(&t, "release");
    let b = recs.iter().find(|r| r["rec"] == "dataset" && r["name"] == "big").unwrap();
    let src = b["dataset_doc_source"].as_str().unwrap();
    assert!(src.len() <= 4096 && src.len() > 4000, "capped near 4 KiB, got {}", src.len());
}

#[test]
fn a_cap_that_falls_inside_a_character_backs_off_to_its_start() {
    // One ASCII byte first puts byte 4096 in the middle of a two-byte character.
    let long = format!("\\name{{odd}}\n\\alias{{odd}}\n\\source{{a{}}}\n", "é".repeat(3000));
    let t = tree(&[("DESCRIPTION", DESC), ("data/odd.csv", "a;b\n1;2\n"), ("man/odd.Rd", &long)]);
    let recs = records(&t, "release");
    let r = recs.iter().find(|r| r["rec"] == "dataset" && r["name"] == "odd").unwrap();
    assert_eq!(r["dataset_doc_source"].as_str().unwrap().len(), 4095);
}

#[test]
fn only_a_data_directory_dataset_takes_its_page_source_and_format() {
    // data() never loads inst/extdata, so a page sharing its name does not document it.
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("inst/extdata/extra.csv", "a,b\n1,2\n"),
        ("man/extra.Rd", "\\name{extra}\n\\alias{extra}\n\\title{Extra}\n\\format{A frame.}\n\\source{Somewhere.}\n"),
    ]);
    let recs = records(&t, "release");
    let e = recs.iter().find(|r| r["rec"] == "dataset" && r["name"] == "extra").unwrap();
    assert_eq!(e["origin_dir"], "extdata");
    assert_eq!(e["title"], "Extra", "titles keep their rule");
    assert_eq!(e["dataset_doc_source"], Value::Null);
    assert_eq!(e["dataset_doc_format"], Value::Null);
    assert!(e.as_object().unwrap().contains_key("dataset_doc_source"));
    assert!(e.as_object().unwrap().contains_key("dataset_doc_format"));
}

#[test]
fn a_page_without_a_title_gives_way_to_a_titled_page_for_the_same_name() {
    // The untitled pages sort first; the titled page gives all three fields.
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("data/x.csv", "a;b\n1;2\n"),
        ("data/y.csv", "a;b\n1;2\n"),
        ("data/z.csv", "a;b\n1;2\n"),
        ("man/a_x.Rd", "\\name{a_x}\n\\alias{x}\n\\source{Untitled source.}\n"),
        ("man/a_y.Rd", "\\name{a_y}\n\\alias{y}\n\\title{ }\n"),
        ("man/a_z.Rd", "\\name{a_z}\n\\alias{z}\n\\format{A frame.}\n\\source{Only page.}\n"),
        ("man/b_x.Rd", "\\name{b_x}\n\\alias{x}\n\\title{The X}\n\\format{A frame.}\n\\source{Titled source.}\n"),
        ("man/b_y.Rd", "\\name{b_y}\n\\alias{y}\n\\title{The Y}\n\\format{A frame.}\n\\source{Y source.}\n"),
        ("man/c_x.Rd", "\\name{c_x}\n\\alias{x}\n\\title{A later X}\n\\source{Later source.}\n"),
    ]);
    let recs = records(&t, "release");
    let ds = |n: &str| recs.iter().find(|r| r["rec"] == "dataset" && r["name"] == n).cloned().unwrap();
    let x = ds("x");
    assert_eq!(x["title"], "The X");
    assert_eq!(x["dataset_doc_source"], "Titled source.");
    assert_eq!(x["dataset_doc_format"], 1);
    let y = ds("y");
    assert_eq!(y["title"], "The Y", "an empty title gives way too");
    assert_eq!(y["dataset_doc_source"], "Y source.");
    assert_eq!(y["dataset_doc_format"], 1);
    // With no titled page for the name, the untitled page still speaks.
    let z = ds("z");
    assert_eq!(z["title"], Value::Null);
    assert_eq!(z["dataset_doc_source"], "Only page.");
    assert_eq!(z["dataset_doc_format"], 1);
}

#[test]
fn a_kept_help_page_is_read_behind_more_left_out_pages_than_the_cap() {
    // More left-out pages than the help page cap, sorted ahead of the one the release keeps.
    let mut owned: Vec<(String, &str)> = (0..4001)
        .map(|i| (format!("man/a{i:04}.Rd"), "\\name{left}\n\\alias{z}\n\\title{Left out}\n\\source{Left out.}\n"))
        .collect();
    owned.push(("man/z.Rd".to_string(), "\\name{z}\n\\alias{z}\n\\title{The Z}\n\\format{A frame.}\n\\source{Kept source.}\n"));
    owned.push(("data/z.csv".to_string(), "a;b\n1;2\n"));
    owned.push((".Rbuildignore".to_string(), "^man/a[0-9]+\\.Rd$\n"));
    owned.push(("DESCRIPTION".to_string(), DESC));
    let files: Vec<(&str, &str)> = owned.iter().map(|(p, t)| (p.as_str(), *t)).collect();
    let t = tree(&files);
    let recs = records(&t, "git");
    let z = recs.iter().find(|r| r["rec"] == "dataset" && r["name"] == "z").unwrap();
    assert_eq!(z["title"], "The Z");
    assert_eq!(z["dataset_doc_source"], "Kept source.");
    assert_eq!(z["dataset_doc_format"], 1);
}

#[test]
fn a_source_written_with_brace_groups_stores_its_text_without_braces() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("data/grouped.csv", "a;b\n1;2\n"),
        (
            "man/grouped.Rd",
            "\\name{grouped}\n\\alias{grouped}\n\\docType{data}\n\\title{Grouped}\n\\source{{T.R. Fleming}, after \\enc{M\u{fc}ller}{Mueller} on the \\eqn{r}{r}-largest events, \\insertRef{key2022}{pkg}.}\n",
        ),
    ]);
    let recs = records(&t, "release");
    let g = recs.iter().find(|r| r["rec"] == "dataset" && r["name"] == "grouped").unwrap();
    assert_eq!(
        g["dataset_doc_source"],
        "T.R. Fleming, after M\u{fc}ller on the r-largest events, key2022."
    );
}
