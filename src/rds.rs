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
const CLOSXP: u8 = 3;
const ENVSXP: u8 = 4;
const LANGSXP: u8 = 6;
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
const ALTREP_SXP: u8 = 238;
// Singleton markers: a type byte and nothing else. R writes these where a slot
// holds one of its well-known constants.
const GLOBALENV_SXP: u8 = 253;
const UNBOUNDVALUE_SXP: u8 = 252;
const MISSINGARG_SXP: u8 = 251;
const BASENAMESPACE_SXP: u8 = 250;
const EMPTYENV_SXP: u8 = 242;
const BASEENV_SXP: u8 = 241;
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
            // No payload follows any of these, so consuming the type byte is
            // the whole job. Erroring on them lost the file for the sake of a
            // marker that carries no data.
            0 | NILVALUE | GLOBALENV_SXP | UNBOUNDVALUE_SXP | MISSINGARG_SXP
            | BASENAMESPACE_SXP | EMPTYENV_SXP | BASEENV_SXP => {
                Ok(Node { val: Val::Nil, attr: None })
            }
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
            LISTSXP | LANGSXP => {
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
            // ALTREP: a compact or deferred representation standing in for an
            // ordinary vector. Modern R writes these by default at serialization
            // version 3, so a reader that rejects them loses the whole file, not
            // just the one column: `data.frame(id = 1:5)` round-trips as nothing
            // at all. The payload is always three items, so even a class this
            // does not know how to expand can be stepped over without
            // desynchronizing the stream.
            // An environment or a closure inside a dataset: a model fit or a
            // saved workspace object. Neither is data worth profiling, but both
            // must be consumed exactly, because an environment occupies a slot
            // in the reference table and a closure carries three payload items.
            // Skipping either by erroring loses every object in the file.
            ENVSXP => {
                // Reserve the reference slot before descending, so any REFSXP
                // written after this one resolves to the right index.
                let slot = self.refs.len();
                self.refs.push(Node { val: Val::S4, attr: None });
                let _locked = self.i32()?;
                let _enclos = self.item()?;
                let _frame = self.item()?;
                let _hashtab = self.item()?;
                let attr_node = self.item()?;
                let attr = match attr_node.val {
                    Val::Nil => None,
                    _ => Some(Box::new(attr_node)),
                };
                let node = Node { val: Val::S4, attr };
                self.refs[slot] = node.clone();
                Ok(node)
            }
            CLOSXP => {
                let attr = self.maybe_attr(ha)?;
                if hg {
                    let _tag = self.item()?;
                }
                let _env = self.item()?;
                let _formals = self.item()?;
                let _body = self.item()?;
                Ok(Node { val: Val::S4, attr })
            }
            ALTREP_SXP => {
                let info = self.item()?;
                let state = self.item()?;
                let attr_node = self.item()?;
                let attr = match attr_node.val {
                    Val::Nil => None,
                    _ => Some(Box::new(attr_node)),
                };
                let cls = altrep_class(&info).unwrap_or_default();
                let val = match cls.as_str() {
                    // state is c(length, start, step); expand it back to the
                    // vector R would have materialized on access.
                    "compact_intseq" => expand_seq(&state, true),
                    "compact_realseq" => expand_seq(&state, false),
                    // A wrapper carries the real vector as the first element of
                    // its state and adds only metadata, so unwrap to it.
                    _ if cls.starts_with("wrap_") => first_element(&state).map(|n| n.val.clone()),
                    // Anything else: the state is usually the materialized data
                    // (deferred_string, and the expanded form of any compact
                    // class), so prefer it over failing the file.
                    _ => Some(state.val.clone()),
                };
                match val {
                    Some(v) => Ok(Node { val: v, attr }),
                    None => Ok(Node { val: state.val, attr }),
                }
            }
            other => Err(format!("unhandled SEXPTYPE {other}")),
        }
    }
}

