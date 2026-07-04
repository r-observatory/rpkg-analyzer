// rpkg-analyzer: a pure function of one extracted R package source tree.
// Reads a directory, emits newline-delimited JSON metric records on stdout.
// This first cut covers the structure, DESCRIPTION (DCF), and NAMESPACE groups,
// which map one-to-one onto the current pipeline's structure.R / parse_dcf /
// parse_namespace.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tree_sitter::Parser;

// ---- file walking -----------------------------------------------------------

/// All files under `root`, relative to it, excluding the .git directory.
/// Mirrors what `git archive` would hand the pipeline (tracked tree, no .git).
fn list_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn rec(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            if name == ".git" {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                rec(&p, root, out);
            } else if let Ok(rel) = p.strip_prefix(root) {
                // package-root-relative, POSIX separators, no leading ./
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    rec(root, root, &mut out);
    out.sort();
    out
}

/// Binary / non-code extensions excluded from LOC (carried from structure.R).
fn is_noncode(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower == "md5" || lower.ends_with("/md5") {
        return true;
    }
    let ext = lower.rsplit('.').next().unwrap_or("");
    matches!(
        ext,
        "rda" | "rdata" | "pdf" | "png" | "jpg" | "jpeg" | "gif" | "bmp" | "svg" | "ico"
            | "woff" | "woff2" | "eot" | "ttf" | "otf" | "gz" | "zip" | "tar" | "bz2" | "xz"
            | "7z" | "dll" | "so" | "dylib" | "o" | "a" | "lib" | "pyd" | "class" | "jar"
            | "pyc" | "xlsx" | "xls" | "docx" | "doc" | "pptx" | "ppt" | "mp3" | "mp4"
            | "ogg" | "wav" | "avi" | "mov"
    )
}

/// R's line count: length(strsplit(content, "\n")) with the trailing-empty drop.
/// Rust's `lines()` reproduces that exactly (and handles CRLF).
fn loc(content: &str) -> usize {
    content.lines().count()
}

fn read(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

fn file_ext(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_string(),
        _ => "(none)".to_string(),
    }
}

// ---- DESCRIPTION (DCF) ------------------------------------------------------

/// Fold DCF continuation lines (leading whitespace) and return field -> value.
/// Last field wins on duplicates, matching read.dcf's single-record behaviour.
fn parse_dcf(text: &str) -> BTreeMap<String, String> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut cur_key: Option<String> = None;
    let mut cur_val = String::new();
    fn flush(fields: &mut BTreeMap<String, String>, k: &Option<String>, v: &str) {
        if let Some(k) = k {
            fields.insert(k.clone(), v.trim().to_string());
        }
    }
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            cur_val.push(' ');
            cur_val.push_str(line.trim());
        } else if let Some((k, v)) = line.split_once(':') {
            flush(&mut fields, &cur_key, &cur_val);
            cur_key = Some(k.trim().to_string());
            cur_val = v.trim().to_string();
        }
    }
    flush(&mut fields, &cur_key, &cur_val);
    fields
}

/// Parse a dependency field ("pkg (>= 1.0), other") into bare package names,
/// dropping version constraints and the pseudo-package R.
fn dep_names(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| {
            let s = s.trim();
            let name = s.split('(').next().unwrap_or(s).trim();
            name.to_string()
        })
        .filter(|s| !s.is_empty() && s != "R")
        .collect()
}

// ---- NAMESPACE (parsed as R via tree-sitter) --------------------------------

#[derive(Default)]
struct Namespace {
    exports: Vec<String>,
    export_patterns: usize,
    s3_methods: usize,
    export_classes: usize,
    export_methods: usize,
    import_from: usize,
    imports_whole: usize,
    use_dyn_lib: usize,
}

