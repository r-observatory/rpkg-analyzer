// Metadata reader for R's serialization format (.rda / .RData / .rds) under a
// package's data/ directory and R/sysdata.rda. A pure function of the bytes: no
// R, no build, no network, so it runs identically on any archived version.
//
// It reads structure (class, dimensions, per-column names/types/factor levels)
// from headers and attributes, then makes one bounded pass over the values to
// add a data-quality profile (missing counts, unique counts, numeric ranges) and
// a tiered fingerprint (schema / shape / content / per-column / row sketch) so
// datasets can be matched across packages. Handles gzip, bzip2 and xz framing
// and serialization versions 2 and 3; S4 objects degrade to their class rather
// than crash. Exposed behind the --datasets flag.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;

const SYMSXP: u8 = 1;
const LISTSXP: u8 = 2;
const CHARSXP: u8 = 9;
const LGLSXP: u8 = 10;
const INTSXP: u8 = 13;
const REALSXP: u8 = 14;
const CPLXSXP: u8 = 15;
const STRSXP: u8 = 16;
const VECSXP: u8 = 19;
const RAWSXP: u8 = 24;
const S4SXP: u8 = 25;
const NILVALUE: u8 = 254;
const REFSXP: u8 = 255;
const NA_INT: i32 = i32::MIN;

// Above this many cells in one vector, skip the value pass and keep structure
// only, so a pathologically large object cannot blow up time or memory.
const CELL_CAP: usize = 8_000_000;
// Bottom-k row sketch size (KMV), for Jaccard/containment between datasets.
const SKETCH_K: usize = 32;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
fn fnv(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[derive(Clone)]
#[allow(dead_code)] // Blob.len retained for future complex/raw reporting
enum Val {
    Nil,
    Sym(String),
    Char(Option<String>),
    Str(Vec<Option<String>>),
    Ints { len: usize, vals: Vec<i32>, logical: bool },
    Reals { len: usize, vals: Vec<f64> },
    Blob { len: usize },
    Vec(Vec<Node>),
    List { tag: Box<Node>, car: Box<Node>, cdr: Box<Node> },
    S4,
}

#[derive(Clone)]
struct Node {
    val: Val,
    attr: Option<Box<Node>>,
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
    ver: i32,
    refs: Vec<Node>,
    budget: u32,
}

impl<'a> Reader<'a> {
    fn need(&self, n: usize) -> Result<(), String> {
        if self.p.checked_add(n).map_or(true, |e| e > self.b.len()) {
            Err("truncated stream".into())
        } else {
            Ok(())
        }
    }
    fn i32(&mut self) -> Result<i32, String> {
        self.need(4)?;
        let v = i32::from_be_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        Ok(v)
    }
    fn f64(&mut self) -> Result<f64, String> {
        self.need(8)?;
        let v = f64::from_be_bytes(self.b[self.p..self.p + 8].try_into().unwrap());
        self.p += 8;
        Ok(v)
    }
    fn vlen(&mut self) -> Result<usize, String> {
        let n = self.i32()?;
        if n == -1 {
            let hi = self.i32()? as i64;
            let lo = self.i32()? as i64;
            Ok(((hi << 32) | (lo & 0xffff_ffff)) as usize)
        } else if n < 0 {
            Err("negative length".into())
        } else {
            Ok(n as usize)
        }
    }
    fn skip(&mut self, n: usize) -> Result<(), String> {
        self.need(n)?;
        self.p += n;
        Ok(())
    }
    fn maybe_attr(&mut self, ha: bool) -> Result<Option<Box<Node>>, String> {
        if ha {
            Ok(Some(Box::new(self.item()?)))
        } else {
            Ok(None)
        }
    }
    fn item(&mut self) -> Result<Node, String> {
        if self.budget == 0 {
            return Err("item budget exceeded".into());
        }
        self.budget -= 1;
        let f = self.i32()?;
        let t = (f & 0xFF) as u8;
        let ha = f & (1 << 9) != 0;
        let hg = f & (1 << 10) != 0;
        match t {
            0 | NILVALUE => Ok(Node { val: Val::Nil, attr: None }),
            REFSXP => {
                let mut idx = (f >> 8) as usize;
                if idx == 0 {
                    idx = self.i32()? as usize;
                }
                idx.checked_sub(1)
                    .and_then(|i| self.refs.get(i))
                    .cloned()
                    .ok_or_else(|| "bad reference index".to_string())
            }
            SYMSXP => {
                let name = self.item()?;
                let s = match name.val {
                    Val::Char(Some(x)) => x,
                    _ => String::new(),
                };
                let node = Node { val: Val::Sym(s), attr: None };
                self.refs.push(node.clone());
                Ok(node)
            }
            CHARSXP => {
                let n = self.i32()?;
                if n < 0 {
                    Ok(Node { val: Val::Char(None), attr: None })
                } else {
                    let n = n as usize;
                    self.need(n)?;
                    let s = String::from_utf8_lossy(&self.b[self.p..self.p + n]).into_owned();
                    self.p += n;
                    Ok(Node { val: Val::Char(Some(s)), attr: None })
                }
            }
            STRSXP => {
                let n = self.vlen()?;
                let mut v = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    let e = self.item()?;
                    v.push(match e.val {
                        Val::Char(x) => x,
                        _ => None,
                    });
                }
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Str(v), attr })
            }
            LGLSXP | INTSXP => {
                let n = self.vlen()?;
                let vals = if n <= CELL_CAP {
                    let mut s = Vec::with_capacity(n);
                    for _ in 0..n {
                        s.push(self.i32()?);
                    }
                    s
                } else {
                    self.skip(4 * n)?;
                    Vec::new()
                };
                let attr = self.maybe_attr(ha)?;
                Ok(Node {
                    val: Val::Ints { len: n, vals, logical: t == LGLSXP },
                    attr,
                })
            }
            REALSXP => {
                let n = self.vlen()?;
                let vals = if n <= CELL_CAP {
                    let mut s = Vec::with_capacity(n);
                    for _ in 0..n {
                        s.push(self.f64()?);
                    }
                    s
                } else {
                    self.skip(8 * n)?;
                    Vec::new()
                };
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Reals { len: n, vals }, attr })
            }
            CPLXSXP => {
                let n = self.vlen()?;
                self.skip(16 * n)?;
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Blob { len: n }, attr })
            }
            RAWSXP => {
                let n = self.vlen()?;
                self.skip(n)?;
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Blob { len: n }, attr })
            }
            VECSXP => {
                let n = self.vlen()?;
                let mut els = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    els.push(self.item()?);
                }
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Vec(els), attr })
            }
            LISTSXP => {
                let attr = self.maybe_attr(ha)?;
                let tag = if hg {
                    Box::new(self.item()?)
                } else {
                    Box::new(Node { val: Val::Nil, attr: None })
                };
                let car = Box::new(self.item()?);
                let cdr = Box::new(self.item()?);
                Ok(Node { val: Val::List { tag, car, cdr }, attr })
            }
            S4SXP => {
                let attr = if ha {
                    self.item().ok().map(Box::new)
                } else {
                    None
                };
                Ok(Node { val: Val::S4, attr })
            }
            other => Err(format!("unhandled SEXPTYPE {other}")),
        }
    }
}