/// Coordinate-reference and extent fields for a geometry column.
///
/// An sf object is an ordinary data frame whose geometry column carries the
/// spatial metadata as its own attributes, so nothing here needs a new decode:
/// the per-column loop already builds this column's attribute list to find
/// factor levels. Without these fields a spatial dataset is indistinguishable
/// from any other table, which is why nothing on the site could say where in the
/// world a dataset describes.
fn spatial_fields(col: &Node, o: &mut Value) {
    let pairs = attr_pairs(col);
    let class = attr(&pairs, "class").map(str_vec).unwrap_or_default();
    if !class.iter().any(|c| c.starts_with("sfc")) {
        return;
    }
    // sfc_POINT/sfc -> POINT
    if let Some(g) = class.iter().find(|c| c.starts_with("sfc_")) {
        o["geom_type"] = json!(g.trim_start_matches("sfc_"));
    }
    o["is_geometry"] = json!(true);
    if let Val::Vec(items) = &col.val {
        o["n_geometries"] = json!(items.len());
    }
    if let Some(crs) = attr(&pairs, "crs") {
        // crs is a two-element list of (input, wkt), tagged by `names`.
        let cp = attr_pairs(crs);
        let names = attr(&cp, "names").map(str_vec).unwrap_or_default();
        if let Val::Vec(items) = &crs.val {
            for (i, nm) in names.iter().enumerate() {
                let txt = items.get(i).map(str_vec).unwrap_or_default();
                let Some(first) = txt.first() else { continue };
                match nm.as_str() {
                    "input" => {
                        o["crs_input"] = json!(first);
                        if let Some(code) = first.strip_prefix("EPSG:") {
                            if let Ok(n) = code.parse::<i64>() {
                                o["crs_epsg"] = json!(n);
                            }
                        }
                    }
                    // A WKT2 string runs to a few KB and the value is in having
                    // it at all, not in storing every axis definition.
                    "wkt" => o["crs_wkt"] = json!(first.chars().take(512).collect::<String>()),
                    _ => {}
                }
            }
        }
    }
    if let Some(Val::Reals { vals, .. }) = attr(&pairs, "bbox").map(|n| &n.val) {
        if vals.len() >= 4 {
            o["bbox"] = json!(vals[..4]);
        }
    }
}

/// Time-index fields for a series object, from attributes the reader already
/// decodes and has never looked at.
///
/// `tsp` is c(start, end, frequency) on ts and mts. zoo and xts instead carry an
/// explicit `index` vector, whose first and last values give the same span, and
/// whose own class says whether those numbers are days or seconds.
fn series_fields(pairs: &[(String, &Node)], rec: &mut Value) {
    if let Some(Val::Reals { vals, .. }) = attr(pairs, "tsp").map(|n| &n.val) {
        if vals.len() >= 3 {
            rec["ts_start"] = json!(vals[0]);
            rec["ts_end"] = json!(vals[1]);
            rec["ts_frequency"] = json!(vals[2]);
        }
    }
    if let Some(idx) = attr(pairs, "index") {
        let nums: Option<(f64, f64, usize)> = match &idx.val {
            Val::Reals { vals, .. } if !vals.is_empty() => {
                Some((vals[0], vals[vals.len() - 1], vals.len()))
            }
            Val::Ints { vals, .. } if !vals.is_empty() => {
                Some((vals[0] as f64, vals[vals.len() - 1] as f64, vals.len()))
            }
            _ => None,
        };
        if let Some((lo, hi, n)) = nums {
            rec["index_start"] = json!(lo);
            rec["index_end"] = json!(hi);
            rec["index_n"] = json!(n);
            // The index's own class is what makes those numbers meaningful:
            // days since epoch for Date, seconds for POSIXct.
            let icls = attr_pairs(idx);
            let c = attr(&icls, "class").map(str_vec).unwrap_or_default();
            if !c.is_empty() {
                rec["index_class"] = json!(c.join("/"));
            }
        }
    }
    if let Some(Val::Reals { vals, .. }) = attr(pairs, "frequency").map(|n| &n.val) {
        if !vals.is_empty() {
            rec["ts_frequency"] = json!(vals[0]);
        }
    }
}

