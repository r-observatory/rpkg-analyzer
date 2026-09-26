// The test suite a release carries: which framework runs it, what its unit of test
// is, and how many testthat blocks CRAN skips.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;
use tree_sitter::{Node, Parser};

const DECLARABLE: [&str; 5] = ["testthat", "tinytest", "RUnit", "testit", "unitizer"];
const CRAN_SKIPS: [&str; 2] = ["skip_on_cran", "skip_if_offline"];

fn re(p: &str) -> regex::Regex {
    regex::Regex::new(p).unwrap()
}

static TEST_R_FILE: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^(?:tests|inst/tinytest|inst/unitTests)/.*\.[Rr]$"));
static HELPER: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^tests/testthat/(?:helper|setup)[^/]*$"));
static TESTTHAT_TEST: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^tests/testthat/test[^/]*\.[Rr]$"));
static SCRIPT: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^tests/[^/]+\.[Rr]$"));
static ROUT_SAVE: LazyLock<regex::Regex> = LazyLock::new(|| re(r"^tests/[^/]+\.Rout\.save$"));
static WRAPPER_FILE: LazyLock<regex::Regex> =
    LazyLock::new(|| re(r"^(?:tests/testthat/(?:helper|setup)[^/]*|R/[^/]*)\.[Rr]$"));
