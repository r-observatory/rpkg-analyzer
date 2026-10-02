mod common;
use common::*;
use serde_json::Value;

fn stdout_of(dir: &str) -> Vec<u8> {
    let out = run(&[dir, "--input-kind", "release"]);
    assert!(out.status.success(), "analyzer exited {:?}", out.status.code());
    assert!(out.stderr.is_empty(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// R, native and C graphs with enough edges that a hash order would show.
fn graph_package() -> Tree {
    let mut r = String::new();
    let mut c = String::new();
    for i in 0..30 {
        r.push_str(&format!(
            "r{i:02} <- function(x) {{ r{:02}(x); r{:02}(x); r{:02}(x); .Call(C_c{i:02}, x) }}\n",
            (i + 1) % 30,
            (i + 7) % 30,
            (i + 13) % 30
        ));
        c.push_str(&format!(
            "SEXP c{i:02}(SEXP x) {{ c{:02}(x); c{:02}(x); return x; }}\n",
            (i + 1) % 30,
            (i + 11) % 30
        ));
    }
    tree(&[("DESCRIPTION", DESC), ("R/a.R", &r), ("src/a.c", &c)])
}

#[test]
fn every_run_writes_the_same_bytes() {
    let t = graph_package();
    let first = stdout_of(t.path());
    for _ in 0..5 {
        assert_eq!(stdout_of(t.path()), first, "a graph package");
    }
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pkg");
    let first = stdout_of(fixtures);
    for _ in 0..2 {
        assert_eq!(stdout_of(fixtures), first, "the dataset fixtures");
    }
}

#[test]
fn call_edges_come_out_in_node_order_within_each_graph() {
    let t = graph_package();
    let out = String::from_utf8(stdout_of(t.path())).expect("utf-8 output");
    let recs: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).expect("one JSON record per line")).collect();
    let edges = |graph: &str| -> Vec<(String, String)> {
        recs.iter()
            .filter(|r| r["rec"] == "call_edge" && r["graph"] == graph)
            .map(|r| (r["from"].as_str().unwrap().to_string(), r["to"].as_str().unwrap().to_string()))
            .collect()
    };
    // A graph's nodes are its function records, in the order they are printed.
    let position = |lang: &str| -> std::collections::HashMap<String, usize> {
        recs.iter()
            .filter(|r| r["rec"] == "function" && r["lang"] == lang)
            .enumerate()
            .map(|(i, r)| (r["name"].as_str().unwrap().to_string(), i))
            .collect()
    };
    for (graph, lang) in [("r", "r"), ("c", "c")] {
        let pos = position(lang);
        let got = edges(graph);
        assert!(got.len() >= 30, "{graph} has {} edges", got.len());
        let mut want = got.clone();
        want.sort_by_key(|(a, b)| (pos[a], pos[b]));
        assert_eq!(got, want, "{graph} edges in node order");
    }
    let native = edges("native");
    assert_eq!(native.len(), 30);
    let mut want = native.clone();
    want.sort();
    assert_eq!(native, want, "native edges in name order");
}

/// The dataset records of a run are the ones the data mode prints, in its
/// order, and nothing is printed after them or among the other records.
#[test]
fn dataset_records_end_the_output_in_the_order_of_the_data_mode() {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pkg");
    let whole = String::from_utf8(stdout_of(fixtures)).expect("utf-8 output");
    let data = run(&["--datasets", fixtures]);
    assert!(data.status.success(), "the data mode exited {:?}", data.status.code());
    let data = String::from_utf8(data.stdout).expect("utf-8 output");
    assert_eq!(data.lines().count(), 127, "one record per saved object");
    assert!(whole.ends_with(&data), "the dataset records end the output");
    for line in whole[..whole.len() - data.len()].lines() {
        let rec: Value = serde_json::from_str(line).expect("one JSON record per line");
        assert_ne!(rec["rec"], "dataset", "a dataset record among the others");
    }
}
