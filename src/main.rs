#![recursion_limit = "512"]
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
    patterns: Vec<String>,
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
                    "exportPattern" => {
                        ns.export_patterns += 1;
                        ns.patterns.extend(arg_idents);
                    }
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

// ---- tests + CI -------------------------------------------------------------

fn exists(files: &[String], p: &str) -> bool {
    files.iter().any(|f| f == p)
}

fn loc_sum(root: &Path, paths: &[&str]) -> usize {
    paths.iter().filter_map(|p| read(root, p)).map(|c| loc(&c)).sum()
}

/// Values from YAML inline-array syntax `key: [a, b, 'c']`. `key` may be a regex.
fn yaml_inline_array(content: &str, key: &str) -> Vec<String> {
    if content.is_empty() {
        return vec![];
    }
    let re = regex::Regex::new(&format!(r"(?m)^\s+{key}:\s*\[([^\]]+)\]")).unwrap();
    let mut vals = Vec::new();
    for cap in re.captures_iter(content) {
        for part in cap[1].split(',') {
            let v: String = part.trim().chars().filter(|c| !matches!(c, '"' | '\'' | '`')).collect();
            if !v.is_empty() {
                vals.push(v);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    vals.retain(|v| seen.insert(v.clone()));
    vals
}

/// Distinct (os, r-version) matrix combinations across GHA workflow files.
fn gha_matrix_breadth(root: &Path, ci_yml: &[&str]) -> i64 {
    if ci_yml.is_empty() {
        return 0;
    }
    let pair_re = regex::Regex::new(r"\{[^}\n]*\bos:\s*[^}\n]+\}").unwrap();
    let os_re = regex::Regex::new(r#"\bos:\s*['"]?([^,'"{}\s]+)"#).unwrap();
    let r_re = regex::Regex::new(r#"\br(?:-version)?:\s*['"]?([^,'"{}\s]+)"#).unwrap();
    let mut max_b = 0i64;
    for f in ci_yml {
        let Some(content) = read(root, f) else { continue };
        if content.is_empty() {
            continue;
        }
        let os_vals = yaml_inline_array(&content, "os");
        let r_vals = yaml_inline_array(&content, "r(?:-version)?");
        let cross = if !os_vals.is_empty() && !r_vals.is_empty() {
            (os_vals.len() * r_vals.len()) as i64
        } else {
            os_vals.len().max(r_vals.len()) as i64
        };
        let mut combos = Vec::new();
        for pm in pair_re.find_iter(&content) {
            let pair = pm.as_str();
            let Some(os_c) = os_re.captures(pair) else { continue };
            let combo = match r_re.captures(pair) {
                Some(rc) => format!("{}:{}", &os_c[1], &rc[1]),
                None => format!("{}:any", &os_c[1]),
            };
            combos.push(combo);
        }
        let mut seen = std::collections::HashSet::new();
        combos.retain(|c| seen.insert(c.clone()));
        max_b = max_b.max(cross.max(combos.len() as i64));
    }
    max_b
}

struct Tests {
    has_tests: bool,
    test_to_code_ratio: Option<f64>,
    testthat_edition: Option<i64>,
    snapshot_test_count: i64,
    test_isolation_libs: Vec<String>,
    exported_fn_test_linkage: Option<f64>,
    stochastic_seed_discipline: Option<f64>,
    ci_present: bool,
    ci_type: Vec<String>,
    ci_matrix_breadth: i64,
    ci_pr_gated: bool,
}

fn metrics_tests(
    desc: &BTreeMap<String, String>,
    root: &Path,
    files: &[String],
    exports: &[String],
) -> Tests {
    let test_files = find_files(files, r"^tests/");
    let has_tests = !test_files.is_empty();

    let r_files = find_files(files, r"^R/");
    let loc_r = loc_sum(root, &r_files);
    let loc_tests = loc_sum(root, &test_files);
    let test_to_code_ratio = (loc_r != 0).then(|| loc_tests as f64 / loc_r as f64);

    let testthat_edition = desc
        .get("Config/testthat/edition")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok());

    let test_r_files = find_files(files, r"^tests/.*\.[Rr]$");
    let snap_re = regex::Regex::new(r"expect_snapshot\s*\(").unwrap();
    let snapf_re = regex::Regex::new(r"expect_snapshot_file\s*\(").unwrap();
    let mut snap_calls = 0i64;
    for f in &test_r_files {
        if let Some(c) = read(root, f) {
            snap_calls += snap_re.find_iter(&c).count() as i64;
            snap_calls += snapf_re.find_iter(&c).count() as i64;
        }
    }
    let snapshot_test_count = snap_calls + find_files(files, r"^tests/testthat/_snaps/").len() as i64;

    let test_content: String = test_files
        .iter()
        .filter_map(|f| read(root, f))
        .collect::<Vec<_>>()
        .join("\n");

    let mut test_isolation_libs = Vec::new();
    for lib in ["withr", "mockr", "httptest2", "webfakes"] {
        let pat = format!(
            r#"\b{lib}::|library\(\s*['"]?{lib}['"]?\s*\)|require\(\s*['"]?{lib}['"]?\s*\)"#
        );
        if regex::Regex::new(&pat).unwrap().is_match(&test_content) {
            test_isolation_libs.push(lib.to_string());
        }
    }
    if regex::Regex::new(r"\blocal_mocked_bindings\s*\(").unwrap().is_match(&test_content) {
        test_isolation_libs.push("local_mocked_bindings".to_string());
    }

    let real_exports: Vec<&String> = exports.iter().filter(|e| !e.starts_with("pattern:")).collect();
    let exported_fn_test_linkage = if real_exports.is_empty() {
        None
    } else if test_content.is_empty() {
        Some(0.0)
    } else {
        let n = real_exports
            .iter()
            .filter(|fname| {
                let pat = format!(r"\b{}\b", regex::escape(fname));
                regex::Regex::new(&pat).unwrap().is_match(&test_content)
            })
            .count();
        Some(n as f64 / real_exports.len() as f64)
    };

    let stoch_re = regex::Regex::new(r"\b(?:sample|runif|rnorm|rbinom)\s*\(").unwrap();
    let seed_re = regex::Regex::new(r"\bset\.seed\s*\(").unwrap();
    let stoch_files: Vec<&&str> = test_r_files
        .iter()
        .filter(|f| read(root, f).map(|c| stoch_re.is_match(&c)).unwrap_or(false))
        .collect();
    let stochastic_seed_discipline = if stoch_files.is_empty() {
        None
    } else {
        let seeded = stoch_files
            .iter()
            .filter(|f| read(root, f).map(|c| seed_re.is_match(&c)).unwrap_or(false))
            .count();
        Some(seeded as f64 / stoch_files.len() as f64)
    };

    let ci_yml = find_files(files, r"^\.github/workflows/.*\.ya?ml$");
    let has_travis = exists(files, ".travis.yml");
    let has_appveyor = exists(files, "appveyor.yml");
    let has_circleci = !find_files(files, r"^\.circleci/").is_empty();
    let ci_present = !ci_yml.is_empty() || has_travis || has_appveyor || has_circleci;
    let mut ci_type = Vec::new();
    if !ci_yml.is_empty() {
        ci_type.push("github-actions".to_string());
    }
    if has_travis {
        ci_type.push("travis".to_string());
    }
    if has_appveyor {
        ci_type.push("appveyor".to_string());
    }
    if has_circleci {
        ci_type.push("circleci".to_string());
    }
    let ci_matrix_breadth = gha_matrix_breadth(root, &ci_yml);
    let ci_pr_gated = ci_yml
        .iter()
        .any(|f| read(root, f).map(|c| c.contains("pull_request")).unwrap_or(false));

    Tests {
        has_tests,
        test_to_code_ratio,
        testthat_edition,
        snapshot_test_count,
        test_isolation_libs,
        exported_fn_test_linkage,
        stochastic_seed_discipline,
        ci_present,
        ci_type,
        ci_matrix_breadth,
        ci_pr_gated,
    }
}

// ---- functions (surface) ----------------------------------------------------

struct Functions {
    n_exports: Option<i64>,
    n_internal: Option<i64>,
    nse_surface_n: Option<i64>,
    nse_surface_frac: Option<f64>,
    triple_colon_count: i64,
    triple_colon_pkgs: i64,
}

/// Top-level R/ function definitions via the regex; first occurrence wins.
/// Returns (name, file, 1-based line) in discovery order.
fn build_fn_lookup(root: &Path, r_files: &[&str]) -> Vec<(String, String, usize)> {
    let re = regex::Regex::new(r"^([A-Za-z.][A-Za-z0-9_.]*)\s*(?:<<?-|=)\s*function\s*\(").unwrap();
    let mut lookup = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for f in r_files {
        let Some(content) = read(root, f) else { continue };
        for (i, ln) in content.lines().enumerate() {
            if let Some(c) = re.captures(ln) {
                let nm = c[1].to_string();
                if seen.insert(nm.clone()) {
                    lookup.push((nm, f.to_string(), i + 1));
                }
            }
        }
    }
    lookup
}

/// Approximate function body via brace-depth from the definition line (cap 200).
fn extract_body(lines: &[&str], start_idx: usize) -> String {
    let n = lines.len();
    if start_idx > n {
        return String::new();
    }
    let mut depth: i64 = 0;
    let mut seen_open = false;
    let limit = (start_idx + 199).min(n);
    for i in start_idx..=limit {
        let ln = lines[i - 1];
        depth += ln.matches('{').count() as i64 - ln.matches('}').count() as i64;
        if ln.contains('{') {
            seen_open = true;
        }
        if seen_open && depth <= 0 {
            return lines[start_idx - 1..i].join("\n");
        }
    }
    let end = (start_idx + 2).min(n);
    lines[start_idx - 1..end].join("\n")
}

fn metrics_functions(root: &Path, files: &[String], ns: &Namespace, has_ns: bool, package: &str) -> Functions {
    let r_files = find_files(files, r"^R/.*\.[Rr]$");
    let explicit: &[String] = &ns.exports;
    let pattern_res: Vec<regex::Regex> =
        ns.patterns.iter().filter_map(|p| regex::Regex::new(p).ok()).collect();
    let is_exported =
        |nm: &str| explicit.iter().any(|e| e == nm) || pattern_res.iter().any(|re| re.is_match(nm));

    let lookup = build_fn_lookup(root, &r_files);
    let r_fn_names: Vec<&String> = lookup.iter().map(|(n, _, _)| n).collect();

    let n_exports = if !has_ns {
        None
    } else if explicit.is_empty() && ns.patterns.is_empty() {
        Some(0)
    } else if !explicit.is_empty() {
        let extra = if ns.patterns.is_empty() {
            0
        } else {
            r_fn_names
                .iter()
                .filter(|nm| is_exported(nm) && !explicit.iter().any(|e| e == **nm))
                .count() as i64
        };
        Some(explicit.len() as i64 + extra)
    } else {
        Some(r_fn_names.iter().filter(|nm| is_exported(nm)).count() as i64)
    };

    let n_exp_in_r = r_fn_names.iter().filter(|nm| is_exported(nm)).count() as i64;
    let n_internal = Some(r_fn_names.len() as i64 - n_exp_in_r);

    let nse_re =
        regex::Regex::new(r"\b(eval|substitute|quote|bquote|match\.call|sys\.call)\s*\(").unwrap();
    let (nse_surface_n, nse_surface_frac) = if !has_ns {
        (None, None)
    } else {
        let mut names: Vec<String> = explicit.to_vec();
        for (nm, _, _) in &lookup {
            if is_exported(nm) && !names.contains(nm) {
                names.push(nm.clone());
            }
        }
        let mut seen = std::collections::HashSet::new();
        names.retain(|n| seen.insert(n.clone()));
        let mut file_lines: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut nse_count = 0i64;
        for nm in &names {
            let Some((_, file, line)) = lookup.iter().find(|(n, _, _)| n == nm) else { continue };
            let cached = file_lines.entry(file.clone()).or_insert_with(|| {
                read(root, file).unwrap_or_default().lines().map(String::from).collect()
            });
            let refs: Vec<&str> = cached.iter().map(String::as_str).collect();
            if nse_re.is_match(&extract_body(&refs, *line)) {
                nse_count += 1;
            }
        }
        let frac = match n_exports {
            Some(ne) if ne > 0 => Some(nse_count as f64 / ne as f64),
            _ => None,
        };
        (Some(nse_count), frac)
    };

    let all_content: String =
        r_files.iter().filter_map(|f| read(root, f)).collect::<Vec<_>>().join("\n");
    let tc_re =
        regex::Regex::new(r"([A-Za-z.][A-Za-z0-9.]*):::([A-Za-z.][A-Za-z0-9._]*)").unwrap();
    let mut triple_colon_count = 0i64;
    let mut ext_pkgs = std::collections::HashSet::new();
    for c in tc_re.captures_iter(&all_content) {
        triple_colon_count += 1;
        if &c[1] != package {
            ext_pkgs.insert(c[1].to_string());
        }
    }

    Functions {
        n_exports,
        n_internal,
        nse_surface_n,
        nse_surface_frac,
        triple_colon_count,
        triple_colon_pkgs: ext_pkgs.len() as i64,
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
    let has_ns = exists(&files, "NAMESPACE");
    let funcs = metrics_functions(&root, &files, &ns, has_ns, &package);

    // --- legal + portability + tests ---
    let legal = metrics_legal(&desc, &root, &files);
    let port = metrics_portability(&desc, &root, &files);
    let tests = metrics_tests(&desc, &root, &files, &ns.exports);

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
        "has_tests": tests.has_tests,
        "test_to_code_ratio": tests.test_to_code_ratio,
        "testthat_edition": tests.testthat_edition,
        "snapshot_test_count": tests.snapshot_test_count,
        "test_isolation_libs": tests.test_isolation_libs,
        "exported_fn_test_linkage": tests.exported_fn_test_linkage,
        "stochastic_seed_discipline": tests.stochastic_seed_discipline,
        "ci_present": tests.ci_present,
        "ci_type": tests.ci_type,
        "ci_matrix_breadth": tests.ci_matrix_breadth,
        "ci_pr_gated": tests.ci_pr_gated,
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
        "n_exports": funcs.n_exports,
        "n_internal": funcs.n_internal,
        "nse_surface_n": funcs.nse_surface_n,
        "nse_surface_frac": funcs.nse_surface_frac,
        "triple_colon_count": funcs.triple_colon_count,
        "triple_colon_pkgs": funcs.triple_colon_pkgs,
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
