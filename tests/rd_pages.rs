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
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["rd_example_pages"], 5);
    assert_eq!(s["rd_example_pages_run"], 2);
    assert_eq!(s["rd_example_pages_donttest_only"], 1);
    assert_eq!(s["rd_example_pages_never_run"], 1);
    assert_eq!(s["rd_example_pages_empty"], 1);
    assert_eq!(s["rd_example_pages_conditional"], 1);
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
