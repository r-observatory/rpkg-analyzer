//! Per-file results under RPKG_ANALYZER_CACHE_DIR, so bytes an earlier version held are not parsed again.
//! A cache that cannot be read or written only misses; it never changes stdout, stderr or the exit status.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

/// Bumped whenever src_facts or a type it returns changes.
pub const CACHE_FORMAT: u32 = 1;
/// Below this size a parse costs less than the round trip.
pub const MIN_BYTES: usize = 2048;
const MAGIC: &[u8; 4] = b"RPAC";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// blake3 over src/, Cargo.toml and Cargo.lock, from build.rs.
pub const BUILD_ID: &str = env!("RPKG_ANALYZER_BUILD_ID");

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct CacheStats {
    pub hits: u64,
    pub errors: u64,
    pub verify_mismatch: u64,
}

pub struct Cache {
    dir: Option<PathBuf>,
    verify: bool,
    pub stats: CacheStats,
}

impl Cache {
    pub fn new(dir: Option<PathBuf>, verify: bool) -> Cache {
        Cache { dir, verify, stats: CacheStats::default() }
    }

    /// RPKG_ANALYZER_CACHE_DIR (unset or empty: no cache) and
    /// RPKG_ANALYZER_CACHE_VERIFY=1 (compute on every hit too, and compare).
    pub fn from_env() -> Cache {
        let dir = std::env::var_os("RPKG_ANALYZER_CACHE_DIR").filter(|d| !d.is_empty()).map(PathBuf::from);
        let verify = std::env::var_os("RPKG_ANALYZER_CACHE_VERIFY").is_some_and(|v| v == "1");
        Cache::new(dir, verify)
    }

    /// The stored value for these bytes, or `compute()` stored for next time.
    pub fn get_or_compute<T, F>(&mut self, kind: &str, ext: &str, bytes: &[u8], compute: F) -> T
    where
        T: Serialize + DeserializeOwned + PartialEq,
        F: FnOnce() -> T,
    {
        let Some(dir) = self.dir.as_ref().filter(|_| bytes.len() >= MIN_BYTES) else {
            return compute();
        };
        let key = key(kind, ext, bytes);
        let hex = blake3::Hash::from(key).to_hex().to_string();
        let path = dir.join(kind).join(&hex[..2]).join(&hex);
        match std::fs::read(&path) {
            Ok(buf) => match decode(&buf, &key).and_then(|p| serde_json::from_slice::<T>(p).map_err(|_| "payload")) {
                Ok(stored) => {
                    self.stats.hits += 1;
                    if !self.verify {
                        return stored;
                    }
                    let fresh = compute();
                    if fresh != stored {
                        self.stats.verify_mismatch += 1;
                    }
                    return fresh;
                }
                Err(_) => self.stats.errors += 1,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => self.stats.errors += 1,
        }
        let fresh = compute();
        if let Ok(payload) = serde_json::to_vec(&fresh) {
            write_entry(&path, &hex, &encode(&key, &payload));
        }
        fresh
    }
}

/// blake3 over the namespace (format, version, build), the kind, the extension
/// and the bytes, each length-prefixed. The path is left out on purpose.
pub fn key(kind: &str, ext: &str, bytes: &[u8]) -> [u8; 32] {
    let format = CACHE_FORMAT.to_le_bytes();
    let mut h = blake3::Hasher::new();
    for part in [&format[..], VERSION.as_bytes(), BUILD_ID.as_bytes(), kind.as_bytes(), ext.as_bytes(), bytes] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    *h.finalize().as_bytes()
}

/// magic, format, version, build, key, payload length, payload, blake3(payload).
pub fn encode(key: &[u8; 32], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 200);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&CACHE_FORMAT.to_le_bytes());
    for part in [VERSION.as_bytes(), BUILD_ID.as_bytes()] {
        out.extend_from_slice(&(part.len() as u16).to_le_bytes());
        out.extend_from_slice(part);
    }
    out.extend_from_slice(key);
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(blake3::hash(payload).as_bytes());
    out
}

/// The payload of an entry written by this build for this key, or why not.
pub fn decode<'a>(buf: &'a [u8], key: &[u8; 32]) -> Result<&'a [u8], &'static str> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&'a [u8], &'static str> {
        let end = at.checked_add(n).filter(|&e| e <= buf.len()).ok_or("short")?;
        let s = &buf[at..end];
        at = end;
        Ok(s)
    };
    if take(4)? != MAGIC {
        return Err("magic");
    }
    if take(4)? != CACHE_FORMAT.to_le_bytes() {
        return Err("format");
    }
    for (want, what) in [(VERSION.as_bytes(), "version"), (BUILD_ID.as_bytes(), "build")] {
        let n = u16::from_le_bytes(take(2)?.try_into().map_err(|_| "short")?) as usize;
        if take(n)? != want {
            return Err(what);
        }
    }
    if take(32)? != key {
        return Err("key");
    }
    let n = u64::from_le_bytes(take(8)?.try_into().map_err(|_| "short")?);
    let payload = take(usize::try_from(n).map_err(|_| "length")?)?;
    if take(32)? != blake3::hash(payload).as_bytes() {
        return Err("checksum");
    }
    if at != buf.len() {
        return Err("trailing");
    }
    Ok(payload)
}

