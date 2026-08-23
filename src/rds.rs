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
const SPECIALSXP: u8 = 7;
const BUILTINSXP: u8 = 8;
const WEAKREFSXP: u8 = 23;
/// The bit in a string's levels field that says it is Latin-1.
const LATIN1_MASK: i32 = 1 << 2;
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

/// How many columns get a per-column record in the emitted schema.
///
/// The schema is one JSON object per column, so its size tracks the width of the
/// object and nothing else. A document-term matrix runs to six figures of
/// columns, which is megabytes of description for something no reader scrolls
/// through and no page renders: the consumer draws a few hundred rows at most.
/// The true width is reported as ncol regardless, so capping the detail loses the
/// tail rather than the fact.
const COLUMN_DETAIL_CAP: usize = 512;
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
enum Val {
    Nil,
    Sym(String),
    Char(Option<String>),
    Str(Vec<Option<String>>),
    Ints { len: usize, vals: Vec<i32>, logical: bool },
    Reals { len: usize, vals: Vec<f64> },
    /// Values whose bytes are not kept: complex and raw, which are read for
    /// their length rather than their contents, and a generated sequence too
    /// long to hold. `of` is what they are, which is otherwise lost.
    Blob { len: usize, of: &'static str, digest: u64 },
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
    /// Hash a span without keeping it. Complex and raw values are read for
    /// their size rather than their contents, but a dataset with no
    /// fingerprint is dropped before it reaches anywhere, so what cannot be
    /// held can at least be identified.
    fn digest_span(&mut self, n: usize) -> Result<u64, String> {
        self.need(n)?;
        Ok(fnv(&self.b[self.p..self.p + n]))
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
        // The levels field carries a string's encoding.
        let levs = f >> 12;
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
                // A length of -1 is the missing string.
                let n = self.i32()?;
                let val = if n < 0 {
                    Val::Char(None)
                } else {
                    let n = n as usize;
                    self.need(n)?;
                    let raw = &self.b[self.p..self.p + n];
                    // Latin-1 is not UTF-8, and reading it as though it were
                    // replaces every accented character with a marker: the
                    // text is wrong, and so is the fingerprint taken over it.
                    let s = if levs & LATIN1_MASK != 0 {
                        raw.iter().map(|b| *b as char).collect::<String>()
                    } else {
                        String::from_utf8_lossy(raw).into_owned()
                    };
                    self.p += n;
                    Val::Char(Some(s))
                };
                // A string carries no attributes of its own, but an older file
                // can still have written one, and it has to be consumed or
                // everything after it is read at the wrong offset.
                if ha {
                    self.item()?;
                }
                Ok(Node { val, attr: None })
            }
            // A builtin or special names the function rather than describing
            // it. Saving one is unusual, but failing on it costs the file it
            // sits in rather than the object.
            SPECIALSXP | BUILTINSXP => {
                let n = self.i32()?;
                if n < 0 {
                    return Err("negative builtin name length".into());
                }
                self.skip(n as usize)?;
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Nil, attr })
            }
            // A weak reference carries nothing, but it does take a slot in the
            // reference table, and a slot missed there shifts every reference
            // after it onto the wrong object.
            WEAKREFSXP => {
                self.refs.push(Node { val: Val::Nil, attr: None });
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Nil, attr })
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
                let digest = self.digest_span(16 * n)?;
                self.skip(16 * n)?;
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Blob { len: n, of: "complex", digest }, attr })
            }
            RAWSXP => {
                let n = self.vlen()?;
                let digest = self.digest_span(n)?;
                self.skip(n)?;
                let attr = self.maybe_attr(ha)?;
                Ok(Node { val: Val::Blob { len: n, of: "raw", digest }, attr })
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
    // The Rmetrics series keeps its index and its zone under names of its own,
    // and both are read now, so neither is an attribute nobody claimed.
    "positions", "FinCenter",
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

/// How many leftover attributes one object reports. The list is a description
/// rather than a payload, so a pathological object cannot turn it into one.
const ATTRS_OTHER_CAP: usize = 24;

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
    // With what each one is and how much of it there is, not only its name.
    // Most of what a class-specific reader does not extract is a plain
    // attribute sitting on the object: a Surv's censoring type, a tsibble's
    // index and key, a dist's Size. Naming them said they existed; saying what
    // kind of thing each is and how long it is turns "not read" into "read as
    // an unstructured attribute", and it degrades to something rather than
    // nothing for every class with no reader at all.
    let mut other: Vec<(String, &Node)> = pairs
        .iter()
        .filter(|(k, _)| {
            !CONSUMED_ATTRS.contains(&k.as_str()) && !extra.contains(&k.as_str())
                || (matches!(k.as_str(), "label" | "comment" | "units") && out.get(k).is_none())
        })
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    other.sort_by(|a, b| a.0.cmp(&b.0));
    other.dedup_by(|a, b| a.0 == b.0);
    if !other.is_empty() {
        other.truncate(ATTRS_OTHER_CAP);
        out["attrs_other"] = json!(other
            .iter()
            .map(|(k, v)| {
                let mut a = json!({ "name": k, "kind": node_kind_name(v), "len": col_len(v) });
                if let Some(p) = attr_preview(v) {
                    a["values"] = p;
                } else if let Some((lo, hi)) = attr_range(v) {
                    // Too many to list, but a number has ends. A timeSeries
                    // keeps its timestamps in an attribute nothing else reads,
                    // and its two ends are the span of the series.
                    a["min"] = json!(lo);
                    a["max"] = json!(hi);
                }
                // What it could have been set to. An unset setting is only
                // legible next to the choices it was picking between.
                let vp = attr_pairs(v);
                if let Some(lv) = attr(&vp, "levels") {
                    let lv = str_vec(lv);
                    if !lv.is_empty() && lv.len() <= 12 {
                        a["levels"] = json!(lv);
                    }
                }
                a
            })
            .collect::<Vec<_>>());
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
    // Geometries that are present but hold nothing. They count as rows and
    // draw as nothing, so a map with a hundred of them is not the map its row
    // count suggests.
    if let Some(Val::Ints { vals, .. }) = attr(&pairs, "n_empty").map(|n| &n.val) {
        if let Some(v) = vals.first() {
            o["n_empty"] = json!(*v as i64);
        }
    }
    if let Val::Vec(items) = &col.val {
        o["n_geometries"] = json!(items.len());
        // Each geometry names its own dimension before its type: XY, or XYZ
        // where there is a height, or XYM where there is a measure. sf reports
        // it beside the geometry type, and it is not in the column's class.
        if let Some(first) = items.first() {
            let gc = attr(&attr_pairs(first), "class").map(str_vec).unwrap_or_default();
            if let Some(d) = gc.iter().find(|c| matches!(c.as_str(), "XY" | "XYZ" | "XYM" | "XYZM")) {
                o["geom_dimension"] = json!(d);
            }
        }
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
    // Which attribute holds the index depends on who wrote the class. zoo and
    // xts use `index`; the Rmetrics series keeps the same thing under
    // `positions`, and reading only the first name left a timeSeries with its
    // timestamps sitting in the unclaimed-attribute list while the object
    // reported no span at all.
    let is_rmetrics = attr(pairs, "FinCenter").is_some() || attr(pairs, "positions").is_some();
    if let Some(Val::Reals { vals, .. }) = attr(pairs, "tsp").map(|n| &n.val) {
        if vals.len() >= 3 {
            rec["ts_start"] = json!(vals[0]);
            rec["ts_end"] = json!(vals[1]);
            rec["ts_frequency"] = json!(vals[2]);
            // How much time the series covers, in the unit its start and end
            // are given in. Two series can share a start and a frequency and
            // cover a century apart.
            rec["ts_span"] = json!(round_stat(vals[1] - vals[0]));
        }
    }
    if let Some(idx) = attr(pairs, "index").or_else(|| attr(pairs, "positions")) {
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
            // In the same unit as the index: days for a Date, seconds for a
            // POSIXct, which is what the class below says.
            rec["index_span"] = json!(round_stat(hi - lo));
            // How far apart the observations are, and whether they are evenly
            // so. A daily series with a fortnight missing from the middle and
            // one observed daily throughout have the same start, end and
            // count, and are not the same data. The usual step is the one that
            // occurs most often; anything longer is a gap.
            let steps: Vec<f64> = match &idx.val {
                Val::Reals { vals, .. } => vals.windows(2).map(|w| w[1] - w[0]).collect(),
                Val::Ints { vals, .. } => {
                    vals.windows(2).map(|w| w[1] as f64 - w[0] as f64).collect()
                }
                _ => Vec::new(),
            };
            if !steps.is_empty() {
                let mut counts: std::collections::HashMap<u64, usize> = Default::default();
                for st in &steps {
                    *counts.entry(st.to_bits()).or_default() += 1;
                }
                if let Some((bits, hits)) = counts.iter().max_by_key(|(_, c)| **c) {
                    let modal = f64::from_bits(*bits);
                    rec["index_delta"] = json!(round_stat(modal));
                    rec["index_regular"] = json!(*hits == steps.len());
                    let gaps = steps.iter().filter(|st| **st > modal).count();
                    if gaps > 0 {
                        rec["index_n_gaps"] = json!(gaps);
                        let biggest = steps.iter().cloned().fold(f64::MIN, f64::max);
                        rec["index_max_gap"] = json!(round_stat(biggest));
                    }
                }
                // An index that goes backwards, or stands still, is not the
                // ordered thing a series is taken to be.
                if steps.iter().any(|st| *st < 0.0) {
                    rec["index_sorted"] = json!(false);
                } else if steps.iter().any(|st| *st == 0.0) {
                    rec["index_has_duplicates"] = json!(true);
                }
            }
            // The index's own class is what makes those numbers meaningful.
            // xts keeps it under tclass rather than class, so reading only the
            // one left an xts series with an index and no way to read it.
            let icls = attr_pairs(idx);
            let c = attr(&icls, "class")
                .map(str_vec)
                .filter(|v| !v.is_empty())
                .or_else(|| attr(&icls, "tclass").map(str_vec))
                .unwrap_or_default();
            if !c.is_empty() {
                rec["index_class"] = json!(c.join("/"));
            } else if is_rmetrics {
                // Seconds since the epoch, with the zone kept beside them
                // under a name of its own.
                rec["index_class"] = json!("POSIXct");
                if let Some(fc) = attr(pairs, "FinCenter") {
                    let z = str_vec(fc);
                    if let Some(first) = z.first() {
                        if !first.is_empty() && rec.get("index_tz").is_none() {
                            rec["index_tz"] = json!(first);
                        }
                    }
                }
            }
            if let Some(tz) = attr_text(&icls, "tzone", 64) {
                if !tz.is_empty() {
                    rec["index_tz"] = json!(tz);
                }
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
        return Some(Val::Blob { len: n, of: if want_int { "integer" } else { "numeric" }, digest: 0 });
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
        // Row names are integer or character in anything written this century,
        // but older files number their rows with doubles, and a frame whose
        // rows cannot be counted is reported as having none.
        Val::Reals { len, .. } => Some(*len as i64),
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
        Val::Blob { of, .. } => of,
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
        // The whole value at once: its elements were never held, so there is
        // one thing to say about it and it is the same for every position.
        Val::Blob { len, digest, .. } => {
            buf.push(1);
            buf.extend_from_slice(&(*len as u64).to_be_bytes());
            buf.extend_from_slice(&digest.to_be_bytes());
            let _ = i;
            // Present, not missing. The bytes of a complex or raw value are
            // not kept, which is a limit on what can be said about it and not
            // a claim that nobody wrote one down: a column of three complex
            // numbers was reporting three missing values and no distinct ones,
            // and its absences were counted into the dataset's missing total.
            false
        }
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
        // A complex or raw value has a length even though its elements were
        // read for size rather than kept. Reporting zero made it look empty,
        // and an empty column produces no fingerprint, which drops the dataset
        // before it reaches anywhere.
        Val::Blob { len, .. } => *len,
        // An S4 object states its shape in a Dim slot, so a list of matrices
        // is not a list of nothing.
        Val::S4 => {
            let p = attr_pairs(col);
            match attr(&p, "Dim").map(|n| &n.val) {
                Some(Val::Ints { vals, .. }) if vals.len() >= 2 => {
                    (vals[0].max(0) as usize).saturating_mul(vals[1].max(0) as usize)
                }
                _ => 0,
            }
        }
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
        Val::Blob { len, digest, .. } => fnv(&[(*len as u64).to_be_bytes(), digest.to_be_bytes()].concat()),
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
/// Sorting a column to find its middle is the one part of this that grows with
/// the data, so a column past this size reports its mean and its range and
/// leaves the quantiles alone rather than holding a second copy of itself.
const QUANTILE_CAP: usize = 5_000_000;
/// A factor can have thousands of levels. The common ones are the description.
// Raised from twelve once the counts stopped being drawn as bars: a number per
// level costs a line and a bar costs a row, so the same space holds far more of
// them, and a factor's levels are the one part of it worth seeing whole.
const LEVEL_COUNT_CAP: usize = 50;
/// Past this many distinct values a commonest one describes nothing.
const MODE_CARDINALITY_CAP: usize = 20;
/// And it has to account for enough of the column to be worth naming.
const MODE_MIN_SHARE: f64 = 0.2;
/// Layers are named and few. Past this they behave like the columns of a wide
/// matrix, and listing them describes the storage rather than the data.
const MARGIN_NAME_CAP: usize = 20;

/// The type 7 quantile, which is what quantile() gives by default.
fn quantile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let h = (sorted.len() as f64 - 1.0) * p;
    let lo = h.floor() as usize;
    let hi = h.ceil() as usize;
    sorted[lo] + (h - lo as f64) * (sorted[hi.min(sorted.len() - 1)] - sorted[lo])
}

/// Whether the variation in a grid runs along its rows or along its columns.
///
/// A summary per row would be one set of figures for every row, which on a
/// large matrix is more numbers than the matrix holds. Summarising the margins
/// instead costs the same fourteen figures whatever the size: the mean of each
/// row reduced to a six number summary, and the same for the columns. A grid
/// whose row means spread widely while its column means barely move is saying
/// where its structure is, which a summary over every cell as one vector
/// cannot say at all.
///
/// Column-major storage, so cell (i, j) sits at j * nrow + i. Missing values
/// are left out of the mean they fall in rather than poisoning it, and a
/// margin with nothing in it contributes no mean rather than a NaN.
fn margin_summaries(node: &Node, nrow: usize, ncol: usize, rec: &mut Value) {
    if nrow < 2 || ncol < 2 {
        return;
    }
    // Reading a value out of the flat vector, with R's two spellings of
    // missing folded into one.
    let at: Box<dyn Fn(usize) -> Option<f64>> = match &node.val {
        Val::Ints { vals, logical, .. } if !*logical => {
            let v = vals.clone();
            Box::new(move |k| v.get(k).copied().filter(|x| *x != NA_INT).map(|x| x as f64))
        }
        Val::Reals { vals, .. } => {
            let v = vals.clone();
            Box::new(move |k| v.get(k).copied().filter(|x| !x.is_nan()))
        }
        _ => return,
    };
    let len = match &node.val {
        Val::Ints { len, .. } | Val::Reals { len, .. } => *len,
        _ => return,
    };
    if len != nrow.saturating_mul(ncol) {
        return;
    }

    let mut row_sum = vec![0f64; nrow];
    let mut row_n = vec![0u32; nrow];
    let mut col_sum = vec![0f64; ncol];
    let mut col_n = vec![0u32; ncol];
    for j in 0..ncol {
        for i in 0..nrow {
            if let Some(x) = at(j * nrow + i) {
                row_sum[i] += x;
                row_n[i] += 1;
                col_sum[j] += x;
                col_n[j] += 1;
            }
        }
    }
    let means = |sum: &[f64], n: &[u32]| -> Vec<f64> {
        sum.iter()
            .zip(n)
            .filter(|(_, c)| **c > 0)
            .map(|(s, c)| s / *c as f64)
            .collect()
    };
    emit_margin(rec, "row", means(&row_sum, &row_n));
    emit_margin(rec, "col", means(&col_sum, &col_n));
}

/// One margin's means, as the six numbers summary() prints plus a spread.
fn emit_margin(rec: &mut Value, side: &str, mut m: Vec<f64>) {
    if m.len() < 2 {
        return;
    }
    let n = m.len() as f64;
    let mean = m.iter().sum::<f64>() / n;
    // Sample variance, matching sd() rather than the population form.
    let var = m.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    m.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    for (suffix, v) in [
        ("min", m[0]),
        ("q1", quantile(&m, 0.25)),
        ("median", quantile(&m, 0.5)),
        ("mean", mean),
        ("q3", quantile(&m, 0.75)),
        ("max", m[m.len() - 1]),
        ("sd", var.sqrt()),
    ] {
        if v.is_finite() {
            rec[format!("{side}_mean_{suffix}")] = json!(round_stat(v));
        }
    }
}

/// Summarise the values of something that is not a table.
///
/// A grid has no columns to hang a summary on, so the numbers describe the
/// whole of it at once, the way summary() of a matrix's values does rather
/// than its per-column form: one set of figures for a dataset rather than one
/// per column, which for a wide matrix would be thousands.
///
/// `over` says what was summarised, because it is not always every cell. A
/// sparse matrix holds only its non-zeros, and a mean taken over those is a
/// different number from a mean over the grid they sit in.
fn lift_value_summary(rec: &mut Value, values: &Node, over: &'static str) {
    let name = "values".to_string();
    let Some(p) = profile_columns(&[values], std::slice::from_ref(&name)) else { return };
    let Some(c) = p.columns.first() else { return };
    let mut any = false;
    for k in [
        "type", "mean", "median", "q1", "q3", "col_min", "col_max",
        "n_true", "n_false", "min_nchar", "max_nchar", "n_blank", "n_infinite",
        "max_infinite", "min_infinite", "n_infinite_pos", "n_infinite_neg", "n_nan",
        "n_zero", "p_zero",
        "sd", "sort_order", "is_integer_valued", "n_missing_leading", "n_missing_trailing", "max_missing_run",
        "n_outliers", "n_outliers_low", "n_outliers_high", "mode_value", "mode_share", "skewness", "kurtosis",
    ] {
        if let Some(v) = c.get(k) {
            if !v.is_null() {
                rec[k] = v.clone();
                any = true;
            }
        }
    }
    if let Some(v) = c.get("n_unique") {
        rec["n_unique"] = v.clone();
    }
    if rec.get("n_missing_total").is_none() {
        rec["n_missing_total"] = json!(p.n_missing_total);
    }
    if any {
        rec["summary_over"] = json!(over);
    }
}

/// The labels along each margin of a grid.
///
/// A table of counts is unreadable without them: a two by three matrix of
/// numbers says nothing, and the same matrix labelled by treatment and outcome
/// says what the study found. For a multivariate series the second margin is
/// the series names.
fn margin_fields(pairs: &[(String, &Node)], rec: &mut Value) {
    let Some(dn) = attr(pairs, "dimnames") else { return };
    let Val::Vec(margins) = &dn.val else { return };
    let mnames = attr(&attr_pairs(dn), "names").map(str_vec).unwrap_or_default();
    let mut out = Vec::new();
    for (i, m) in margins.iter().enumerate() {
        let labels = str_vec(m);
        if labels.is_empty() {
            continue;
        }
        let mut e = json!({ "margin": i + 1, "n_labels": labels.len() });
        if let Some(nm) = mnames.get(i) {
            if !nm.is_empty() {
                e["name"] = json!(nm);
            }
        }
        if labels.len() <= MARGIN_LABEL_CAP {
            e["labels"] = json!(labels);
        } else {
            e["labels"] = json!(labels.iter().take(MARGIN_LABEL_CAP).collect::<Vec<_>>());
            e["labels_truncated"] = json!(true);
        }
        out.push(e);
    }
    if !out.is_empty() {
        rec["dimnames"] = json!(out);
    }
}

const MARGIN_LABEL_CAP: usize = 50;

/// The unit a value is measured in, written the way it is read.
///
/// The units package keeps a numerator and a denominator rather than a string,
/// so a column of speeds says it holds metres and seconds and leaves the rest
/// to whoever reads it. A number without its unit is not a measurement.
fn unit_label(n: &Node) -> Option<String> {
    // A list of two named slots rather than two attributes, so the names sit
    // beside the values rather than above them.
    let p = attr_pairs(n);
    let names = attr(&p, "names").map(str_vec).unwrap_or_default();
    let Val::Vec(items) = &n.val else { return None };
    let slot = |want: &str| -> Vec<String> {
        names
            .iter()
            .position(|x| x == want)
            .and_then(|i| items.get(i))
            .map(str_vec)
            .unwrap_or_default()
    };
    let num = slot("numerator");
    let den = slot("denominator");
    if num.is_empty() && den.is_empty() {
        return None;
    }
    let top = if num.is_empty() { "1".to_string() } else { num.join("*") };
    Some(if den.is_empty() { top } else { format!("{top}/{}", den.join("*")) })
}

/// The parts of a value that are written down beside it rather than computed:
/// what a factor's levels are and in what order, which zone a moment is in,
/// and what a number is a number of.
fn declared_fields(pairs: &[(String, &Node)], class: &[String], out: &mut Value) {
    if class.iter().any(|c| c == "factor") {
        if let Some(l) = attr(pairs, "levels") {
            let lv = str_vec(l);
            if !lv.is_empty() {
                // In the order the factor declares, which is the order it
                // sorts and plots in, and is not the order of the counts.
                out["levels"] = json!(lv.iter().take(LEVEL_LIST_CAP).collect::<Vec<_>>());
                if lv.len() > LEVEL_LIST_CAP {
                    out["levels_truncated"] = json!(true);
                }
            }
        }
        if class.iter().any(|c| c == "ordered") {
            out["is_ordered"] = json!(true);
        }
    }
    if let Some(tz) = attr_text(pairs, "tzone", 64) {
        if !tz.is_empty() {
            out["tz"] = json!(tz);
        }
    }
    if let Some(u) = attr(pairs, "units").and_then(unit_label) {
        out["units"] = json!(u);
    }
}

const LEVEL_LIST_CAP: usize = 50;

/// How many slots of a list are named or described before the description
/// becomes longer than the thing it describes.
const LIST_ELEM_CAP: usize = 50;
const LIST_DEPTH_CAP: u32 = 12;

/// The class of a node as a reader would name it, for describing a slot.
/// The two ends of a numeric attribute too long to list.
///
/// Listing values stops at a handful, which leaves a long one described by its
/// length alone. A number has ends whatever its length, and for the attributes
/// that carry a sequence those ends are the whole of what a reader wants: the
/// positions slot of a timeSeries is its timestamps, and its ends are the span
/// of the series.
fn attr_range(n: &Node) -> Option<(f64, f64)> {
    let vals: Vec<f64> = match &n.val {
        Val::Ints { vals, logical: false, .. } => {
            vals.iter().filter(|v| **v != NA_INT).map(|v| *v as f64).collect()
        }
        Val::Reals { vals, .. } => vals.iter().copied().filter(|v| v.is_finite()).collect(),
        _ => return None,
    };
    if vals.len() < 2 {
        return None;
    }
    let lo = vals.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if lo.is_finite() && hi.is_finite() && lo != hi {
        Some((round_stat(lo), round_stat(hi)))
    } else {
        None
    }
}

/// What a short attribute actually holds.
///
/// Naming an attribute and giving its length says an sf frame has an
/// `sf_column` of one character, which is true and is not the answer: the
/// answer is "geometry". Attributes that carry a setting rather than data are
/// almost always this short, and reading them is the difference between
/// knowing a setting exists and knowing what it is set to.
///
/// Bounded hard, because this runs on every attribute of every object: a few
/// values, each short, and nothing at all for anything longer. A factor is
/// rendered through its own levels, since its codes mean nothing without them.
fn attr_preview(n: &Node) -> Option<Value> {
    const MAX_VALS: usize = 6;
    const MAX_CHARS: usize = 32;
    let pairs = attr_pairs(n);
    let levels = attr(&pairs, "levels").map(str_vec);
    let clip = |t: &str| -> String {
        if t.chars().count() > MAX_CHARS {
            t.chars().take(MAX_CHARS).collect::<String>() + "..."
        } else {
            t.to_string()
        }
    };
    let out: Vec<String> = match &n.val {
        Val::Str(v) if v.len() <= MAX_VALS => {
            v.iter().map(|o| o.as_deref().map(clip).unwrap_or_else(|| "NA".into())).collect()
        }
        Val::Ints { vals, logical, .. } if vals.len() <= MAX_VALS => vals
            .iter()
            .map(|i| {
                if *i == NA_INT {
                    "NA".to_string()
                } else if *logical {
                    if *i == 0 { "FALSE".into() } else { "TRUE".into() }
                } else if let Some(lv) = &levels {
                    // A factor's codes are indices into its levels, and the
                    // codes on their own say nothing.
                    lv.get((*i - 1).max(0) as usize).cloned().unwrap_or_else(|| i.to_string())
                } else {
                    i.to_string()
                }
            })
            .collect(),
        Val::Reals { vals, .. } if vals.len() <= MAX_VALS => vals
            .iter()
            .map(|f| if f.is_nan() { "NA".to_string() } else { round_stat(*f).to_string() })
            .collect(),
        _ => return None,
    };
    if out.is_empty() {
        return None;
    }
    // Paired with its names where it has them. An sf's agr attribute is three
    // values that are all NA, which is accurate and says nothing; the same
    // three read NAME=NA, AREA=NA, BIR74=NA and the reader can see it is a
    // setting per column that nobody set.
    let names = attr(&pairs, "names").map(str_vec).unwrap_or_default();
    if names.len() == out.len() {
        return Some(json!(names
            .iter()
            .zip(&out)
            .map(|(n, v)| format!("{}={}", clip(n), v))
            .collect::<Vec<_>>()));
    }
    Some(json!(out))
}

fn node_kind_name(n: &Node) -> String {
    let pairs = attr_pairs(n);
    let cls = attr(&pairs, "class").map(str_vec).unwrap_or_default();
    if !cls.is_empty() {
        return cls.join("/");
    }
    match &n.val {
        Val::Ints { logical: true, .. } => "logical".into(),
        Val::Ints { .. } => "integer".into(),
        Val::Reals { .. } => "numeric".into(),
        Val::Str(_) => "character".into(),
        Val::Vec(_) => "list".into(),
        Val::Blob { of, .. } => (*of).into(),
        Val::Nil => "NULL".into(),
        Val::S4 => "S4".into(),
        _ => "object".into(),
    }
}

/// How deep the nesting goes, so a flat list of vectors is distinguishable
/// from a configuration tree.
fn list_depth(n: &Node, at: u32) -> u32 {
    if at >= LIST_DEPTH_CAP {
        return at;
    }
    match &n.val {
        Val::Vec(items) => items
            .iter()
            .map(|c| list_depth(c, at + 1))
            .max()
            .unwrap_or(at + 1),
        _ => at,
    }
}

/// Describe the slots of a list, whether it stands alone as a dataset or sits
/// inside a frame as a column.
///
/// A list was a black box: a length and nothing else, so a train and test
/// split, a set of fitted models and a tree of settings were the same record.
/// Inside a frame it was worse than uninformative. A nested tibble reports the
/// number of groups as its row count, so a table of two thousand observations
/// was catalogued as twenty rows, with no field anywhere saying otherwise and
/// no trace of the inner columns, which are the whole reason for the nesting.
/// How far the element profiler will descend. Each level profiles the level
/// below it, so this is the one thing standing between a nested object and an
/// account of every leaf it has.
const ELEM_PROFILE_DEPTH: u32 = 2;

thread_local! {
    static ELEM_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn describe_list(items: &[Node], names: &[String], out: &mut Value) {
    if items.is_empty() {
        return;
    }
    if !names.is_empty() {
        out["element_names"] = json!(names.iter().take(LIST_ELEM_CAP).collect::<Vec<_>>());
    }
    let kinds: Vec<String> = items.iter().take(LIST_ELEM_CAP).map(node_kind_name).collect();
    // One kind repeated says the slots are alike, which is most of what a
    // reader wants; the full list matters only when they are not.
    let mut uniq: Vec<&String> = Vec::new();
    for k in &kinds {
        if !uniq.contains(&k) {
            uniq.push(k);
        }
    }
    if uniq.len() == 1 {
        out["element_class"] = json!(uniq[0]);
    } else {
        out["element_classes"] = json!(kinds);
    }
    let lens: Vec<i64> = items.iter().map(|i| col_len(i) as i64).collect();
    if let (Some(lo), Some(hi)) = (lens.iter().min(), lens.iter().max()) {
        out["element_len_min"] = json!(lo);
        out["element_len_max"] = json!(hi);
        out["element_len_total"] = json!(lens.iter().sum::<i64>());
    }
    // What each slot holds, not only the range across all of them. A list of
    // cross-validation folds could say two elements, lengths two to two, and
    // leave a reader unable to ask how big the first fold was. The lengths are
    // already computed above; this is keeping them rather than reducing them.
    if !lens.is_empty() {
        out["element_lens"] = json!(lens.iter().take(LIST_ELEM_CAP).collect::<Vec<_>>());
    }
    let d = items.iter().map(|i| list_depth(i, 1)).max().unwrap_or(1);
    if d > 1 {
        out["max_depth"] = json!(d);
    }
    // A slot holding a table is the case that matters most: the rows inside
    // are the observations the dataset actually has.
    // Slots holding nothing at all. They count towards the length and draw as
    // nothing, so a list of ten with four empty is not the list its length
    // suggests.
    let n_empty = items
        .iter()
        .filter(|i| matches!(i.val, Val::Nil) || col_len(i) == 0)
        .count();
    if n_empty > 0 {
        out["n_empty_slots"] = json!(n_empty);
    }
    let mut inner_rows = 0i64;
    let mut inner_schema: Option<Vec<String>> = None;
    let mut schema_same = true;
    let mut n_frames = 0usize;
    // A table can sit at any depth. Cross-validation folds keep theirs one
    // level down, and counting only direct slots reported a dataset of forty
    // rows as holding none at all.
    fn walk_frames(
        items: &[Node],
        depth: u32,
        rows: &mut i64,
        n: &mut usize,
        schema: &mut Option<Vec<String>>,
        same: &mut bool,
    ) {
        if depth > LIST_DEPTH_CAP {
            return;
        }
        for it in items {
            let ip = attr_pairs(it);
            let icls = attr(&ip, "class").map(str_vec).unwrap_or_default();
            if icls.iter().any(|c| c == "data.frame") {
                *n += 1;
                *rows += nrow_of(attr(&ip, "row.names")).unwrap_or(0);
                let nm = attr(&ip, "names").map(str_vec).unwrap_or_default();
                match schema {
                    None => *schema = Some(nm),
                    Some(prev) => {
                        if *prev != nm {
                            *same = false;
                        }
                    }
                }
            } else if let Val::Vec(inner) = &it.val {
                walk_frames(inner, depth + 1, rows, n, schema, same);
            }
        }
    }
    walk_frames(items, 1, &mut inner_rows, &mut n_frames, &mut inner_schema, &mut schema_same);
    // The same count per slot rather than only summed over the list. Without
    // it a list can report two hundred nested rows and give no way to ask
    // which element they came from, which is the question anyone looking at a
    // list of folds or a list of per-group tables actually has. Reported only
    // when some slot holds a table, so an ordinary list gains nothing.
    let per_inner: Vec<i64> = items
        .iter()
        .take(LIST_ELEM_CAP)
        .map(|it| {
            let (mut rows, mut n) = (0i64, 0usize);
            let (mut schema, mut same) = (None, true);
            walk_frames(
                std::slice::from_ref(it),
                1,
                &mut rows,
                &mut n,
                &mut schema,
                &mut same,
            );
            rows
        })
        .collect();
    if per_inner.iter().any(|&r| r > 0) {
        out["element_inner_nrow"] = json!(per_inner);
    }
    // A list's elements are columns of a kind: one named slot each, holding
    // values. Profiling them with the reader that profiles a frame's columns
    // gives an element the same figures a column carries, in the same shape,
    // so a slot holding numbers can say what is in it rather than only how
    // many of them there are. Capped with the rest of the per-element work,
    // because a list of five hundred slots is a different kind of object and
    // the first fifty describe it.
    let all: Vec<&Node> = items.iter().take(LIST_ELEM_CAP).collect();
    let all_names: Vec<String> = if names.is_empty() {
        // R's own way of pointing at an unnamed slot.
        (1..=all.len()).map(|i| format!("[[{i}]]")).collect()
    } else {
        names.iter().take(LIST_ELEM_CAP).cloned().collect()
    };
    // Only the slots that can be read, carrying their own names. The profiler
    // refuses a set it cannot read whole, so one slot holding NULL cost the
    // profile of every other slot beside it: a list of a NULL, three integers,
    // another NULL and a string described none of the four. A slot that cannot
    // be read is still named and classed by the fields above, so leaving it
    // out of this pass loses nothing and keeps the rest.
    let mut refs: Vec<&Node> = Vec::new();
    let mut elem_names: Vec<String> = Vec::new();
    if all.len() == all_names.len() {
        for (nd, nm) in all.iter().zip(&all_names) {
            let readable = matches!(nd.val, Val::Ints { .. } | Val::Reals { .. } | Val::Str(_) | Val::Vec(_))
                || matches!(nd.val, Val::Blob { of, .. } if of == "raw" || of == "complex");
            if readable {
                refs.push(nd);
                elem_names.push(nm.clone());
            }
        }
    }
    // Bounded, because profiling an element profiles its elements in turn. One
    // sf inside a list ran down into all hundred of its polygons and took
    // fifty kilobytes to say so. Two levels answers what a reader asks of a
    // nested object, which is what is in it and what is in that.
    if !refs.is_empty() && refs.len() == elem_names.len() {
        let depth = ELEM_DEPTH.with(|d| d.get());
        if depth < ELEM_PROFILE_DEPTH {
            ELEM_DEPTH.with(|d| d.set(depth + 1));
            let profiled = profile_columns(&refs, &elem_names);
            ELEM_DEPTH.with(|d| d.set(depth));
            if let Some(pr) = profiled {
                out["elements"] = json!(pr.columns);
            }
        }
    }
    if n_frames > 0 {
        out["inner_nrow_total"] = json!(inner_rows);
        if let Some(nm) = inner_schema {
            out["inner_ncol"] = json!(nm.len());
            if schema_same {
                out["inner_names"] = json!(nm.iter().take(LIST_ELEM_CAP).collect::<Vec<_>>());
            } else {
                // The slots disagree, so there is no one schema to report and
                // saying the first one would be a guess.
                out["inner_schema_varies"] = json!(true);
            }
        }
    }
}

/// Which kind of table this is, named rather than left in a class chain.
///
/// A tibble, a data.table and a plain frame all inherit from data.frame, and
/// the difference is only visible to something willing to read the whole
/// inheritance chain and know the precedence. Anything matching on the class
/// gets it wrong in one direction or the other: an exact match on "data.frame"
/// counts no tibbles at all, and a substring match counts everything.
///
/// Spatialness is deliberately not part of this. An sf object is a frame of
/// some flavour that also has geometry, and is_spatial already says so.
fn frame_flavour(class: &[String]) -> &'static str {
    let has = |n: &str| class.iter().any(|c| c == n);
    if has("data.table") {
        "data.table"
    } else if has("tbl_df") || has("tbl") {
        "tibble"
    } else {
        "data.frame"
    }
}

/// Bring a geometry column's description up onto the dataset that holds it.
///
/// sf prints the geometry type, the dimension, the bounding box and the
/// projection above the table, because that is what identifies the data. They
/// were reachable only by opening the column list, which is not where a reader
/// of a catalogue would look.
fn lift_spatial(rec: &mut Value) {
    let Some(cols) = rec.get("columns").and_then(|c| c.as_array()) else { return };
    let Some(geo) = cols.iter().find(|c| c.get("is_geometry").and_then(|v| v.as_bool()).unwrap_or(false))
    else {
        return;
    };
    let geo = geo.clone();
    for k in ["geom_type", "geom_dimension", "n_geometries", "bbox", "crs_input", "crs_epsg", "crs_wkt"] {
        if let Some(v) = geo.get(k) {
            if !v.is_null() {
                rec[k] = v.clone();
            }
        }
    }
    rec["is_spatial"] = json!(true);
}

/// Whether a missing double is R's NA or a NaN the arithmetic produced.
///
/// Both are not-a-number and `is.na()` is true for either, which is why they
/// collapse into one count everywhere they are treated as missing. They do not
/// mean the same thing: NA is a value nobody recorded, and NaN is a value a
/// calculation could not produce, so a column full of the second is a division
/// that went wrong rather than a survey nobody answered.
///
/// R tells them apart the way this does, by the payload: NA_real_ is a NaN
/// carrying 1954 in its low word, set in R_ValueOfNA, and every other NaN is
/// one the data actually contains.
fn is_r_na(v: f64) -> bool {
    v.is_nan() && (v.to_bits() & 0xFFFF_FFFF) as u32 == 1954
}

/// Enough digits to be faithful without turning a summary into noise.
///
/// Six decimal places or six significant digits, whichever keeps more.
///
/// Places alone annihilate anything small: a column of picosecond
/// measurements came back with a mean, a median and a standard deviation of
/// zero, sitting between a minimum and a maximum that were not zero, because
/// those two do not come through here. A summary that contradicts its own
/// bounds is worse than one carrying a few digits too many.
///
/// Significant digits alone would cost precision at the other end, where a
/// kurtosis of 84.930144 has six places worth keeping and only four digits
/// after the leading two. Taking the finer of the two scales gives up neither.
fn round_stat(v: f64) -> f64 {
    if !v.is_finite() || v == 0.0 {
        return v;
    }
    let mag = v.abs().log10().floor();
    // Beyond what a scale factor can survive, the value is already as precise
    // as it is going to get.
    if !(-290.0..=290.0).contains(&mag) {
        return v;
    }
    let scale = 1e6f64.max(10f64.powf(5.0 - mag));
    (v * scale).round() / scale
}

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
    // A complex or raw value is the third case: its elements were not kept,
    // but the whole of it was hashed on the way past, so it can be identified
    // even though it cannot be summarised. A generated sequence too long to
    // hold cannot: nothing was hashed, and a fingerprint over its length alone
    // would make two different sequences of the same length one dataset.
    let any_unread = cols.iter().any(|c| {
        !matches!(c.val, Val::Ints { .. } | Val::Reals { .. } | Val::Str(_) | Val::Vec(_))
            && !matches!(c.val, Val::Blob { of, .. } if of == "raw" || of == "complex")
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
        // A column whose bytes are not kept. Every cell of one hashes to the
        // same thing, because the digest is of the whole vector rather than of
        // the cell, so a distinct count off those hashes would say one however
        // many different values are in there.
        let is_opaque = matches!(col.val, Val::Blob { .. });
        // Whether the levels are ranked, which decides which of them are worth
        // keeping when there are more than the cap allows.
        let is_ordered = is_factor
            && attr(&pairs, "class")
                .map(str_vec)
                .unwrap_or_default()
                .iter()
                .any(|c| c == "ordered");
        let is_int = matches!(col.val, Val::Ints { .. }) && !is_factor;
        let is_logical = ty == "logical";

        let mut hasher = blake3::Hasher::new();
        hasher.update(ty.as_bytes());
        let mut n_missing = 0u64;
        let mut uniq: HashSet<u64> = HashSet::new();
        let mut ch = Vec::with_capacity(n);
        let mut min: Option<f64> = None;
        let mut max: Option<f64> = None;
        let mut buf = Vec::with_capacity(16);
        // What summary() would say about the column. A range on its own says
        // where the values stop, not where they sit: a column running to a
        // single large outlier and one spread evenly across the same interval
        // look identical until the middle is described.
        let mut sum = 0.0f64;
        let mut vals_for_quantiles: Vec<f64> = Vec::new();
        let quantiles_wanted = is_num && n <= QUANTILE_CAP;
        if quantiles_wanted {
            vals_for_quantiles.reserve(n);
        }
        let mut n_true = 0u64;
        let mut n_false = 0u64;
        let mut n_infinite = 0u64;
        let mut n_finite = 0u64;
        let mut n_zero = 0u64;
        // Spread, order and the shape of the gaps, all worked out on the pass
        // the values are already being read on.
        let mut welford_n = 0u64;
        let mut welford_mean = 0.0f64;
        let mut welford_m2 = 0.0f64;
        let mut prev_val: Option<f64> = None;
        let mut ascending = true;
        let mut descending = true;
        let mut all_whole = true;
        let mut n_missing_leading = 0u64;
        let mut n_missing_trailing = 0u64;
        let mut max_missing_run = 0u64;
        let mut cur_missing_run = 0u64;
        let mut seen_present = false;
        let mut max_infinite = false;
        let mut min_infinite = false;
        // Counted rather than flagged: a column with one infinity in it and a
        // column that is half infinite both said "true" before.
        let mut n_inf_pos = 0u64;
        let mut n_inf_neg = 0u64;
        // A NaN the data contains, as against an NA nobody recorded.
        let mut n_nan = 0u64;
        let mut n_blank = 0u64;
        let mut min_nchar: Option<usize> = None;
        let mut max_nchar: Option<usize> = None;
        let mut level_counts: std::collections::HashMap<i32, u64> = Default::default();

        // A column can be longer than the frame is tall. A matrix column holds
        // one cell per row per sub-column, so a three-row frame carrying a
        // three by two matrix has six values in that column, and reading it to
        // the frame's row count profiled the first sub-column and called it
        // the column: a matrix of one to six reported a maximum of three.
        //
        // The values are all of it; the row identity is only the first n, so
        // the hash that says which row this is stops where the rows do.
        let cn = col_len(col).max(n);
        for i in 0..cn {
            buf.clear();
            let missing = push_cell(&mut buf, col, i, if is_factor { Some(&levels) } else { None });
            let h = fnv(&buf);
            if i < n {
                hasher.update(&buf);
                ch.push(h);
            }
            // Distinct values, not counting the absence of one. How many are
            // missing is its own field, and counting them here as well made a
            // column of five values with one missing report five distinct
            // values and one missing, which cannot both be true.
            if !missing {
                uniq.insert(h);
            }
            if missing {
                n_missing += 1;
                // A missing double is either R's NA or a NaN the arithmetic
                // produced. push_cell calls both absent because is.na() does,
                // so this is the only place either is in reach, and which one
                // it was is a different finding: nobody recorded it, against a
                // calculation that could not produce it.
                if let Val::Reals { vals, .. } = &col.val {
                    if let Some(v) = vals.get(i) {
                        if v.is_nan() && !is_r_na(*v) {
                            n_nan += 1;
                        }
                    }
                }
                cur_missing_run += 1;
                max_missing_run = max_missing_run.max(cur_missing_run);
                if !seen_present {
                    n_missing_leading += 1;
                }
                n_missing_trailing += 1;
            } else if is_num {
                seen_present = true;
                cur_missing_run = 0;
                n_missing_trailing = 0;
                let v = match &col.val {
                    Val::Reals { vals, .. } => vals[i],
                    Val::Ints { vals, .. } => vals[i] as f64,
                    _ => 0.0,
                };
                // Infinities are set aside rather than folded in. One of them
                // turns a mean into an infinity and a maximum into something
                // JSON cannot write, which is how a column of ninety-nine
                // ordinary numbers came to describe none of them. The finite
                // figures are the ones a reader wants; that an infinity is
                // there is recorded beside them.
                if v.is_infinite() {
                    n_infinite += 1;
                    if v > 0.0 {
                        max_infinite = true;
                        n_inf_pos += 1;
                    } else {
                        min_infinite = true;
                        n_inf_neg += 1;
                    }
                } else {
                    // Not reachable for a NaN: push_cell has already called it
                    // missing and the branch above took it.
                    if v == 0.0 {
                        n_zero += 1;
                    }
                    // Welford, so the spread costs one pass and no second copy.
                    welford_n += 1;
                    let d = v - welford_mean;
                    welford_mean += d / welford_n as f64;
                    welford_m2 += d * (v - welford_mean);
                    if v != v.trunc() {
                        all_whole = false;
                    }
                    if let Some(prev) = prev_val {
                        if v < prev {
                            ascending = false;
                        }
                        if v > prev {
                            descending = false;
                        }
                    }
                    prev_val = Some(v);
                    min = Some(min.map_or(v, |m| m.min(v)));
                    max = Some(max.map_or(v, |m| m.max(v)));
                    sum += v;
                    n_finite += 1;
                    if quantiles_wanted {
                        vals_for_quantiles.push(v);
                    }
                }
                if is_logical {
                    if v != 0.0 {
                        n_true += 1;
                    } else {
                        n_false += 1;
                    }
                }
            }
            if !missing {
                if is_factor {
                    if let Val::Ints { vals, .. } = &col.val {
                        *level_counts.entry(vals[i]).or_default() += 1;
                    }
                } else if ty == "character" {
                    if let Val::Str(v) = &col.val {
                        if let Some(Some(t)) = v.get(i) {
                            let len = t.chars().count();
                            if len == 0 {
                                n_blank += 1;
                            }
                            min_nchar = Some(min_nchar.map_or(len, |m: usize| m.min(len)));
                            max_nchar = Some(max_nchar.map_or(len, |m: usize| m.max(len)));
                        }
                    }
                }
            }
        }
        let fp = hex128(&hasher.finalize());
        n_missing_total += n_missing;

        let name = names.get(j).cloned().unwrap_or_default();
        let btype = if is_factor { "factor" } else { ty };
        schema_src.push_str(&format!("{name}:{btype}|"));
        shape_src.push_str(&format!("{btype}|"));

        // is_factor is not written when false: it says the same thing as the
        // type, and a field repeated on every column of every dataset to say
        // "no" is a tenth of what the column list costs.
        let mut o = json!({
            "name": name, "type": ty,
            "n_missing": n_missing, "col_fp": fp
        });
        // Left out rather than reported as one, which is what the shared
        // digest would make it.
        if !is_opaque {
            o["n_unique"] = json!(uniq.len());
        }
        // A column with a dim of its own is a matrix inside the frame, which
        // is why it holds more values than the frame has rows.
        if let Some(dm) = attr(&pairs, "dim") {
            if let Val::Ints { vals, .. } = &dm.val {
                if vals.len() == 2 {
                    o["cell_nrow"] = json!(vals[0]);
                    o["cell_ncol"] = json!(vals[1]);
                }
            }
        }
        if is_factor {
            o["is_factor"] = json!(true);
        }
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
        // A logical column's range is 0 to 1, which says nothing. The counts do.
        if is_logical {
            o["n_true"] = json!(n_true);
            o["n_false"] = json!(n_false);
            o.as_object_mut().map(|m| m.remove("col_min"));
            o.as_object_mut().map(|m| m.remove("col_max"));
        }
        if ty == "character" {
            if let (Some(lo), Some(hi)) = (min_nchar, max_nchar) {
                o["min_nchar"] = json!(lo);
                o["max_nchar"] = json!(hi);
            }
            if n_blank > 0 {
                o["n_blank"] = json!(n_blank);
            }
        }
        // JSON has no way to write an infinity, so a mean or a maximum that is
        // one comes out as null, which reads the same as not having been
        // worked out. Counting them says which it is, and a column holding one
        // at all is worth knowing about.
        if n_infinite > 0 {
            o["n_infinite"] = json!(n_infinite);
            // Which end of the range runs off, so a reader can say the maximum
            // is infinite while still having a finite one to scale a chart by.
            if max_infinite {
                o["max_infinite"] = json!(true);
            }
            if min_infinite {
                o["min_infinite"] = json!(true);
            }
            // How many at each end, which the two flags could not say.
            if n_inf_pos > 0 {
                o["n_infinite_pos"] = json!(n_inf_pos);
            }
            if n_inf_neg > 0 {
                o["n_infinite_neg"] = json!(n_inf_neg);
            }
        }
        // Counted inside the missing total, because R counts it there and a
        // reader comparing the two would otherwise find them disagreeing.
        // Reported separately, because a value a calculation could not produce
        // is a different finding from one nobody recorded.
        if n_nan > 0 {
            o["n_nan"] = json!(n_nan);
        }
        // How much of a grid is nothing. A sparse matrix announces this in its
        // class; a dense one that is nine tenths zeros has the same shape of
        // data and says nothing, so the count is kept for both and sparseness
        // becomes a matter of degree rather than of type.
        if is_num && !is_logical && n_finite > 0 {
            o["n_zero"] = json!(n_zero);
            o["p_zero"] = json!(round_stat(n_zero as f64 / n_finite as f64));
        }
        if is_num && !is_logical && welford_n > 1 {
            let sd = (welford_m2 / (welford_n as f64 - 1.0)).sqrt();
            o["sd"] = json!(round_stat(sd));
        }
        // Whether the values are already in order, which is what makes a column
        // an index rather than a measurement.
        if is_num && !is_logical && welford_n > 1 {
            o["sort_order"] = json!(if ascending && descending {
                "constant"
            } else if ascending {
                "ascending"
            } else if descending {
                "descending"
            } else {
                "unsorted"
            });
            // A double column holding only whole numbers is a count that was
            // stored as a measurement.
            if matches!(col.val, Val::Reals { .. }) && all_whole {
                o["is_integer_valued"] = json!(true);
            }
        }
        // Where the gaps are, not only how many. A series that starts late is
        // a different thing from one that is patchy throughout.
        if n_missing > 0 {
            if n_missing_leading > 0 {
                o["n_missing_leading"] = json!(n_missing_leading);
            }
            if n_missing_trailing > 0 {
                o["n_missing_trailing"] = json!(n_missing_trailing);
            }
            o["max_missing_run"] = json!(max_missing_run);
        }
        let n_seen = n_finite;
        if is_num && !is_logical && n_seen > 0 {
            o["mean"] = json!(round_stat(sum / n_seen as f64));
            if quantiles_wanted && !vals_for_quantiles.is_empty() {
                vals_for_quantiles.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let q1 = quantile(&vals_for_quantiles, 0.25);
                let q3 = quantile(&vals_for_quantiles, 0.75);
                o["median"] = json!(round_stat(quantile(&vals_for_quantiles, 0.5)));
                o["q1"] = json!(round_stat(q1));
                o["q3"] = json!(round_stat(q3));
                // Values outside the fences a boxplot draws. A count of them
                // is what separates a heavy tail from a clerical error, and
                // says whether a mean can be taken at face value.
                let iqr = q3 - q1;
                if iqr > 0.0 {
                    let (lo, hi) = (q1 - 1.5 * iqr, q3 + 1.5 * iqr);
                    let below = vals_for_quantiles.iter().filter(|v| **v < lo).count();
                    let above = vals_for_quantiles.iter().filter(|v| **v > hi).count();
                    if below + above > 0 {
                        o["n_outliers"] = json!(below + above);
                        o["n_outliers_low"] = json!(below);
                        o["n_outliers_high"] = json!(above);
                    }
                }
                // The commonest value, when there are few enough distinct ones
                // for that to mean something. A column of codes has one; a
                // column of measurements does not.
                if uniq.len() <= MODE_CARDINALITY_CAP && uniq.len() > 1 {
                    let mut best: Option<(f64, usize)> = None;
                    let mut i = 0usize;
                    while i < vals_for_quantiles.len() {
                        let v = vals_for_quantiles[i];
                        let mut j = i;
                        while j < vals_for_quantiles.len() && vals_for_quantiles[j] == v {
                            j += 1;
                        }
                        let run = j - i;
                        if best.map(|(_, c)| run > c).unwrap_or(true) {
                            best = Some((v, run));
                        }
                        i = j;
                    }
                    // Only when the commonest value is actually common. Every
                    // value of a column of measurements occurs once, and the
                    // first of them is not a mode: naming it reports one row in
                    // twelve as though it were a pattern.
                    if let Some((v, c)) = best {
                        let share = c as f64 / vals_for_quantiles.len() as f64;
                        if c >= 2 && share >= MODE_MIN_SHARE {
                            o["mode_value"] = json!(round_stat(v));
                            o["mode_share"] = json!(round_stat(share));
                        }
                    }
                }
                // How lopsided and how heavy-tailed, on the moments the same
                // pass already has the mean and spread for.
                if welford_n > 2 {
                    let m = welford_mean;
                    let sd = (welford_m2 / (welford_n as f64 - 1.0)).sqrt();
                    if sd > 0.0 {
                        let n = welford_n as f64;
                        let (mut m3, mut m4) = (0.0f64, 0.0f64);
                        for v in &vals_for_quantiles {
                            let z = (v - m) / sd;
                            m3 += z * z * z;
                            m4 += z * z * z * z;
                        }
                        o["skewness"] = json!(round_stat(m3 / n));
                        o["kurtosis"] = json!(round_stat(m4 / n - 3.0));
                    }
                }
            }
        }
        // Which levels a factor actually uses, and how often. The level list
        // says what is possible; this says what is there.
        if is_factor && !level_counts.is_empty() {
            let mut pairs: Vec<(i32, u64)> = level_counts.into_iter().collect();
            if is_ordered {
                // Declared order, which is what an ordered factor means. Taking
                // the commonest and re-sorting them gives an arbitrary window
                // into a sequence: keeping the first fifty in order keeps the
                // sequence, which is the part that carries the meaning.
                pairs.sort_by_key(|a| a.0);
            } else {
                pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            }
            let shown: Vec<Value> = pairs
                .iter()
                .take(LEVEL_COUNT_CAP)
                .map(|(code, count)| {
                    let label = levels
                        .get((*code - 1).max(0) as usize)
                        .and_then(|l| l.clone())
                        .unwrap_or_else(|| format!("{code}"));
                    json!({ "level": label, "n": count })
                })
                .collect();
            o["level_counts"] = json!(shown);
            if pairs.len() > LEVEL_COUNT_CAP {
                o["level_counts_truncated"] = json!(true);
            }
        }
        {
            let cp = attr_pairs(col);
            let ccls = attr(&cp, "class").map(str_vec).unwrap_or_default();
            declared_fields(&cp, &ccls, &mut o);
        }
        // A list column: the slots are where the observations actually are.
        if let Val::Vec(items) = &col.val {
            let inames = attr(&attr_pairs(col), "names").map(str_vec).unwrap_or_default();
            describe_list(items, &inames, &mut o);
        }
        spatial_fields(col, &mut o);
        describe_attrs(&attr_pairs(col), &mut o, &[]);
        // Emit detail for the leading columns only. The cap is on what is
        // WRITTEN, never on what is hashed: col_fp_bytes below still covers every
        // column, so content_fp is unchanged and the content-addressed store does
        // not see a new identity for data that has not moved. ncol is recorded
        // separately from the true column count, so a reader can still see how
        // many there are and tell that this list is the first of them.
        if col_json.len() < COLUMN_DETAIL_CAP {
            col_json.push(o);
        }
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
                // A symmetric matrix keeps one triangle, so its stored count
                // is roughly half its non-zeros: every off-diagonal entry
                // stands for two. Reporting the stored count as the number of
                // non-zeros understates the matrix by almost half.
                if parts.map(|(_, shape, _)| shape == "symmetric").unwrap_or(false) {
                    if let (Some(i), Some(pp)) = (attr(&pairs, "i"), attr(&pairs, "p")) {
                        if let (Val::Ints { vals: iv, .. }, Val::Ints { vals: pv, .. }) =
                            (&i.val, &pp.val)
                        {
                            let mut on_diag = 0i64;
                            for col in 0..pv.len().saturating_sub(1) {
                                let (lo, hi) = (pv[col].max(0) as usize, pv[col + 1].max(0) as usize);
                                for k in lo..hi.min(iv.len()) {
                                    if iv[k] as usize == col {
                                        on_diag += 1;
                                    }
                                }
                            }
                            let stored_now = stored as i64;
                            rec["n_nonzero"] = json!(stored_now * 2 - on_diag);
                        }
                    }
                }

                    let cells = r.saturating_mul(c);
                    if cells > 0 {
                        // Through the same rounding as every other statistic,
                        // which keeps significant digits as well as places. A
                        // million by a million matrix holding one value is one
                        // part in a trillion dense, and rounding that to six
                        // decimal places stored it as nought, which is the one
                        // thing a density is never allowed to be here.
                        rec["density"] = json!(round_stat(stored as f64 / cells as f64));
                    }
                    rec["length"] = json!(stored as i64);
                    rec["n_cells"] = json!(cells);
                    // Fingerprint the values actually stored, so two matrices
                    // holding the same data dedup even though the class wrapping
                    // them says nothing about it.
                    let mut cols: Vec<&Node> = Vec::new();
                    if let Some(x) = x {
                        // The non-zeros are the data; the grid they sit in is
                        // described by n_stored, n_cells and density beside it.
                        lift_value_summary(&mut rec, x, "stored values");
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
                // The size of a cell on the ground, which is what makes the
                // extent and the grid comparable between rasters.
                if let (Some(bb), Some(nrf), Some(ncf)) =
                    (rec.get("bbox").cloned(), Some(nr as f64), Some(nc as f64))
                {
                    if let Some(b) = bb.as_array() {
                        if b.len() == 4 && nrf > 0.0 && ncf > 0.0 {
                            let (x0, y0, x1, y1) = (
                                b[0].as_f64().unwrap_or(0.0),
                                b[1].as_f64().unwrap_or(0.0),
                                b[2].as_f64().unwrap_or(0.0),
                                b[3].as_f64().unwrap_or(0.0),
                            );
                            rec["resolution"] =
                                json!([round_stat((x1 - x0) / ncf), round_stat((y1 - y0) / nrf)]);
                        }
                    }
                }
                if let Some(f) = attr(&pairs, "file") {
                    let fp = attr_pairs(f);
                    // The value standing in for "nothing here". An unmasked
                    // sentinel of -9999 wrecks a mean without saying so, and a
                    // reader who knows one is declared can tell. Infinite means
                    // none was set.
                    if let Some(Val::Reals { vals, .. }) = attr(&fp, "nodatavalue").map(|n| &n.val) {
                        if let Some(v) = vals.first() {
                            if v.is_finite() {
                                rec["nodata_value"] = json!(round_stat(*v));
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
                        // Layers are named and few, so unlike the columns of a
                        // wide matrix they are worth listing.
                        let names = str_vec(nm);
                        if !names.is_empty() && names.len() <= MARGIN_NAME_CAP {
                            rec["layer_names"] = json!(names);
                        }
                    }
                    // Whether the cells are here or behind a file handle, which
                    // tells a reader whether these figures cost any input at
                    // all, and raster's own per-layer range while we are here.
                    if let Some(Val::Ints { vals, .. }) = attr(&dp, "inmemory").map(|n| &n.val) {
                        rec["in_memory"] = json!(vals.first().copied().unwrap_or(0) != 0);
                    }
                    let have = matches!(attr(&dp, "haveminmax").map(|n| &n.val),
                                        Some(Val::Ints { vals, .. }) if vals.first().copied().unwrap_or(0) != 0);
                    if have {
                        for (slot, key) in [("min", "layer_min"), ("max", "layer_max")] {
                            if let Some(Val::Reals { vals, .. }) = attr(&dp, slot).map(|n| &n.val) {
                                if !vals.is_empty() && vals.len() <= MARGIN_NAME_CAP {
                                    rec[key] = json!(vals.iter().map(|v| round_stat(*v)).collect::<Vec<_>>());
                                }
                            }
                        }
                    }
                    if let Some(v) = attr(&dp, "values") {
                        if col_len(v) > 0 {
                            // The cells of the grid, which is what a raster is.
                            lift_value_summary(&mut rec, v, "cells");
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
        rec["frame_class"] = json!(frame_flavour(&class));
        // What the rows are sorted and looked up by. A keyed table is a
        // different thing to work with from the same rows unkeyed, and the key
        // is the closest thing a data.table has to a primary key.
        if let Some(k) = attr(&pairs, "sorted") {
            let cols = str_vec(k);
            if !cols.is_empty() {
                rec["dt_key"] = json!(cols);
            }
        }
        if let Some(ix) = attr(&pairs, "index") {
            // Each secondary index is an attribute of its own on that node,
            // named after the columns it covers and prefixed to keep it out of
            // the way of anything reading names.
            let names: Vec<String> = attr_pairs(ix)
                .into_iter()
                .map(|(k, _)| k.trim_start_matches('_').to_string())
                .filter(|n| !n.is_empty() && n != "names")
                .collect();
            if !names.is_empty() {
                rec["dt_indices"] = json!(names);
            }
        }
        // dplyr keeps the grouping as a table of its own: the columns grouped
        // by, and one row per group.
        if let Some(g) = attr(&pairs, "groups") {
            let gp = attr_pairs(g);
            let gvars: Vec<String> = attr(&gp, "names")
                .map(str_vec)
                .unwrap_or_default()
                .into_iter()
                .filter(|n| n != ".rows")
                .collect();
            if !gvars.is_empty() {
                rec["group_vars"] = json!(gvars);
            }
            if let Some(n) = nrow_of(attr(&gp, "row.names")) {
                rec["n_groups"] = json!(n);
            }
        }
        // dplyr keeps its grouping in the object, and a grouped frame behaves
        // differently from the same rows ungrouped.
        if class.iter().any(|c| c == "grouped_df") {
            rec["is_grouped"] = json!(true);
        }
        // A rowwise frame uses the same grouping machinery with no grouping
        // variables, so it reported a group count equal to its row count and
        // nothing to say why. The class is in the chain, which is already
        // being read.
        if class.iter().any(|c| c == "rowwise_df") {
            rec["is_rowwise"] = json!(true);
        }
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
                lift_spatial(&mut rec);
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
            margin_fields(&pairs, &mut rec);
            series_fields(&pairs, &mut rec);
            describe_attrs(&pairs, &mut rec, &[]);
            // A grid has no columns to hang a summary on, so it is described
            // whole. Without this a matrix said how big it was and nothing
            // about what was in it.
            lift_value_summary(&mut rec, node, "cells");
            // And where the variation in it lies, which reading every cell as
            // one vector cannot say.
            if n_dim == 2 {
                margin_summaries(
                    node,
                    extent[0].max(0) as usize,
                    extent[1].max(0) as usize,
                    &mut rec,
                );
            }
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
        // Read for their length rather than their contents, but a length and a
        // type is a great deal more than "object" and nothing.
        Val::Blob { len, .. } => ("vector", *len),
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
    declared_fields(&pairs, &class, &mut rec);
    // A broken-down time is stored as nine or eleven parallel vectors, one per
    // field, so reading it as the list it is reports a length of eleven for
    // any number of timestamps: the fields get counted instead of the times.
    // The times are the length of any one component, and the years are a
    // component of their own, offset from 1900 the way struct tm carries them.
    let posixlt = class.iter().any(|c| c == "POSIXlt");
    if posixlt {
        if let Val::Vec(items) = &node.val {
            let inames = attr(&pairs, "names").map(str_vec).unwrap_or_default();
            let field = |n: &str| -> Option<&Node> {
                inames.iter().position(|k| k == n).and_then(|i| items.get(i))
            };
            if let Some(sec) = field("sec") {
                let n = col_len(sec) as i64;
                rec["length"] = json!(n);
                rec["n_fields"] = json!(items.len());
                rec["index_class"] = json!("POSIXlt");
                if let Some(y) = field("year") {
                    let years: Vec<i64> = match &y.val {
                        Val::Ints { vals, .. } => {
                            vals.iter().filter(|v| **v != NA_INT).map(|v| *v as i64 + 1900).collect()
                        }
                        Val::Reals { vals, .. } => {
                            vals.iter().filter(|v| !v.is_nan()).map(|v| *v as i64 + 1900).collect()
                        }
                        _ => Vec::new(),
                    };
                    if let (Some(lo), Some(hi)) = (years.iter().min(), years.iter().max()) {
                        rec["year_min"] = json!(lo);
                        rec["year_max"] = json!(hi);
                    }
                }
            }
        }
    } else if let Val::Vec(items) = &node.val {
        let inames = attr(&pairs, "names").map(str_vec).unwrap_or_default();
        describe_list(items, &inames, &mut rec);
    }
    describe_attrs(&pairs, &mut rec, &[]);
    if kind == "vector" || kind == "list" {
        // What a vector holds, which the record said nothing about: a length
        // alone does not distinguish a column of numbers from a column of
        // names. Taken from the value rather than from the profile, so a raw
        // or complex vector, whose bytes are not kept, still says what it is.
        let (ty, is_factor, nlev) = base_type(node);
        rec["type"] = json!(ty);
        if is_factor {
            rec["is_factor"] = json!(true);
            if nlev >= 0 {
                rec["n_levels"] = json!(nlev);
            }
        }
        if let Some(p) = profile_columns(&[node], std::slice::from_ref(&name.to_string())) {
            rec["n_missing_total"] = json!(p.n_missing_total);
            rec["n_unique"] = json!(p.columns.first().and_then(|c| c.get("n_unique")).cloned().unwrap_or(json!(null)));
            // The summary the profile already worked out, which was being
            // kept only for the columns of a table: a time series or a zoo is
            // a vector, and had a start and a frequency but nothing about the
            // values themselves.
            for k in [
                "col_min", "col_max", "mean", "median", "q1", "q3",
                "n_true", "n_false", "min_nchar", "max_nchar", "n_blank",
                "level_counts", "level_counts_truncated", "n_infinite",
                "max_infinite", "min_infinite",
                "n_zero", "p_zero",
                "sd", "sort_order", "is_integer_valued", "n_missing_leading", "n_missing_trailing", "max_missing_run",
                "n_outliers", "n_outliers_low", "n_outliers_high", "mode_value", "mode_share", "skewness", "kurtosis",
            ] {
                if let Some(v) = p.columns.first().and_then(|c| c.get(k)) {
                    if !v.is_null() {
                        rec[k] = v.clone();
                    }
                }
            }
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

// ---- the save format R used before 1.4.0 -------------------------------------
//
// Written by R 0.99 through 1.3.1 and still present in the archive: every
// binary dataset in a package whose last release predates 2002 is in this
// format, and the XDR reader cannot see any of them.
//
// It is not a stream. The file names its symbols, then maps node numbers to
// identifiers, then defines each node, and cross-references are by identifier
// rather than by position, so the graph is rebuilt from the map afterwards.
// Everything below is derived from the files themselves.

#[derive(Debug)]
enum V1Payload {
    /// A dotted pair, and the types written like one: a closure, a call, a
    /// promise. None of the latter is data, but each has to be consumed
    /// exactly or every node after it is read at the wrong offset.
    Triple(i64, i64, i64),
    Text(String),
    Ints(Vec<i32>, bool),
    Reals(Vec<f64>),
    Refs(Vec<i64>),
    Opaque,
}

struct V1Node {
    typ: i32,
    attrib: i64,
    payload: V1Payload,
}

struct V1Toks<'a> {
    t: Vec<&'a str>,
    i: usize,
}

impl<'a> V1Toks<'a> {
    /// Whitespace-delimited, except that a quoted name is one token.
    fn new(text: &'a str) -> Self {
        let mut t = Vec::new();
        let b = text.as_bytes();
        let mut i = 0usize;
        while i < b.len() {
            if b[i].is_ascii_whitespace() {
                i += 1;
                continue;
            }
            let start = i;
            if b[i] == b'"' {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
            } else {
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
            }
            t.push(&text[start..i]);
        }
        V1Toks { t, i: 0 }
    }
    fn next(&mut self) -> Result<&'a str, String> {
        let v = self.t.get(self.i).ok_or("truncated v1 stream")?;
        self.i += 1;
        Ok(v)
    }
    fn int(&mut self) -> Result<i64, String> {
        let t = self.next()?;
        t.parse::<i64>()
            .or_else(|_| t.parse::<f64>().map(|f| f as i64))
            .map_err(|_| format!("v1: expected a number, found {t:?}"))
    }
    fn text(&mut self) -> Result<String, String> {
        let t = self.next()?;
        Ok(t.strip_prefix('"')
            .and_then(|x| x.strip_suffix('"'))
            .unwrap_or(t)
            .to_string())
    }
    fn remaining(&self) -> usize {
        self.t.len().saturating_sub(self.i)
    }
}

/// A length that is negative, or larger than the file could hold, means the
/// stream is not what it claims and everything after it would be read at the
/// wrong offset.
fn v1_len(n: i64) -> Result<usize, String> {
    if n < 0 || n > 1 << 30 {
        return Err(format!("v1: implausible length {n}"));
    }
    Ok(n as usize)
}

fn v1_int(t: &str) -> i32 {
    if t == "NA" {
        return NA_INT;
    }
    t.parse::<i32>()
        .or_else(|_| t.parse::<f64>().map(|f| f as i32))
        .unwrap_or(NA_INT)
}

fn v1_real(t: &str) -> f64 {
    match t {
        "NA" | "NaN" => f64::NAN,
        "Inf" | "inf" => f64::INFINITY,
        "-Inf" | "-inf" => f64::NEG_INFINITY,
        _ => t.parse::<f64>().unwrap_or(f64::NAN),
    }
}

fn read_ascii_v1(text: &str) -> Result<Vec<(String, Node)>, String> {
    let mut tk = V1Toks::new(text);
    if tk.next()? != "1976" {
        return Err("not the pre-1.4.0 ASCII format".into());
    }
    let (nsym, nnode) = (tk.int()?, tk.int()?);
    let _ntotal = tk.int()?;
    if nsym < 0 || nnode < 0 || nsym > 1_000_000 || nnode > 5_000_000 {
        return Err("v1: implausible table sizes".into());
    }
    let mut names: std::collections::HashMap<i64, String> = Default::default();
    for _ in 0..nsym {
        let _idx = tk.int()?;
        let id = tk.int()?;
        names.insert(id, tk.text()?);
    }
    // Node number to identifier. Others refer to a node by its identifier, and
    // the definitions below are keyed by node number, so this is the join
    // between them. Not position: a file is free to define its nodes in an
    // order other than the one it lists them in.
    let mut ids: std::collections::HashMap<i64, i64> = Default::default();
    for _ in 0..nnode {
        let idx = tk.int()?;
        ids.insert(idx, tk.int()?);
    }
    // Definitions run until one token is left: the identifier of the root.
    let mut nodes: std::collections::HashMap<i64, V1Node> = Default::default();
    while tk.remaining() > 1 {
        let _idx = tk.int()?;
        let typ = tk.int()? as i32;
        // The object bit, then the levels field. Neither is needed: a class is
        // read from the attribute list, which is where it also lives.
        let _object = tk.int()?;
        let _levels = tk.int()?;
        let attrib = tk.int()?;
        let payload = match typ {
            2 | 3 | 5 | 6 | 17 => V1Payload::Triple(tk.int()?, tk.int()?, tk.int()?),
            9 => {
                // A negative length would mean no text follows. Nothing in the
                // archive writes one, and R refuses to load a file that does,
                // so stop rather than take the next token for the string and
                // read everything after it at the wrong offset.
                let len = tk.int()?;
                if len < 0 {
                    return Err("v1: negative string length".into());
                }
                V1Payload::Text(tk.text()?)
            }
            10 | 13 => {
                let n = v1_len(tk.int()?)?;
                let mut v = Vec::with_capacity(n.min(1 << 20));
                for _ in 0..n {
                    v.push(v1_int(tk.next()?));
                }
                V1Payload::Ints(v, typ == 10)
            }
            14 => {
                let n = v1_len(tk.int()?)?;
                let mut v = Vec::with_capacity(n.min(1 << 20));
                for _ in 0..n {
                    v.push(v1_real(tk.next()?));
                }
                V1Payload::Reals(v)
            }
            16 | 19 => {
                let n = v1_len(tk.int()?)?;
                let mut v = Vec::with_capacity(n.min(1 << 20));
                for _ in 0..n {
                    v.push(tk.int()?);
                }
                V1Payload::Refs(v)
            }
            0 | 1 => V1Payload::Opaque,
            other => return Err(format!("v1: unhandled type {other}")),
        };
        let id = ids.get(&_idx).copied().unwrap_or(_idx);
        nodes.insert(id, V1Node { typ, attrib, payload });
    }
    let root = tk.int()?;
    let mut out = Vec::new();
    let mut cur = root;
    let mut guard = 0;
    while cur != -1 && guard < 10_000 {
        guard += 1;
        let Some(n) = nodes.get(&cur) else { break };
        let V1Payload::Triple(car, cdr, tag) = n.payload else { break };
        let name = names.get(&tag).cloned().unwrap_or_default();
        let node = v1_build(car, &nodes, &names, 0);
        if !name.is_empty() {
            out.push((name, node));
        }
        cur = cdr;
    }
    if out.is_empty() {
        return Err("v1: no named objects".into());
    }
    Ok(out)
}

fn v1_build(
    id: i64,
    nodes: &std::collections::HashMap<i64, V1Node>,
    names: &std::collections::HashMap<i64, String>,
    depth: u32,
) -> Node {
    if depth > 64 {
        return Node { val: Val::Nil, attr: None };
    }
    if let Some(nm) = names.get(&id) {
        return Node { val: Val::Sym(nm.clone()), attr: None };
    }
    let Some(n) = nodes.get(&id) else {
        return Node { val: Val::Nil, attr: None };
    };
    let attr = if n.attrib == -1 {
        None
    } else {
        match v1_build(n.attrib, nodes, names, depth + 1) {
            Node { val: Val::Nil, .. } => None,
            other => Some(Box::new(other)),
        }
    };
    let val = match &n.payload {
        V1Payload::Text(t) => Val::Char(Some(t.clone())),
        V1Payload::Ints(v, logical) => Val::Ints { len: v.len(), vals: v.clone(), logical: *logical },
        V1Payload::Reals(v) => Val::Reals { len: v.len(), vals: v.clone() },
        V1Payload::Refs(v) if n.typ == 16 => Val::Str(
            v.iter()
                .map(|r| match v1_build(*r, nodes, names, depth + 1).val {
                    Val::Char(c) => c,
                    _ => None,
                })
                .collect(),
        ),
        V1Payload::Refs(v) => Val::Vec(v.iter().map(|r| v1_build(*r, nodes, names, depth + 1)).collect()),
        V1Payload::Triple(car, cdr, tag) if n.typ == 2 => Val::List {
            tag: Box::new(v1_build(*tag, nodes, names, depth + 1)),
            car: Box::new(v1_build(*car, nodes, names, depth + 1)),
            cdr: Box::new(v1_build(*cdr, nodes, names, depth + 1)),
        },
        _ => Val::Nil,
    };
    Node { val, attr }
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
    // R before 1.4.0 wrote a different format entirely, and a package whose
    // last release predates 2002 has all of its data in it.
    if bytes.len() >= 5 && &bytes[0..4] == b"1976" {
        let text = String::from_utf8_lossy(&bytes);
        let objs = read_ascii_v1(&text)?;
        return Ok(objs
            .into_iter()
            .map(|(nm, node)| (nm, node, "rda".to_string(), 1, comp.to_string()))
            .collect());
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
// R's data() recognises a closed list of extensions and reads each one a fixed
// way. It never sniffs the file. `.tab`, `.txt` and `.TXT` go through
// read.table's default separator, which is a run of whitespace and not a tab;
// `.csv` and `.CSV` are read with a semicolon, which is not the usual meaning of
// the name. Guessing instead of following the list produced a different table
// from the one anybody loading the package gets: the whitespace files collapsed
// into a single column carrying the whole header as its name.
const WS: char = '\0';
const DATA_TEXT_EXTS: &[(&str, char)] = &[
    ("tab", WS), ("txt", WS), ("TXT", WS),
    ("tab.gz", WS), ("txt.gz", WS),
    ("tab.bz2", WS), ("txt.bz2", WS),
    ("tab.xz", WS), ("txt.xz", WS),
    ("csv", ';'), ("CSV", ';'),
    ("csv.gz", ';'), ("csv.bz2", ';'), ("csv.xz", ';'),
];

/// The separator R would use for this file, or None if data() cannot load it at
/// all. `.dat`, `.tsv` and `.rds` are not on the list: a file with one of those
/// names sits in data/ unreachable, so reporting it as a dataset would describe
/// something no user can load.
pub fn text_data_sep(fname: &str) -> Option<char> {
    DATA_TEXT_EXTS.iter().find_map(|(ext, sep)| {
        let suffix = format!(".{ext}");
        (fname.len() > suffix.len() && fname.ends_with(&suffix)).then_some(*sep)
    })
}

/// read.table's field splitter: quotes protect a separator, an unquoted # ends
/// the line, and under the whitespace separator a run counts once.
fn split_fields(line: &str, sep: char) -> Vec<String> {
    match split_quoted(line, sep, true) {
        Some(f) => f,
        // An apostrophe in an unquoted field opens a quote that never closes.
        // R treats that as a broken file; read the line literally instead of
        // swallowing the rest of it.
        None => split_quoted(line, sep, false).unwrap_or_default(),
    }
}

fn split_quoted(line: &str, sep: char, quoting: bool) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in line.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '#' {
                    break;
                } else if quoting && (c == '"' || c == '\'') {
                    quote = Some(c);
                    started = true;
                } else if sep == WS && c.is_whitespace() {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                } else if sep != WS && c == sep {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                } else {
                    cur.push(c);
                    started = true;
                }
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started || sep != WS {
        out.push(cur);
    }
    Some(out.iter().map(|f| f.trim().to_string()).collect())
}

/// R runs header names through make.names, so the name recorded here is the one
/// a caller sees, and the schema fingerprint is taken over the same strings.
fn make_names(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '_' { c } else { '.' })
        .collect();
    let first = out.chars().next().unwrap_or('.');
    let second = out.chars().nth(1);
    if out.is_empty()
        || first.is_ascii_digit()
        || first == '_'
        || (first == '.' && second.map(|c| c.is_ascii_digit()).unwrap_or(false))
    {
        out.insert(0, 'X');
    }
    out
}

fn decompress_text(raw: Vec<u8>) -> Option<(String, &'static str)> {
    use std::io::Read;
    let (bytes, comp): (Vec<u8>, &'static str) = if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        let mut o = Vec::new();
        flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut o).ok()?;
        (o, "gzip")
    } else if raw.starts_with(b"BZh") {
        let mut o = Vec::new();
        bzip2::read::BzDecoder::new(&raw[..]).read_to_end(&mut o).ok()?;
        (o, "bzip2")
    } else if raw.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        let mut o = Vec::new();
        xz2::read::XzDecoder::new(&raw[..]).read_to_end(&mut o).ok()?;
        (o, "xz")
    } else {
        (raw, "none")
    };
    // readLines ends a line at \n, at \r\n, and at a lone \r. Rust's str::lines
    // ends one at \n and nothing else, so a file written with classic Mac endings
    // arrived as ONE line: the table reader took the whole thing for a header with
    // nothing under it and reported a frame of zero rows carrying one column per
    // field in the file. One such file came out as 2,537,269 columns whose
    // per-column schema ran to 306 MB.
    //
    // \r\n is unambiguous and always folds. A LONE \r only folds in a file that
    // has no \n anywhere, which is what a classic Mac file looks like. The
    // narrower rule matters because fields are split a line at a time and quoting
    // is applied within a line: a \r inside a quoted field of an otherwise
    // LF-terminated file is data, and folding it would break the row into two and
    // cost the whole file, since a row of the wrong width is refused.
    let lossy = String::from_utf8_lossy(&bytes);
    // Most files carry no \r at all, and those pay one scan rather than a rewrite.
    let text = if lossy.contains('\r') {
        let folded = lossy.replace("\r\n", "\n");
        if folded.contains('\n') { folded } else { folded.replace('\r', "\n") }
    } else {
        lossy.into_owned()
    };
    Some((text, comp))
}

/// Split a delimited table that has already been read into memory.
fn parse_table(text: &str, sep: char) -> Option<(Vec<Node>, Vec<String>, usize)> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut header: Option<Vec<String>> = None;
    // Split on either ending. A file written on a Mac before OS X separates its
    // lines with a carriage return and nothing else, which is not a line break
    // to most readers: the whole file arrives as one line, and a table of three
    // thousand rows is reported as having none.
    for line in text.split(['\n', '\r']) {
        let f = split_fields(line, sep);
        if f.is_empty() || f.iter().all(|x| x.is_empty()) {
            continue;
        }
        match header {
            None => header = Some(f),
            Some(_) => rows.push(f),
        }
    }
    let mut names: Vec<String> = header?.iter().map(|n| make_names(n)).collect();
    let ncol = names.len();
    if ncol == 0 {
        return None;
    }
    // A header with nothing under it is a line of text, not a table. Every
    // per-column statistic would be vacuous, and profiling it emits a record
    // asserting a dataset where none exists. This is also the shape a misread
    // file collapses to, so refusing it is the second guard on the same failure.
    if rows.is_empty() {
        return None;
    }
    // read.table's row-names rule: a header one field shorter than the body
    // means the first field of each row names it rather than being data.
    let has_row_names = rows.first().map(|r| r.len() == ncol + 1).unwrap_or(false);
    // read.table defaults to fill = FALSE and refuses a file whose rows do not
    // all hold the same number of fields. Padding a short row instead invented a
    // table: two tabs in a row collapse to one separator under the whitespace
    // rule, so a file with an empty cell is one data() cannot load at all, and
    // reporting a tidy frame for it describes something nobody can get.
    let want = if has_row_names { ncol + 1 } else { ncol };
    if rows.iter().any(|r| r.len() != want) {
        return None;
    }
    if has_row_names {
        for r in rows.iter_mut() {
            r.remove(0);
        }
    }
    let mut cells: Vec<Vec<String>> = vec![Vec::new(); ncol];
    for r in &rows {
        for (j, cell) in cells.iter_mut().enumerate() {
            cell.push(r.get(j).cloned().unwrap_or_default());
        }
    }
    for (j, n) in names.iter_mut().enumerate() {
        if n.is_empty() {
            *n = format!("V{}", j + 1);
        }
    }
    let cols: Vec<Node> = (0..ncol).map(|j| infer_column(&cells[j])).collect();
    Some((cols, names, rows.len()))
}

/// A file under data/, read by the rules data() applies to its extension.
///
/// The second value describes a file that does not appear to be what its name
/// says. data() reads a .csv with semicolons, so a comma-separated one placed
/// there loads as a single column whose name is the whole header line. That is
/// what a reader gets and so it is what is reported, but the file is plainly
/// meant to be read another way, and saying so is more use than a column named
/// after two hundred characters of header.
fn read_text(
    path: &Path,
    fname: &str,
) -> Option<(Vec<Node>, Vec<String>, usize, &'static str, &'static str, Option<(char, usize)>)> {
    let sep = text_data_sep(fname)?;
    let (text, comp) = decompress_text(std::fs::read(path).ok()?)?;
    let fmt = if sep == WS { "tab" } else { "csv" };
    let (cols, names, nrow) = parse_table(&text, sep)?;
    // Only worth asking when the rules produced one column, which is the shape
    // a wrong separator always leaves behind.
    let mut alt = None;
    if names.len() == 1 {
        for cand in [',', ';', '\t', '|'] {
            if cand == sep {
                continue;
            }
            if let Some((_, n2, _)) = parse_table(&text, cand) {
                if n2.len() > 1 {
                    match alt {
                        Some((_, best)) if best >= n2.len() => {}
                        _ => alt = Some((cand, n2.len())),
                    }
                }
            }
        }
    }
    Some((cols, names, nrow, fmt, comp, alt))
}

/// A file under inst/extdata, where none of data()'s conventions apply. Nothing
/// loads these by name, so the extension carries no promise: a .csv here is an
/// ordinary comma-separated file rather than the semicolon one data() expects.
/// The separator is the one the header actually uses.
fn read_text_free(path: &Path) -> Option<(Vec<Node>, Vec<String>, usize, &'static str, &'static str, Option<(char, usize)>)> {
    let (text, comp) = decompress_text(std::fs::read(path).ok()?)?;
    let header = text.lines().find(|l| !l.trim().is_empty())?;
    let (sep, fmt) = [(',', "csv"), ('\t', "tsv"), (';', "csv"), ('|', "psv")]
        .into_iter()
        .map(|(c, f)| (c, f, header.matches(c).count()))
        .filter(|(_, _, n)| *n > 0)
        .max_by_key(|(_, _, n)| *n)
        .map(|(c, f, _)| (c, f))
        .unwrap_or((WS, "txt"));
    let (cols, names, nrow) = parse_table(&text, sep)?;
    Some((cols, names, nrow, fmt, comp, None))
}

/// Emit one `dataset` record per dataset shipped under `root`'s data/ directory
/// and R/sysdata.rda. Never panics on a bad file; it degrades with a note.
pub fn scan_package(root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let mut targets: Vec<(std::path::PathBuf, bool)> = Vec::new();
    let titles = rd_titles(root);
    if let Ok(rd) = std::fs::read_dir(root.join("data")) {
        let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        paths.sort();
        // One dataset name, one file. A package may carry the same dataset as
        // both a saved image and a csv; data() loads whichever comes first in
        // its own order and never opens the other, so describing both puts a
        // second row in the catalogue under a name that resolves to one
        // dataset, and the row it adds is for the copy nobody reads.
        let mut best: std::collections::BTreeMap<String, (usize, std::path::PathBuf)> =
            Default::default();
        for p in paths {
            let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let Some(rank) = data_ext_rank(fname) else { continue };
            let name = dataset_name(fname);
            match best.get(&name) {
                Some((r, _)) if *r <= rank => {}
                _ => {
                    best.insert(name, (rank, p));
                }
            }
        }
        for (_, (_, p)) in best {
            targets.push((p, false));
        }
    }
    let sys = root.join("R").join("sysdata.rda");
    if sys.exists() {
        targets.push((sys, true));
    }
    let n_loadable = targets.len();
    // inst/extdata is where R tells authors to put data files that are not
    // datasets. Nothing loads them by name, so they are absent from the
    // catalogue entirely, and it is often the only real-world data a package
    // carries. Walked rather than listed: some packages nest a whole package
    // tree under here.
    // A source tree keeps it at inst/extdata; installing strips the inst/, so
    // an installed tree keeps it at extdata. Accept either.
    let mut extra: Vec<std::path::PathBuf> = Vec::new();
    let ext_root = {
        let src = root.join("inst").join("extdata");
        if src.is_dir() { src } else { root.join("extdata") }
    };
    walk_extdata(&ext_root, EXTDATA_DEPTH, &mut extra);
    for p in extra {
        targets.push((p, false));
    }

    for (i, (path, internal)) in targets.into_iter().enumerate() {
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let lower = fname.to_lowercase();
        let from_extdata = i >= n_loadable;
        let origin_dir = if from_extdata {
            "extdata"
        } else if internal {
            "sysdata"
        } else {
            "data"
        };
        let rel = if from_extdata {
            let tail = path
                .strip_prefix(&ext_root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| fname.to_string());
            format!("inst/extdata/{tail}")
        } else {
            format!("{}/{}", if internal { "R" } else { "data" }, fname)
        };
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let name = dataset_name(fname);

        // data() loads .rda and .RData, and nothing else binary. A .rds in data/
        // is unreachable, and in an installed package data/Rdata.rds is the
        // lazy-load index rather than a dataset: reading it named a dataset
        // "Rdata" once per package and gave it a fingerprint.
        let is_rbin = lower.ends_with(".rda") || lower.ends_with(".rdata");
        let is_script = lower.ends_with(".r");
        // Only the extensions data() actually dispatches on. A .tsv or a .dat
        // in data/ is not loadable, so describing one would put a dataset in the
        // catalogue that nobody can reach.
        let is_text = !is_rbin && text_data_sep(fname).is_some();

        // Under extdata the extension promises nothing, so anything we have a
        // reader for is worth reading and the rest is left alone: a catalogue
        // row for a .xlsx we cannot open carries no more than its own name.
        let (is_rbin, is_script, is_text) = if from_extdata {
            if size > EXTDATA_PARSE_LIMIT {
                continue;
            }
            if !extdata_is_readable(&lower) {
                continue;
            }
            let bin = lower.ends_with(".rds") || lower.ends_with(".rda") || lower.ends_with(".rdata");
            let txt = !bin;
            (bin, false, txt)
        } else {
            (is_rbin, is_script, is_text)
        };

        let before = out.len();
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
            let read = if from_extdata { read_text_free(&path) } else { read_text(&path, fname) };
            match read {
                Some((cols, names, nrow, fmt, comp, alt)) => {
                    let refs: Vec<&Node> = cols.iter().collect();
                    let mut rec = json!({
                        "rec": "dataset", "name": name, "file": rel, "format": fmt,
                        "compression": comp, "compressed_bytes": size, "internal": internal,
                        "class": "data.frame", "kind": "table", "nrow": nrow,
                        "ncol": names.len(), "confidence": "degraded",
                        "notes": "text: column types inferred"
                    });
                    // What a reader gets is above; this says the file does not
                    // look like what its name claims, which is more use than a
                    // column named after the whole header line.
                    if let Some((c, n)) = alt {
                        rec["delimiter_looks_like"] =
                            json!(if c == '\t' { "tab".to_string() } else { c.to_string() });
                        rec["delimiter_would_give_ncol"] = json!(n as i64);
                    }
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
                    "confidence": "degraded", "notes": "text: data() cannot load this file"
                })),
            }
        }
        for r in out[before..].iter_mut() {
            r["origin_dir"] = json!(origin_dir);
            if let Some(t) = r.get("name").and_then(|n| n.as_str()).and_then(|n| titles.get(n)) {
                r["title"] = json!(t);
            }
        }
    }
    out
}

/// What a package keeps under inst/extdata, counted by extension.
///
/// R points authors here for data files that are not datasets, so what is in
/// here is a different question from what a package exports: nothing loads it
/// by name and much of it is not tabular at all. The inventory answers whether
/// a package has such files and roughly what they are, without opening the ones
/// that would need a parser of their own.
///
/// `read` counts the files that also produced a dataset record, `unread` the
/// rest. It rides the summary record, which reaches the database without any
/// change to the pipeline, so this survives even if the per-file records do not.
pub fn extdata_inventory(root: &Path) -> Value {
    let ext_root = {
        let src = root.join("inst").join("extdata");
        if src.is_dir() { src } else { root.join("extdata") }
    };
    if !ext_root.is_dir() {
        return Value::Null;
    }
    let mut files = Vec::new();
    walk_extdata(&ext_root, EXTDATA_DEPTH, &mut files);
    // Size per extension, not just per directory. A file we read carries its own
    // byte count on its record the way one under data/ does; a file we do not
    // read has no record to carry anything, and one total for the directory
    // cannot say whether it is four sequence files or four text files.
    let mut read: std::collections::BTreeMap<String, (i64, i64)> = Default::default();
    let mut unread: std::collections::BTreeMap<String, (i64, i64)> = Default::default();
    let mut bytes: i64 = 0;
    let mut largest: i64 = 0;
    // The names of the files we do not open. A file we read carries its own path
    // on its own record; one we leave shut has nothing else to say what it is,
    // and "gz 1/11.0M" is a great deal less use than dm3_upstream2000.fa.gz.
    // Capped, and the counts above give the true total when the list is short.
    let mut unread_files: Vec<String> = Vec::new();
    for p in &files {
        let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let lower = fname.to_lowercase();
        let n = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) as i64;
        bytes += n;
        largest = largest.max(n);
        let ext = match lower.rsplit_once('.') {
            Some((_, e)) if !e.is_empty() && e.len() <= 12 => e.to_string(),
            _ => "none".to_string(),
        };
        let readable = extdata_is_readable(&lower);
        if !readable && unread_files.len() < EXTDATA_NAME_CAP {
            // Relative to the directory, so nesting stays visible.
            unread_files.push(
                p.strip_prefix(&ext_root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| fname.to_string()),
            );
        }
        let side = if readable { &mut read } else { &mut unread };
        let slot = side.entry(ext).or_default();
        slot.0 += 1;
        slot.1 += n;
    }
    let shape = |m: std::collections::BTreeMap<String, (i64, i64)>| -> Value {
        Value::Object(
            m.into_iter()
                .map(|(k, (n, b))| (k, json!({ "n": n, "bytes": b })))
                .collect(),
        )
    };
    json!({
        "files": files.len() as i64,
        "bytes": bytes,
        "largest_bytes": largest,
        "read": shape(read),
        "unread": shape(unread),
        "unread_files": unread_files,
    })
}

/// The formats worth opening here: the ones we would expect under data/ anyway.
/// A spreadsheet or an image is left to the inventory.
fn extdata_is_readable(lower: &str) -> bool {
    lower.ends_with(".rds")
        || lower.ends_with(".rda")
        || lower.ends_with(".rdata")
        || [".csv", ".tsv", ".tab", ".txt", ".psv", ".dat"]
            .iter()
            .any(|e| lower.contains(e))
}

/// The title of each dataset's help page, keyed by the name it documents.
///
/// A catalogue that lists a hundred names and says nothing about any of them
/// is a poor catalogue, and the package has already written the sentence: an
/// Rd page for a dataset carries a title and an alias naming what it documents.
fn rd_titles(root: &Path) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Ok(rd) = std::fs::read_dir(root.join("man")) else { return out };
    let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths.iter().take(RD_FILE_CAP) {
        let is_rd = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("rd"))
            .unwrap_or(false);
        if !is_rd {
            continue;
        }
        let Ok(raw) = std::fs::read(&p) else { continue };
        if raw.len() > RD_SIZE_CAP {
            continue;
        }
        let text = String::from_utf8_lossy(&raw);
        let Some(title) = rd_field(&text, "title") else { continue };
        let title = rd_plain(&title);
        if title.is_empty() {
            continue;
        }
        for alias in rd_all_fields(&text, "alias") {
            out.entry(rd_plain(&alias)).or_insert_with(|| title.clone());
        }
    }
    out
}

const RD_FILE_CAP: usize = 4000;
const RD_SIZE_CAP: usize = 1 << 20;

/// The body of the first `\tag{...}`, counting braces so a title holding its
/// own markup comes back whole.
fn rd_field(text: &str, tag: &str) -> Option<String> {
    rd_all_fields(text, tag).into_iter().next()
}

fn rd_all_fields(text: &str, tag: &str) -> Vec<String> {
    let needle = format!("\\{tag}");
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(hit) = text[from..].find(&needle) {
        let mut i = from + hit + needle.len();
        // Only a brace may follow, or this is a longer tag that merely starts
        // the same way.
        while i < b.len() && (b[i] == b' ' || b[i] == b'\n' || b[i] == b'\r' || b[i] == b'\t') {
            i += 1;
        }
        if i >= b.len() || b[i] != b'{' {
            from = from + hit + needle.len();
            continue;
        }
        let start = i + 1;
        let mut depth = 1i32;
        let mut j = start;
        while j < b.len() && depth > 0 {
            match b[j] {
                b'{' if j == 0 || b[j - 1] != b'\\' => depth += 1,
                b'}' if j == 0 || b[j - 1] != b'\\' => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        if depth == 0 {
            out.push(text[start..j - 1].to_string());
        }
        from = j.min(b.len());
    }
    out
}

/// Rd markup as Markdown.
///
/// A title is written for a reader and often marks up a name or a phrase.
/// Throwing the markup away loses the distinction between a word and the name
/// of a function; carrying it as Rd puts a backslash in the middle of a
/// sentence. Markdown is what the page it lands on can render.
fn rd_plain(s: &str) -> String {
    let out = rd_to_markdown(s, 0);
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(300).collect()
}

fn rd_to_markdown(s: &str, depth: u32) -> String {
    if depth > 8 {
        return String::new();
    }
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'\\' {
            let c = s[i..].chars().next().unwrap_or(' ');
            out.push(if c == '\n' || c == '\t' || c == '\r' { ' ' } else { c });
            i += c.len_utf8();
            continue;
        }
        // An escaped brace or backslash stands for itself.
        if i + 1 < b.len() && matches!(b[i + 1], b'{' | b'}' | b'\\' | b'%') {
            out.push(b[i + 1] as char);
            i += 2;
            continue;
        }
        let name_start = i + 1;
        let mut j = name_start;
        while j < b.len() && (b[j].is_ascii_alphanumeric()) {
            j += 1;
        }
        let cmd = &s[name_start..j];
        if j < b.len() && b[j] == b'{' {
            let start = j + 1;
            let mut depth_b = 1i32;
            let mut k = start;
            while k < b.len() && depth_b > 0 {
                match b[k] {
                    b'{' if b[k - 1] != b'\\' => depth_b += 1,
                    b'}' if b[k - 1] != b'\\' => depth_b -= 1,
                    _ => {}
                }
                k += 1;
            }
            let inner = if depth_b == 0 { &s[start..k - 1] } else { &s[start..] };
            let rendered = rd_to_markdown(inner, depth + 1);
            match cmd {
                "code" | "command" | "env" | "file" | "kbd" | "option" | "pkg" | "samp" => {
                    if !rendered.is_empty() {
                        out.push('`');
                        out.push_str(&rendered);
                        out.push('`');
                    }
                }
                "emph" | "var" | "dfn" | "cite" => {
                    out.push('*');
                    out.push_str(&rendered);
                    out.push('*');
                }
                "strong" | "bold" => {
                    out.push_str("**");
                    out.push_str(&rendered);
                    out.push_str("**");
                }
                "dQuote" => {
                    out.push('"');
                    out.push_str(&rendered);
                    out.push('"');
                }
                "sQuote" => {
                    out.push('\'');
                    out.push_str(&rendered);
                    out.push('\'');
                }
                // A cross-reference reads as the thing it names.
                _ => out.push_str(&rendered),
            }
            i = k;
        } else {
            // A bare command such as \R or \dots stands for a word.
            out.push_str(match cmd {
                "R" => "R",
                "dots" | "ldots" => "...",
                _ => "",
            });
            i = j.max(i + 1);
        }
    }
    out
}

/// The extensions data() looks for, in the order it prefers them. When several
/// files under data/ share a dataset name, it takes the first of these that
/// loads and never reads the rest, so a package carrying both a .rda and a
/// .csv of one dataset has only the .rda in it as far as anyone can tell.
const DATA_EXTS_BY_PRECEDENCE: &[&str] = &[
    "R", "r", "RData", "rdata", "rda",
    "tab", "txt", "TXT", "tab.gz", "txt.gz", "tab.bz2", "txt.bz2", "tab.xz", "txt.xz",
    "csv", "CSV", "csv.gz", "csv.bz2", "csv.xz",
];

/// Where a file sits in that order, or None when data() would not load it.
fn data_ext_rank(fname: &str) -> Option<usize> {
    DATA_EXTS_BY_PRECEDENCE.iter().position(|ext| {
        let suffix = format!(".{ext}");
        fname.len() > suffix.len() && fname.ends_with(&suffix)
    })
}

/// How deep to follow directories under inst/extdata, and how much of a file to
/// take on. The tail is long: a package may keep an 11 MB sequence file here,
/// and fingerprinting one describes nothing a reader would ask about.
const EXTDATA_DEPTH: u32 = 3;
const EXTDATA_PARSE_LIMIT: u64 = 8 << 20;
const EXTDATA_FILE_CAP: usize = 200;
const EXTDATA_NAME_CAP: usize = 100;

fn walk_extdata(dir: &Path, depth: u32, out: &mut Vec<std::path::PathBuf>) {
    if depth == 0 || out.len() >= EXTDATA_FILE_CAP {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if out.len() >= EXTDATA_FILE_CAP {
            return;
        }
        if p.is_dir() {
            walk_extdata(&p, depth - 1, out);
        } else {
            out.push(p);
        }
    }
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
    fn cols(r: &Value) -> Vec<String> {
        r["columns"]
            .as_array()
            .map(|a| a.iter().map(|c| s(c, "name")).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_whitespace_table_is_not_split_on_tabs() {
        // read.table's default separator is a run of whitespace. Splitting on
        // the tab character instead collapsed these files into one column
        // carrying the entire header as its name.
        let r = by_name("txt_plain");
        assert_eq!(r["nrow"], 3);
        assert_eq!(r["ncol"], 3);
        assert_eq!(cols(&r), ["grade", "sex", "score"]);
    }

    #[test]
    fn a_header_one_field_short_names_the_rows() {
        // Four fields per row against three names means the first is a row name
        // rather than data, which is how R reads it.
        let r = by_name("tab_rownames");
        assert_eq!(r["ncol"], 3);
        assert_eq!(cols(&r), ["Expt", "Run", "Speed"]);
        assert_eq!(r["nrow"], 3);
        // The shape alone does not prove it: dropping the rule keeps three
        // columns and shifts the values along by one. Expt is 1, 1, 2 once the
        // row names are out of the way, and 1, 2, 3 while they are still in it.
        assert_eq!(r["columns"][0]["col_max"], 2);
    }

    #[test]
    fn a_csv_under_data_is_separated_by_semicolons() {
        // Not what the name suggests, but it is what data() does, so a comma
        // file put here really does load as a single column and the catalogue
        // should say so rather than quietly reading it the sensible way.
        let semi = by_name("csv_semicolon");
        assert_eq!(semi["ncol"], 3);
        assert_eq!(cols(&semi), ["height", "weight", "sex"]);
        let comma = by_name("csv_comma");
        assert_eq!(comma["ncol"], 1);
        assert_eq!(cols(&comma), ["height.weight.sex"]);
    }

    #[test]
    fn quotes_protect_a_separator_inside_a_field() {
        let r = by_name("csv_quoted");
        assert_eq!(r["ncol"], 2);
        assert_eq!(cols(&r), ["city", "pop"]);
        assert_eq!(r["nrow"], 2);
    }

    #[test]
    fn a_column_is_summarised_the_way_summary_would() {
        // Every number here was checked against summary() and quantile() on the
        // same data. A range says where the values stop; these say where they
        // sit, which is the difference between a column running to one large
        // outlier and one spread evenly over the same interval.
        let r = by_name("summary_kinds");
        let by = |n: &str| -> Value {
            r["columns"].as_array().unwrap().iter()
                .find(|c| s(c, "name") == n).unwrap().clone()
        };
        let num = by("num");
        assert_eq!(num["mean"], 27.1875);
        assert_eq!(num["q1"], 2.25);
        assert_eq!(num["median"], 3.5);
        assert_eq!(num["q3"], 28.4375);

        // A logical column's range is nought to one, which says nothing.
        let lgl = by("lgl");
        assert_eq!(lgl["n_true"], 3);
        assert_eq!(lgl["n_false"], 1);
        assert!(lgl.get("col_min").is_none());

        let chr = by("chr");
        assert_eq!(chr["n_blank"], 1);
        assert_eq!(chr["min_nchar"], 0);
        assert_eq!(chr["max_nchar"], 3);

        // The level list says what is possible; the counts say what is there.
        let fac = by("fac");
        let counts = fac["level_counts"].as_array().unwrap();
        assert_eq!(counts.len(), 3);
        assert_eq!(s(&counts[0], "level"), "a");
        assert_eq!(counts[0]["n"], 2);

        // A date is a number of days, and summarised as one.
        assert!(by("dat").get("median").is_some());
    }

    #[test]
    fn a_bare_vector_says_what_it_holds() {
        // A length alone does not distinguish a column of numbers from a
        // column of names, and the range was being worked out and then kept
        // only for the columns of a table.
        let n = by_name("vec_numeric");
        assert_eq!(s(&n, "type"), "numeric");
        assert_eq!(n["length"], 5);
        assert_eq!(n["n_missing_total"], 1);
        assert_eq!(n["col_min"], 1.5);
        assert_eq!(n["col_max"], 100.25);

        let c = by_name("vec_character");
        assert_eq!(s(&c, "type"), "character");
        // Distinct values, not counting the absence of one: summary() reports
        // N.unique this way, and how many are missing is its own field.
        assert_eq!(c["n_unique"], 2);
        assert_eq!(c["n_missing_total"], 1);

        let f = by_name("vec_factor");
        assert_eq!(s(&f, "type"), "factor");
        assert_eq!(f["n_levels"], 3);
    }

    #[test]
    fn a_grid_is_summarised_whole() {
        // A matrix, an array and a raster have no columns to hang a summary
        // on, so the numbers describe all of the cells at once. Each was
        // checked against summary() of the same values.
        let m = by_name("mat_named"); // matrix(1:12, nrow = 3)
        assert_eq!(s(&m, "summary_over"), "cells");
        assert_eq!(m["mean"], 6.5);
        assert_eq!(m["q1"], 3.75);
        assert_eq!(m["q3"], 9.25);

        let a = by_name("arr_3d"); // array(1:24, dim = c(2, 3, 4))
        assert_eq!(s(&a, "summary_over"), "cells");
        assert_eq!(a["mean"], 12.5);
        assert_eq!(a["q1"], 6.75);

        let r = by_name("raster_layer"); // cells 1:200
        assert_eq!(s(&r, "summary_over"), "cells");
        assert_eq!(r["mean"], 100.5);
        assert_eq!(r["q3"], 150.25);
    }

    #[test]
    fn a_raster_says_what_its_figures_cost_and_what_stands_for_nothing() {
        // The cell size makes two rasters comparable, the sentinel is what
        // wrecks a mean without saying so, and whether the cells are in hand
        // tells a reader whether any of this cost a gigabyte of reading.
        let r = by_name("raster_named_brick");
        assert_eq!(r["resolution"], serde_json::json!([0.5, 0.5]));
        assert_eq!(r["nodata_value"], -9999.0);
        assert_eq!(r["in_memory"], true);
        // Layers are named and few, so they are listed and ranged one by one
        // the way columns are, rather than folded into the whole.
        assert_eq!(r["layer_names"], serde_json::json!(["elev", "depth"]));
        assert_eq!(r["layer_min"], serde_json::json!([1.0, 2.0]));
        assert_eq!(r["layer_max"], serde_json::json!([200.0, 400.0]));
        assert_eq!(r["n_layers"], 2);
    }

    #[test]
    fn a_column_says_what_shape_its_values_take() {
        // Each figure checked against the same arithmetic in R.
        let r = by_name("shape_kinds");
        let by = |n: &str| -> Value {
            r["columns"].as_array().unwrap().iter()
                .find(|c| s(c, "name") == n).unwrap().clone()
        };
        let sk = by("skewed");
        assert_eq!(sk["skewness"], 9.08412);
        assert_eq!(sk["kurtosis"], 84.930144);
        // One value far outside the fences a boxplot would draw, which is what
        // separates a heavy tail from a clerical error.
        // Two beyond the usual fences: one far out, and one just past them,
        // which is what makes the width of the fence testable.
        assert_eq!(sk["n_outliers"], 2);
        assert_eq!(sk["n_outliers_high"], 2);
        assert!(sk.get("n_outliers_low").map(|v| v == 0).unwrap_or(true));

        // A column of codes has a commonest value worth naming; a column of
        // measurements does not, and past twenty distinct values none is named.
        let cd = by("codes");
        assert_eq!(cd["mode_value"], 1.0);
        assert_eq!(cd["mode_share"], 0.6);
        assert!(by_name("stats_kinds")["columns"].as_array().unwrap().iter()
            .find(|c| s(c, "name") == "rising").unwrap().get("n_outliers").is_none());
    }

    #[test]
    fn a_series_says_how_evenly_it_is_observed() {
        // A start, an end and a count are the same for a series observed daily
        // throughout and one with a fortnight missing from the middle.
        let ok = by_name("z_regular");
        assert_eq!(ok["index_delta"], 1.0);
        assert_eq!(ok["index_regular"], true);
        assert!(ok.get("index_n_gaps").is_none());

        let gap = by_name("z_gappy");
        assert_eq!(gap["index_delta"], 1.0);
        assert_eq!(gap["index_regular"], false);
        assert_eq!(gap["index_n_gaps"], 1);
        assert_eq!(gap["index_max_gap"], 14.0);
    }

    #[test]
    fn a_symmetric_matrix_counts_both_halves() {
        // It stores one triangle, so every off-diagonal entry it holds stands
        // for two. Reporting what is stored understates it by nearly half;
        // this is the number nnzero() gives.
        let r = by_name("sym_sparse");
        assert_eq!(s(&r, "matrix_shape"), "symmetric");
        assert_eq!(r["n_stored"], 4);
        assert_eq!(r["n_nonzero"], 6);
        // A general matrix stores everything it has, so the two agree and only
        // one of them is worth writing.
        assert!(by_name("spm_dgc").get("n_nonzero").is_none());
    }

    #[test]
    fn a_column_says_how_spread_ordered_and_gapped_it_is() {
        // All of it on the pass the values are already read on.
        let r = by_name("stats_kinds");
        let by = |n: &str| -> Value {
            r["columns"].as_array().unwrap().iter()
                .find(|c| s(c, "name") == n).unwrap().clone()
        };
        // Checked against sd() on the same numbers.
        assert_eq!(by("spread")["sd"], 2.13809);

        // Order is what makes a column an index rather than a measurement.
        assert_eq!(s(&by("rising"), "sort_order"), "ascending");
        assert_eq!(s(&by("falling"), "sort_order"), "descending");

        // A double column holding only whole numbers is a count that was
        // stored as a measurement.
        assert_eq!(by("wholes")["is_integer_valued"], true);
        assert!(by("rising").get("is_integer_valued").is_none()); // already integer

        // Where the gaps fall, not only how many there are.
        let late = by("late");
        assert_eq!(late["n_missing_leading"], 3);
        assert_eq!(late["max_missing_run"], 3);
        assert!(late.get("n_missing_trailing").is_none());

        let patchy = by("patchy");
        assert_eq!(patchy["max_missing_run"], 1);
        assert_eq!(patchy["n_missing_trailing"], 1);
        assert!(patchy.get("n_missing_leading").is_none());
    }

    #[test]
    fn a_list_says_what_is_in_its_slots() {
        // A list was a length and nothing else, so a train and test split, a
        // set of fitted models and a tree of settings were the same record.
        let sp = by_name("list_of_frames");
        assert_eq!(sp["element_names"], serde_json::json!(["train", "test"]));
        assert_eq!(s(&sp, "element_class"), "data.frame");
        // The rows inside the slots are the observations the dataset has.
        assert_eq!(sp["inner_nrow_total"], 15);
        assert_eq!(sp["inner_names"], serde_json::json!(["x", "y"]));

        // Slots of different kinds are listed rather than summarised as one.
        let par = by_name("list_parallel");
        assert_eq!(par["element_classes"], serde_json::json!(["integer", "character", "numeric"]));
        assert_eq!(par["element_len_min"], 5);
        assert_eq!(par["element_len_max"], 5);

        // Depth separates a flat list from a tree.
        assert_eq!(by_name("list_deep")["max_depth"], 4);
        assert!(by_name("list_parallel").get("max_depth").is_none());
    }

    #[test]
    fn a_table_is_found_however_deep_it_sits() {
        // Folds keep their tables a level below the slots, so counting only
        // direct slots reported forty-three rows as none at all.
        let r = by_name("nested_folds");
        assert_eq!(s(&r, "element_class"), "list");
        assert_eq!(r["max_depth"], 3);
        assert_eq!(r["inner_nrow_total"], 43);
        assert_eq!(r["inner_names"], serde_json::json!(["x"]));
    }

    #[test]
    fn slots_holding_nothing_are_counted() {
        // A NULL and an empty vector take up a slot and are not there, so a
        // list of four with two empty is not the list its length suggests.
        let r = by_name("holey_list");
        assert_eq!(r["length"], 4);
        assert_eq!(r["n_empty_slots"], 2);
        assert!(by_name("unnamed_list").get("n_empty_slots").is_none());
        // Nothing names an unnamed list.
        assert!(by_name("unnamed_list").get("element_names").is_none());
    }

    #[test]
    fn an_s4_slot_has_the_size_it_states() {
        // Its length is not its size: a matrix says how big it is in a slot,
        // so a list of two of them is not a list of nothing.
        let r = by_name("list_of_s4");
        assert_eq!(s(&r, "element_class"), "ddiMatrix");
        assert_eq!(r["element_len_total"], 25); // three by three plus four by four
    }

    #[test]
    fn a_nested_table_reports_the_rows_it_actually_holds() {
        // The outer frame counts groups, not observations. Fifteen rows were
        // being catalogued as three, and the inner columns, which are the whole
        // reason for the nesting, were invisible.
        let r = by_name("nested_tibble");
        assert_eq!(r["nrow"], 3);
        let cols = r["columns"].as_array().unwrap();
        let data = cols.iter().find(|c| s(c, "name") == "data").unwrap();
        assert_eq!(data["inner_nrow_total"], 15);
        assert_eq!(data["inner_names"], serde_json::json!(["v", "w"]));
        assert_eq!(data["element_len_total"], 6);
    }

    #[test]
    fn what_is_written_beside_the_values_is_kept() {
        // The order a factor declares is the order it sorts and plots in, and
        // is not the order of its counts.
        let f = by_name("ord_factor");
        assert_eq!(f["levels"], serde_json::json!(["lo", "mid", "hi"]));
        assert_eq!(f["is_ordered"], true);

        // A moment without a zone is not a moment.
        assert_eq!(s(&by_name("tz_stamps"), "tz"), "America/Chicago");

        // And a number without its unit is not a measurement.
        assert_eq!(s(&by_name("unit_speeds"), "units"), "m/s");
    }

    #[test]
    fn a_table_of_counts_carries_its_labels() {
        // Two by two of numbers says nothing; the same labelled by treatment
        // and outcome says what the study found.
        let r = by_name("labelled_table");
        let dn = r["dimnames"].as_array().unwrap();
        assert_eq!(dn.len(), 2);
        assert_eq!(s(&dn[0], "name"), "treat");
        assert_eq!(dn[0]["labels"], serde_json::json!(["A", "B"]));
        assert_eq!(s(&dn[1], "name"), "outcome");
        assert_eq!(dn[1]["labels"], serde_json::json!(["hit", "miss"]));
    }

    #[test]
    fn a_table_says_how_it_is_keyed_and_grouped() {
        // A keyed table is a different thing to work with from the same rows
        // unkeyed, and the key is the nearest thing it has to a primary key.
        let k = by_name("keyed_dt");
        assert_eq!(k["dt_key"], serde_json::json!(["a"]));
        assert_eq!(k["dt_indices"], serde_json::json!(["b"]));

        let g = by_name("grouped_tbl");
        assert_eq!(g["group_vars"], serde_json::json!(["g"]));
        assert_eq!(g["n_groups"], 2);
    }

    #[test]
    fn a_missing_number_says_which_kind_of_missing_it_is() {
        // is.na() is true for both, which is why they collapse into one count
        // wherever missingness is what matters. They are different findings: a
        // column of NA is a survey nobody answered and a column of NaN is a
        // division that went wrong. R distinguishes them by the payload it
        // writes into NA_real_, and the bytes carry it.
        let r = by_name("na_and_nan");
        let cols = r["columns"].as_array().unwrap();
        let by = |n: &str| cols.iter().find(|c| c["name"] == n).unwrap().clone();

        // Two NA, three NaN, four finite, two +Inf and one -Inf.
        let m = by("mixed");
        assert_eq!(m["n_missing"], 5, "R counts NaN among the missing, and so does this");
        assert_eq!(m["n_nan"], 3, "three of those five were NaN");
        assert_eq!(m["n_infinite"], 3);
        assert_eq!(m["n_infinite_pos"], 2, "counted by sign, not flagged at each end");
        assert_eq!(m["n_infinite_neg"], 1);
        // An infinity is not missing, so it is not in that count.
        assert_eq!(m["mean"], 2.5, "and the finite values are what the mean is taken over");

        // Neither kind is reported where it is not present.
        let na = by("just_na");
        assert_eq!(na["n_missing"], 3);
        assert!(na.get("n_nan").is_none(), "a column of plain NA reports no NaN");
        let nan = by("just_nan");
        assert_eq!(nan["n_missing"], 2);
        assert_eq!(nan["n_nan"], 2, "and a column of NaN reports every one");
        assert!(nan.get("n_infinite").is_none());
    }

    #[test]
    fn a_run_of_missing_values_is_measured_where_it_falls() {
        // Three at the front, three at the back, none longer than three. A
        // column read in order is a different thing from a bag of values, and
        // where the gaps are is the difference.
        let c = by_name("na_and_nan")["columns"].as_array().unwrap().iter()
            .find(|c| c["name"] == "runs").unwrap().clone();
        assert_eq!(c["n_missing"], 6);
        assert_eq!(c["n_missing_leading"], 3);
        assert_eq!(c["n_missing_trailing"], 3);
        assert_eq!(c["max_missing_run"], 3);
        // Scattered singles are not a run, whatever the total.
        let na = by_name("na_and_nan")["columns"].as_array().unwrap().iter()
            .find(|c| c["name"] == "just_na").unwrap().clone();
        assert_eq!(na["n_missing"], 3);
        assert_eq!(na["max_missing_run"], 1, "three separate gaps are not a gap of three");
    }

    #[test]
    fn a_summary_does_not_contradict_its_own_bounds() {
        // Rounding to six decimal places annihilated everything below about a
        // millionth, so a column of picoseconds reported a mean, a median and
        // a standard deviation of zero sitting between a minimum and a maximum
        // that were not zero. The bounds do not come through the rounding, so
        // the summary disagreed with itself.
        let t = by_name("tiny_values");
        let cols = t["columns"].as_array().unwrap();
        let by = |n: &str| -> Value {
            cols.iter().find(|c| c["name"] == n).unwrap().clone()
        };
        for name in ["pico", "nano"] {
            let c = by(name);
            let f = |k: &str| c[k].as_f64().unwrap_or(f64::NAN);
            let (lo, hi) = (f("col_min"), f("col_max"));
            assert!(lo > 0.0 && hi > 0.0, "{name} has non-zero bounds to begin with");
            for k in ["mean", "median", "q1", "q3"] {
                assert!(f(k) > 0.0, "{name}'s {k} is not rounded away to zero");
                assert!(
                    f(k) >= lo && f(k) <= hi,
                    "{name}'s {k} lies between its own bounds"
                );
            }
            assert!(f("sd") > 0.0, "{name} has a standard deviation, three distinct values apart");
        }
        // The mean of 1, 2 and 3 picoseconds is 2 picoseconds.
        assert!((by("pico")["mean"].as_f64().unwrap() - 2e-12).abs() < 1e-18);
        // And the other end keeps the places it had: six decimals on a value
        // with only four digits after its leading two, which taking
        // significant digits alone would have cost.
        let shape = by_name("shape_kinds");
        let skewed = shape["columns"].as_array().unwrap().iter()
            .find(|c| c["name"] == "skewed").unwrap().clone();
        assert_eq!(skewed["kurtosis"], 84.930144);
    }

    #[test]
    fn a_grid_says_which_way_its_variation_runs() {
        // Reading every cell as one vector gives these two the same summary:
        // one is the other transposed, so the multiset of values is identical.
        // The margins are the only thing that tells them apart.
        let r = by_name("rows_vary");
        let c = by_name("cols_vary");
        for k in ["mean", "median", "col_min", "col_max"] {
            assert_eq!(r[k], c[k], "the cell summary cannot tell a matrix from its transpose ({k})");
        }

        let f = |v: &Value, k: &str| v[k].as_f64().unwrap_or(f64::NAN);
        // rows_vary has a level per row, so its row means spread and its
        // column means barely move.
        assert!(
            f(&r, "row_mean_sd") > 40.0,
            "row means spread when the rows carry the structure"
        );
        assert!(
            f(&r, "col_mean_sd") < 1.0,
            "and the column means barely move"
        );
        // Transposed, the two swap exactly.
        assert!((f(&c, "col_mean_sd") - f(&r, "row_mean_sd")).abs() < 1e-6);
        assert!((f(&c, "row_mean_sd") - f(&r, "col_mean_sd")).abs() < 1e-6);
        // Both margins are means of the same cells, so both average to the
        // grand mean whichever way they are taken.
        assert!((f(&r, "row_mean_mean") - f(&r, "col_mean_mean")).abs() < 1e-6);
        assert!((f(&r, "row_mean_mean") - f(&r, "mean")).abs() < 1e-6);
        // Six numbers in summary()'s own order.
        for k in ["row_mean_min", "row_mean_q1", "row_mean_median", "row_mean_mean",
                  "row_mean_q3", "row_mean_max"] {
            assert!(r[k].is_number(), "{k} is reported");
        }
        assert!(f(&r, "row_mean_min") <= f(&r, "row_mean_median"));
        assert!(f(&r, "row_mean_median") <= f(&r, "row_mean_max"));
    }

    #[test]
    fn a_margin_mean_skips_the_missing_rather_than_being_poisoned_by_one() {
        // rowMeans(m, na.rm = TRUE) on the fixture is 5, 8, 7.5. One missing
        // value taken into the sum would make the whole row unreportable.
        let m = by_name("margin_na");
        let f = |k: &str| m[k].as_f64().unwrap_or(f64::NAN);
        assert!((f("row_mean_min") - 5.0).abs() < 1e-6);
        assert!((f("row_mean_max") - 8.0).abs() < 1e-6);
        assert!((f("row_mean_median") - 7.5).abs() < 1e-6);
        assert!(f("row_mean_min").is_finite(), "no NaN reaches the record");

        // One row is not a distribution of row means.
        let thin = by_name("margin_thin");
        assert!(thin.get("row_mean_sd").is_none(), "a single row has no margin to summarise");
        assert!(thin.get("col_mean_sd").is_none(), "and neither margin is reported for it");
    }

    #[test]
    fn a_list_says_how_big_each_element_is_on_its_own() {
        // The aggregate could already say three elements and two hundred and
        // seven nested rows. Neither number lets a reader ask how big fold2
        // was, and it is the one they have.
        let l = by_name("uneven_folds");
        assert_eq!(l["element_inner_nrow"], serde_json::json!([50, 150, 7]));
        assert_eq!(l["inner_nrow_total"], serde_json::json!(207));
        assert_eq!(l["element_lens"], serde_json::json!([1, 1, 1]));
        assert_eq!(l["element_names"], serde_json::json!(["fold1", "fold2", "fold3"]));

        // A list holding no tables has no per-element count to report, rather
        // than a row of zeros.
        let h = by_name("holey_list");
        assert!(h.get("element_inner_nrow").is_none());
        // Lengths are still kept, because those every list has.
        assert_eq!(h["element_lens"], serde_json::json!([0, 1, 3, 0]));
    }

    #[test]
    fn a_table_says_which_kind_of_table_it_is() {
        // All of these inherit from data.frame, and telling them apart means
        // reading a whole inheritance chain and knowing its precedence. An
        // exact match on the class counts no tibbles; a substring match counts
        // everything. Naming it once removes the choice.
        assert_eq!(s(&by_name("flav_plain"), "frame_class"), "data.frame");
        assert_eq!(s(&by_name("flav_tibble"), "frame_class"), "tibble");
        assert_eq!(s(&by_name("flav_data_table"), "frame_class"), "data.table");

        // A grouped frame is still a tibble, and says separately that it
        // carries grouping.
        let g = by_name("flav_grouped");
        assert_eq!(s(&g, "frame_class"), "tibble");
        assert_eq!(g["is_grouped"], true);
        assert!(by_name("flav_tibble").get("is_grouped").is_none());

        // Spatialness is a different question from which kind of table it is.
        let sf = by_name("real_sf_nc");
        assert_eq!(s(&sf, "frame_class"), "data.frame");
        assert!(sf["is_spatial"].as_bool().unwrap_or(false));
    }

    #[test]
    fn a_dense_grid_of_zeros_says_so() {
        // Sparseness is a matter of degree. A matrix that is nineteen
        // twentieths zeros has the same shape of data as a sparse one and no
        // class to announce it.
        let r = by_name("mostly_zero");
        assert_eq!(r["n_zero"], 95);
        assert_eq!(r["p_zero"], 0.95);
        // And one with none says none rather than staying silent.
        assert_eq!(by_name("mat_named")["n_zero"], 0);
    }

    #[test]
    fn a_sparse_matrix_summarises_what_it_stores() {
        // The non-zeros are the data. A mean over them is not a mean over the
        // grid they sit in, so the record says which it is, and n_stored and
        // n_cells beside it say how far apart those two numbers are.
        let r = by_name("spm_dgc");
        assert_eq!(s(&r, "summary_over"), "stored values");
        assert_eq!(r["mean"], 2.0);
        assert_eq!(r["n_stored"], 3);
        assert_eq!(r["n_cells"], 64);

        // A logical sparse matrix counts rather than averages.
        let l = by_name("spm_lgc");
        assert_eq!(s(&l, "summary_over"), "stored values");
        assert!(l.get("n_true").is_some());
        assert!(l.get("mean").is_none());
    }

    #[test]
    fn an_infinite_value_is_counted_rather_than_left_as_a_blank() {
        // JSON cannot write an infinity, so a maximum that is one comes out as
        // null, which reads the same as never having been worked out. One
        // infinity among ninety-nine ordinary numbers is enough to lose the
        // maximum of the column silently.
        let r = by_name("with_infinity");
        let cols = r["columns"].as_array().unwrap();
        let v = cols.iter().find(|c| s(c, "name") == "v").unwrap();
        // Every figure is over the finite values, so none of them is missing:
        // the largest finite value is 99, and that the real maximum runs off
        // the end is said separately rather than left as a blank.
        assert_eq!(v["n_infinite"], 1);
        assert_eq!(v["max_infinite"], true);
        assert!(v.get("min_infinite").is_none());
        assert_eq!(v["col_max"], 99.0);
        assert_eq!(v["col_min"], 1.0);
        assert_eq!(v["mean"], 50.0);
        assert_eq!(v["median"], 50.0);

        // A column without one says nothing about infinities at all, and its
        // figures are unchanged.
        let w = cols.iter().find(|c| s(c, "name") == "w").unwrap();
        assert!(w.get("n_infinite").is_none());
        assert!(w.get("max_infinite").is_none());
        assert_eq!(w["col_max"], 100.0);
        assert_eq!(w["mean"], 50.5);
    }

    #[test]
    fn everything_holding_values_is_summarised_or_says_why_not() {
        // The one that fails when a new kind of object arrives and nobody
        // decides what its summary should be. Anything absent from the list
        // below is expected to describe its values.
        let excused: &[(&str, &str)] = &[
            ("data.frame", "a frame with no rows has no values"),
            ("list", "a list holds no single kind of value"),
            ("graph", "a graph is edges, not a grid of values"),
            ("object", "a function is not data"),
            ("(none)", "read as a class name only"),
            ("ngCMatrix", "a pattern matrix stores positions, not values"),
            ("ddiMatrix", "a unit diagonal stores nothing"),
            ("PackedSpatRaster", "terra keeps its cells in a blob we do not open"),
            ("PackedSpatVector", "terra keeps its cells in a blob we do not open"),
        ];
        for r in records() {
            let kind = {
                let k = s(&r, "kind");
                if k.is_empty() { "(none)".to_string() } else { k }
            };
            if excused.iter().any(|(k, _)| *k == kind) {
                continue;
            }
            // Read for their size rather than their contents: there is a
            // length and a fingerprint, and nothing to average.
            if matches!(s(&r, "type").as_str(), "raw" | "complex") {
                continue;
            }
            let has_own = r.get("mean").is_some()
                || r.get("n_true").is_some()
                || r.get("min_nchar").is_some()
                || r.get("level_counts").is_some()
                || r.get("n_infinite").is_some();
            let has_cols = r["columns"].as_array().map(|cs| {
                cs.iter().any(|c| {
                    c.get("mean").is_some()
                        || c.get("n_true").is_some()
                        || c.get("min_nchar").is_some()
                        || c.get("level_counts").is_some()
                        || c.get("n_infinite").is_some()
                })
            }).unwrap_or(false);
            assert!(
                has_own || has_cols,
                "{} ({}) holds values and describes none of them",
                s(&r, "name"),
                kind
            );
        }
    }

    #[test]
    fn a_series_says_how_much_time_it_covers() {
        // Two series can share a start and a frequency and cover a century
        // apart, and reading that off a start and an end is arithmetic a
        // reader should not have to do.
        // 48 monthly observations run from the first to the forty-eighth, so
        // the span is forty-seven months expressed in years.
        let t = by_name("ts_month");
        assert!((t["ts_span"].as_f64().unwrap() - 47.0 / 12.0).abs() < 1e-6);

        // An index-based series measures its span in the index's own unit,
        // which its class names: seconds here, thirty hours of them.
        let x = by_name("xts_series");
        assert_eq!(s(&x, "index_class"), "POSIXct/POSIXt");
        assert_eq!(s(&x, "index_tz"), "UTC");
        assert_eq!(x["index_span"], 108000.0);
    }

    #[test]
    fn a_geometry_says_which_dimensions_it_has() {
        // sf prints the dimension beside the geometry type, and it is not in
        // the column's class: each geometry names its own.
        let flat = by_name("real_sf_nc");
        assert_eq!(s(&flat, "geom_dimension"), "XY");
        let tall = by_name("sf_three_d");
        assert_eq!(s(&tall, "geom_dimension"), "XYZ");
        assert_eq!(s(&tall, "geom_type"), "POINT");
    }

    #[test]
    fn a_spatial_frame_describes_itself_without_opening_its_columns() {
        // What sf prints above the table: the geometry type, the dimension,
        // the extent and the projection. They were reachable only by opening
        // the column list, which is not where a reader would look.
        let r = by_name("real_sf_nc");
        assert!(r["is_spatial"].as_bool().unwrap_or(false));
        assert!(!s(&r, "geom_type").is_empty());
        assert!(r["bbox"].as_array().map(|b| b.len() == 4).unwrap_or(false));
        assert!(r.get("crs_epsg").is_some());
        assert!(r.get("n_geometries").is_some());
    }

    #[test]
    fn a_series_is_summarised_as_well_as_dated() {
        // A time series had a start and a frequency and nothing about the
        // values, because the summary was being kept only for the columns of a
        // table and a series is a vector.
        let r = by_name("ts_month");
        assert_eq!(r["ts_frequency"], 12.0);
        assert_eq!(r["mean"], 24.5);
        assert_eq!(r["median"], 24.5);
        assert_eq!(r["q1"], 12.75);
        assert_eq!(r["q3"], 36.25);
    }

    #[test]
    fn a_raw_or_complex_vector_has_a_length_and_a_type() {
        // Their bytes are read for size rather than kept, which left them
        // described as objects of length zero.
        let r = by_name("vec_raw");
        assert_eq!(s(&r, "type"), "raw");
        assert_eq!(r["length"], 3);
        let c = by_name("vec_complex");
        assert_eq!(s(&c, "type"), "complex");
        assert_eq!(c["length"], 2);
        // Hashed on the way past, so they can still be told apart and still
        // reach anywhere that drops a dataset with no fingerprint.
        assert!(!s(&r, "content_fp").is_empty());
        assert!(!s(&c, "content_fp").is_empty());
        assert_eq!(s(&r, "content_fp"), s(&by_name("vec_raw_same"), "content_fp"));
        // One byte apart, so the same length and the same type: only the
        // contents can tell these two apart.
        assert_ne!(s(&r, "content_fp"), s(&by_name("vec_raw_diff"), "content_fp"));
        assert_ne!(s(&r, "content_fp"), s(&c, "content_fp"));
    }

    #[test]
    fn a_carriage_return_is_a_line_break() {
        // Written on a Mac before OS X: carriage returns and no line feeds.
        // Treating the file as one line reports a table as having no rows.
        let r = by_name("cr_endings");
        assert_eq!(r["nrow"], 3);
        assert_eq!(r["ncol"], 2);
        assert_eq!(cols(&r), ["a", "b"]);
    }

    #[test]
    fn a_file_that_is_not_what_its_name_says_is_said_to_be() {
        // data() reads a .csv with semicolons, so a comma-separated one really
        // does load as a single column named after the whole header. That is
        // what a reader gets and so it is what is reported, but the file is
        // plainly meant to be read another way and saying so is more use than
        // a column named after the header line.
        let r = by_name("comma_in_csv");
        assert_eq!(r["ncol"], 1);
        assert_eq!(s(&r, "delimiter_looks_like"), ",");
        assert_eq!(r["delimiter_would_give_ncol"], 3);
        // A file that is what it says carries no such claim.
        assert!(s(&by_name("csv_semicolon"), "delimiter_looks_like").is_empty());
    }

    #[test]
    fn a_dataset_takes_the_title_from_its_help_page() {
        // The alias names what the page documents, so one page can title
        // several datasets, and the markup in a title has to come off.
        let r = by_name("altrep_frame");
        assert_eq!(s(&r, "title"), "Readings from the `example` instrument");
        let other = by_name("plain_frame");
        assert_eq!(s(&other, "title"), "Readings from the `example` instrument");
        // A dataset with no page of its own says nothing rather than guessing.
        assert!(s(&by_name("ts_month"), "title").is_empty());
    }

    #[test]
    fn one_dataset_name_is_one_dataset() {
        // A package may carry the same name as both a saved image and a csv.
        // data() takes the image and never opens the csv, so describing both
        // puts a second row under a name that resolves to one dataset, and the
        // row it adds is for the copy nobody reads. In the wild that copy is
        // often the one the package neglected, which is how a dataset with
        // fifty columns came to be described as having one.
        let recs = records();
        let hits: Vec<_> = recs.iter().filter(|r| s(r, "name") == "one_per_name").collect();
        assert_eq!(hits.len(), 1, "one name, one record");
        assert_eq!(hits[0]["ncol"], 2);
        assert_eq!(s(&hits[0], "format"), "rda");
        assert_eq!(cols(hits[0]), ["from_rda", "second"]);
    }

    #[test]
    fn the_same_bytes_mean_different_things_by_directory() {
        // Identical content in both places. Under data/ the semicolon rule makes
        // it one column; under extdata no rule applies and it is an ordinary
        // comma file. Reading extdata by data()'s rules would flatten it.
        let under_data = by_name("csv_comma");
        let under_ext = by_name("ext_comma");
        assert_eq!(under_data["ncol"], 1);
        assert_eq!(under_ext["ncol"], 3);
        assert_eq!(cols(&under_ext), ["height", "weight", "sex"]);
    }

    #[test]
    fn an_rds_is_read_under_extdata_though_not_under_data() {
        // data() cannot load a .rds, so one under data/ is not a dataset. Under
        // extdata nothing is loaded by name anyway, and the file is readable.
        let r = by_name("ext_object");
        assert_eq!(r["nrow"], 4);
        assert_eq!(r["ncol"], 2);
        assert!(!s(&r, "content_fp").is_empty());
        assert!(records().iter().all(|r| s(r, "file") != "data/notdata.rds"));
    }

    #[test]
    fn every_record_says_which_directory_it_came_from() {
        for r in records() {
            let d = s(&r, "origin_dir");
            assert!(
                ["data", "sysdata", "extdata"].contains(&d.as_str()),
                "{} has no origin: {d:?}",
                s(&r, "name")
            );
        }
    }

    #[test]
    fn the_inventory_says_what_extdata_holds_without_opening_it() {
        let inv = extdata_inventory(Path::new("tests/fixtures/pkg"));
        assert_eq!(inv["files"], 5);
        // The formats we would expect under data/ anyway, opened and counted.
        assert_eq!(inv["read"]["csv"]["n"], 2);
        assert_eq!(inv["read"]["rds"]["n"], 1);
        assert_eq!(inv["read"]["tsv"]["n"], 1);
        // A spreadsheet is listed and left shut, but its size is still known:
        // that is the whole point of listing something we do not open.
        assert_eq!(inv["unread"]["xlsx"]["n"], 1);
        assert!(inv["unread"]["xlsx"]["bytes"].as_i64().unwrap_or(0) > 0);
        // The per-extension sizes account for the directory total.
        let summed: i64 = ["read", "unread"]
            .iter()
            .flat_map(|k| inv[k].as_object().unwrap().values())
            .map(|v| v["bytes"].as_i64().unwrap_or(0))
            .sum();
        assert_eq!(summed, inv["bytes"].as_i64().unwrap());
        assert!(inv["largest_bytes"].as_i64().unwrap_or(0) > 0);
        // The file we do not open is still named, and named relative to the
        // directory so a nested one is distinguishable from a top-level one.
        assert_eq!(inv["unread_files"], serde_json::json!(["ext_ignored.xlsx"]));
    }

    #[test]
    fn a_package_without_extdata_says_so() {
        let inv = extdata_inventory(Path::new("tests/fixtures"));
        assert!(inv.is_null());
    }

    #[test]
    fn a_format_we_have_no_reader_for_is_left_alone() {
        // A row carrying only a filename is not worth the storage.
        assert!(records().iter().all(|r| s(r, "name") != "ext_ignored"));
    }

    #[test]
    fn extdata_is_walked_into_its_subdirectories() {
        let r = by_name("ext_nested");
        assert_eq!(s(&r, "origin_dir"), "extdata");
        assert_eq!(s(&r, "file"), "inst/extdata/nested/ext_nested.csv");
    }

    #[test]
    fn the_save_format_from_before_2002_is_read() {
        // R wrote this until 1.4.0, and every binary dataset in a package whose
        // last release predates 2002 is in it. It is not a stream: the file
        // names its symbols, maps node numbers to identifiers, defines each
        // node, and names the root last, so the graph is rebuilt from the map.
        // Reading it as XDR failed on the first byte and lost the file.
        let r = by_name("df");
        assert_eq!(s(&r, "class"), "data.frame");
        assert_eq!(r["nrow"], 3);
        assert_eq!(r["ncol"], 2);
        assert_eq!(r["format_version"], 1);
        assert_eq!(s(&r, "confidence"), "exact");
        assert!(!s(&r, "content_fp").is_empty());
        // The factor keeps its levels, which are reached through the attribute
        // list of a column rather than through the frame.
        let cols = r["columns"].as_array().unwrap();
        assert_eq!(s(&cols[0], "type"), "numeric");
        assert_eq!(s(&cols[1], "type"), "factor");
        assert_eq!(cols[1]["n_unique"], 2);
    }

    #[test]
    fn the_old_format_carries_its_special_values() {
        // R writes NA, NaN, Inf and -Inf into this format by name. Both of the
        // first two count as missing, as R counts them, and neither infinity
        // does. A second object shares the file, the way they do in the archive.
        let r = by_name("specials");
        assert_eq!(r["length"], 5);
        assert_eq!(r["n_missing_total"], 2);
        // The archive holds more than a thousand missing integers, so a whole
        // number written as NA has to survive as one rather than as a zero.
        let i = by_name("int_na");
        assert_eq!(i["length"], 4);
        assert_eq!(i["n_missing_total"], 2);
        assert_eq!(s(&r, "confidence"), "exact");
        assert!(!s(&r, "content_fp").is_empty());
    }

    #[test]
    fn a_ragged_file_is_not_padded_into_a_table() {
        // Two tabs in a row collapse to one separator under the whitespace rule,
        // so read.table refuses the file. Padding the short row instead reported
        // a tidy frame for something data() cannot load at all.
        let r = by_name("ws_ragged");
        assert_eq!(s(&r, "confidence"), "degraded");
        assert!(s(&r, "content_fp").is_empty());
    }

    #[test]
    fn a_value_holding_a_space_is_split_the_way_r_splits_it() {
        // R gets three columns here, but only because the leftmost field becomes
        // a row name and the space inside the value acts as a separator. The
        // catalogue should agree with what a caller actually loads.
        let r = by_name("ws_value_has_space");
        assert_eq!(r["nrow"], 2);
        assert_eq!(r["ncol"], 3);
        assert_eq!(cols(&r), ["name", "city", "n"]);
    }

    #[test]
    fn quotes_survive_whitespace_separation() {
        let r = by_name("ws_quoted");
        assert_eq!(r["ncol"], 2);
        assert_eq!(r["nrow"], 2);
        assert_eq!(r["columns"][0]["n_unique"], 2);
    }

    #[test]
    fn a_comment_does_not_become_data() {
        let r = by_name("txt_comments");
        assert_eq!(r["nrow"], 2);
        assert_eq!(r["ncol"], 2);
        assert_eq!(cols(&r), ["x", "y"]);
    }

    #[test]
    fn all_three_compressions_are_read() {
        for (name, comp) in [("txt_gz", "gzip"), ("txt_bz2", "bzip2"), ("txt_xz", "xz")] {
            let r = by_name(name);
            assert_eq!(s(&r, "compression"), comp, "{name}");
            assert_eq!(r["nrow"], 3, "{name}");
            assert_eq!(r["ncol"], 2, "{name}");
        }
    }

    #[test]
    fn a_file_data_cannot_load_is_not_called_a_dataset() {
        // .dat and .tsv have no entry in data(), so a file with either name sits
        // in data/ unreachable. Reporting one would put a dataset in the
        // catalogue that nobody can load.
        assert!(
            records().iter().all(|r| s(r, "name") != "notdata"),
            "a format data() cannot load was reported as a dataset"
        );
        assert!(
            records().iter().all(|r| s(r, "file") != "data/notdata.rds"),
            ".rds is not loadable from data/ and must not be reported"
        );
    }

    #[test]
    fn one_table_written_two_ways_carries_one_fingerprint() {
        let a = by_name("same_as_rda");
        let b = by_name("same_as_text");
        assert!(!s(&a, "content_fp").is_empty());
        assert_eq!(s(&a, "content_fp"), s(&b, "content_fp"));
        assert_eq!(s(&a, "schema_fp"), s(&b, "schema_fp"));
    }

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
    fn the_same_text_in_two_encodings_is_the_same_data() {
        // R records a string's encoding beside it. Reading latin1 as though it
        // were UTF-8 replaces every accented character with a marker, so the
        // text is wrong and the same table stored the other way stops matching
        // it. They carry one fingerprint because they are one table.
        let a = by_name("latin_txt");
        let b = by_name("utf8_txt");
        assert!(!s(&a, "content_fp").is_empty());
        assert_eq!(s(&a, "content_fp"), s(&b, "content_fp"));
    }

    #[test]
    fn a_string_carrying_an_attribute_is_read_past() {
        // A string has no attributes of its own, but an older file can still
        // have written one, and R reads it and throws it away. Leaving it in
        // the stream takes the next string with it.
        let r = by_name("s");
        assert_eq!(r["length"], 2);
        // Both strings are present. Leaving the attribute in the stream makes
        // the reader take it for the second string, which then reads as
        // missing: a wrong answer rather than a failure, and a different
        // fingerprint for data that has not changed.
        assert_eq!(r["n_missing_total"], 0);
        assert_eq!(s(&r, "confidence"), "exact");
    }

    #[test]
    fn a_builtin_does_not_cost_the_data_beside_it() {
        // Saving a builtin is unusual, but it names its function rather than
        // describing it, and reading it as anything else leaves the rest of
        // the file at the wrong offset.
        let r = by_name("after_builtin");
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
        let got = r["attrs_other"].as_array().cloned().unwrap_or_default();
        let named = |n: &str| got.iter().any(|a| a["name"] == n);
        for want in ["collected", "license", "source_url"] {
            assert!(named(want), "{want} should be reported");
        }
        // The names this reader does consume are not repeated back as unknowns.
        assert!(!named("class") && !named("names") && !named("row.names"));
        // Naming an attribute says it exists. What kind of thing it is and how
        // much of it there is says something about it, which for a class with
        // no reader of its own is the only description there will be.
        for a in &got {
            assert!(a["kind"].is_string(), "every leftover attribute says what it is");
            assert!(a["len"].is_number(), "and how long it is");
        }
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
        assert_eq!(recs.len(), 118, "one record per saved object");
        // Not everything saved under data/ is data. These carry behaviour rather
        // than observations, so there is nothing to fingerprint; what they must
        // still do is read cleanly, because a file that fails mid-object takes
        // every object after it down as well.
        let objects = [
            "oo_r6", "oo_refclass", "oo_s7", "cmp_fun", "builtin_fn",
            // terra keeps its values in a raw blob, so the grid is described but
            // the cells are not fingerprinted.
            "terra_packed",
        ];
        // Deliberately broken: R refuses it, so we must too.
        let unloadable = ["ws_ragged"];
        for r in &recs {
            let name = s(r, "name");
            let notes = s(r, "notes");
            if unloadable.contains(&name.as_str()) {
                assert_eq!(s(r, "confidence"), "degraded", "{name} should not load");
                continue;
            }
            assert!(
                !notes.contains("unhandled") && !notes.contains("truncated"),
                "{name} did not read: {notes}"
            );
            if objects.contains(&name.as_str()) {
                continue;
            }
            // A text file declares no types, so they are inferred and the record
            // says so. It still has to profile.
            if !notes.starts_with("text:") {
                assert_ne!(s(r, "confidence"), "degraded", "{name} degraded");
            }
            assert!(!s(r, "content_fp").is_empty(), "{name} has no fingerprint");
        }
    }

    /// A file written with classic Mac line endings is still a table.
    ///
    /// `str::lines()` splits on \n only, so a bare-\r file used to arrive as one
    /// line: the whole thing became the header, `rows` stayed empty, and the
    /// result was a "data frame" of zero rows and one column per field in the
    /// file. That is how a 2,537,269-column record with a 306 MB schema was made.
    #[test]
    fn cr_only_line_endings_parse_as_rows_not_as_one_header() {
        let text = "a,b,c\r1,2,3\r4,5,6\r";
        let (cols, names, nrow) = parse_table(&normalize_for_test(text), ',')
            .expect("a CR-terminated file is a table");
        assert_eq!(names.len(), 3, "three columns, not one per field in the file");
        assert_eq!(nrow, 2, "and two data rows");
        assert_eq!(cols.len(), 3);
    }

    #[test]
    fn crlf_line_endings_parse_as_rows() {
        let (_, names, nrow) = parse_table(&normalize_for_test("a\tb\r\n1\t2\r\n3\t4\r\n"), '\t')
            .expect("a CRLF file is a table");
        assert_eq!(names.len(), 2);
        assert_eq!(nrow, 2);
    }

    /// The normalization the readers apply, so these tests exercise the same rule
    /// decompress_text applies rather than restating it.
    fn normalize_for_test(t: &str) -> String {
        t.replace("\r\n", "\n").replace('\r', "\n")
    }

    /// The one above tests parse_table against text already normalized, which
    /// proves nothing about who normalizes it. This drives the real reader over a
    /// real file, which is the path a package takes.
    #[test]
    fn read_text_free_normalizes_line_endings_off_disk() {
        let dir = std::env::temp_dir().join(format!("rpkg_cr_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Wide and CR-only, the shape that produced a 2.5M-column record.
        let width = 300usize;
        let header: Vec<String> = (0..width).map(|i| format!("v{i}")).collect();
        let row: Vec<String> = (0..width).map(|i| i.to_string()).collect();
        let cr = format!("{}\r{}\r{}\r", header.join("\t"), row.join("\t"), row.join("\t"));
        let path = dir.join("cr_only.tsv");
        std::fs::write(&path, cr.as_bytes()).unwrap();

        let (cols, names, nrow, fmt, _comp, _alt) =
            read_text_free(&path).expect("a CR-only file still reads");
        assert_eq!(names.len(), width, "one column per field in the HEADER, not in the file");
        assert_eq!(nrow, 2, "both data rows were seen");
        assert_eq!(cols.len(), width);
        assert_eq!(fmt, "tsv", "the separator is sniffed from the real header line");

        // The defect this replaces: one line means one row of fields, so ncol
        // would have been width * 3 and nrow 0.
        assert_ne!(names.len(), width * 3, "the whole file is not the header");

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    /// A header with nothing under it is a line of text, not a dataset. Every
    /// per-column statistic would be vacuous.
    #[test]
    fn a_header_with_no_rows_is_not_a_table() {
        assert!(parse_table("a,b,c\n", ',').is_none(), "header only is refused");
        assert!(parse_table("a,b,c", ',').is_none(), "header with no terminator is refused");
        assert!(parse_table("\n\n", ',').is_none(), "blank lines are refused");
        assert!(parse_table("a,b\n1,2\n", ',').is_some(), "a header plus a row is a table");
    }

    /// Per-column detail is capped, and the cap must not move a fingerprint: the
    /// hash covers every column even when the emitted list stops.
    #[test]
    fn wide_frames_cap_emitted_detail_without_moving_the_fingerprint() {
        let width = COLUMN_DETAIL_CAP + 40;
        let mut header = Vec::with_capacity(width);
        let mut row = Vec::with_capacity(width);
        for i in 0..width {
            header.push(format!("c{i}"));
            row.push(i.to_string());
        }
        let text = format!("{}\n{}\n", header.join(","), row.join(","));
        let (cols, names, _) = parse_table(&text, ',').expect("a wide table still parses");
        let refs: Vec<&Node> = cols.iter().collect();
        let p = profile_columns(&refs, &names).expect("a wide frame profiles");

        assert_eq!(p.columns.len(), COLUMN_DETAIL_CAP, "the emitted list stops at the cap");
        assert_eq!(names.len(), width, "while the real width is untouched");

        // Changing a column PAST the cap must still change content_fp, which is
        // what proves the hash was not truncated along with the list.
        let mut row2 = row.clone();
        row2[width - 1] = "999999".to_string();
        let text2 = format!("{}\n{}\n", header.join(","), row2.join(","));
        let (cols2, names2, _) = parse_table(&text2, ',').unwrap();
        let refs2: Vec<&Node> = cols2.iter().collect();
        let p2 = profile_columns(&refs2, &names2).unwrap();
        assert_ne!(
            p.content_fp, p2.content_fp,
            "a column beyond the cap still contributes to the content fingerprint"
        );
    }

    /// A record never says that the reader stopped early: an extdata walk that
    /// hit its cap looks exactly like a package with 200 files, and a truncated
    /// attribute list looks exactly like an object with 24 attributes. That
    /// makes every downstream census of these a floor rather than a total, and
    /// the README is the only place a consumer can learn it. So the numbers
    /// there are these numbers, and moving a cap without saying so fails here.
    #[test]
    fn the_readme_states_the_bounds_the_records_do_not() {
        let readme = std::fs::read_to_string("README.md").expect("read README.md");
        for (what, phrase) in [
            ("the extdata file cap", format!("{EXTDATA_FILE_CAP} files")),
            ("the extdata walk depth", format!("depth {EXTDATA_DEPTH}")),
            ("the leftover-attribute cap", format!("{ATTRS_OTHER_CAP} entries")),
            ("the row sketch size", format!("bottom-{SKETCH_K}")),
            ("the per-column detail cap", format!("{COLUMN_DETAIL_CAP} columns")),
        ] {
            assert!(
                readme.contains(&phrase),
                "the README does not state {what}: no \"{phrase}\" in it"
            );
        }
    }
}
