mod common;
use common::*;
use serde_json::Value;

#[test]
fn the_widened_rule_counts_what_r_builds() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("vignettes/intro.qmd", "---\ntitle: x\n---\n%\\VignetteEngine{quarto::html}\n```{r}\n1\n```\n"),
        ("vignettes/paper.asis", "%\\VignetteEngine{R.rsp::asis}\n"),
        ("vignettes/old.Rnw", "<<>>=\n1\n@\n"),
        ("vignettes/README.md", "# not a vignette\n"),
        ("vignettes/articles/web.Rmd", "%\\VignetteEngine{knitr::rmarkdown}\n```{r}\n1\n```\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["has_vignettes"], true);
    assert_eq!(s["num_vignettes"], 3);
    assert_eq!(s["vignette_dynamic"], true);
    assert_eq!(s["vignette_eval_gated"], 0);
}

#[test]
fn every_vignette_static_reads_zero_and_none_reads_null() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("vignettes/a.Rmd", "%\\VignetteEngine{knitr::rmarkdown}\n```{r, eval=FALSE}\n1\n```\n"),
        ("vignettes/b.Rmd", "%\\VignetteEngine{knitr::rmarkdown}\n```{r}\n2\n```\n"),
        ("vignettes/b.Rmd.orig", "precomputed source\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["vignette_dynamic"], false);
    let none = tree(&[("DESCRIPTION", DESC)]);
    let s = summary(&none, "release");
    assert_eq!(s["has_vignettes"], false);
    assert_eq!(s["num_vignettes"], 0);
    assert_eq!(s["vignette_dynamic"], Value::Null);
    assert_eq!(s["vignette_eval_gated"], Value::Null);
}

#[test]
fn a_gated_vignette_is_counted_and_is_not_static() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("vignettes/a.Rmd", "%\\VignetteEngine{knitr::rmarkdown}\n```{r}\nknitr::opts_chunk$set(eval = identical(Sys.getenv(\"NOT_CRAN\"), \"true\"))\n```\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["vignette_eval_gated"], 1);
    assert_eq!(s["vignette_dynamic"], true);
}