/// Writes beside the entry and renames over it, so a reader sees all of an
/// entry or none. A failure removes the temporary file and is otherwise ignored.
fn write_entry(path: &Path, hex: &str, bytes: &[u8]) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!("{hex}.{}.{nanos}.tmp", std::process::id()));
    if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rpa-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn big() -> Vec<u8> {
        vec![b'x'; MIN_BYTES]
    }

    fn entries(dir: &Path) -> Vec<PathBuf> {
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
    fn an_entry_reads_back_and_every_damaged_byte_is_refused() {
        let k = key("src", "c", b"int f(void);");
        let buf = encode(&k, b"{\"a\":1}");
        assert_eq!(decode(&buf, &k), Ok(&b"{\"a\":1}"[..]));
        for i in 0..buf.len() {
            let mut bad = buf.clone();
            bad[i] ^= 0x01;
            assert!(decode(&bad, &k).is_err(), "a flipped bit at byte {i} was accepted");
        }
        for n in 0..buf.len() {
            assert!(decode(&buf[..n], &k).is_err(), "a truncation to {n} bytes was accepted");
        }
        let mut longer = buf.clone();
        longer.push(0);
        assert_eq!(decode(&longer, &k), Err("trailing"));
        assert!(decode(&vec![0u8; buf.len()], &k).is_err(), "a zeroed entry was accepted");
        let other = key("src", "cpp", b"int f(void);");
        assert_eq!(decode(&buf, &other), Err("key"), "an entry for another key");
    }

    #[test]
    fn the_key_covers_the_extension_and_the_kind_but_not_a_path() {
        let a = key("src", "c", b"int f(void);");
        assert_eq!(a, key("src", "c", b"int f(void);"));
        assert_ne!(a, key("src", "cpp", b"int f(void);"));
        assert_ne!(a, key("r", "c", b"int f(void);"));
        assert_ne!(a, key("src", "c", b"int g(void);"));
        // Length prefixes keep the parts apart.
        assert_ne!(key("src", "ca", b"b"), key("src", "c", b"ab"));
        assert_eq!(BUILD_ID.len(), 64, "a blake3 hex digest");
    }

    #[test]
    fn a_miss_computes_and_stores_and_a_hit_does_not_compute() {
        let dir = scratch("hit");
        let mut c = Cache::new(Some(dir.clone()), false);
        let calls = Cell::new(0);
        let v: Vec<String> = c.get_or_compute("src", "c", &big(), || {
            calls.set(calls.get() + 1);
            vec!["one".to_string()]
        });
        assert_eq!((v, calls.get(), c.stats), (vec!["one".to_string()], 1, CacheStats::default()));
        let again: Vec<String> = c.get_or_compute("src", "c", &big(), || {
            calls.set(calls.get() + 1);
            vec!["never".to_string()]
        });
        assert_eq!(again, vec!["one".to_string()]);
        assert_eq!(calls.get(), 1, "a hit does not compute");
        assert_eq!(c.stats.hits, 1);
        assert!(entries(&dir).iter().all(|p| p.extension().is_none_or(|e| e != "tmp")), "no temporary file left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_files_and_no_directory_always_compute() {
        let dir = scratch("small");
        let mut c = Cache::new(Some(dir.clone()), false);
        let small = vec![b'x'; MIN_BYTES - 1];
        for _ in 0..2 {
            let v: u32 = c.get_or_compute("src", "c", &small, || 7);
            assert_eq!(v, 7);
        }
        assert_eq!(c.stats, CacheStats::default());
        assert!(!dir.exists(), "nothing written for a small file");
        let mut off = Cache::new(None, false);
        for _ in 0..2 {
            let v: u32 = off.get_or_compute("src", "c", &big(), || 7);
            assert_eq!(v, 7);
        }
        assert_eq!(off.stats, CacheStats::default());
    }

    #[test]
    fn a_damaged_entry_is_counted_recomputed_and_replaced() {
        let dir = scratch("damage");
        let mut c = Cache::new(Some(dir.clone()), false);
        let _: u32 = c.get_or_compute("src", "c", &big(), || 7);
        let files = entries(&dir);
        assert_eq!(files.len(), 1);
        let mut bytes = std::fs::read(&files[0]).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&files[0], &bytes).unwrap();
        let v: u32 = c.get_or_compute("src", "c", &big(), || 7);
        assert_eq!((v, c.stats.errors, c.stats.hits), (7, 1, 0));
        let v: u32 = c.get_or_compute("src", "c", &big(), || 8);
        assert_eq!((v, c.stats.hits), (7, 1), "the rewritten entry is read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_mode_computes_on_a_hit_and_counts_a_mismatch() {
        let dir = scratch("verify");
        let mut c = Cache::new(Some(dir.clone()), false);
        let _: u32 = c.get_or_compute("src", "c", &big(), || 7);
        let mut v = Cache::new(Some(dir.clone()), true);
        let same: u32 = v.get_or_compute("src", "c", &big(), || 7);
        assert_eq!((same, v.stats.hits, v.stats.verify_mismatch), (7, 1, 0));
        let differs: u32 = v.get_or_compute("src", "c", &big(), || 9);
        assert_eq!((differs, v.stats.hits, v.stats.verify_mismatch), (9, 2, 1), "the computed value wins");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_that_cannot_hold_entries_only_misses() {
        let file = scratch("file");
        std::fs::write(&file, b"not a directory").unwrap();
        let mut c = Cache::new(Some(file.clone()), false);
        for _ in 0..2 {
            let v: u32 = c.get_or_compute("src", "c", &big(), || 7);
            assert_eq!(v, 7);
        }
        assert_eq!(c.stats.hits, 0);
        let _ = std::fs::remove_file(&file);

        use std::os::unix::fs::PermissionsExt;
        let ro = scratch("readonly");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let mut c = Cache::new(Some(ro.clone()), false);
        for _ in 0..2 {
            let v: u32 = c.get_or_compute("src", "c", &big(), || 7);
            assert_eq!(v, 7);
        }
        assert_eq!(c.stats.hits, 0);
        assert!(entries(&ro).is_empty(), "nothing written, nothing left behind");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&ro);
    }
}
