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