// ---- attribute helpers ------------------------------------------------------

fn attr_pairs(node: &Node) -> Vec<(String, &Node)> {
    let mut out = Vec::new();
    let mut cur = node.attr.as_deref();
    while let Some(n) = cur {
        if let Val::List { tag, car, cdr } = &n.val {
            if let Val::Sym(name) = &tag.val {
                out.push((name.clone(), car.as_ref()));
            }
            cur = Some(cdr.as_ref());
        } else {
            break;
        }
    }
    out
}
fn attr<'b>(pairs: &[(String, &'b Node)], key: &str) -> Option<&'b Node> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
}
fn str_vec(node: &Node) -> Vec<String> {
    if let Val::Str(v) = &node.val {
        v.iter().filter_map(|s| s.clone()).collect()
    } else {
        Vec::new()
    }
}
fn opt_str_vec(node: &Node) -> Vec<Option<String>> {
    if let Val::Str(v) = &node.val {
        v.clone()
    } else {
        Vec::new()
    }
}

fn nrow_of(rn: Option<&Node>) -> Option<i64> {
    match &rn?.val {
        Val::Ints { len, vals, .. } => {
            if *len == 2 && vals.len() == 2 && vals[0] == NA_INT {
                Some((vals[1] as i64).abs())
            } else {
                Some(*len as i64)
            }
        }
        Val::Str(v) => Some(v.len() as i64),
        _ => None,
    }
}

