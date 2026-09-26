mod common;
use common::*;
use serde_json::{json, Value};

#[test]
fn a_citation_file_in_the_release_is_read_statically() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("inst/CITATION", "bibentry(\"Article\", title = \"T\", journal = \"R Journal\", year = \"2021\", doi = \"10.32614/RJ-2021-001\")\n"),
        ("inst/REFERENCES.bib", "@article{x, title={y}}\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["has_citation"], true);
    assert_eq!(s["citation_read"], "literal");
    assert_eq!(s["citation_kind"], "publication");
    assert_eq!(s["citation_n_entries"], 1);
    assert_eq!(s["citation_bibtype"], json!(["article"]));
    assert_eq!(s["citation_dois"], json!(["10.32614/rj-2021-001"]));
    assert_eq!(s["citation_venue"], json!(["rjournal"]));
    assert_eq!(s["has_rd_bibliography"], true);
}

#[test]
fn no_citation_file_reads_zero_with_null_details() {
    let t = tree(&[("DESCRIPTION", DESC)]);
    let s = summary(&t, "release");
    assert_eq!(s["has_citation"], false);
    assert_eq!(s["has_rd_bibliography"], false);
    for k in ["citation_read", "citation_kind", "citation_n_entries", "citation_bibtype", "citation_dois", "citation_venue"] {
        assert_eq!(s[k], Value::Null, "{k}");
    }
}

#[test]
fn a_citation_file_the_build_leaves_out_is_not_in_the_release() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("inst/CITATION", "bibentry(\"Manual\", title = \"T\")\n"),
        (".Rbuildignore", "^inst/CITATION$\n"),
    ]);
    let s = summary(&t, "git");
    assert_eq!(s["has_citation"], false);
    assert_eq!(s["build_ignored"], json!(["inst/CITATION"]));
}
