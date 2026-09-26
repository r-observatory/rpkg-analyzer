mod common;
use common::*;

#[test]
fn lowercase_rd_pages_count_toward_every_rate() {
    let page = |name: &str| format!("\\name{{{name}}}\n\\alias{{{name}}}\n\\title{{T}}\n\\value{{v}}\n\\examples{{\n{name}()\n}}\n");
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("NAMESPACE", "export(f)\nexport(g)\n"),
        ("R/a.R", "f <- function() 1\ng <- function() 2\n"),
        ("man/f.Rd", &page("f")),
        ("man/g.rd", &page("g")),
        ("man/macros/m.Rd", "\\newcommand{\\x}{y}\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["help_pages_with_examples"], 2);
    assert_eq!(s["examples_coverage"], 1.0, "man/macros is not a help page");
    assert_eq!(s["roxygen_doc_coverage"], 1.0);
    assert_eq!(s["value_doc_rate"], 1.0);
    assert_eq!(s["doclines_per_fn_mean"], 7.0, "man/g.rd counts and man/macros does not");
    assert_eq!(s["doclines_per_fn_median"], 7.0);
}

#[test]
fn example_pages_are_counted_by_how_a_check_runs_them() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("man/a.Rd", "\\name{a}\n\\examples{\na()\n}\n"),
        ("man/b.Rd", "\\name{b}\n\\examples{\n\\donttest{b()}\n}\n"),
        ("man/c.Rd", "\\name{c}\n\\examples{\n\\dontrun{c()}\n}\n\\examples{\n# nothing\n}\n"),
        ("man/d.Rd", "\\name{d}\n\\examples{\n# nothing\n}\n"),
        ("man/e.Rd", "\\name{e}\n\\examples{\nif (requireNamespace(\"x\")) e()\n}\n"),
        ("man/f.Rd", "\\name{f}\n\\title{no examples}\n"),
        (".Rbuildignore", "^man/c\\.Rd$\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["rd_example_pages"], 5);
    assert_eq!(s["rd_example_pages_run"], 2);
    assert_eq!(s["rd_example_pages_donttest_only"], 1);
    assert_eq!(s["rd_example_pages_never_run"], 1);
    assert_eq!(s["rd_example_pages_empty"], 1);
    assert_eq!(s["rd_example_pages_conditional"], 1);
    let g = summary(&t, "git");
    assert_eq!(g["rd_example_pages"], 4, "a page the build leaves out is not counted on git input");
    assert_eq!(g["rd_example_pages_never_run"], 0);
}

#[test]
fn no_example_pages_leaves_the_buckets_null() {
    let t = tree(&[("DESCRIPTION", DESC), ("man/f.Rd", "\\name{f}\n")]);
    let s = summary(&t, "release");
    assert_eq!(s["rd_example_pages"], 0);
    for k in ["run", "donttest_only", "never_run", "empty", "conditional"] {
        assert_eq!(s[format!("rd_example_pages_{k}")], serde_json::Value::Null, "{k}");
    }
}

#[test]
fn help_topics_are_counted_and_coverage_uses_exported_pages() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("NAMESPACE", "export(f)\nexport(g)\nexport(\"%>%\")\n"),
        ("man/fixpkg-package.Rd", "\\name{fixpkg-package}\n\\alias{fixpkg}\n\\docType{package}\n"),
        ("man/f.Rd", "\\name{f}\n\\alias{f}\n\\examples{\nf()\n}\n"),
        ("man/g.Rd", "\\name{g}\n\\alias{g}\n"),
        ("man/pipe.Rd", "\\name{pipe}\n\\alias{\\%>\\%}\n"),
        ("man/h.Rd", "\\name{h}\n\\keyword{internal}\n% \\keyword{data}\n"),
        ("man/d.Rd", "\\name{d}\n\\docType{data}\n\\examples{\nd\n}\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["n_help_topics"], 6);
    assert_eq!(s["n_help_topics_internal"], 1);
    assert_eq!(s["n_help_topics_data"], 1, "a commented-out \\keyword is not read");
    assert_eq!(s["n_help_topics_package"], 1);
    let cov = s["examples_coverage_fn"].as_f64().unwrap();
    assert!((cov - 1.0 / 3.0).abs() < 1e-9, "f has examples; g and the %>% page, an escaped alias, do not: {cov}");
    assert_eq!(s["examples_coverage_fn_basis"], "exports");
}

#[test]
fn without_a_plain_export_the_denominator_falls_back() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("NAMESPACE", "exportPattern(\"^[^\\\\.]\")\n"),
        ("man/f.Rd", "\\name{f}\n\\examples{\nf()\n}\n"),
        ("man/h.Rd", "\\name{h}\n\\keyword{internal}\n"),
        ("man/k.Rd", "\\name{k-class}\n\\docType{class}\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["examples_coverage_fn"], 1.0);
    assert_eq!(s["examples_coverage_fn_basis"], "not_internal");
    let none = tree(&[("DESCRIPTION", DESC)]);
    let s = summary(&none, "release");
    assert_eq!(s["n_help_topics"], 0);
    assert_eq!(s["examples_coverage_fn"], serde_json::Value::Null);
    assert_eq!(s["examples_coverage_fn_basis"], serde_json::Value::Null);
}