fn base_type(col: &Node) -> (&'static str, bool, i64) {
    let pairs = attr_pairs(col);
    let class = attr(&pairs, "class").map(str_vec).unwrap_or_default();
    if class.iter().any(|c| c == "factor") {
        let nlev = attr(&pairs, "levels")
            .map(|n| if let Val::Str(v) = &n.val { v.len() as i64 } else { 0 })
            .unwrap_or(0);
        return ("factor", true, nlev);
    }
    if class.iter().any(|c| c == "Date") {
        return ("Date", false, -1);
    }
    if class.iter().any(|c| c == "POSIXct") {
        return ("POSIXct", false, -1);
    }
    let t = match &col.val {
        Val::Ints { logical: true, .. } => "logical",
        Val::Ints { .. } => "integer",
        Val::Reals { .. } => "numeric",
        Val::Str(_) => "character",
        Val::Blob { .. } => "complex",
        Val::Vec(_) => "list",
        _ => "unknown",
    };
    (t, false, -1)
}

// ---- value pass: profile + fingerprints -------------------------------------

// Append the canonical bytes of column `col`'s cell `i` to `buf`. A missing cell
// is a single 0 byte; factors resolve to their level LABEL (so re-leveling does
// not change the fingerprint); doubles fold -0.0 to 0.0.
fn push_cell(buf: &mut Vec<u8>, col: &Node, i: usize, levels: Option<&[Option<String>]>) -> bool {
    match &col.val {
        Val::Ints { vals, .. } => {
            let v = vals.get(i).copied().unwrap_or(NA_INT);
            if v == NA_INT {
                buf.push(0);
                return true;
            }
            buf.push(1);
            if let Some(lv) = levels {
                let idx = (v as isize - 1) as usize;
                if let Some(Some(lbl)) = lv.get(idx) {
                    buf.extend_from_slice(lbl.as_bytes());
                }
                buf.push(0);
            } else {
                buf.extend_from_slice(&v.to_be_bytes());
            }
            false
        }
        Val::Reals { vals, .. } => {
            let v = vals.get(i).copied().unwrap_or(f64::NAN);
            if v.is_nan() {
                buf.push(0);
                return true;
            }
            buf.push(1);
            let v = if v == 0.0 { 0.0 } else { v };
            buf.extend_from_slice(&v.to_be_bytes());
            false
        }
        Val::Str(vals) => match vals.get(i) {
            Some(Some(s)) => {
                buf.push(1);
                buf.extend_from_slice(s.as_bytes());
                buf.push(0);
                false
            }
            _ => {
                buf.push(0);
                true
            }
        },
        _ => {
            buf.push(0);
            true
        }
    }
}

fn col_len(col: &Node) -> usize {
    match &col.val {
        Val::Ints { vals, .. } => vals.len(),
        Val::Reals { vals, .. } => vals.len(),
        Val::Str(v) => v.len(),
        _ => 0,
    }
}

fn hex128(h: &blake3::Hash) -> String {
    h.to_hex()[..32].to_string()
}

struct Profile {
    columns: Vec<Value>,
    n_missing_total: u64,
    schema_fp: String,
    shape_fp: String,
    content_fp: String,
    row_sketch: Vec<String>,
    capped: bool,
}