static TINYTEST_CALL: LazyLock<regex::Regex> = LazyLock::new(|| re(r"tinytest::"));
static RUNIT_CALL: LazyLock<regex::Regex> =
    LazyLock::new(|| re(r#"RUnit::|library\(\s*['"]?RUnit|runTestSuite|BiocGenerics:::testPackage"#));
// The pre-0.5.0 per-file classifier, which still decides testit.
static RE_TESTTHAT: LazyLock<regex::Regex> = LazyLock::new(|| re(r"\b(?:test_that|describe|it)\s*\("));
static RE_UNITTEST: LazyLock<regex::Regex> = LazyLock::new(|| re(r"\bok(?:_group)?\s*\("));
static RE_EXPECT: LazyLock<regex::Regex> = LazyLock::new(|| re(r"\bexpect_\w+\s*\("));
static RE_RUNIT_FN: LazyLock<regex::Regex> = LazyLock::new(|| re(r"(?m)^\s*test[.\w]*\s*(?:<-|=)\s*function"));
static RE_RUNIT_CHECK: LazyLock<regex::Regex> =
    LazyLock::new(|| re(r"\bcheck(?:Equals|True|Identical|Exception)\w*\s*\("));
static RE_ASSERT: LazyLock<regex::Regex> = LazyLock::new(|| re(r"\bassert\s*\("));

fn is_testit(path: &str, text: &str) -> bool {
    !RE_TESTTHAT.is_match(text)
        && !RE_UNITTEST.is_match(text)
        && !(path.starts_with("inst/tinytest") || RE_EXPECT.is_match(text))
        && !(RE_RUNIT_FN.is_match(text) || RE_RUNIT_CHECK.is_match(text))
        && RE_ASSERT.is_match(text)
}

pub struct TestSuite {
    /// None when test directories hold files but no framework could be named.
    pub primary: Option<&'static str>,
    pub used: Vec<&'static str>,
    pub declared: Vec<&'static str>,
    pub n_units: Option<i64>,
    pub unit: Option<&'static str>,
    pub n_rout_save: i64,
    /// testthat blocks, set only when testthat is the primary framework.
    pub blocks: Option<BlockCounts>,
}

pub fn test_suite(root: &Path, files: &[String], suggests: &[String]) -> TestSuite {
    let texts: Vec<(&String, String)> = files
        .iter()
        .filter(|f| TEST_R_FILE.is_match(f) && !HELPER.is_match(f))
        .filter_map(|f| crate::read_lossy(root, f).map(|t| (f, t)))
        .collect();
    let with = |pick: &dyn Fn(&str, &str) -> bool| -> Vec<&(&String, String)> {
        texts.iter().filter(|(f, t)| pick(f, t)).collect()
    };
    // A framework's runner counts as one of its files, so a runner never tips a tie.
    let tinytest = with(&|f, t| f.starts_with("inst/tinytest/") || f == "tests/tinytest.R" || TINYTEST_CALL.is_match(t));
    let runit = with(&|f, t| f.starts_with("inst/unitTests/") || RUNIT_CALL.is_match(t));
    let testit = with(&|f, t| is_testit(f, t));
    let testthat_files: Vec<&String> =
        files.iter().filter(|f| TESTTHAT_TEST.is_match(f) || *f == "tests/testthat.R").collect();
    let scripts: Vec<&String> = files.iter().filter(|f| SCRIPT.is_match(f)).collect();
    let n_unitizer = files.iter().filter(|f| f.starts_with("tests/unitizer/")).count();

    // Pushed in the tie-break order: testthat, tinytest, RUnit, testit, unitizer, scripts.
    let mut used: Vec<&'static str> = Vec::new();
    // Helper and setup files alone are support code, not a testthat suite.
    if files.iter().any(|f| f.starts_with("tests/testthat/") && !HELPER.is_match(f)) {
        used.push("testthat");
    }
    if files.iter().any(|f| f.starts_with("inst/tinytest/") || f == "tests/tinytest.R") || !tinytest.is_empty() {
        used.push("tinytest");
    }
    if files.iter().any(|f| f.starts_with("inst/unitTests/")) || !runit.is_empty() {
        used.push("RUnit");
    }
    if !testit.is_empty() {
        used.push("testit");
    }
    if n_unitizer > 0 {
        used.push("unitizer");
    }
    if used.is_empty() && !scripts.is_empty() {
        used.push("scripts");
    }

    let n_files = |fw: &str| match fw {
        "testthat" => testthat_files.len(),
        "tinytest" => tinytest.len(),
        "RUnit" => runit.len(),
        "testit" => testit.len(),
        "unitizer" => n_unitizer,
        _ => scripts.len(),
    };
    let mut primary: Option<&'static str> = None;
    for &fw in &used {
        if primary.is_none_or(|p| n_files(fw) > n_files(p)) {
            primary = Some(fw);
        }
    }
    let any_test_files = files
        .iter()
        .any(|f| f.starts_with("tests/") || f.starts_with("inst/tinytest/") || f.starts_with("inst/unitTests/"));
    if primary.is_none() && !any_test_files {
        primary = Some("none");
    }

    let count = |set: &[&(&String, String)], re: &regex::Regex| -> i64 {
        set.iter().map(|(_, t)| re.find_iter(t).count() as i64).sum()
    };
    let blocks = (primary == Some("testthat")).then(|| cran_skipped_blocks(root, files));
    let (n_units, unit) = match primary {
        Some("testthat") => (blocks.as_ref().map(|b| b.total), Some("test_block")),
        Some("tinytest") => (Some(count(&tinytest, &RE_EXPECT)), Some("expectation")),
        Some("RUnit") => (Some(count(&runit, &RE_RUNIT_FN)), Some("test_function")),
        Some("testit") => (Some(count(&testit, &RE_ASSERT)), Some("expectation")),
        Some("scripts") => (Some(scripts.len() as i64), Some("script_file")),
        _ => (None, None),
    };
    TestSuite {
        primary,
        declared: DECLARABLE.iter().copied().filter(|d| suggests.iter().any(|s| s == d)).collect(),
        used,
        n_units,
        unit,
        n_rout_save: files.iter().filter(|f| ROUT_SAVE.is_match(f)).count() as i64,
        blocks,
    }
}

pub struct BlockCounts {
    pub total: i64,
    /// Blocks CRAN skips, at least: wrappers deeper than one level are missed.
    pub skipped: i64,
    /// The runner calls test_check() only when NOT_CRAN is set.
    pub gated: bool,
}

fn text<'a>(n: Node, src: &'a str) -> &'a str {
    n.utf8_text(src.as_bytes()).unwrap_or("")
}

fn parse(parser: &mut Parser, src: &str) -> Option<tree_sitter::Tree> {
    parser.parse(src, None)
}

// The walks below keep an explicit stack: a long generated expression nests one level
// per term, and a recursive walk would overflow on it.
fn calls_any(node: Node, src: &str, pick: &dyn Fn(&str) -> bool) -> bool {
    let mut st = vec![node];
    while let Some(n) = st.pop() {
        if n.kind() == "call" && crate::call_fn_name(&n, src.as_bytes()).is_some_and(|name| pick(&name)) {
            return true;
        }
        let mut c = n.walk();
        st.extend(n.children(&mut c));
    }
    false
}

fn is_not_cran_if(node: Node, src: &str) -> bool {
    node.kind() == "if_statement"
        && node.child_by_field_name("condition").is_some_and(|c| text(c, src).contains("NOT_CRAN"))
}

