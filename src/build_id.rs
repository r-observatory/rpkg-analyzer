// The build identity: blake3 over every file under src/ by relative path, in
// sorted order, then Cargo.toml and Cargo.lock. build.rs includes this file.
use std::path::{Path, PathBuf};

fn build_id_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            build_id_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

pub fn build_id(root: &Path) -> String {
    let mut files = Vec::new();
    build_id_files(&root.join("src"), &mut files);
    files.sort();
    files.push(root.join("Cargo.toml"));
    files.push(root.join("Cargo.lock"));
    let mut h = blake3::Hasher::new();
    for f in &files {
        let rel = f.strip_prefix(root).unwrap_or(f).to_string_lossy().replace('\\', "/");
        let bytes = std::fs::read(f).unwrap_or_default();
        h.update(&(rel.len() as u64).to_le_bytes());
        h.update(rel.as_bytes());
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    h.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rpa-build-id-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, text) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        root
    }

    #[test]
    fn any_source_manifest_or_lock_byte_changes_the_id() {
        let base = [
            ("src/main.rs", "fn main() {}"),
            ("src/sub/a.rs", "pub fn a() {}"),
            ("Cargo.toml", "[package]"),
            ("Cargo.lock", "version = 4"),
            ("README.md", "readme"),
        ];
        let id0 = build_id(&tree("base", &base));
        assert_eq!(id0.len(), 64);
        assert_eq!(build_id(&tree("again", &base)), id0, "the same bytes give the same id");
        for (i, (path, _)) in base.iter().enumerate() {
            let mut changed = base.to_vec();
            changed[i].1 = "changed";
            let id = build_id(&tree(&format!("changed{i}"), &changed));
            if *path == "README.md" {
                assert_eq!(id, id0, "a file outside src/ and the manifests is not hashed");
            } else {
                assert_ne!(id, id0, "{path} changed");
            }
        }
        let mut more = base.to_vec();
        more.push(("src/new.rs", ""));
        assert_ne!(build_id(&tree("more", &more)), id0, "a new empty file under src/");
        let mut moved = base.to_vec();
        moved[1].0 = "src/sub/b.rs";
        assert_ne!(build_id(&tree("moved", &moved)), id0, "a renamed file");
    }
}