// Profile a set of columns (a data frame, or a single atomic vector as one
// column). Returns None if values were not materialized (over the cap).
fn profile_columns(cols: &[&Node], names: &[String]) -> Option<Profile> {
    // A column with materialized values; a value-less (capped) column aborts.
    let n = cols.iter().map(|c| col_len(c)).min().unwrap_or(0);
    let any_lenless = cols.iter().any(|c| col_len(c) == 0 && !matches!(c.val, Val::Str(_)));
    if n == 0 || any_lenless {
        return None;
    }

    let mut col_json = Vec::with_capacity(cols.len());
    let mut col_fp_bytes: Vec<u8> = Vec::new();
    let mut cell_hashes: Vec<Vec<u64>> = Vec::with_capacity(cols.len());
    let mut n_missing_total = 0u64;
    let mut schema_src = String::new();
    let mut shape_src = String::new();

    for (j, col) in cols.iter().enumerate() {
        let (ty, is_factor, nlev) = base_type(col);
        let pairs = attr_pairs(col);
        let levels: Vec<Option<String>> = if is_factor {
            attr(&pairs, "levels").map(opt_str_vec).unwrap_or_default()
        } else {
            Vec::new()
        };
        let is_num = matches!(col.val, Val::Reals { .. })
            || (matches!(col.val, Val::Ints { .. }) && !is_factor);
        let is_int = matches!(col.val, Val::Ints { .. }) && !is_factor;

        let mut hasher = blake3::Hasher::new();
        hasher.update(ty.as_bytes());
        let mut n_missing = 0u64;
        let mut uniq: HashSet<u64> = HashSet::new();
        let mut ch = Vec::with_capacity(n);
        let mut min: Option<f64> = None;
        let mut max: Option<f64> = None;
        let mut buf = Vec::with_capacity(16);

        for i in 0..n {
            buf.clear();
            let missing = push_cell(&mut buf, col, i, if is_factor { Some(&levels) } else { None });
            hasher.update(&buf);
            let h = fnv(&buf);
            ch.push(h);
            uniq.insert(h);
            if missing {
                n_missing += 1;
            } else if is_num {
                let v = match &col.val {
                    Val::Reals { vals, .. } => vals[i],
                    Val::Ints { vals, .. } => vals[i] as f64,
                    _ => 0.0,
                };
                min = Some(min.map_or(v, |m| m.min(v)));
                max = Some(max.map_or(v, |m| m.max(v)));
            }
        }
        let fp = hex128(&hasher.finalize());
        n_missing_total += n_missing;

        let name = names.get(j).cloned().unwrap_or_default();
        let btype = if is_factor { "factor" } else { ty };
        schema_src.push_str(&format!("{name}:{btype}|"));
        shape_src.push_str(&format!("{btype}|"));

        let mut o = json!({
            "name": name, "type": ty, "is_factor": is_factor,
            "n_missing": n_missing, "n_unique": uniq.len(), "col_fp": fp
        });
        if nlev >= 0 {
            o["n_levels"] = json!(nlev);
        }
        if let (Some(mn), Some(mx)) = (min, max) {
            if is_int {
                o["col_min"] = json!(mn as i64);
                o["col_max"] = json!(mx as i64);
            } else {
                o["col_min"] = json!(mn);
                o["col_max"] = json!(mx);
            }
        }
        col_json.push(o);
        col_fp_bytes.extend_from_slice(fp.as_bytes());
        col_fp_bytes.push(b'|');
        cell_hashes.push(ch);
    }

    let schema_fp = hex128(&blake3::hash(schema_src.as_bytes()));
    let shape_fp = hex128(&blake3::hash(shape_src.as_bytes()));
    let content_fp = hex128(&blake3::hash(&col_fp_bytes));

    // Bottom-k sketch of per-row hashes for row-level similarity.
    let mut row_hashes: Vec<u64> = Vec::with_capacity(n);
    for i in 0..n {
        let mut h = FNV_OFFSET;
        for col in &cell_hashes {
            h ^= col[i];
            h = h.wrapping_mul(FNV_PRIME);
        }
        row_hashes.push(h);
    }
    row_hashes.sort_unstable();
    row_hashes.dedup();
    row_hashes.truncate(SKETCH_K);
    let row_sketch = row_hashes.iter().map(|h| format!("{h:016x}")).collect();

    Some(Profile {
        columns: col_json,
        n_missing_total,
        schema_fp,
        shape_fp,
        content_fp,
        row_sketch,
        capped: false,
    })
}

