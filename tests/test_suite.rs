mod common;
use common::*;
use serde_json::{json, Value};

const TT_DESC: &str = "Package: fixpkg\nVersion: 1.0.0\nSuggests: knitr, testthat (>= 3.0.0)\n";

#[test]
fn testthat_blocks_are_the_unit_and_cran_skips_are_counted() {
    let t = tree(&[
        ("DESCRIPTION", TT_DESC),
        ("tests/testthat.R", "library(testthat)\ntest_check(\"fixpkg\")\n"),
        ("tests/testthat/helper-skip.R", "skip_slow <- function() testthat::skip_on_cran()\n"),
        (
            "tests/testthat/test-a.R",
            "test_that(\"a\", { expect_true(TRUE) })\ntest_that(\"b\", { skip_on_cran(); expect_true(TRUE) })\ntest_that(\"c\", { skip_slow(); expect_true(TRUE) })\nif (identical(Sys.getenv(\"NOT_CRAN\"), \"true\")) {\n  test_that(\"d\", expect_true(TRUE))\n}\ndescribe(\"e\", { it(\"f\", expect_true(TRUE)) })\n",
        ),
        ("tests/testthat/test-b.R", "skip_on_cran()\ntest_that(\"g\", expect_true(TRUE))\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["test_framework_primary"], "testthat");
    assert_eq!(s["test_frameworks_used"], json!(["testthat"]));
    assert_eq!(s["test_frameworks_declared"], json!(["testthat"]));
    assert_eq!(s["test_unit"], "test_block");
    assert_eq!(s["n_test_units"], 6, "a, b, c, d, f and g; describe() is not a block");
    assert_eq!(s["n_test_blocks"], 6);
    assert_eq!(s["n_test_blocks_cran_skipped"], 4, "b, c through its helper, d behind NOT_CRAN, g after a top-level skip");
    assert_eq!(s["tests_gated_not_cran"], false);
}

#[test]
fn a_runner_gated_on_not_cran_skips_every_block() {
    let t = tree(&[
        ("DESCRIPTION", TT_DESC),
        ("tests/testthat.R", "if (identical(Sys.getenv(\"NOT_CRAN\"), \"true\")) {\n  testthat::test_check(\"fixpkg\")\n}\n"),
        ("tests/testthat/test-a.R", "test_that(\"a\", expect_true(TRUE))\ntest_that(\"b\", expect_true(TRUE))\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["tests_gated_not_cran"], true);
    assert_eq!(s["n_test_blocks_cran_skipped"], 2);
}

#[test]
fn tinytest_counts_expectations_and_a_testthat_helper_names_no_framework() {
    let t = tree(&[
        ("DESCRIPTION", "Package: fixpkg\nVersion: 1.0.0\nSuggests: tinytest\n"),
        ("tests/tinytest.R", "if (requireNamespace(\"tinytest\", quietly = TRUE)) tinytest::test_package(\"fixpkg\")\n"),
        ("inst/tinytest/test_a.R", "expect_equal(1, 1)\nexpect_true(TRUE)\nexpect_error(stop())\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["test_framework_primary"], "tinytest");
    assert_eq!(s["test_unit"], "expectation");
    assert_eq!(s["n_test_units"], 3);
    assert_eq!(s["n_test_blocks"], Value::Null, "blocks are a testthat measure");

    let helper = tree(&[
        ("DESCRIPTION", TT_DESC),
        ("tests/testthat/helper-x.R", "tinytest::expect_equal(1, 1)\n"),
        ("tests/testthat/test-a.R", "test_that(\"a\", expect_true(TRUE))\n"),
    ]);
    assert_eq!(summary(&helper, "release")["test_frameworks_used"], json!(["testthat"]));
}

#[test]
fn runit_counts_test_functions() {
    let t = tree(&[
        ("DESCRIPTION", "Package: fixpkg\nVersion: 1.0.0\nSuggests: RUnit, BiocGenerics\n"),
        ("tests/runTests.R", "BiocGenerics:::testPackage(\"fixpkg\")\n"),
        ("inst/unitTests/test_a.R", "test.one <- function() checkTrue(TRUE)\ntestTwo <- function() checkEquals(1, 1)\n"),
    ]);
    let s = summary(&t, "git");
    assert_eq!(s["test_framework_primary"], "RUnit");
    assert_eq!(s["test_frameworks_used"], json!(["RUnit"]));
    assert_eq!(s["test_frameworks_declared"], json!(["RUnit"]));
    assert_eq!(s["test_unit"], "test_function");
    assert_eq!(s["n_test_units"], 2);
}

#[test]
fn plain_scripts_count_files_and_rout_save_is_counted() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("tests/a.R", "stopifnot(TRUE)\n"),
        ("tests/b.R", "stopifnot(TRUE)\n"),
        ("tests/a.Rout.save", "> stopifnot(TRUE)\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["test_framework_primary"], "scripts");
    assert_eq!(s["test_unit"], "script_file");
    assert_eq!(s["n_test_units"], 2);
    assert_eq!(s["n_rout_save"], 1);
    // A testthat runner with no tests/testthat/ directory runs nothing of testthat.
    let runner_only = tree(&[("DESCRIPTION", TT_DESC), ("tests/testthat.R", "test_check(\"fixpkg\")\n")]);
    assert_eq!(summary(&runner_only, "release")["test_framework_primary"], "scripts");
}

#[test]
fn ties_go_to_the_earlier_framework() {
    let t = tree(&[
        ("DESCRIPTION", DESC),
        ("tests/testthat/test-a.R", "test_that(\"a\", expect_true(TRUE))\n"),
        ("inst/tinytest/test_a.R", "expect_true(TRUE)\n"),
    ]);
    let s = summary(&t, "release");
    assert_eq!(s["test_frameworks_used"], json!(["testthat", "tinytest"]));
    assert_eq!(s["test_framework_primary"], "testthat");
}

#[test]
fn none_only_when_the_test_directories_are_empty() {
    let none = tree(&[("DESCRIPTION", DESC)]);
    let s = summary(&none, "release");
    assert_eq!(s["test_framework_primary"], "none");
    assert_eq!(s["test_frameworks_used"], json!([]));
    assert_eq!(s["n_test_units"], Value::Null);
    assert_eq!(s["test_unit"], Value::Null);
    let odd = tree(&[("DESCRIPTION", DESC), ("tests/README.md", "notes\n")]);
    assert_eq!(summary(&odd, "release")["test_framework_primary"], Value::Null, "files but no framework is undetermined");
}

#[test]
fn testit_counts_assertions_and_unitizer_names_no_unit() {
    let testit = tree(&[
        ("DESCRIPTION", "Package: fixpkg\nVersion: 1.0.0\nSuggests: testit\n"),
        ("tests/test-all.R", "library(testit)\ntest_pkg(\"fixpkg\")\n"),
        ("tests/testit/test-a.R", "assert(\"one\", 1 == 1)\nassert(\"two\", TRUE)\n"),
    ]);
    let s = summary(&testit, "release");
    assert_eq!(s["test_framework_primary"], "testit");
    assert_eq!(s["test_frameworks_declared"], json!(["testit"]));
    assert_eq!(s["test_unit"], "expectation");
    assert_eq!(s["n_test_units"], 2);

    let unitizer = tree(&[
        ("DESCRIPTION", "Package: fixpkg\nVersion: 1.0.0\nSuggests: unitizer\n"),
        ("tests/run.R", "unitizer::unitize_dir(\"unitizer\")\n"),
        ("tests/unitizer/a.R", "x <- 1\n"),
    ]);
    let s = summary(&unitizer, "release");
    assert_eq!(s["test_framework_primary"], "unitizer");
    assert_eq!(s["test_frameworks_used"], json!(["unitizer"]));
    assert_eq!(s["n_test_units"], Value::Null, "unitizer stores expressions, not discrete units");
    assert_eq!(s["test_unit"], Value::Null);
    assert_eq!(s["n_test_blocks"], Value::Null);
}