/// Split a column-major flat vector into per-column nodes.
///
/// Returns None past the value cap or when the payload is not a materialized
/// atomic vector, so the caller keeps structure and drops the fingerprint rather
/// than reporting one it could not compute.
fn slice_matrix(node: &Node, nrow: usize, ncol: usize) -> Option<Vec<Node>> {
    if nrow == 0 || ncol == 0 || nrow.checked_mul(ncol)? > CELL_CAP {
        return None;
    }
    let take = |lo: usize, hi: usize| -> Option<Val> {
        match &node.val {
            Val::Ints { vals, logical, .. } if vals.len() >= hi => Some(Val::Ints {
                len: hi - lo,
                vals: vals[lo..hi].to_vec(),
                logical: *logical,
            }),
            Val::Reals { vals, .. } if vals.len() >= hi => {
                Some(Val::Reals { len: hi - lo, vals: vals[lo..hi].to_vec() })
            }
            Val::Str(v) if v.len() >= hi => Some(Val::Str(v[lo..hi].to_vec())),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(ncol);
    for j in 0..ncol {
        let val = take(j * nrow, (j + 1) * nrow)?;
        out.push(Node { val, attr: None });
    }
    Some(out)
}

// ---- ALTREP helpers ---------------------------------------------------------

/// The serialized ALTREP class is a pairlist whose first element is the class
/// name symbol. Only the name is needed to decide how to expand the state.
fn altrep_class(info: &Node) -> Option<String> {
    match &info.val {
        Val::Sym(s) => Some(s.clone()),
        Val::List { car, .. } => match &car.val {
            Val::Sym(s) => Some(s.clone()),
            _ => None,
        },
        Val::Vec(items) => items.first().and_then(|n| match &n.val {
            Val::Sym(s) => Some(s.clone()),
            _ => None,
        }),
        _ => None,
    }
}

/// First element of a state container, for the wrap_* classes whose payload is
/// the wrapped vector followed by metadata.
fn first_element(state: &Node) -> Option<&Node> {
    match &state.val {
        Val::Vec(items) => items.first(),
        Val::List { car, .. } => Some(car.as_ref()),
        _ => None,
    }
}

/// Expand a compact sequence state of c(length, start, step).
///
/// Materializing keeps every downstream consumer (value pass, fingerprint, per
/// column profile) working on an ordinary vector with no special cases. Past
/// CELL_CAP the length is kept and the values dropped, which is how the reader
/// already treats complex and raw.
fn expand_seq(state: &Node, want_int: bool) -> Option<Val> {
    let p = match &state.val {
        Val::Reals { vals, .. } if vals.len() >= 3 => vals.clone(),
        Val::Ints { vals, .. } if vals.len() >= 3 => vals.iter().map(|&v| v as f64).collect(),
        _ => return None,
    };
    let n = if p[0].is_finite() && p[0] >= 0.0 { p[0] as usize } else { return None };
    let (start, step) = (p[1], p[2]);
    if n > CELL_CAP {
        return Some(Val::Blob { len: n });
    }
    if want_int {
        let vals = (0..n)
            .map(|i| {
                let v = start + step * i as f64;
                if v.is_finite() && v.abs() < i32::MAX as f64 { v as i32 } else { NA_INT }
            })
            .collect();
        Some(Val::Ints { len: n, vals, logical: false })
    } else {
        Some(Val::Reals { len: n, vals: (0..n).map(|i| start + step * i as f64).collect() })
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
        Val::Vec(items) => match items.get(i) {
            Some(el) => {
                buf.push(1);
                buf.extend_from_slice(&digest_node(el, 0).to_be_bytes());
                false
            }
            None => {
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
        // A list column is a column. Reporting zero here made the whole frame
        // abort, which is why an sf object, whose geometry is a list column,
        // produced no fingerprint and was dropped downstream entirely.
        Val::Vec(v) => v.len(),
        _ => 0,
    }
}

/// Order-sensitive digest of a node's contents, for hashing a list cell.
///
/// A list element is an arbitrary tree (an sf geometry is a list of coordinate
/// matrices), so the cell is represented by a digest of that tree rather than by
/// its bytes. Two geometrically identical features digest the same; two
/// different ones do not, which is all the fingerprint needs.
fn digest_node(n: &Node, depth: u32) -> u64 {
    if depth > 12 {
        return FNV_OFFSET;
    }
    let mut h = match &n.val {
        Val::Nil => fnv(b"nil"),
        Val::Sym(s) => fnv(s.as_bytes()),
        Val::Char(Some(s)) => fnv(s.as_bytes()),
        Val::Char(None) => fnv(b"na"),
        Val::Str(v) => {
            let mut acc = fnv(b"str");
            for x in v {
                acc ^= match x {
                    Some(s) => fnv(s.as_bytes()),
                    None => fnv(b"na"),
                };
                acc = acc.wrapping_mul(FNV_PRIME);
            }
            acc
        }
        Val::Ints { vals, .. } => {
            let mut acc = fnv(b"int");
            for v in vals {
                acc ^= fnv(&v.to_be_bytes());
                acc = acc.wrapping_mul(FNV_PRIME);
            }
            acc
        }
        Val::Reals { vals, .. } => {
            let mut acc = fnv(b"real");
            for v in vals {
                acc ^= fnv(&v.to_be_bytes());
                acc = acc.wrapping_mul(FNV_PRIME);
            }
            acc
        }
        Val::Blob { len } => fnv(&(*len as u64).to_be_bytes()),
        Val::Vec(items) => {
            let mut acc = fnv(b"vec");
            for it in items {
                acc ^= digest_node(it, depth + 1);
                acc = acc.wrapping_mul(FNV_PRIME);
            }
            acc
        }
        Val::List { car, cdr, .. } => {
            digest_node(car, depth + 1) ^ digest_node(cdr, depth + 1).wrapping_mul(FNV_PRIME)
        }
        Val::S4 => fnv(b"s4"),
    };
    // Fold in the attribute list so two structurally equal values with different
    // classes are not treated as the same cell.
    for (k, v) in attr_pairs(n) {
        if k == "class" {
            h ^= fnv(k.as_bytes()) ^ digest_node(v, depth + 1);
        }
    }
    h
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
        spatial_fields(col, &mut o);
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

// ---- S4 dimension extractors ------------------------------------------------

fn int1(n: &Node) -> Option<i64> {
    if let Val::Ints { vals, .. } = &n.val {
        vals.first().map(|v| *v as i64)
    } else {
        None
    }
}

// Search an S4 slot tree for the first matrix (a node with a length>=2 `dim`
// attribute) and return (nrow, ncol). Recovers the assay dimensions of a
// SummarizedExperiment-family object without hardcoding the exact slot chain,
// which varies across Bioconductor versions.
fn find_matrix_dim(node: &Node, depth: u32) -> Option<(i64, i64)> {
    if depth == 0 {
        return None;
    }
    let pairs = attr_pairs(node);
    if let Some(dim) = attr(&pairs, "dim") {
        if let Val::Ints { vals, .. } = &dim.val {
            if vals.len() >= 2 {
                return Some((vals[0] as i64, vals[1] as i64));
            }
        }
    }
    for (_, v) in &pairs {
        if let Some(d) = find_matrix_dim(v, depth - 1) {
            return Some(d);
        }
    }
    match &node.val {
        Val::Vec(els) => {
            for e in els {
                if let Some(d) = find_matrix_dim(e, depth - 1) {
                    return Some(d);
                }
            }
        }
        Val::List { car, cdr, .. } => {
            if let Some(d) = find_matrix_dim(car, depth - 1) {
                return Some(d);
            }
            if let Some(d) = find_matrix_dim(cdr, depth - 1) {
                return Some(d);
            }
        }
        _ => {}
    }
    None
}

// (nrow, ncol) of an S4 DataFrame/DFrame from its nrows slot and listData length.
fn dataframe_dims(pairs: &[(String, &Node)]) -> Option<(i64, i64)> {
    let nrow = attr(pairs, "nrows").and_then(int1)?;
    let ncol = attr(pairs, "listData")
        .map(|n| if let Val::Vec(v) = &n.val { v.len() as i64 } else { 0 })
        .unwrap_or(0);
    Some((nrow, ncol))
}

// Element count via an elementMetadata DataFrame's nrows (GRanges and friends).
fn element_meta_len(pairs: &[(String, &Node)]) -> Option<i64> {
    let em = attr(pairs, "elementMetadata")?;
    attr(&attr_pairs(em), "nrows").and_then(int1)
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
        let mut rec = json!({
            "rec": "dataset", "name": name, "file": file, "format": fmt,
            "format_version": ver, "compression": comp, "compressed_bytes": size,
            "internal": internal, "class": format!("S4:{klass}"), "s4_package": pkg,
            "confidence": "degraded"
        });
        // DataFrame-like S4 (S4Vectors): dims from the nrows/listData slots.
        if (klass == "DFrame" || klass == "DataFrame") && dataframe_dims(&pairs).is_some() {
            let (r, c) = dataframe_dims(&pairs).unwrap();
            rec["kind"] = json!("DataFrame");
            rec["nrow"] = json!(r);
            rec["ncol"] = json!(c);
            rec["notes"] = json!("s4-dataframe");
        } else if let Some((r, c)) = find_matrix_dim(&node, 16) {
            // SummarizedExperiment family: assay matrix dims (features x samples).
            rec["kind"] = json!(klass.clone());
            rec["nrow"] = json!(r);
            rec["ncol"] = json!(c);
            rec["notes"] = json!("s4-assay-dims");
        } else if let Some(l) = element_meta_len(&pairs) {
            // GRanges and friends: length from elementMetadata.
            rec["kind"] = json!(klass.clone());
            rec["length"] = json!(l);
            rec["notes"] = json!("s4-elementmetadata-length");
        } else {
            rec["notes"] = json!("s4-class-only");
        }
        return rec;
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
            let extent: Vec<i64> = vals.iter().map(|&v| v as i64).collect();
            // Only a two-dimensional array is a matrix. Reporting the first two
            // extents of a 2x3x4 array as nrow and ncol did not describe it, it
            // described a different object.
            let n_dim = extent.len();
            let (nr, nc) = if n_dim == 2 {
                (Some(extent[0]), Some(extent[1]))
            } else {
                (None, None)
            };
            let total: i64 = extent.iter().copied().filter(|d| *d >= 0).product();
            let kind = if n_dim == 2 { "matrix" } else { "array" };
            let cls = if class.is_empty() {
                kind.to_string()
            } else {
                class.join("/")
            };
            let mut rec = json!({
                "rec": "dataset", "name": name, "file": file, "format": fmt,
                "format_version": ver, "compression": comp, "compressed_bytes": size,
                "internal": internal, "class": cls, "kind": kind,
                "nrow": nr, "ncol": nc, "dim": extent, "n_dim": n_dim,
                "length": total, "confidence": "exact"
            });
            if let Some(dn) = attr(&pairs, "dimnames") {
                rec["has_dimnames"] = json!(!matches!(dn.val, Val::Nil));
            }
            series_fields(&pairs, &mut rec);
            // A matrix is a table of values like any other, so it gets a real
            // fingerprint rather than being dropped for want of one. Column-major
            // storage means column j is a contiguous run, so the flat vector
            // slices into columns without copying the values twice.
            // Two dimensions slice into columns; anything else is fingerprinted
            // as the flat vector it is, so a 3-D array still dedups against an
            // identical copy instead of being dropped for want of a fingerprint.
            let sliced = if n_dim == 2 {
                let (r, c) = (extent[0].max(0) as usize, extent[1].max(0) as usize);
                slice_matrix(node, r, c)
            } else {
                None
            };
            {
                let flat = [node];
                let (refs, names): (Vec<&Node>, Vec<String>) = match &sliced {
                    Some(cols) => (
                        cols.iter().collect(),
                        (0..cols.len()).map(|j| format!("V{}", j + 1)).collect(),
                    ),
                    None => (flat.to_vec(), vec![name.to_string()]),
                };
                {
                    if let Some(p) = profile_columns(&refs, &names) {
                        rec["n_missing_total"] = json!(p.n_missing_total);
                        rec["schema_fp"] = json!(p.schema_fp);
                        rec["shape_fp"] = json!(p.shape_fp);
                        rec["content_fp"] = json!(p.content_fp);
                        rec["row_sketch"] = json!(p.row_sketch);
                    }
                }
            }
            return rec;
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
    series_fields(&pairs, &mut rec);
    if kind == "vector" || kind == "list" {
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

// Dataset name from a file name, stripping a compression suffix then the type
// extension: "gapminder.tab.gz" -> "gapminder", "mtcars.rds" -> "mtcars".
fn dataset_name(fname: &str) -> String {
    let mut s = fname.to_string();
    for suf in [".gz", ".bz2", ".xz"] {
        if let Some(t) = s.strip_suffix(suf) {
            s = t.to_string();
        }
    }
    s.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or(s)
}

fn is_na_text(s: &str) -> bool {
    s.is_empty() || s.eq_ignore_ascii_case("NA") || s == "N/A" || s == "."
}

// Infer a column type from delimited-text cells, building the same Node the
// binary path produces so the value profile and fingerprints are shared (and a
// text dataset can match a serialized one with the same data).
fn infer_column(cells: &[String]) -> Node {
    let non_na: Vec<&str> = cells.iter().map(|s| s.as_str()).filter(|c| !is_na_text(c)).collect();
    let all_int = !non_na.is_empty()
        && non_na.iter().all(|c| !c.contains('.') && c.parse::<i32>().is_ok());
    let all_logical = !non_na.is_empty()
        && non_na
            .iter()
            .all(|c| matches!(*c, "TRUE" | "FALSE" | "T" | "F" | "true" | "false"));
    let all_num = !non_na.is_empty() && non_na.iter().all(|c| c.parse::<f64>().is_ok());
    if all_int {
        let vals = cells
            .iter()
            .map(|c| if is_na_text(c) { NA_INT } else { c.parse().unwrap_or(NA_INT) })
            .collect::<Vec<_>>();
        Node { val: Val::Ints { len: vals.len(), vals, logical: false }, attr: None }
    } else if all_logical {
        let vals = cells
            .iter()
            .map(|c| {
                if is_na_text(c) {
                    NA_INT
                } else if c.eq_ignore_ascii_case("true") || *c == "T" {
                    1
                } else {
                    0
                }
            })
            .collect::<Vec<_>>();
        Node { val: Val::Ints { len: vals.len(), vals, logical: true }, attr: None }
    } else if all_num {
        let vals = cells
            .iter()
            .map(|c| if is_na_text(c) { f64::NAN } else { c.parse().unwrap_or(f64::NAN) })
            .collect::<Vec<_>>();
        Node { val: Val::Reals { len: vals.len(), vals }, attr: None }
    } else {
        let vals = cells
            .iter()
            .map(|c| if is_na_text(c) { None } else { Some(c.clone()) })
            .collect::<Vec<_>>();
        Node { val: Val::Str(vals), attr: None }
    }
}

// Parse a delimited-text data file (optionally gzip'd). Returns columns, names,
// row count, format tag, and compression.
fn read_text(path: &Path, lower: &str) -> Option<(Vec<Node>, Vec<String>, usize, &'static str, &'static str)> {
    let raw = std::fs::read(path).ok()?;
    let (bytes, comp): (Vec<u8>, &'static str) = if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        use std::io::Read;
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut o = Vec::new();
        d.read_to_end(&mut o).ok()?;
        (o, "gzip")
    } else {
        (raw, "none")
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut it = text.lines().filter(|l| !l.trim().is_empty());
    let header = it.next()?;
    let (delim, fmt) = if lower.contains(".tsv") || lower.contains(".tab") {
        ('\t', "tab")
    } else if lower.contains(".csv") {
        (',', "csv")
    } else if header.contains('\t') && header.matches('\t').count() >= header.matches(',').count() {
        ('\t', "txt")
    } else if header.contains(',') {
        (',', "txt")
    } else if header.contains(';') {
        (';', "txt")
    } else {
        (',', "txt")
    };
    let split = |s: &str| -> Vec<String> {
        s.split(delim).map(|f| f.trim().trim_matches('"').to_string()).collect()
    };
    let names = split(header);
    let ncol = names.len();
    if ncol == 0 {
        return None;
    }
    let mut cells: Vec<Vec<String>> = vec![Vec::new(); ncol];
    let mut nrow = 0usize;
    for line in it {
        let f = split(line);
        for (j, cell) in cells.iter_mut().enumerate() {
            cell.push(f.get(j).cloned().unwrap_or_default());
        }
        nrow += 1;
    }
    let cols: Vec<Node> = (0..ncol).map(|j| infer_column(&cells[j])).collect();
    Some((cols, names, nrow, fmt, comp))
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
        let lower = fname.to_lowercase();
        let rel = format!("{}/{}", if internal { "R" } else { "data" }, fname);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let name = dataset_name(fname);

        let is_rbin = lower.ends_with(".rda") || lower.ends_with(".rdata") || lower.ends_with(".rds");
        let is_script = lower.ends_with(".r");
        let is_text = !is_rbin
            && [".csv", ".tab", ".txt", ".tsv"].iter().any(|e| lower.contains(e));

        if is_rbin {
            match read_file(&path) {
                Ok(recs) => {
                    for (nm, node, fmt, ver, comp) in recs {
                        let nm = if nm.is_empty() { name.clone() } else { nm };
                        out.push(describe(&nm, &rel, &node, &fmt, ver, &comp, internal, size));
                    }
                }
                Err(e) => out.push(json!({
                    "rec": "dataset", "name": name, "file": rel,
                    "internal": internal, "compressed_bytes": size,
                    "confidence": "degraded", "notes": e
                })),
            }
        } else if is_script {
            out.push(json!({
                "rec": "dataset", "name": name, "file": rel, "format": "script",
                "internal": internal, "compressed_bytes": size,
                "confidence": "needs_r", "notes": "R script data (requires R)"
            }));
        } else if is_text {
            match read_text(&path, &lower) {
                Some((cols, names, nrow, fmt, comp)) => {
                    let refs: Vec<&Node> = cols.iter().collect();
                    let mut rec = json!({
                        "rec": "dataset", "name": name, "file": rel, "format": fmt,
                        "compression": comp, "compressed_bytes": size, "internal": internal,
                        "class": "data.frame", "kind": "table", "nrow": nrow,
                        "ncol": names.len(), "confidence": "degraded",
                        "notes": "text: column types inferred"
                    });
                    if let Some(p) = profile_columns(&refs, &names) {
                        rec["columns"] = json!(p.columns);
                        rec["n_missing_total"] = json!(p.n_missing_total);
                        rec["schema_fp"] = json!(p.schema_fp);
                        rec["shape_fp"] = json!(p.shape_fp);
                        rec["content_fp"] = json!(p.content_fp);
                        rec["row_sketch"] = json!(p.row_sketch);
                    }
                    out.push(rec);
                }
                None => out.push(json!({
                    "rec": "dataset", "name": name, "file": rel,
                    "internal": internal, "compressed_bytes": size,
                    "confidence": "degraded", "notes": "unreadable text data"
                })),
            }
        }
    }
    out
}

// ---- tests ------------------------------------------------------------------
//
// The fixtures under tests/fixtures/pkg/data are generated by
// tests/fixtures/make.R and committed, so the suite needs no R to run. Each one
// exists for a case that used to be read wrongly or not at all.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn records() -> Vec<Value> {
        scan_package(Path::new("tests/fixtures/pkg"))
    }
    fn by_name(name: &str) -> Value {
        records()
            .into_iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("no dataset record named {name}"))
    }
    fn s(v: &Value, k: &str) -> String {
        v[k].as_str().unwrap_or_default().to_string()
    }

    /// Modern R writes 1:n as a compact sequence. A reader that rejects the type
    /// loses every object in the file, not just that column.
    #[test]
    fn altrep_frame_is_read_not_dropped() {
        let r = by_name("altrep_frame");
        assert_eq!(r["nrow"], 5, "the frame has five rows");
        assert_eq!(r["ncol"], 2, "and two columns");
        assert_eq!(s(&r, "confidence"), "exact");
        assert!(!s(&r, "content_fp").is_empty(), "and a fingerprint");
    }

    /// The whole point of expanding rather than skipping: the same data written
    /// either way has to dedup, or a compact copy hides from the shared-data
    /// discovery entirely.
    #[test]
    fn altrep_and_plain_agree_on_the_fingerprint() {
        let (a, b) = (by_name("altrep_frame"), by_name("plain_frame"));
        assert_eq!(s(&a, "content_fp"), s(&b, "content_fp"));
        assert_eq!(s(&a, "schema_fp"), s(&b, "schema_fp"));
    }

    #[test]
    fn compact_and_wrapped_vectors_keep_their_length() {
        assert_eq!(by_name("altrep_seq")["length"], 10);
        assert_eq!(by_name("altrep_wrap")["length"], 3);
    }

    /// A 2x3x4 array is not a 2x3 matrix. Reporting the first two extents as
    /// nrow and ncol described a different object.
    #[test]
    fn a_three_dimensional_array_is_not_a_matrix() {
        let r = by_name("arr_3d");
        assert_eq!(s(&r, "kind"), "array");
        assert_eq!(r["n_dim"], 3);
        assert_eq!(r["dim"], serde_json::json!([2, 3, 4]));
        assert!(r["nrow"].is_null(), "a 3-D array has no single row count");
        assert_eq!(r["length"], 24, "but it does have a size");
    }

    #[test]
    fn matrices_are_fingerprinted_and_sized() {
        let r = by_name("mat_named");
        assert_eq!(s(&r, "kind"), "matrix");
        assert_eq!((r["nrow"].clone(), r["ncol"].clone()), (3.into(), 4.into()));
        assert_eq!(r["length"], 12, "rows times columns");
        assert!(!s(&r, "content_fp").is_empty(), "matrices are data too");
        assert_eq!(r["has_dimnames"], true);
    }

    /// A list column made col_len report zero, which aborted the profile for the
    /// entire frame. That is why an sf object produced no fingerprint and was
    /// dropped before it reached storage.
    #[test]
    fn a_frame_with_a_list_column_still_profiles() {
        let r = by_name("sf_like");
        assert_eq!(s(&r, "confidence"), "exact");
        assert!(!s(&r, "content_fp").is_empty());
        assert_eq!(r["ncol"], 2);
    }

    #[test]
    fn a_geometry_column_carries_its_crs_and_extent() {
        let r = by_name("sf_like");
        let cols: Vec<Value> = serde_json::from_value(r["columns"].clone()).unwrap();
        let g = cols.iter().find(|c| c["is_geometry"] == true).expect("a geometry column");
        assert_eq!(g["crs_epsg"], 4326);
        assert_eq!(s(g, "geom_type"), "POINT");
        assert_eq!(g["n_geometries"], 3);
        assert_eq!(g["bbox"], serde_json::json!([1.0, 2.0, 5.0, 6.0]));
    }

    #[test]
    fn a_time_series_reports_its_span_and_frequency() {
        let r = by_name("ts_month");
        assert_eq!(r["ts_frequency"], 12.0, "monthly");
        assert_eq!(r["ts_start"], 1949.0);
        assert!(r["ts_end"].as_f64().unwrap() > 1952.0);
    }

    /// An mts is a matrix, so the span has to survive that branch too.
    #[test]
    fn a_multivariate_series_keeps_both_shape_and_span() {
        let r = by_name("ts_multi");
        assert_eq!(s(&r, "kind"), "matrix");
        assert_eq!(r["ncol"], 2);
        assert_eq!(r["ts_frequency"], 4.0, "quarterly");
        assert_eq!(r["ts_start"], 2000.0);
    }

    /// Lists were a third of the dataset records and none of them carried a
    /// fingerprint, so none of them reached storage.
    #[test]
    fn a_plain_list_is_fingerprinted() {
        let r = by_name("plain_list");
        assert_eq!(s(&r, "kind"), "list");
        assert_eq!(r["length"], 3);
        assert!(!s(&r, "content_fp").is_empty());
    }

    #[test]
    fn every_fixture_is_readable() {
        let recs = records();
        assert_eq!(recs.len(), 13, "one record per fixture");
        for r in &recs {
            let name = s(r, "name");
            assert_ne!(s(r, "confidence"), "degraded", "{name} degraded");
            assert!(!s(r, "content_fp").is_empty(), "{name} has no fingerprint");
        }
    }
}
