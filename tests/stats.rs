mod common;
use common::*;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn analyze(dir: &str, stats: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rpkg-analyzer"));
    cmd.args([dir, "--input-kind", "release"])
        .env_remove("RPKG_ANALYZER_STATS")
        .env_remove("RPKG_ANALYZER_CACHE_DIR")
        .env_remove("RPKG_ANALYZER_CACHE_VERIFY");
    if let Some(p) = stats {
        cmd.env("RPKG_ANALYZER_STATS", p);
    }
    cmd.output().expect("run the analyzer")
}

fn package() -> Tree {
    tree(&[
        ("DESCRIPTION", DESC),
        ("R/a.R", "f <- function(x) g(x)\ng <- function(x) x\n"),
        ("src/a.c", "int one(void) { return 1; }\n"),
        ("tests/t.R", "stopifnot(TRUE)\n"),
        ("data/d.csv", "a;b\n1;2\n"),
    ])
}

#[test]
fn the_stats_line_is_written_only_when_asked_and_changes_nothing_else() {
    let t = package();
    // The stats file sits outside the package, so the analyzer never reads it.
    let side = tree(&[("keep", "")]);
    let path = side.root.join("stats.ndjson");

    let plain = analyze(t.path(), None);
    assert!(plain.status.success());
    assert!(plain.stderr.is_empty());
    assert!(!path.exists(), "no stats file unless asked");

    for _ in 0..2 {
        let with = analyze(t.path(), Some(&path));
        assert!(with.status.success());
        assert!(with.stderr.is_empty(), "stderr: {}", String::from_utf8_lossy(&with.stderr));
        assert_eq!(with.stdout, plain.stdout, "stdout is the same with stats on");
    }
    let text = std::fs::read_to_string(&path).expect("stats file");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "one line per run, appended");
    let s: Value = serde_json::from_str(lines[1]).expect("a JSON line");
    let mut keys: Vec<&str> = s.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "build", "cache_errors", "compiled", "data", "data_kept_max", "data_over_budget", "ms", "ms_compiled",
            "ms_data", "ms_other", "ms_r", "ms_tests", "peak_rss_kb", "peak_vm_kb", "r", "tests", "verify_mismatch"
        ]
    );
    // The peaks are the system's where it keeps them, which Linux does, and
    // null elsewhere. The address space holds the 64 MiB of the run's stack.
    if cfg!(target_os = "linux") {
        let kb = |k: &str| s[k].as_u64().unwrap_or_else(|| panic!("{k} is a count: {}", s[k]));
        assert!(kb("peak_rss_kb") > 0);
        assert!(kb("peak_vm_kb") >= kb("peak_rss_kb"));
        assert!(kb("peak_vm_kb") >= 64 << 10, "{} kB of address space", kb("peak_vm_kb"));
    } else if !Path::new("/proc/self/status").exists() {
        assert!(s["peak_rss_kb"].is_null(), "{}", s["peak_rss_kb"]);
        assert!(s["peak_vm_kb"].is_null(), "{}", s["peak_vm_kb"]);
    }
    assert_eq!(s["build"], env!("CARGO_PKG_VERSION"));
    assert_eq!(s["compiled"]["files"], 1);
    assert_eq!(s["compiled"]["hits"], 0);
    assert_eq!(s["r"]["files"], 1);
    assert_eq!(s["tests"]["files"], 1);
    assert_eq!(s["data"]["files"], 1);
    // The one data file is two columns of one integer: an object of 48 bytes
    // and a name of 56 a column, four bytes a cell, sixteen a row.
    assert_eq!(s["data_kept_max"], 2 * (48 + 56 + 4) + 16);
    assert_eq!(s["data_over_budget"], 0);
    assert_eq!(s["cache_errors"], 0);
    assert_eq!(s["verify_mismatch"], 0);
    let ms = |k: &str| s[k].as_f64().unwrap_or_else(|| panic!("{k} is a number"));
    for k in ["ms_compiled", "ms_r", "ms_tests", "ms_data", "ms_other"] {
        assert!(ms(k) >= 0.0, "{k} is not negative");
    }
    let parts = ms("ms_compiled") + ms("ms_r") + ms("ms_tests") + ms("ms_data");
    assert!(parts <= ms("ms") + 0.01, "the phases fit inside the total");
}

/// A table one row past eight million is counted as over, and is read and
/// described all the same: the count stops nothing.
#[test]
#[cfg_attr(debug_assertions, ignore = "an unoptimized build takes minutes over a table this long")]
fn a_table_past_the_row_line_is_counted_and_still_read() {
    let rows = 8_000_001;
    let table = format!("a\n{}", "1\n".repeat(rows));
    let t = tree(&[("DESCRIPTION", DESC), ("data/long.csv", &table)]);
    let side = tree(&[("keep", "")]);
    let path = side.root.join("stats.ndjson");
    let out = analyze(t.path(), Some(&path));
    assert!(out.status.success());
    assert!(out.stderr.is_empty(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let found: Vec<Value> = String::from_utf8(out.stdout)
        .expect("utf-8 output")
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("one JSON record per line"))
        .filter(|r| r["rec"] == "dataset")
        .collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["nrow"], rows);
    assert_eq!(found[0]["columns"][0]["type"], "integer");
    assert_eq!(found[0]["columns"][0]["n_missing"], 0);
    let text = std::fs::read_to_string(&path).expect("stats file");
    let s: Value = serde_json::from_str(text.trim_end()).expect("one JSON line");
    assert_eq!(s["data_over_budget"], 1);
    assert_eq!(s["data_kept_max"], 48 + 56 + (4 + 16) * rows);
}

#[test]
fn a_stats_path_that_cannot_be_written_changes_nothing() {
    let t = package();
    let plain = analyze(t.path(), None);
    let bad = analyze(t.path(), Some(Path::new("/nonexistent-rpkg-analyzer-dir/stats.ndjson")));
    assert!(bad.status.success());
    assert!(bad.stderr.is_empty());
    assert_eq!(bad.stdout, plain.stdout);
}
