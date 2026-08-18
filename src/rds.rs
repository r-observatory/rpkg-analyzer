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
const PROMSXP: u8 = 5;
const DOTSXP: u8 = 17;
const EXTPTRSXP: u8 = 22;
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
const EXPRSXP: u8 = 20;
const BCODESXP: u8 = 21;
const RAWSXP: u8 = 24;
const S4SXP: u8 = 25;
const ALTREP_SXP: u8 = 238;
// Singleton markers: a type byte and nothing else. R writes these where a slot
// holds one of its well-known constants.
const GLOBALENV_SXP: u8 = 253;
const UNBOUNDVALUE_SXP: u8 = 252;
const MISSINGARG_SXP: u8 = 251;
const BASENAMESPACE_SXP: u8 = 250;
const BCREPDEF: i32 = 244;
const BCREPREF: i32 = 243;
const ATTRLANGSXP: i32 = 240;
const ATTRLISTSXP: i32 = 239;
const PERSISTSXP: u8 = 247;
const PACKAGESXP: u8 = 248;
const NAMESPACESXP: u8 = 249;
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
    /// Code vector then constant pool.
    fn bc_body(&mut self) -> Result<(), String> {
        self.item()?;
        let n = self.i32()?;
        for _ in 0..n.max(0) {
            let t = self.i32()?;
            match t {
                t if t == BCODESXP as i32 => self.bc_body()?,
                BCREPDEF | BCREPREF | ATTRLANGSXP | ATTRLISTSXP => self.bc_lang(t)?,
                t if t == LANGSXP as i32 || t == LISTSXP as i32 => self.bc_lang(t)?,
                // Anything else is an ordinary object, and the tag just read is a
                // marker rather than its flags, so it reads its own header.
                _ => { self.item()?; }
            }
        }
        Ok(())
    }

    /// The language objects inside a constant pool, which are shared by
    /// reference within one pool and so are written in their own dialect.
    fn bc_lang(&mut self, t: i32) -> Result<(), String> {
        if t == BCREPREF {
            self.i32()?;
            return Ok(());
        }
        let mut t = t;
        if t == BCREPDEF {
            self.i32()?;
            t = self.i32()?;
        }
        if t == ATTRLANGSXP || t == ATTRLISTSXP || t == LANGSXP as i32 || t == LISTSXP as i32 {
            if t == ATTRLANGSXP || t == ATTRLISTSXP {
                self.item()?;
            }
            self.item()?;
            let car = self.i32()?;
            self.bc_lang(car)?;
            let cdr = self.i32()?;
            self.bc_lang(cdr)?;
            return Ok(());
        }
        self.item()?;
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
            VECSXP | EXPRSXP => {
                let n = self.vlen()?;
                let mut els = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    els.push(self.item()?);
                }
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Vec(els), attr })
            }
            LISTSXP | LANGSXP | CLOSXP | PROMSXP | DOTSXP => {
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
                // Propagate rather than swallow. `.ok()` here turned a failed
                // read into a silent resume from a desynchronized position, so
                // the object came back with a truncated slot list and the bytes
                // after it were parsed as further top-level objects: one raster
                // produced two records, the second of them garbage. A failure
                // has to end the file, not quietly corrupt it.
                let attr = self.maybe_attr(ha)?;
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
            // An external pointer. data.table puts one on every table as
            // `.internal.selfref`, so rejecting the type lost the whole file for
            // one of the most widely shipped data classes on CRAN. Like an
            // environment it takes a reference-table slot, then two items.
            // A namespace, package or persistent-object reference. All three
            // are written as R's string vector: a zero, a length, then that many
            // strings, and all three take a reference-table slot. A reference
            // class or an S7 object names its defining namespace, so rejecting
            // the type lost every object in those files.
            NAMESPACESXP | PACKAGESXP | PERSISTSXP => {
                let _zero = self.i32()?;
                let n = self.i32()?;
                let mut parts = Vec::new();
                for _ in 0..n.max(0) {
                    if let Val::Char(Some(t)) = self.item()?.val {
                        parts.push(t);
                    }
                }
                let node = Node { val: Val::Sym(parts.join("::")), attr: None };
                self.refs.push(node.clone());
                Ok(node)
            }
            // Byte code. Never content in its own right, but a closure body is
            // compiled, and a compiled body sits inside anything that carries a
            // function: a reference class, an S7 object, a formula's environment.
            // Reading it as a plain vector desynchronised the stream and cost the
            // whole file, not just the object holding it.
            BCODESXP => {
                let _nreps = self.i32()?;
                self.bc_body()?;
                Ok(Node { val: Val::Nil, attr: None })
            }
            EXTPTRSXP => {
                let slot = self.refs.len();
                self.refs.push(Node { val: Val::Nil, attr: None });
                let _prot = self.item()?;
                let _tag = self.item()?;
                let attr = self.maybe_attr(ha)?;
                let node = Node { val: Val::Nil, attr };
                self.refs[slot] = node.clone();
                Ok(node)
            }
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

/// Attribute names this reader consumes by name. Everything else is metadata an
/// author chose to attach, and is reported rather than silently ignored.
const CONSUMED_ATTRS: &[&str] = &[
    "class", "levels", "row.names", "names", "dim", "dimnames", "package", "nrows",
    "listData", "elementMetadata", "tsp", "index", "frequency", "crs", "bbox",
    "data", "proj4string", "coords", "coords.nrs", "comment", "label", "units",
    ".xData", "S7_class",
];

// Slots a particular class is known for, passed by the branch that read them.
// These stay per-class rather than global because the names are ordinary: an S7
// object may well have a property called `x`, and suppressing it everywhere to
// keep a sparse matrix tidy would lose the thing worth recording.
const MATRIX_SLOTS: &[&str] = &["Dim", "Dimnames", "i", "j", "p", "x", "uplo", "diag", "factors"];
// `file`, `legend`, `history`, `title` and `z` describe how a raster was made
// rather than what it holds.
const RASTER_SLOTS: &[&str] = &[
    "ncols", "extent", "srs", "file", "legend", "history", "title", "z", "rotated", "rotation",
];

/// Short scalar text from an attribute, for the ones worth storing whole.
fn attr_text(pairs: &[(String, &Node)], key: &str, cap: usize) -> Option<String> {
    let v = attr(pairs, key)?;
    let t = match &v.val {
        Val::Str(items) => items.first().and_then(|x| x.clone())?,
        Val::Sym(x) => x.clone(),
        Val::Char(Some(x)) => x.clone(),
        _ => return None,
    };
    if t.is_empty() {
        return None;
    }
    Some(t.chars().take(cap).collect())
}

/// Human-written description and any attribute names this reader does not
/// otherwise consume.
///
/// Authors attach a great deal of metadata that nothing was looking at: `label`
/// and `variable.labels` from the survey packages, `units`, base R's `comment`,
/// readr's `spec`, tsibble's `key`, and one-off things like a source URL or a
/// licence. Recording the NAMES of whatever is not consumed costs a short list
/// and makes the corpus self-describing: after one pass the set of attributes
/// CRAN actually uses is measured rather than guessed, which is the only sound
/// basis for deciding what to promote to a field of its own later.
fn describe_attrs(pairs: &[(String, &Node)], out: &mut Value, extra: &[&str]) {
    if let Some(t) = attr_text(pairs, "label", 200) {
        out["label"] = json!(t);
    }
    if let Some(t) = attr_text(pairs, "comment", 400) {
        out["comment"] = json!(t);
    }
    if let Some(t) = attr_text(pairs, "units", 40) {
        out["units"] = json!(t);
    }
    // An attribute this reader tried to read but could not extract a value from
    // (units is sometimes a list rather than a string) still gets reported by
    // name. Dropping it would say it was not there, which is a different claim.
    let mut other: Vec<String> = pairs
        .iter()
        .map(|(k, _)| k.clone())
        .filter(|k| {
            !CONSUMED_ATTRS.contains(&k.as_str())
                && !extra.contains(&k.as_str())
                || (matches!(k.as_str(), "label" | "comment" | "units") && out.get(k).is_none())
        })
        .collect();
    other.sort();
    other.dedup();
    if !other.is_empty() {
        // Bounded: a pathological object cannot turn this into a payload.
        other.truncate(24);
        out["attrs_other"] = json!(other);
    }
}

/// Authority code from a WKT string, taken from the last ID[...] clause.
///
/// WKT2 nests ID clauses (the datum and the ellipsoid carry their own), and the
/// one describing the CRS itself is the outermost, which is written last.
fn wkt_epsg(wkt: &str) -> Option<i64> {
    let mut found = None;
    let mut rest = wkt;
    while let Some(i) = rest.find("ID[") {
        let tail = &rest[i + 3..];
        let end = match tail.find(']') {
            Some(e) => e,
            None => break,
        };
        let body = &tail[..end];
        let mut parts = body.splitn(2, ',');
        let auth = parts.next().unwrap_or("").trim().trim_matches('"');
        if auth.eq_ignore_ascii_case("EPSG") {
            if let Some(code) = parts.next() {
                if let Ok(n) = code.trim().trim_matches('"').parse::<i64>() {
                    found = Some(n);
                }
            }
        }
        rest = &tail[end..];
    }
    found
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
                    "wkt" => {
                        // The authority code is the LAST thing in a WKT2 string,
                        // so it has to be read before truncating: sf's own nc
                        // example names its CRS "NAD27" rather than "EPSG:4267",
                        // and the only 4267 in the object is that trailing ID.
                        if let Some(code) = wkt_epsg(first) {
                            o["crs_epsg"] = json!(code);
                        }
                        // 128, not 512. Measured on real sf objects the WKT was
                        // 76% of everything a geometry column adds, for text that
                        // mostly restates crs_epsg. The head carries the CRS name
                        // and datum, which is what a reader wants when there is no
                        // authority code; the axis definitions are not worth
                        // 50,000 copies.
                        o["crs_wkt"] = json!(first.chars().take(128).collect::<String>());
                    }
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
    // Two different things used to be one. A column whose values were never
    // materialized (over the cap, or a type this reader skips) genuinely cannot
    // be profiled. A frame with no rows can: its schema is known, its content is
    // empty, and an empty table is a fact rather than a failure. Conflating them
    // reported every zero-row dataset as "value scan skipped (size cap)", which
    // named a cause that had not happened.
    let n = cols.iter().map(|c| col_len(c)).min().unwrap_or(0);
    // Materialized is the test, not non-empty: `integer(0)` is a column whose
    // values were read and there are none of them, while a capped or skipped
    // column has no values to read at all. Only the second can defeat a profile.
    let any_unread = cols.iter().any(|c| {
        !matches!(c.val, Val::Ints { .. } | Val::Reals { .. } | Val::Str(_) | Val::Vec(_))
    });
    if any_unread {
        return None;
    }
    let _ = n;

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
        describe_attrs(&attr_pairs(col), &mut o, &[]);
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

fn num1(n: &Node) -> Option<f64> {
    match &n.val {
        Val::Reals { vals, .. } => vals.first().copied(),
        Val::Ints { vals, .. } => vals.first().map(|v| *v as f64),
        _ => None,
    }
}

/// The Matrix package names a class for what it is: a value type, a shape and a
/// storage layout, in that order. Reading the name is how a symmetric matrix
/// becomes legible, because it stores one triangle and its stored count is
/// otherwise half of what a reader would expect for its density.
fn matrix_class_parts(klass: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let p = klass.strip_suffix("Matrix")?;
    if p == "p" || p == "ind" {
        return Some(("pattern", "permutation", "index"));
    }
    let b = p.as_bytes();
    if b.len() != 3 {
        return None;
    }
    let value = match b[0] {
        b'd' => "double",
        b'l' => "logical",
        b'n' => "pattern",
        b'i' => "integer",
        b'z' => "complex",
        _ => return None,
    };
    let shape = match b[1] {
        b'g' => "general",
        b's' => "symmetric",
        b't' => "triangular",
        b'd' => "diagonal",
        b'p' => "positive-definite",
        _ => return None,
    };
    let storage = match b[2] {
        b'C' => "column-compressed",
        b'R' => "row-compressed",
        b'T' => "triplet",
        b'i' => "diagonal",
        b'e' | b'y' | b'r' | b'o' => "dense",
        b'p' => "packed",
        _ => return None,
    };
    Some((value, shape, storage))
}

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
        // Not everything arriving here is S4. A reference class and an R6 object
        // are environments wearing a class, and S7 sets the S4 bit for
        // compatibility, so all three used to be labelled S4 and were not.
        let system = if class.iter().any(|c| c == "R6") {
            "R6"
        } else if class.iter().any(|c| c == "S7_object") || attr(&pairs, "S7_class").is_some() {
            "S7"
        } else if attr(&pairs, ".xData").is_some() {
            "RefClass"
        } else {
            "S4"
        };
        let mut rec = json!({
            "rec": "dataset", "name": name, "file": file, "format": fmt,
            "format_version": ver, "compression": comp, "compressed_bytes": size,
            "internal": internal, "class": format!("{system}:{klass}"), "s4_package": pkg,
            "object_system": system,
            "confidence": "degraded"
        });
        // sp: the spatial classes are S4 whose slots are the attribute list, so
        // the attributes carry the whole object. A Spatial*DataFrame keeps its
        // table in `data`, its extent in `bbox` and its projection in
        // `proj4string`, none of which was read: an sp object arrived as a class
        // name and nothing else.
        let sp_data = attr(&pairs, "data");
        let sp_bbox = attr(&pairs, "bbox");
        if sp_bbox.is_some() || klass.starts_with("Spatial") {
            rec["kind"] = json!(klass.clone());
            rec["is_spatial"] = json!(true);
            if let Some(d) = sp_data {
                let dp = attr_pairs(d);
                if let Some(r) = nrow_of(attr(&dp, "row.names")) {
                    rec["nrow"] = json!(r);
                }
                if let Val::Vec(cols) = &d.val {
                    rec["ncol"] = json!(cols.len());
                    let names = attr(&dp, "names").map(str_vec).unwrap_or_default();
                    let refs: Vec<&Node> = cols.iter().collect();
                    if let Some(pr) = profile_columns(&refs, &names) {
                        rec["n_missing_total"] = json!(pr.n_missing_total);
                        rec["schema_fp"] = json!(pr.schema_fp);
                        rec["shape_fp"] = json!(pr.shape_fp);
                        rec["content_fp"] = json!(pr.content_fp);
                        rec["row_sketch"] = json!(pr.row_sketch);
                        rec["columns"] = json!(pr.columns);
                    }
                }
            }
            // bbox is a 2x2 matrix stored column-major as xmin, ymin, xmax, ymax.
            if let Some(Val::Reals { vals, .. }) = sp_bbox.map(|n| &n.val) {
                if vals.len() >= 4 {
                    rec["bbox"] = json!(vals[..4]);
                }
            }
            if let Some(pj) = attr(&pairs, "proj4string") {
                let pp = attr_pairs(pj);
                if let Some(args) = attr(&pp, "projargs") {
                    let txt = str_vec(args);
                    if let Some(first) = txt.first() {
                        if !first.is_empty() {
                            rec["crs_input"] = json!(first);
                            if let Some(i) = first.find("+init=epsg:") {
                                let code: String = first[i + 11..]
                                    .chars()
                                    .take_while(|c| c.is_ascii_digit())
                                    .collect();
                                if let Ok(n) = code.parse::<i64>() {
                                    rec["crs_epsg"] = json!(n);
                                }
                            }
                        }
                    }
                }
            }
            if rec.get("content_fp").is_some() {
                rec["confidence"] = json!("exact");
                rec["notes"] = json!("sp-spatial");
            } else {
                rec["notes"] = json!("sp-spatial-geometry-only");
            }
            describe_attrs(&pairs, &mut rec, &[]);
            return rec;
        }
        // Matrix and other S4 classes state their shape in a `Dim` slot, which
        // is an ordinary integer pair sitting in the attribute list. Without it
        // a sparse matrix reported a class name and no shape at all.
        if let Some(Val::Ints { vals, .. }) = attr(&pairs, "Dim").map(|n| &n.val) {
            if vals.len() >= 2 {
                let (r, c) = (vals[0] as i64, vals[1] as i64);
                rec["kind"] = json!(klass.clone());
                rec["nrow"] = json!(r);
                rec["ncol"] = json!(c);
                rec["notes"] = json!("s4-dim-slot");
                // The `x` slot holds the stored values. For a sparse matrix that
                // is the non-zero count, and it is the number that means
                // anything: reporting a 100x100 with three entries as 10,000
                // cells describes the grid it is embedded in, not the data.
                let parts = matrix_class_parts(&klass);
                if let Some((value, shape, storage)) = parts {
                    rec["matrix_value_type"] = json!(value);
                    rec["matrix_shape"] = json!(shape);
                    rec["matrix_storage"] = json!(storage);
                }
                // Which triangle a symmetric or triangular matrix keeps. It does
                // not change any count, but without it the shape above is the
                // only thing saying why the count is roughly half the grid.
                if let Some(u) = attr_text(&pairs, "uplo", 4) {
                    rec["matrix_uplo"] = json!(u);
                }
                // A unit diagonal is not written down. Its entries are still
                // non-zero, so a matrix marked this way carries min(nrow, ncol)
                // more than its slots hold, and n_stored counts them.
                let unit_diag = attr_text(&pairs, "diag", 4).as_deref() == Some("U");
                if attr(&pairs, "diag").is_some() {
                    rec["matrix_diag"] = json!(if unit_diag { "unit" } else { "stored" });
                }
                // A pattern matrix records only where the non-zeros are and has
                // no values slot at all, so counting the values slot called it
                // empty and fell through to describing the dense grid instead.
                let x = attr(&pairs, "x");
                let idx = attr(&pairs, "i").or_else(|| attr(&pairs, "j"));
                let stored = match x {
                    Some(n) if col_len(n) > 0 => col_len(n),
                    _ => idx.map(col_len).unwrap_or(0),
                } + if unit_diag { r.min(c) as usize } else { 0 };
                if stored > 0 {
                    rec["n_stored"] = json!(stored);
                    let cells = r.saturating_mul(c);
                    if cells > 0 {
                        rec["density"] = json!((stored as f64 / cells as f64 * 1e6).round() / 1e6);
                    }
                    rec["length"] = json!(stored as i64);
                    rec["n_cells"] = json!(cells);
                    // Fingerprint the values actually stored, so two matrices
                    // holding the same data dedup even though the class wrapping
                    // them says nothing about it.
                    let mut cols: Vec<&Node> = Vec::new();
                    if let Some(x) = x {
                        cols.push(x);
                    }
                    if let Some(i) = idx {
                        cols.push(i);
                    }
                    if let Some(pp) = attr(&pairs, "p") {
                        cols.push(pp);
                    }
                    if !cols.is_empty() {
                        let names: Vec<String> =
                            cols.iter().enumerate().map(|(k, _)| format!("s{k}")).collect();
                        if let Some(pr) = profile_columns(&cols, &names) {
                            rec["n_missing_total"] = json!(pr.n_missing_total);
                            rec["schema_fp"] = json!(pr.schema_fp);
                            rec["shape_fp"] = json!(pr.shape_fp);
                            rec["content_fp"] = json!(pr.content_fp);
                            rec["row_sketch"] = json!(pr.row_sketch);
                            rec["confidence"] = json!("exact");
                        }
                    }
                } else {
                    rec["length"] = json!(r.saturating_mul(c));
                }
                describe_attrs(&pairs, &mut rec, MATRIX_SLOTS);
                return rec;
            }
        }

        // raster predates terra and is largely superseded by it, but its objects
        // are still bundled widely, and every one of them was arriving as a
        // class name. The grid is in plain `nrows`/`ncols` slots; what made this
        // look hard was the layer data underneath, which is byte-compiled and
        // used to desynchronise the read.
        if klass.starts_with("Raster") {
            if let (Some(nr), Some(nc)) = (
                attr(&pairs, "nrows").and_then(int1),
                attr(&pairs, "ncols").and_then(int1),
            ) {
                rec["kind"] = json!(klass.clone());
                rec["nrow"] = json!(nr);
                rec["ncol"] = json!(nc);
                rec["length"] = json!(nr.saturating_mul(nc));
                rec["is_spatial"] = json!(true);
                if let Some(e) = attr(&pairs, "extent") {
                    let ep = attr_pairs(e);
                    let g = |k: &str| attr(&ep, k).and_then(num1);
                    if let (Some(x0), Some(y0), Some(x1), Some(y1)) =
                        (g("xmin"), g("ymin"), g("xmax"), g("ymax"))
                    {
                        rec["bbox"] = json!([x0, y0, x1, y1]);
                    }
                }
                // `srs` holds the WKT on current versions, `crs` the older
                // proj4 string, and files in the wild carry either.
                // `srs` carries whichever form the file was written with: an
                // authority string on the short path, full WKT on the long one.
                if let Some(t) = attr_text(&pairs, "srs", 4000) {
                    if !t.is_empty() {
                        rec["crs_input"] = json!(t.chars().take(128).collect::<String>());
                        if let Some(rest) = t.strip_prefix("EPSG:") {
                            if let Ok(code) = rest.trim().parse::<i64>() {
                                rec["crs_epsg"] = json!(code);
                            }
                        } else if t.contains("ID[") || t.contains("AUTHORITY[") {
                            rec["crs_wkt"] = json!(t.chars().take(128).collect::<String>());
                            if let Some(code) = wkt_epsg(&t) {
                                rec["crs_epsg"] = json!(code);
                            }
                        }
                    }
                }
                if rec.get("crs_input").is_none() {
                    if let Some(cr) = attr(&pairs, "crs") {
                        let cp = attr_pairs(cr);
                        if let Some(t) = attr_text(&cp, "projargs", 128) {
                            if !t.is_empty() {
                                rec["crs_input"] = json!(t);
                            }
                        }
                    }
                }
                // A layer keeps its cells in the `data` slot, and a stack or a
                // brick keeps one name per layer there.
                if let Some(d) = attr(&pairs, "data") {
                    let dp = attr_pairs(d);
                    if let Some(nm) = attr(&dp, "names") {
                        let n = col_len(nm);
                        if n > 0 {
                            rec["n_layers"] = json!(n as i64);
                        }
                    }
                    if let Some(v) = attr(&dp, "values") {
                        if col_len(v) > 0 {
                            let cols = vec![v];
                            let names = vec!["values".to_string()];
                            if let Some(pr) = profile_columns(&cols, &names) {
                                rec["n_missing_total"] = json!(pr.n_missing_total);
                                rec["schema_fp"] = json!(pr.schema_fp);
                                rec["shape_fp"] = json!(pr.shape_fp);
                                rec["content_fp"] = json!(pr.content_fp);
                                rec["row_sketch"] = json!(pr.row_sketch);
                                rec["confidence"] = json!("exact");
                            }
                        }
                    }
                }
                rec["notes"] = json!("raster-grid");
                describe_attrs(&pairs, &mut rec, RASTER_SLOTS);
                return rec;
            }
        }

        // terra ships rasters and vectors through wrap(), whose S4 form keeps
        // the whole geometry in a `definition` string. Parsing it is the only
        // way to get the grid: the values slot alone made an 8x12 raster look
        // like a 96x1 matrix.
        if klass.starts_with("Packed") {
            if let Some(def) = attr_text(&pairs, "definition", 400) {
                let num = |key: &str| -> Option<i64> {
                    let i = def.find(key)?;
                    def[i + key.len()..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse()
                        .ok()
                };
                if let (Some(nc), Some(nr)) = (num("ncols="), num("nrows=")) {
                    rec["nrow"] = json!(nr);
                    rec["ncol"] = json!(nc);
                    rec["length"] = json!(nr.saturating_mul(nc));
                }
                if let Some(n) = num("nlyrs=") {
                    rec["n_layers"] = json!(n);
                }
                for k in ["xmin=", "xmax=", "ymin=", "ymax="] {
                    let _ = k;
                }
                if let Some(i) = def.find("crs='") {
                    let crs: String =
                        def[i + 5..].chars().take_while(|c| *c != '\'').collect();
                    if !crs.is_empty() {
                        rec["crs_input"] = json!(crs);
                    }
                }
                rec["kind"] = json!(klass.clone());
                rec["is_spatial"] = json!(true);
                rec["notes"] = json!("terra-packed");
                describe_attrs(&pairs, &mut rec, &["definition"]);
                return rec;
            }
        }
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
        describe_attrs(&pairs, &mut rec, &[]);
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
        describe_attrs(&pairs, &mut rec, &[]);
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
            describe_attrs(&pairs, &mut rec, &[]);
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
    // igraph stores a graph as a bare list of ten: vertex count, directed flag,
    // then the two edge-endpoint vectors. Without reading it a graph reported
    // "a list of length 10", which describes igraph's layout rather than the
    // graph.
    if class.iter().any(|c| c == "igraph") {
        if let Val::Vec(items) = &node.val {
            if items.len() >= 4 {
                // igraph writes the vertex count as a double, so reading only
                // integers found nothing.
                let nv = match &items[0].val {
                    Val::Reals { vals, .. } => vals.first().map(|v| *v as i64),
                    Val::Ints { vals, .. } => vals.first().map(|v| *v as i64),
                    _ => None,
                };
                if let Some(n) = nv {
                    rec["n_vertices"] = json!(n);
                }
                let edges = col_len(&items[2]);
                rec["n_edges"] = json!(edges);
                if let Val::Ints { vals, .. } = &items[1].val {
                    rec["directed"] = json!(vals.first().copied().unwrap_or(0) != 0);
                }
                rec["kind"] = json!("graph");
                rec["length"] = json!(edges as i64);
            }
        }
    }
    describe_attrs(&pairs, &mut rec, &[]);
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

    /// Real sf, not a hand-built imitation. The nc example names its CRS
    /// "NAD27" and puts the authority code only in the trailing ID clause of a
    /// WKT longer than the stored cap, so reading the code after truncating
    /// silently lost it.
    #[test]
    fn a_real_sf_object_yields_its_epsg_from_the_wkt() {
        let r = by_name("real_sf_nc");
        assert_eq!(s(&r, "confidence"), "exact");
        let cols: Vec<Value> = serde_json::from_value(r["columns"].clone()).unwrap();
        let g = cols.iter().find(|c| c["is_geometry"] == true).expect("a geometry column");
        assert_eq!(g["crs_epsg"], 4267, "read from ID[\"EPSG\",4267] at the end of the WKT");
        assert_eq!(s(g, "crs_input"), "NAD27", "which is not an EPSG string");
        assert_eq!(s(g, "geom_type"), "MULTIPOLYGON");
        assert_eq!(g["n_geometries"], 100);
    }

    #[test]
    fn a_projected_sf_object_keeps_its_declared_code() {
        let r = by_name("real_sf_points");
        let cols: Vec<Value> = serde_json::from_value(r["columns"].clone()).unwrap();
        let g = cols.iter().find(|c| c["is_geometry"] == true).unwrap();
        assert_eq!(g["crs_epsg"], 3857);
        assert_eq!(s(g, "geom_type"), "POINT");
    }

    /// sp is S4 and its slots are the attribute list: the table in `data`, the
    /// extent in `bbox`, the projection in `proj4string`. None was read, so an
    /// sp object arrived as a class name and nothing else.
    #[test]
    fn an_sp_object_is_more_than_its_class_name() {
        let r = by_name("real_sp_points");
        assert_eq!(s(&r, "confidence"), "exact");
        assert_eq!(r["nrow"], 3, "from the data slot");
        assert_eq!(r["is_spatial"], true);
        assert!(r["bbox"].is_array(), "and an extent");
        assert!(!s(&r, "content_fp").is_empty());
    }

    /// Every data.table carries `.internal.selfref`, an external pointer. The
    /// type was unhandled, and because that aborts the stream it lost the file.
    #[test]
    fn a_data_table_survives_its_external_pointer() {
        for n in ["real_datatable", "a_datatable"] {
            let r = by_name(n);
            assert_eq!(s(&r, "confidence"), "exact", "{n}");
            assert!(!s(&r, "content_fp").is_empty(), "{n} has no fingerprint");
        }
    }

    #[test]
    fn real_series_objects_report_their_span() {
        let air = by_name("real_ts_air");
        assert_eq!(air["ts_frequency"], 12.0);
        assert_eq!(air["ts_start"], 1949.0);
        assert_eq!(air["length"], 144);

        let z = by_name("real_zoo_date");
        assert_eq!(z["index_n"], 50);
        assert_eq!(s(&z, "index_class"), "Date", "so the index numbers mean days");
    }

    /// An empty table is a fact, not a failure. It used to be reported as
    /// "value scan skipped (size cap)", naming a cause that had not happened.
    #[test]
    fn compiled_code_does_not_cost_the_rest_of_its_file() {
        // The regression this whole reader exists for: a compiled function body
        // used to be read as a plain vector, which left the stream out of step
        // and lost every object saved after it.
        let r = by_name("after_compiled");
        assert_eq!(s(&r, "confidence"), "exact");
        assert_eq!(r["nrow"], 4);
        assert_eq!(r["ncol"], 2);
    }

    #[test]
    fn a_pattern_matrix_counts_its_positions_not_its_grid() {
        // It has no values slot at all, so counting values called it empty and
        // fell back to describing the 8x8 grid it is embedded in.
        let r = by_name("spm_ngc");
        assert_eq!(s(&r, "matrix_value_type"), "pattern");
        assert_eq!(r["n_stored"], 3);
        assert_eq!(r["n_cells"], 64);
        assert_eq!(s(&r, "confidence"), "exact");
        assert!(!s(&r, "content_fp").is_empty());
    }

    #[test]
    fn a_row_compressed_matrix_is_read_like_a_column_one() {
        // It indexes with `j` where the column form uses `i`.
        let r = by_name("spm_dgr");
        assert_eq!(s(&r, "matrix_storage"), "row-compressed");
        assert_eq!(r["n_stored"], 3);
        assert!(!s(&r, "content_fp").is_empty());
    }

    #[test]
    fn a_symmetric_matrix_says_it_keeps_one_triangle() {
        // Without the shape, its stored count reads as half the non-zeros it has.
        let r = by_name("spm_dsc");
        assert_eq!(s(&r, "matrix_shape"), "symmetric");
        assert!(!s(&r, "matrix_uplo").is_empty());
    }

    #[test]
    fn a_unit_diagonal_is_counted_though_it_is_not_stored() {
        let r = by_name("spm_unit_diag");
        assert_eq!(s(&r, "matrix_diag"), "unit");
        assert_eq!(r["n_stored"], 4);
    }

    #[test]
    fn the_object_systems_are_told_apart() {
        // All three are environments or S4-flagged objects and all three used to
        // be labelled S4, which is wrong for every one of them.
        assert_eq!(s(&by_name("oo_refclass"), "object_system"), "RefClass");
        assert_eq!(s(&by_name("oo_r6"), "object_system"), "R6");
        assert_eq!(s(&by_name("oo_s7"), "object_system"), "S7");
    }

    #[test]
    fn a_raster_reports_its_grid_and_its_ground() {
        let r = by_name("raster_layer");
        assert_eq!(r["nrow"], 10);
        assert_eq!(r["ncol"], 20);
        assert_eq!(r["n_layers"], 1);
        assert_eq!(r["crs_epsg"], 4326);
        assert_eq!(r["bbox"], serde_json::json!([0.0, 0.0, 10.0, 5.0]));
        assert_eq!(s(&r, "confidence"), "exact");
    }

    #[test]
    fn a_multi_layer_raster_counts_its_layers_and_reads_its_wkt() {
        // This one writes the projection as full WKT, where the single layer
        // writes the short authority form. The code has to come out of both.
        let r = by_name("raster_brick");
        assert_eq!(r["n_layers"], 2);
        assert_eq!(r["crs_epsg"], 4326);
    }

    #[test]
    fn a_zero_row_frame_is_described_not_blamed() {
        let r = by_name("frame_zero_rows");
        assert_eq!(s(&r, "confidence"), "exact");
        assert_eq!(r["nrow"], 0);
        assert!(!s(&r, "content_fp").is_empty());
    }

    /// Authors attach far more metadata than this reader consumes by name.
    /// Reporting the names of what is left makes the corpus self-describing:
    /// after one pass, what CRAN actually uses is measured rather than guessed.
    #[test]
    fn unconsumed_attributes_are_named_not_ignored() {
        let r = by_name("author_metadata");
        let names: Vec<String> = serde_json::from_value(r["attrs_other"].clone()).unwrap();
        for want in ["collected", "license", "source_url"] {
            assert!(names.iter().any(|n| n == want), "{want} should be reported");
        }
        // The names this reader does consume are not repeated back as unknowns.
        assert!(!names.iter().any(|n| n == "class" || n == "names" || n == "row.names"));
    }

    /// A human-written description of the data, which nothing was reading.
    #[test]
    fn a_label_and_a_comment_are_captured() {
        assert_eq!(s(&by_name("labelled_frame"), "label"), "Survey wave 1");
        assert_eq!(s(&by_name("commented"), "comment"), "Collected 2019, see vignette");
    }

    /// The survey packages put the label on the column, not the object.
    #[test]
    fn a_column_carries_its_own_label_and_units() {
        let r = by_name("hmisc_labels");
        let cols: Vec<Value> = serde_json::from_value(r["columns"].clone()).unwrap();
        let c = cols.iter().find(|c| s(c, "name") == "wt").expect("the wt column");
        assert_eq!(s(c, "label"), "Body weight");
        assert_eq!(s(c, "units"), "kg");
    }

    /// Matrix states its shape in a Dim slot; without reading it a sparse matrix
    /// was a class name and nothing else.
    ///
    /// `length` is the count of values actually stored, not the dense product.
    /// A 100x100 holding three entries is three numbers in a grid, and calling
    /// that 10,000 describes the grid rather than the data; the dense figure is
    /// kept beside it as n_cells.
    #[test]
    fn a_sparse_matrix_reports_what_it_stores() {
        let r = by_name("sparse_matrix");
        assert_eq!(r["nrow"], 2);
        assert_eq!(r["ncol"], 2);
        assert_eq!(r["n_cells"], 4, "the grid it sits in");
        assert_eq!(r["n_stored"], 2, "the values it actually holds");
        assert_eq!(r["length"], 2, "and that is what its size means");
        assert!(!s(&r, "content_fp").is_empty(), "fingerprinted from the stored values");
    }

    /// A hundred-by-hundred matrix holding three values: density is what
    /// distinguishes it from a dense one, and the dense count hides that.
    #[test]
    fn a_large_sparse_matrix_reports_its_density() {
        let r = by_name("sparse_big");
        assert_eq!(r["n_cells"], 10000);
        assert_eq!(r["n_stored"], 3);
        assert!(r["density"].as_f64().unwrap() < 0.001);
        assert_eq!(s(&r, "confidence"), "exact");
    }

    /// terra ships rasters through wrap(), which keeps the grid in a definition
    /// string. Reading only the values slot made an 8x12 raster a 96x1 matrix.
    #[test]
    fn a_packed_raster_reports_its_grid_not_its_value_vector() {
        let r = by_name("terra_packed");
        assert_eq!(r["nrow"], 8);
        assert_eq!(r["ncol"], 12);
        assert_eq!(r["length"], 96);
        assert_eq!(r["n_layers"], 1);
        assert_eq!(s(&r, "crs_input"), "OGC:CRS84");
        assert_eq!(r["is_spatial"], true);
    }

    /// A graph reported "a list of length 10", which describes igraph's own
    /// layout rather than the graph.
    #[test]
    fn a_graph_reports_vertices_and_edges() {
        let ring = by_name("igraph_ring");
        assert_eq!(s(&ring, "kind"), "graph");
        assert_eq!(ring["n_vertices"], 10);
        assert_eq!(ring["n_edges"], 10);
        assert_eq!(ring["directed"], false);

        let w = by_name("igraph_weighted");
        assert_eq!(w["n_vertices"], 20, "read as a double, not an integer");
    }

    #[test]
    fn a_four_dimensional_array_keeps_its_whole_extent() {
        let r = by_name("arr_4d_dbl");
        assert_eq!(r["n_dim"], 4);
        assert_eq!(r["dim"], serde_json::json!([2, 3, 4, 5]));
        assert_eq!(r["length"], 120);
    }

    #[test]
    fn every_fixture_is_readable() {
        let recs = records();
        assert_eq!(recs.len(), 45, "one record per saved object");
        // Not everything saved under data/ is data. These carry behaviour rather
        // than observations, so there is nothing to fingerprint; what they must
        // still do is read cleanly, because a file that fails mid-object takes
        // every object after it down as well.
        let objects = [
            "oo_r6", "oo_refclass", "oo_s7", "cmp_fun",
            // terra keeps its values in a raw blob, so the grid is described but
            // the cells are not fingerprinted.
            "terra_packed",
        ];
        for r in &recs {
            let name = s(r, "name");
            let notes = s(r, "notes");
            assert!(
                !notes.contains("unhandled") && !notes.contains("truncated"),
                "{name} did not read: {notes}"
            );
            if objects.contains(&name.as_str()) {
                continue;
            }
            assert_ne!(s(r, "confidence"), "degraded", "{name} degraded");
            assert!(!s(r, "content_fp").is_empty(), "{name} has no fingerprint");
        }
    }
}