/// test_that() and it() calls, in source order, with whether an enclosing if mentions
/// NOT_CRAN. An it() is not searched for more.
fn collect_blocks<'a>(node: Node<'a>, src: &str, gated: bool, out: &mut Vec<(Node<'a>, bool)>) {
    let mut st = vec![(node, gated)];
    while let Some((n, gated)) = st.pop() {
        let gated = gated || is_not_cran_if(n, src);
        if n.kind() == "call" {
            match crate::call_fn_name(&n, src.as_bytes()).as_deref() {
                Some("test_that") => out.push((n, gated)),
                Some("it") => {
                    out.push((n, gated));
                    continue;
                }
                _ => {}
            }
        }
        let mut c = n.walk();
        let children: Vec<Node<'a>> = n.children(&mut c).collect();
        st.extend(children.into_iter().rev().map(|ch| (ch, gated)));
    }
}

/// Top-level functions in the helpers and R/ whose body calls a CRAN skip.
fn skip_wrappers(root: &Path, files: &[String], parser: &mut Parser) -> HashSet<String> {
    let mut out = HashSet::new();
    for f in files.iter().filter(|f| WRAPPER_FILE.is_match(f)) {
        let Some(src) = crate::read_lossy(root, f) else { continue };
        let Some(tree) = parse(parser, &src) else { continue };
        let root_node = tree.root_node();
        let mut c = root_node.walk();
        for top in root_node.children(&mut c) {
            if top.kind() != "binary_operator" {
                continue;
            }
            let (Some(lhs), Some(rhs)) = (top.child_by_field_name("lhs"), top.child_by_field_name("rhs")) else { continue };
            if lhs.kind() == "identifier"
                && rhs.kind() == "function_definition"
                && calls_any(rhs, &src, &|n| CRAN_SKIPS.contains(&n))
            {
                out.insert(text(lhs, &src).to_string());
            }
        }
    }
    out
}

/// Whether every test_check() in the tests/*.R runners sits inside a NOT_CRAN condition.
fn runner_gated(root: &Path, files: &[String], parser: &mut Parser) -> bool {
    let (mut calls, mut gated_calls) = (0usize, 0usize);
    for f in files.iter().filter(|f| SCRIPT.is_match(f)) {
        let Some(src) = crate::read_lossy(root, f) else { continue };
        let Some(tree) = parse(parser, &src) else { continue };
        let mut st = vec![(tree.root_node(), false)];
        while let Some((n, gated)) = st.pop() {
            let gated = gated || is_not_cran_if(n, &src);
            if n.kind() == "call" && crate::call_fn_name(&n, src.as_bytes()).as_deref() == Some("test_check") {
                calls += 1;
                if gated {
                    gated_calls += 1;
                }
            }
            let mut c = n.walk();
            st.extend(n.children(&mut c).map(|ch| (ch, gated)));
        }
    }
    calls > 0 && calls == gated_calls
}

pub fn cran_skipped_blocks(root: &Path, files: &[String]) -> BlockCounts {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_r::LANGUAGE.into()).expect("load tree-sitter-r");
    let wrappers = skip_wrappers(root, files, &mut parser);
    let is_skip = |n: &str| CRAN_SKIPS.contains(&n) || wrappers.contains(n);
    let gated = runner_gated(root, files, &mut parser);
    let (mut total, mut skipped) = (0i64, 0i64);
    for f in files.iter().filter(|f| TESTTHAT_TEST.is_match(f)) {
        let Some(src) = crate::read_lossy(root, f) else { continue };
        let Some(tree) = parse(&mut parser, &src) else { continue };
        let root_node = tree.root_node();
        let mut file_skip = false;
        let mut c = root_node.walk();
        for top in root_node.children(&mut c) {
            // A skip called at top level skips every block after it.
            if top.kind() == "call" && crate::call_fn_name(&top, src.as_bytes()).is_some_and(|n| is_skip(&n)) {
                file_skip = true;
            }
            let mut blocks = Vec::new();
            collect_blocks(top, &src, false, &mut blocks);
            for (b, in_not_cran_if) in blocks {
                total += 1;
                if gated
                    || file_skip
                    || in_not_cran_if
                    || calls_any(b, &src, &is_skip)
                    || text(b, &src).contains("NOT_CRAN")
                {
                    skipped += 1;
                }
            }
        }
    }
    BlockCounts { total, skipped, gated }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_testit_file_is_one_nothing_else_claims() {
        assert!(is_testit("tests/test-a.R", "library(testit)\nassert('x', 1 == 1)\n"));
        assert!(!is_testit("tests/test-a.R", "test_that('x', { assert(TRUE) })\n"));
        assert!(!is_testit("tests/test-a.R", "expect_true(TRUE)\n"));
    }
}
