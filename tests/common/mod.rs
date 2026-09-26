// Shared helpers for the integration tests: build a package tree, run the binary.
#![allow(dead_code)]

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// A package tree under the system temp directory, removed when dropped.
pub struct Tree {
    pub root: PathBuf,
}

impl Tree {
    pub fn path(&self) -> &str {
        self.root.to_str().expect("utf-8 temp path")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Writes each (relative path, bytes) pair under a fresh directory.
pub fn tree_bytes(files: &[(&str, &[u8])]) -> Tree {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!("rpkg-analyzer-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create tree root");
    for (rel, bytes) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("a parent")).expect("create parent");
        std::fs::write(&p, bytes).expect("write fixture file");
    }
    Tree { root }
}

pub fn tree(files: &[(&str, &str)]) -> Tree {
    let as_bytes: Vec<(&str, &[u8])> = files.iter().map(|(p, t)| (*p, t.as_bytes())).collect();
    tree_bytes(&as_bytes)
}

pub fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rpkg-analyzer"))
        .args(args)
        .output()
        .expect("run the analyzer")
}

pub fn records(t: &Tree, kind: &str) -> Vec<Value> {
    let out = run(&[t.path(), "--input-kind", kind]);
    assert!(
        out.status.success(),
        "analyzer exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf-8 output")
        .lines()
        .map(|l| serde_json::from_str(l).expect("one JSON record per line"))
        .collect()
}

pub fn summary(t: &Tree, kind: &str) -> Value {
    records(t, kind)
        .into_iter()
        .find(|r| r["rec"] == "summary")
        .expect("a summary record")
}

pub const DESC: &str = "Package: fixpkg\nVersion: 1.0.0\nLicense: GPL-3\n";
