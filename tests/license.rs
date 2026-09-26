mod common;
use common::*;
use serde_json::Value;

fn pkg(license: &str, files: &[(&str, &str)]) -> Tree {
    let desc = format!("Package: fixpkg\nVersion: 1.0.0\nLicense: {license}\n");
    let mut all: Vec<(&str, &str)> = vec![("DESCRIPTION", desc.as_str())];
    all.extend_from_slice(files);
    tree(&all)
}

#[test]
fn a_filled_template_reads_complete() {
    let t = pkg("MIT + file LICENSE", &[("LICENSE", "YEAR: 2024\nCOPYRIGHT HOLDER: fixpkg authors\n")]);
    assert_eq!(summary(&t, "release")["license_file_completeness"], true);
    let licence = pkg("BSD_3_clause + file LICENCE", &[("LICENCE", "YEAR: 2024\nCOPYRIGHT HOLDER: Ann Lee\n")]);
    assert_eq!(summary(&licence, "release")["license_file_completeness"], true);
    let marked = pkg("MIT + file LICENSE", &[("LICENSE", "\u{feff}YEAR: 2024\r\nCOPYRIGHT HOLDER: <Ann Lee> <ann@x.org>\r\n")]);
    assert_eq!(summary(&marked, "release")["license_file_completeness"], true);
}

#[test]
fn placeholders_one_line_and_missing_files_read_incomplete() {
    for (license, files) in [
        ("MIT + file LICENSE", vec![("LICENSE", "YEAR: <year>\nCOPYRIGHT HOLDER: <name>\n")]),
        ("MIT + file LICENSE", vec![("LICENSE", "COPYRIGHT HOLDER: Ann Lee\n")]),
        ("MIT + file LICENSE", vec![("LICENSE", "YEAR:\nCOPYRIGHT HOLDER: Ann Lee\n")]),
        ("MIT + file LICENSE", vec![("LICENSE", "YEAR:\nCOPYRIGHT HOLDER: \n\n(separated by empty lines) as file LICENSE, and specify\n")]),
        ("MIT + file LICENSE", vec![("LICENSE.md", "MIT License\n")]),
        ("MIT + file LICENSE", vec![]),
    ] {
        let t = pkg(license, &files);
        assert_eq!(summary(&t, "release")["license_file_completeness"], false, "{files:?}");
    }
}

#[test]
fn a_full_license_text_stays_complete_and_no_file_reference_is_null() {
    let full = pkg("MIT + file LICENSE", &[("LICENSE", "MIT License\n\nCopyright (c) 2024 Ann Lee\n")]);
    assert_eq!(summary(&full, "release")["license_file_completeness"], true);
    let gpl = pkg("GPL (>= 2) | file LICENSE", &[("LICENSE", "extra terms\n")]);
    assert_eq!(summary(&gpl, "release")["license_file_completeness"], true);
    let none = pkg("GPL-3", &[]);
    assert_eq!(summary(&none, "release")["license_file_completeness"], Value::Null);
}

#[test]
fn a_latin1_holder_name_does_not_empty_the_file() {
    let t = tree_bytes(&[
        ("DESCRIPTION", b"Package: fixpkg\nVersion: 1.0.0\nLicense: MIT + file LICENSE\n"),
        ("LICENSE", b"YEAR: 2024\nCOPYRIGHT HOLDER: J\xfcrgen M\xfcller\n"),
    ]);
    assert_eq!(summary(&t, "release")["license_file_completeness"], true);
}