/// Walk the NAMESPACE AST and tally directive calls by their function name.
fn parse_namespace(src: &str, parser: &mut Parser) -> Namespace {
    let mut ns = Namespace::default();
    let Some(tree) = parser.parse(src, None) else {
        return ns;
    };
    let root = tree.root_node();
    let bytes = src.as_bytes();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "call" {
            if let Some(func) = n.child(0) {
                let name = func.utf8_text(bytes).unwrap_or("");
                let args = n.child_by_field_name("arguments");
                let arg_idents: Vec<String> = args
                    .map(|a| {
                        let mut c = a.walk();
                        a.children(&mut c)
                            .filter(|ch| ch.kind() == "argument")
                            .filter_map(|ch| ch.utf8_text(bytes).ok())
                            .map(|s| s.trim().trim_matches('"').to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                match name {
                    "export" => ns.exports.extend(arg_idents),
                    "exportPattern" => ns.export_patterns += 1,
                    "S3method" => ns.s3_methods += 1,
                    "exportClasses" => ns.export_classes += 1,
                    "exportMethods" => ns.export_methods += 1,
                    "importFrom" => ns.import_from += 1,
                    "import" => ns.imports_whole += 1,
                    "useDynLib" => ns.use_dyn_lib += 1,
                    _ => {}
                }
            }
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    ns.exports.sort();
    ns.exports.dedup();
    ns
}

// ---- legal ------------------------------------------------------------------

const SPDX_TOKENS: &[&str] = &[
    "GPL-2", "GPL-3", "GPL (>= 2)", "GPL (>= 3)", "LGPL-2", "LGPL-2.1", "LGPL-3",
    "LGPL (>= 2)", "LGPL (>= 2.1)", "MIT", "BSD_2_clause", "BSD_3_clause",
    "Apache License 2.0", "Apache License (>= 2)", "CC0", "CC BY 4.0", "CC-BY-4.0",
    "MPL-2.0", "Artistic-2.0", "AGPL-3", "AGPL (>= 3)", "Unlimited",
    "file LICENSE", "file LICENCE",
];
const OSI_TOKENS: &[&str] = &[
    "GPL-2", "GPL-3", "GPL (>= 2)", "GPL (>= 3)", "LGPL-2", "LGPL-2.1", "LGPL-3",
    "LGPL (>= 2)", "LGPL (>= 2.1)", "MIT", "BSD_2_clause", "BSD_3_clause",
    "Apache License 2.0", "Apache License (>= 2)", "MPL-2.0", "Artistic-2.0",
    "AGPL-3", "AGPL (>= 3)",
];
const TEMPLATE_TOKENS: &[&str] = &["MIT", "BSD_2_clause", "BSD_3_clause"];

struct Legal {
    license: Option<String>,
    spdx_valid: Option<bool>,
    osi_approved: Option<bool>,
    license_file_completeness: Option<bool>,
    copyright_holder_declared: Option<bool>,
}

/// Split a DESCRIPTION License string into canonical tokens (port of .legal_tokenize).
fn legal_tokenize(lic: &str) -> Vec<String> {
    if lic.trim().is_empty() {
        return vec![];
    }
    let ws = regex::Regex::new(r"[ \t]+").unwrap();
    let strip = regex::Regex::new(r"\s*\+\s*file\s+LICEN[SC]E\s*$").unwrap();
    lic.split('|')
        .filter_map(|part| {
            let norm = ws.replace_all(part, " ");
            let norm = norm.trim();
            if norm.is_empty() {
                return None;
            }
            let s = strip.replace(norm, "");
            let s = s.trim();
            Some(if s.is_empty() {
                "file LICENSE".to_string()
            } else {
                s.to_string()
            })
        })
        .collect()
}

fn metrics_legal(desc: &BTreeMap<String, String>, root: &Path, files: &[String]) -> Legal {
    let license = desc
        .get("License")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let tokens = license.as_deref().map(legal_tokenize).unwrap_or_default();
    let all_in = |set: &[&str]| tokens.iter().all(|t| set.contains(&t.as_str()));
    let spdx_valid = (!tokens.is_empty()).then(|| all_in(SPDX_TOKENS));
    let osi_approved = (!tokens.is_empty()).then(|| all_in(OSI_TOKENS));

    let has_file_ref = license
        .as_deref()
        .map(|l| regex::Regex::new(r"\bfile\s+LICEN[SC]E\b").unwrap().is_match(l))
        .unwrap_or(false);
    let license_file_completeness = if license.is_none() || !has_file_ref {
        None
    } else {
        let path = if files.iter().any(|f| f == "LICENSE") {
            Some("LICENSE")
        } else if files.iter().any(|f| f == "LICENCE") {
            Some("LICENCE")
        } else {
            None
        };
        match path {
            None => Some(false),
            Some(p) => {
                let content = read(root, p).unwrap_or_default();
                if content.trim().is_empty() {
                    Some(false)
                } else if tokens.iter().any(|t| TEMPLATE_TOKENS.contains(&t.as_str())) {
                    let year = regex::Regex::new(r"\bYEAR\b").unwrap().is_match(&content);
                    let ch = regex::Regex::new(r"\bCOPYRIGHT HOLDER\b").unwrap().is_match(&content);
                    Some(!(year || ch))
                } else {
                    Some(true)
                }
            }
        }
    };

    let copyright_holder_declared = {
        let authors_r = desc.get("Authors@R").map(|s| s.trim()).filter(|s| !s.is_empty());
        if let Some(ar) = authors_r {
            Some(regex::Regex::new(r#""cph"|'cph'"#).unwrap().is_match(ar))
        } else {
            desc.get("Author")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|_| true)
        }
    };

    Legal { license, spdx_valid, osi_approved, license_file_completeness, copyright_holder_declared }
}

// ---- portability ------------------------------------------------------------

struct Port {
    system_requirements_count: Option<i64>,
    cxx_standard_required: Option<String>,
    nonportable_compiler_flags: i64,
    nonportable_compiler_flags_json: Vec<String>,
    min_r_version: Option<String>,
    has_vignettes: bool,
    vignette_dynamic: Option<bool>,
}

fn find_files<'a>(files: &'a [String], pat: &str) -> Vec<&'a str> {
    let re = regex::Regex::new(pat).unwrap();
    files.iter().filter(|f| re.is_match(f)).map(|s| s.as_str()).collect()
}

fn metrics_portability(desc: &BTreeMap<String, String>, root: &Path, files: &[String]) -> Port {
    // system_requirements_count: split on comma or the word "and", unique lowercased
    let system_requirements_count = desc
        .get("SystemRequirements")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|sr| {
            let re = regex::Regex::new(r"[,]|\band\b").unwrap();
            let mut seen = std::collections::HashSet::new();
            let n = re
                .split(sr)
                .map(|p| p.trim().to_lowercase())
                .filter(|p| !p.is_empty())
                .filter(|p| seen.insert(p.clone()))
                .count();
            (n > 0).then_some(n as i64)
        });

    // cxx_standard_required + nonportable flags: scan src/Makevars(.win)
    let bad_flags = ["-march=native", "-O3", "-funroll-loops", "-ffast-math"];
    let cxx_re = regex::Regex::new(r"^\s*CXX_STD\s*=\s*CXX(\d+)").unwrap();
    let abs_re = regex::Regex::new(r"-[IL]/\S+").unwrap();
    let comment_re = regex::Regex::new(r"^\s*#").unwrap();
    let mut cxx_standard_required: Option<String> = None;
    let mut found_flags: Vec<String> = Vec::new();
    for mf in find_files(files, r"^src/Makevars(\.win)?$") {
        let Some(content) = read(root, mf) else { continue };
        for ln in content.lines() {
            if comment_re.is_match(ln) {
                continue;
            }
            if cxx_standard_required.is_none() {
                if let Some(c) = cxx_re.captures(ln) {
                    cxx_standard_required = Some(format!("C++{}", &c[1]));
                }
            }
            for pat in &bad_flags {
                if ln.contains(pat) && !found_flags.contains(&pat.to_string()) {
                    found_flags.push(pat.to_string());
                }
            }
            for m in abs_re.find_iter(ln) {
                let s = m.as_str().to_string();
                if !found_flags.contains(&s) {
                    found_flags.push(s);
                }
            }
        }
    }
    let nonportable_compiler_flags = found_flags.len() as i64;

    // min_r_version: R (>= x.y.z) in Depends
    let min_r_version = desc
        .get("Depends")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|dep| {
            let re = regex::Regex::new(r"\bR\s*\(\s*>=\s*([0-9]+\.[0-9]+(?:\.[0-9]+)?)\s*\)").unwrap();
            re.captures(dep).map(|c| c[1].to_string())
        });

    // vignettes
    let vig_files = find_files(files, r"^vignettes/.*\.[Rr](md|nw)$");
    let has_vignettes = !vig_files.is_empty();
    let vignette_dynamic = if !has_vignettes {
        None
    } else {
        let rmd_hdr = regex::Regex::new(r"```\{r[^}]*\}").unwrap();
        let rnw_hdr = regex::Regex::new(r"<<[^>]*>>=").unwrap();
        let eval_off = fancy_regex::Regex::new(r"eval\s*=\s*(FALSE|F)(?=[,}\s]|$)").unwrap();
        let mut found_any = false;
        let mut found_active = false;
        for vf in &vig_files {
            let Some(content) = read(root, vf) else { continue };
            if content.is_empty() {
                continue;
            }
            let headers: Vec<&str> = if regex::Regex::new(r"\.[Rr]md$").unwrap().is_match(vf) {
                rmd_hdr.find_iter(&content).map(|m| m.as_str()).collect()
            } else {
                rnw_hdr.find_iter(&content).map(|m| m.as_str()).collect()
            };
            if headers.is_empty() {
                continue;
            }
            found_any = true;
            for h in headers {
                if !eval_off.is_match(h).unwrap_or(false) {
                    found_active = true;
                    break;
                }
            }
            if found_active {
                break;
            }
        }
        Some(if !found_any { true } else { found_active })
    };

    Port {
        system_requirements_count,
        cxx_standard_required,
        nonportable_compiler_flags,
        nonportable_compiler_flags_json: found_flags,
        min_r_version,
        has_vignettes,
        vignette_dynamic,
    }
}

// ---- main -------------------------------------------------------------------

fn main() {
    let dir = std::env::args().nth(1).expect("usage: rpkg-analyzer <package_dir>");
    let root = PathBuf::from(&dir);
    let files = list_files(&root);

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .expect("load tree-sitter-r");

    // --- structure ---
    let n_files = files.len();
    let (mut loc_r, mut loc_src, mut loc_tests, mut loc_docs, mut loc_vignettes) = (0, 0, 0, 0, 0);
    let mut lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut has_src = false;
    let mut r_fn_defs = 0usize;

    for f in &files {
        let in_r = f.starts_with("R/");
        let in_src = f.starts_with("src/");
        let in_tests = f.starts_with("tests/");
        let in_docs = f.starts_with("man/");
        let in_vig = f.starts_with("vignettes/");
        if in_src {
            has_src = true;
        }
        let is_code_dir = in_r || in_src || in_tests || in_docs || in_vig;
        if !is_code_dir || is_noncode(f) {
            continue;
        }
        let Some(content) = read(&root, f) else { continue };
        let l = loc(&content);
        if in_r {
            loc_r += l;
        } else if in_src {
            loc_src += l;
        } else if in_tests {
            loc_tests += l;
        } else if in_docs {
            loc_docs += l;
        } else if in_vig {
            loc_vignettes += l;
        }
        *lang.entry(file_ext(f)).or_insert(0) += l;

        if in_r && f.to_ascii_lowercase().ends_with(".r") {
            if let Some(tree) = parser.parse(&content, None) {
                let root = tree.root_node();
                let mut st = vec![root];
                while let Some(nd) = st.pop() {
                    // count only TOP-LEVEL assigned functions: a function_definition
                    // that is the rhs of a top-level assignment has the program as its
                    // grandparent. Nested / anonymous functions are excluded.
                    if nd.kind() == "function_definition" {
                        let top = nd
                            .parent()
                            .and_then(|p| p.parent())
                            .map(|gp| gp.id() == root.id())
                            .unwrap_or(false);
                        if top {
                            r_fn_defs += 1;
                        }
                    }
                    let mut c = nd.walk();
                    for ch in nd.children(&mut c) {
                        st.push(ch);
                    }
                }
            }
        }
    }
    let loc_total = loc_r + loc_src + loc_tests + loc_docs + loc_vignettes;
    let compiled_share = if loc_total > 0 {
        loc_src as f64 / loc_total as f64
    } else {
        0.0
    };

    // --- DESCRIPTION ---
    let desc = read(&root, "DESCRIPTION").map(|t| parse_dcf(&t)).unwrap_or_default();
    let get = |k: &str| desc.get(k).cloned().unwrap_or_default();
    let mut deps: Vec<String> = Vec::new();
    for field in ["Depends", "Imports"] {
        deps.extend(dep_names(&get(field)));
    }
    // preserve declaration order, dropping later duplicates
    let mut seen = std::collections::HashSet::new();
    deps.retain(|d| seen.insert(d.clone()));
    let n_deps_direct = deps.len();
    let package = get("Package");
    let version = get("Version");

    // --- NAMESPACE ---
    let ns = read(&root, "NAMESPACE")
        .map(|t| parse_namespace(&t, &mut parser))
        .unwrap_or_default();
    let n_exports = ns.exports.len();
    let n_internal = r_fn_defs.saturating_sub(n_exports);

    // --- legal + portability ---
    let legal = metrics_legal(&desc, &root, &files);
    let port = metrics_portability(&desc, &root, &files);

    // --- emit NDJSON ---
    let summary = serde_json::json!({
        "rec": "summary",
        "package": package,
        "version": version,
        "license": legal.license,
        "spdx_valid": legal.spdx_valid,
        "osi_approved": legal.osi_approved,
        "license_file_completeness": legal.license_file_completeness,
        "copyright_holder_declared": legal.copyright_holder_declared,
        "min_r_version": port.min_r_version,
        "system_requirements_count": port.system_requirements_count,
        "cxx_standard_required": port.cxx_standard_required,
        "nonportable_compiler_flags": port.nonportable_compiler_flags,
        "nonportable_compiler_flags_json": port.nonportable_compiler_flags_json,
        "has_vignettes": port.has_vignettes,
        "vignette_dynamic": port.vignette_dynamic,
        "n_files": n_files,
        "loc_total": loc_total,
        "loc_r": loc_r,
        "loc_src": loc_src,
        "loc_tests": loc_tests,
        "loc_docs": loc_docs,
        "loc_vignettes": loc_vignettes,
        "compiled_share": compiled_share,
        "has_src": has_src,
        "lang_breakdown": lang,
        "n_exports": n_exports,
        "n_internal": n_internal,
        "r_fn_defs": r_fn_defs,
        "n_deps_direct": n_deps_direct,
        "dep_list": deps,
        "export_patterns": ns.export_patterns,
        "s3_methods": ns.s3_methods,
        "export_classes": ns.export_classes,
        "export_methods": ns.export_methods,
        "import_from": ns.import_from,
        "imports_whole": ns.imports_whole,
        "use_dyn_lib": ns.use_dyn_lib,
    });
    println!("{summary}");
    for d in &deps {
        println!("{}", serde_json::json!({"rec": "dependency", "package": d}));
    }
    for e in &ns.exports {
        println!("{}", serde_json::json!({"rec": "export", "symbol": e}));
    }
}
