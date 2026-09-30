mod common;
use common::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A C file well past the size the cache skips, with a small call graph.
fn big_c() -> String {
    let mut s = String::new();
    for i in 0..120 {
        s.push_str(&format!("int fn{i:03}(int x) {{ return fn{:03}(x) + {i}; }}\n", (i + 1) % 120));
    }
    assert!(s.len() > 4096);
    s
}

fn analyze(dir: &str, cache: Option<&Path>, verify: bool, stats: &Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rpkg-analyzer"));
    cmd.args([dir, "--input-kind", "release"])
        .env("RPKG_ANALYZER_STATS", stats)
        .env_remove("RPKG_ANALYZER_CACHE_DIR")
        .env_remove("RPKG_ANALYZER_CACHE_VERIFY");
    if let Some(c) = cache {
        cmd.env("RPKG_ANALYZER_CACHE_DIR", c);
    }
    if verify {
        cmd.env("RPKG_ANALYZER_CACHE_VERIFY", "1");
    }
    let out = cmd.output().expect("run the analyzer");
    assert!(out.status.success(), "exit {:?}", out.status.code());
    assert!(out.stderr.is_empty(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    out
}

fn last_stats(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("stats file");
    serde_json::from_str(text.lines().last().expect("a stats line")).expect("JSON")
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut st = vec![dir.to_path_buf()];
    while let Some(d) = st.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { st.push(p) } else { out.push(p) }
        }
    }
    out
}

#[test]
fn cached_runs_print_what_uncached_runs_print() {
    let c = big_c();
    // One blob at an own path, then under a vendored path, then as C++.
    let own = tree(&[("DESCRIPTION", DESC), ("src/big.c", &c)]);
    let vendored = tree(&[("DESCRIPTION", DESC), ("src/rust/vendor/x/big.c", &c), ("src/small.c", "int s(void) { return 0; }\n")]);
    let as_cpp = tree(&[("DESCRIPTION", DESC), ("src/big.cpp", &c)]);
    let side = tree(&[("keep", "")]);
    let dir = side.root.join("cache");
    let stats = side.root.join("stats.ndjson");

    let plain: Vec<Vec<u8>> = [&own, &vendored, &as_cpp].iter().map(|t| analyze(t.path(), None, false, &stats).stdout).collect();

    let cold = analyze(own.path(), Some(&dir), false, &stats);
    assert_eq!(cold.stdout, plain[0]);
    assert_eq!(last_stats(&stats)["compiled"]["hits"], 0, "cold");
    let warm = analyze(own.path(), Some(&dir), false, &stats);
    assert_eq!(warm.stdout, plain[0]);
    assert_eq!(last_stats(&stats)["compiled"]["hits"], 1, "warm");

    let moved = analyze(vendored.path(), Some(&dir), false, &stats);
    assert_eq!(moved.stdout, plain[1], "the vendored path applies its own rules after the lookup");
    assert_eq!(last_stats(&stats)["compiled"]["hits"], 1, "the same blob at a vendored path is the same entry");

    let cpp = analyze(as_cpp.path(), Some(&dir), false, &stats);
    assert_eq!(cpp.stdout, plain[2]);
    assert_eq!(last_stats(&stats)["compiled"]["hits"], 0, "the same bytes as C++ are another entry");

    let verified = analyze(own.path(), Some(&dir), true, &stats);
    assert_eq!(verified.stdout, plain[0]);
    let s = last_stats(&stats);
    assert_eq!((s["compiled"]["hits"].as_u64(), s["verify_mismatch"].as_u64()), (Some(1), Some(0)));

    // Damage every entry: each is refused, counted and recomputed.
    for f in files_under(&dir) {
        let mut b = std::fs::read(&f).unwrap();
        let mid = b.len() / 2;
        b[mid] ^= 0x40;
        std::fs::write(&f, b).unwrap();
    }
    let damaged = analyze(own.path(), Some(&dir), false, &stats);
    assert_eq!(damaged.stdout, plain[0]);
    let s = last_stats(&stats);
    assert_eq!((s["compiled"]["hits"].as_u64(), s["cache_errors"].as_u64()), (Some(0), Some(1)));

    // A cache path that is a file is a cache that only misses.
    let file = side.root.join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    assert_eq!(analyze(own.path(), Some(&file), false, &stats).stdout, plain[0]);
    assert!(files_under(&dir).iter().all(|p| !p.to_string_lossy().ends_with(".tmp")), "no temporary file left");
}

