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
