mod common;
use common::*;

/// The 36 columns both code-metrics pipelines declare in .SUMMARY_050_COLS.
const NEW_COLUMNS: [&str; 36] = [
    "input_kind", "has_citation", "citation_read", "citation_kind", "citation_n_entries", "citation_bibtype",
    "citation_dois", "citation_venue", "has_rd_bibliography", "n_help_topics", "n_help_topics_internal",
    "n_help_topics_data", "n_help_topics_package", "examples_coverage_fn", "examples_coverage_fn_basis",
    "rd_example_pages", "rd_example_pages_run", "rd_example_pages_donttest_only", "rd_example_pages_never_run",
    "rd_example_pages_empty", "rd_example_pages_conditional", "test_framework_primary", "test_frameworks_used",
    "test_frameworks_declared", "n_test_units", "test_unit", "n_rout_save", "n_test_blocks",
    "n_test_blocks_cran_skipped", "tests_gated_not_cran", "vignette_eval_gated", "news_file", "changelog_file",
    "release_notes_source", "build_ignored", "build_ignore_bad_lines",
];

#[test]
fn every_new_column_is_emitted_on_both_input_kinds() {
    let t = tree(&[("DESCRIPTION", DESC)]);
    for kind in ["release", "git"] {
        let s = summary(&t, kind);
        for k in NEW_COLUMNS {
            assert!(s.get(k).is_some(), "{k} missing on {kind} input");
        }
    }
}

#[test]
fn the_retired_columns_are_gone() {
    let t = tree(&[
        ("DESCRIPTION", "Package: fixpkg\nVersion: 1.0.0\nURL: https://arxiv.org/abs/1\nAuthor: Ann Lee\n"),
    ]);
    let s = summary(&t, "release");
    assert!(s.get("has_website").is_none());
    assert!(s.get("copyright_holder_declared").is_none());
}