#[test]
fn four_processes_can_share_one_directory() {
    let c = big_c();
    let own = tree(&[("DESCRIPTION", DESC), ("src/big.c", &c), ("src/other.c", &c.replace("fn", "gn"))]);
    let side = tree(&[("keep", "")]);
    let dir = side.root.join("cache");
    let stats = side.root.join("stats.ndjson");
    let plain = analyze(own.path(), None, false, &stats).stdout;
    for _ in 0..3 {
        let kids: Vec<std::process::Child> = (0..4)
            .map(|_| {
                Command::new(env!("CARGO_BIN_EXE_rpkg-analyzer"))
                    .args([own.path(), "--input-kind", "release"])
                    .env("RPKG_ANALYZER_CACHE_DIR", &dir)
                    .env_remove("RPKG_ANALYZER_STATS")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .expect("spawn")
            })
            .collect();
        for k in kids {
            let out = k.wait_with_output().expect("wait");
            assert!(out.status.success());
            assert!(out.stderr.is_empty());
            assert_eq!(out.stdout, plain);
        }
    }
    let left: Vec<PathBuf> = files_under(&dir);
    assert_eq!(left.len(), 2, "one entry per blob: {left:?}");
    assert!(left.iter().all(|p| !p.to_string_lossy().ends_with(".tmp")));
}

/// A blank `RPKG_ANALYZER_CACHE_DIR:` line in a workflow sets the variable to
/// nothing, which must mean no cache rather than one in the working directory.
#[test]
fn an_empty_cache_variable_means_no_cache() {
    let c = big_c();
    let own = tree(&[("DESCRIPTION", DESC), ("src/big.c", &c)]);
    let cwd = tree(&[("keep", "")]);
    let stats = cwd.root.join("stats.ndjson");
    let plain = analyze(own.path(), None, false, &stats).stdout;
    for _ in 0..2 {
        let out = Command::new(env!("CARGO_BIN_EXE_rpkg-analyzer"))
            .args([own.path(), "--input-kind", "release"])
            .current_dir(&cwd.root)
            .env("RPKG_ANALYZER_CACHE_DIR", "")
            .env_remove("RPKG_ANALYZER_STATS")
            .output()
            .expect("run the analyzer");
        assert!(out.status.success());
        assert_eq!(out.stdout, plain);
    }
    let left: Vec<PathBuf> = files_under(&cwd.root);
    assert_eq!(left.len(), 2, "only the files the test put there: {left:?}");
}

/// Vendored copies of one header sit at two paths in a single version, and a
/// file too large to skip the cache may not be valid UTF-8.
#[test]
fn one_blob_at_two_paths_in_one_version_keeps_both_paths() {
    let c = big_c();
    let mut latin = b"/* caf\xe9 */\n".to_vec();
    latin.extend_from_slice(c.replace("fn", "ln").as_bytes());
    let t = tree_bytes(&[
        ("DESCRIPTION", DESC.as_bytes()),
        ("src/a/x.h", c.as_bytes()),
        ("src/b/x.h", c.as_bytes()),
        ("src/latin.c", &latin),
    ]);
    let side = tree(&[("keep", "")]);
    let dir = side.root.join("cache");
    let stats = side.root.join("stats.ndjson");
    let plain = analyze(t.path(), None, false, &stats).stdout;
    let cached = analyze(t.path(), Some(&dir), false, &stats);
    assert_eq!(cached.stdout, plain);
    let s = last_stats(&stats);
    assert_eq!(s["compiled"]["hits"], 1, "the second path reads the first one's entry");
    assert_eq!(files_under(&dir).len(), 1, "no entry for the file that is not UTF-8");
    let out = String::from_utf8(plain).unwrap();
    for path in ["src/a/x.h", "src/b/x.h"] {
        assert!(out.contains(&format!("\"file\":\"{path}\"")), "function records at {path}");
    }
}