fn s4_class(pairs: &[(String, &Node)]) -> (String, String) {
    let cls = attr(pairs, "class");
    let name = cls
        .and_then(|n| {
            if let Val::Str(v) = &n.val {
                v.first().and_then(|x| x.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "?".to_string());
    let pkg = cls
        .map(attr_pairs)
        .and_then(|p| {
            attr(&p, "package").and_then(|n| {
                if let Val::Str(v) = &n.val {
                    v.first().and_then(|x| x.clone())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();
    (name, pkg)
}

// ---- record building --------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn describe(
    name: &str,
    file: &str,
    node: &Node,
    fmt: &str,
    ver: i32,
    comp: &str,
    internal: bool,
    size: u64,
) -> Value {
    let pairs = attr_pairs(node);
    let class = attr(&pairs, "class").map(str_vec).unwrap_or_default();

    if let Val::S4 = node.val {
        let (klass, pkg) = s4_class(&pairs);
        return json!({
            "rec": "dataset", "name": name, "file": file, "format": fmt,
            "format_version": ver, "compression": comp, "compressed_bytes": size,
            "internal": internal, "class": format!("S4:{klass}"), "s4_package": pkg,
            "confidence": "degraded", "notes": "s4-class-only"
        });
    }

    if class.iter().any(|c| c == "data.frame") {
        let names = attr(&pairs, "names").map(str_vec).unwrap_or_default();
        let nrow = nrow_of(attr(&pairs, "row.names"));
        let has_rownames = match attr(&pairs, "row.names") {
            Some(n) => !matches!(&n.val, Val::Ints { len, vals, .. }
                if *len == 2 && vals.first() == Some(&NA_INT)),
            None => false,
        };
        let els: Vec<&Node> = if let Val::Vec(e) = &node.val {
            e.iter().collect()
        } else {
            Vec::new()
        };
        let ncol = els.len();
        let mut rec = json!({
            "rec": "dataset", "name": name, "file": file, "format": fmt,
            "format_version": ver, "compression": comp, "compressed_bytes": size,
            "internal": internal, "class": class.join("/"), "kind": "data.frame",
            "nrow": nrow, "ncol": ncol, "has_rownames": has_rownames,
            "confidence": "exact"
        });
        match profile_columns(&els, &names) {
            Some(p) => {
                rec["columns"] = json!(p.columns);
                rec["n_missing_total"] = json!(p.n_missing_total);
                rec["schema_fp"] = json!(p.schema_fp);
                rec["shape_fp"] = json!(p.shape_fp);
                rec["content_fp"] = json!(p.content_fp);
                rec["row_sketch"] = json!(p.row_sketch);
                if p.capped {
                    rec["confidence"] = json!("degraded");
                }
            }
            None => {
                // Values not materialized (over the cap): structural columns only.
                let cols: Vec<Value> = els
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let (t, isf, nlev) = base_type(c);
                        let mut o = json!({"name": names.get(i).cloned().unwrap_or_default(), "type": t, "is_factor": isf});
                        if nlev >= 0 {
                            o["n_levels"] = json!(nlev);
                        }
                        o
                    })
                    .collect();
                rec["columns"] = json!(cols);
                rec["confidence"] = json!("degraded");
                rec["notes"] = json!("value scan skipped (size cap)");
            }
        }
        return rec;
    }

    if let Some(dim) = attr(&pairs, "dim") {
        if let Val::Ints { vals, .. } = &dim.val {
            let (nr, nc) = if vals.len() >= 2 {
                (Some(vals[0] as i64), Some(vals[1] as i64))
            } else {
                (None, None)
            };
            let cls = if class.is_empty() {
                "matrix".to_string()
            } else {
                class.join("/")
            };
            return json!({
                "rec": "dataset", "name": name, "file": file, "format": fmt,
                "format_version": ver, "compression": comp, "compressed_bytes": size,
                "internal": internal, "class": cls, "kind": "matrix",
                "nrow": nr, "ncol": nc, "confidence": "exact"
            });
        }
    }

    // Standalone atomic vector or list: fingerprint it as a single column.
    let (kind, len) = match &node.val {
        Val::Ints { len, .. } => ("vector", *len),
        Val::Reals { len, .. } => ("vector", *len),
        Val::Str(v) => ("vector", v.len()),
        Val::Vec(v) => ("list", v.len()),
        _ => ("object", 0),
    };
    let cls = if class.is_empty() {
        kind.to_string()
    } else {
        class.join("/")
    };
    let mut rec = json!({
        "rec": "dataset", "name": name, "file": file, "format": fmt,
        "format_version": ver, "compression": comp, "compressed_bytes": size,
        "internal": internal, "class": cls, "kind": kind, "length": len,
        "confidence": "exact"
    });
    if kind == "vector" {
        if let Some(p) = profile_columns(&[node], std::slice::from_ref(&name.to_string())) {
            rec["n_missing_total"] = json!(p.n_missing_total);
            rec["n_unique"] = json!(p.columns.first().and_then(|c| c.get("n_unique")).cloned().unwrap_or(json!(null)));
            rec["content_fp"] = json!(p.content_fp);
        }
    }
    rec
}

fn decompress(raw: &[u8]) -> Result<(Vec<u8>, &'static str), String> {
    use std::io::Read;
    if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        let mut d = flate2::read::GzDecoder::new(raw);
        let mut out = Vec::new();
        d.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok((out, "gzip"))
    } else if raw.len() >= 3 && &raw[0..3] == b"BZh" {
        let mut d = bzip2::read::BzDecoder::new(raw);
        let mut out = Vec::new();
        d.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok((out, "bzip2"))
    } else if raw.len() >= 6 && raw[0..6] == [0xfd, b'7', b'z', b'X', b'Z', 0x00] {
        let mut d = xz2::read::XzDecoder::new(raw);
        let mut out = Vec::new();
        d.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok((out, "xz"))
    } else {
        Ok((raw.to_vec(), "none"))
    }
}

fn read_file(path: &Path) -> Result<Vec<(String, Node, String, i32, String)>, String> {
    let raw = std::fs::read(path).map_err(|e| e.to_string())?;
    let (bytes, comp) = decompress(&raw)?;
    let is_rda = bytes.len() >= 5
        && (&bytes[0..3] == b"RDX" || &bytes[0..3] == b"RDA" || &bytes[0..3] == b"RDB");
    let mut p = 0usize;
    if is_rda {
        p = bytes
            .iter()
            .position(|&c| c == b'\n')
            .ok_or("missing container magic newline")?
            + 1;
    }
    if p + 2 > bytes.len() {
        return Err("truncated header".into());
    }
    let sel = bytes[p];
    if sel != b'X' {
        return Err(format!("non-XDR encoding '{}'", sel as char));
    }
    p += 2; // 'X' '\n'
    let mut r = Reader { b: &bytes, p, ver: 0, refs: Vec::new(), budget: 5_000_000 };
    let ver = r.i32()?;
    r.ver = ver;
    let _writer = r.i32()?;
    let _min = r.i32()?;
    if ver >= 3 {
        let enclen = r.i32()?;
        if enclen > 0 {
            r.skip(enclen as usize)?;
        }
    }
    let fmt = if is_rda { "rda" } else { "rds" }.to_string();
    let top = r.item()?;

    let mut out = Vec::new();
    if let Val::List { .. } = top.val {
        let mut cur = Some(&top);
        while let Some(nd) = cur {
            if let Val::List { tag, car, cdr } = &nd.val {
                let nm = if let Val::Sym(s) = &tag.val {
                    s.clone()
                } else {
                    String::new()
                };
                out.push((nm, car.as_ref().clone(), fmt.clone(), ver, comp.to_string()));
                cur = Some(cdr.as_ref());
            } else {
                break;
            }
        }
    } else {
        out.push((String::new(), top, fmt, ver, comp.to_string()));
    }
    Ok(out)
}

fn stem(fname: &str) -> String {
    fname.rsplit_once('.').map(|(s, _)| s).unwrap_or(fname).to_string()
}

/// Emit one `dataset` record per dataset shipped under `root`'s data/ directory
/// and R/sysdata.rda. Never panics on a bad file; it degrades with a note.
pub fn scan_package(root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let mut targets: Vec<(std::path::PathBuf, bool)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root.join("data")) {
        let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            targets.push((p, false));
        }
    }
    let sys = root.join("R").join("sysdata.rda");
    if sys.exists() {
        targets.push((sys, true));
    }

    for (path, internal) in targets {
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let rel = format!("{}/{}", if internal { "R" } else { "data" }, fname);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match ext.as_str() {
            "rda" | "rdata" | "rds" => match read_file(&path) {
                Ok(recs) => {
                    for (nm, node, fmt, ver, comp) in recs {
                        let nm = if nm.is_empty() { stem(fname) } else { nm };
                        out.push(describe(&nm, &rel, &node, &fmt, ver, &comp, internal, size));
                    }
                }
                Err(e) => out.push(json!({
                    "rec": "dataset", "name": stem(fname), "file": rel,
                    "internal": internal, "compressed_bytes": size,
                    "confidence": "degraded", "notes": e
                })),
            },
            "r" => out.push(json!({
                "rec": "dataset", "name": stem(fname), "file": rel, "format": "script",
                "internal": internal, "compressed_bytes": size,
                "confidence": "needs_r", "notes": "R script data (requires R)"
            })),
            "csv" | "txt" | "tab" | "tsv" => out.push(json!({
                "rec": "dataset", "name": stem(fname), "file": rel, "format": ext,
                "internal": internal, "compressed_bytes": size,
                "confidence": "degraded", "notes": "text data"
            })),
            _ => {}
        }
    }
    out
}
