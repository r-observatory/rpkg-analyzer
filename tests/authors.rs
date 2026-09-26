mod common;
use common::*;

#[test]
fn the_authors_json_carries_orcid_and_ror_after_roles() {
    let desc = "Package: fixpkg\nVersion: 1.0.0\nAuthors@R: c(\n    person(\"Ann\", \"Lee\", role = c(\"aut\", \"cre\"),\n           comment = c(ORCID = \"0000-0002-1825-0097\")),\n    person(\"ACME\", role = \"fnd\", comment = c(ROR = \"05dxps055\")))\n";
    let t = tree(&[("DESCRIPTION", desc)]);
    let s = summary(&t, "release");
    assert_eq!(s["n_authors"], 2);
    assert_eq!(
        s["authors"],
        r#"[{"given":"Ann","family":"Lee","roles":["aut","cre"],"orcid":"0000-0002-1825-0097"},{"given":"ACME","family":null,"roles":["fnd"],"ror":"05dxps055"}]"#
    );
}

#[test]
fn a_legacy_author_field_is_split_into_clean_names() {
    let desc = "Package: fixpkg\nVersion: 1.0.0\nAuthor: Ann Lee <ann@x.org> (University of X), and Bob Gray\n";
    let t = tree(&[("DESCRIPTION", desc)]);
    let s = summary(&t, "release");
    assert_eq!(s["n_authors"], 2);
    assert_eq!(
        s["authors"],
        r#"[{"given":"Ann","family":"Lee","roles":[],"comment":"University of X"},{"given":"Bob","family":"Gray","roles":[]}]"#
    );
}

#[test]
fn a_non_ascii_comment_before_an_orcid_url_keeps_the_record() {
    let desc = "Package: fixpkg\nVersion: 1.0.0\nAuthors@R: person(\"Ayse\", \"Kaya\", comment = c(\"İTÜ, orcid.org/\"))\n";
    let t = tree(&[("DESCRIPTION", desc)]);
    let s = summary(&t, "release");
    assert_eq!(s["n_authors"], 1);
    assert_eq!(s["authors"], r#"[{"given":"Ayse","family":"Kaya","roles":[],"comment":"İTÜ, orcid.org/"}]"#);
}
