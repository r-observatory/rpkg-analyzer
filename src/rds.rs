// Metadata-only reader for R's serialization format (.rda / .RData / .rds) under
// a package's data/ directory and R/sysdata.rda. A pure function of the bytes:
// no R, no build, no network. Phase 1 covers gzip framing, XDR serialization
// versions 2 and 3, and structural metadata for data.frame / matrix / atomic /
// factor objects, plus graceful class-only degradation for S4. The value scan
// (n_missing, ranges), fingerprints, bzip2/xz, ALTREP, and per-class S4 dim
// extractors are later phases; this module is wired behind the --datasets flag.

use serde_json::{json, Value};
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

#[derive(Clone)]
#[allow(dead_code)] // Blob.len is retained for a later value-scan phase
enum Val {
    Nil,
    Sym(String),
    Char(Option<String>),
    Str(Vec<Option<String>>),
    Ints { len: usize, small: Vec<i32>, logical: bool },
    Reals { len: usize },
    Blob { len: usize }, // complex / raw: payload skipped
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
                let small = if n <= 2 {
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
                    val: Val::Ints { len: n, small, logical: t == LGLSXP },
                    attr,
                })
            }
            REALSXP => {
                let n = self.vlen()?;
                self.skip(8 * n)?;
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Reals { len: n }, attr })
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
                // Read slots+class as attributes; degrade rather than desync if a
                // slot uses a type we do not model.
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
    fn maybe_attr(&mut self, ha: bool) -> Result<Option<Box<Node>>, String> {
        if ha {
            Ok(Some(Box::new(self.item()?)))
        } else {
            Ok(None)
        }
    }
}

// Walk an attribute pairlist into (name, value) pairs.
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

fn nrow_of(rn: Option<&Node>) -> Option<i64> {
    match rn?.val {
        Val::Ints { len, ref small, .. } => {
            if len == 2 && small.len() == 2 && small[0] == NA_INT {
                Some((small[1] as i64).abs())
            } else {
                Some(len as i64)
            }
        }
        Val::Str(ref v) => Some(v.len() as i64),
        _ => None,
    }
}

// (type, is_factor, n_levels or -1)
fn col_type(col: &Node) -> (&'static str, bool, i64) {
    let pairs = attr_pairs(col);
    let class: Vec<String> = attr(&pairs, "class").map(str_vec).unwrap_or_default();
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
            Some(n) => !matches!(&n.val, Val::Ints { len, small, .. }
                if *len == 2 && small.first() == Some(&NA_INT)),
            None => false,
        };
        let (cols, ncol): (Vec<Value>, usize) = if let Val::Vec(els) = &node.val {
            let c = els
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let (t, is_factor, nlev) = col_type(col);
                    let cn = names.get(i).cloned().unwrap_or_default();
                    let mut o = json!({"name": cn, "type": t, "is_factor": is_factor});
                    if nlev >= 0 {
                        o["n_levels"] = json!(nlev);
                    }
                    o
                })
                .collect();
            (c, els.len())
        } else {
            (Vec::new(), 0)
        };
        return json!({
            "rec": "dataset", "name": name, "file": file, "format": fmt,
            "format_version": ver, "compression": comp, "compressed_bytes": size,
            "internal": internal, "class": class.join("/"), "kind": "data.frame",
            "nrow": nrow, "ncol": ncol, "columns": cols, "has_rownames": has_rownames,
            "confidence": "exact"
        });
    }

    if let Some(dim) = attr(&pairs, "dim") {
        if let Val::Ints { small, .. } = &dim.val {
            let (nr, nc) = if small.len() >= 2 {
                (Some(small[0] as i64), Some(small[1] as i64))
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

    let (kind, len) = match &node.val {
        Val::Ints { len, .. } => ("vector", *len),
        Val::Reals { len } => ("vector", *len),
        Val::Str(v) => ("vector", v.len()),
        Val::Vec(v) => ("list", v.len()),
        _ => ("object", 0),
    };
    let cls = if class.is_empty() {
        kind.to_string()
    } else {
        class.join("/")
    };
    json!({
        "rec": "dataset", "name": name, "file": file, "format": fmt,
        "format_version": ver, "compression": comp, "compressed_bytes": size,
        "internal": internal, "class": cls, "kind": kind, "length": len,
        "confidence": "exact"
    })
}

fn decompress(raw: &[u8]) -> Result<(Vec<u8>, &'static str), String> {
    if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        use std::io::Read;
        let mut d = flate2::read::GzDecoder::new(raw);
        let mut out = Vec::new();
        d.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok((out, "gzip"))
    } else if raw.len() >= 3 && &raw[0..3] == b"BZh" {
        use std::io::Read;
        let mut d = bzip2::read::BzDecoder::new(raw);
        let mut out = Vec::new();
        d.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok((out, "bzip2"))
    } else if raw.len() >= 6 && raw[0..6] == [0xfd, b'7', b'z', b'X', b'Z', 0x00] {
        use std::io::Read;
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
/// and R/sysdata.rda. Never panics on a bad file: it degrades to a record with a
/// note instead.
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
                "confidence": "degraded", "notes": "text data (phase 3)"
            })),
            _ => {}
        }
    }
    out
}
