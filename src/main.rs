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

/// the line count, which is `length(strsplit(paste(readLines(f), collapse="\n"), "\n"))`.
/// readLines drops the empty segment after a final newline; paste+strsplit then
/// drops one further trailing empty. Net effect vs a naive line count: a trailing
/// blank line is not counted. `\r` does not affect the count.
fn loc(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    // readLines treats \r\n and bare \r as line terminators too; normalize to \n.
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<&str> = normalized.split('\n').collect();
    if normalized.ends_with('\n') {
        lines.pop(); // readLines: no empty line from the final terminator
    }
    if lines.last() == Some(&"") {
        lines.pop(); // paste + strsplit drops the trailing empty (a blank last line)
    }
    lines.len()
}

fn read(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

/// Median of a slice (average of the two middle values for an even count).
fn median_u64(v: &[u64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let n = s.len();
    Some(if n % 2 == 1 {
        s[n / 2] as f64
    } else {
        (s[n / 2 - 1] + s[n / 2]) as f64 / 2.0
    })
}

/// the is_src: src/ files with a compiled-language extension only (structure.R).
/// Excludes Makevars, configure, .in, etc.
fn is_src_file(path: &str) -> bool {
    if !path.starts_with("src/") {
        return false;
    }
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hxx" | "f" | "f90" | "f95"
    )
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


// ---- docs --------------------------------------------------------------

struct Docs {
    dontrun_example_ratio: Option<f64>,
    undocumented_params_rate: Option<f64>,
    value_doc_rate: Option<f64>,
    references_coverage: Option<f64>,
    roxygen_doc_coverage: Option<f64>,
    has_readme: bool,
    readme_prose_length: Option<i64>,
    has_pkgdown: bool,
    news_present: bool,
    news_structure_quality: Option<f64>,
}

/// Brace-balanced content starting right after a known opening '{' at byte
/// offset `after_open` in `text`. Handles Rd escapes \{ and \}.
/// Returns (content, end) where end = byte offset of the closing '}', or
/// None when braces are unbalanced. Port of docs.R's `.bc`.
fn rd_brace_content(text: &str, after_open: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    if after_open >= bytes.len() {
        return None;
    }
    let mut depth = 1i32;
    let mut i = after_open;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b'\\' && i + 1 < bytes.len() && (bytes[i + 1] == b'{' || bytes[i + 1] == b'}') {
            i += 2;
            continue;
        }
        if ch == b'{' {
            depth += 1;
        } else if ch == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some((text[after_open..i].to_string(), i));
            }
        }
        i += 1;
    }
    None
}

/// First \cmd{...} block in text. Port of docs.R's `.fb`.
fn rd_first_block(text: &str, cmd: &str) -> Option<(String, usize)> {
    let pat = format!(r"\\{}\s*\{{", regex::escape(cmd));
    let re = regex::Regex::new(&pat).ok()?;
    let m = re.find(text)?;
    rd_brace_content(text, m.end())
}

/// All \cmd{...} block contents in text (only successful parses). Port of docs.R's `.ab`.
fn rd_all_blocks(text: &str, cmd: &str) -> Vec<String> {
    let pat = format!(r"\\{}\s*\{{", regex::escape(cmd));
    let Ok(re) = regex::Regex::new(&pat) else { return vec![] };
    re.find_iter(text)
        .filter_map(|m| rd_brace_content(text, m.end()).map(|(c, _)| c))
        .collect()
}

/// Whether text contains at least one \cmd{ marker. Port of docs.R's `.hc`.
fn rd_has_block(text: &str, cmd: &str) -> bool {
    let pat = format!(r"\\{}\s*\{{", regex::escape(cmd));
    regex::Regex::new(&pat).map(|re| re.is_match(text)).unwrap_or(false)
}

/// Extract parameter names from \usage block content (approximate).
/// Port of docs.R's `.uparams`.
fn rd_usage_params(u: &str) -> Vec<String> {
    if u.trim().is_empty() {
        return vec![];
    }
    let comment_re = regex::Regex::new(r"%[^\n]*").unwrap();
    let u = comment_re.replace_all(u, "");
    let dots_re = regex::Regex::new(r"\\dots|\\ldots").unwrap();
    let u = dots_re.replace_all(&u, "...");
    let sig_re = regex::Regex::new(r"[A-Za-z_.][A-Za-z0-9_.]*\s*\(").unwrap();
    let eq_re = regex::Regex::new(r"\s*=.*$").unwrap();

    let mut params: Vec<String> = Vec::new();
    for m in sig_re.find_iter(&u) {
        let after = m.end();
        if after >= u.len() {
            continue;
        }
        let mut depth = 1i32;
        let mut end_i: Option<usize> = None;
        let mut idx = after;
        for ch in u[after..].chars() {
            if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 {
                    end_i = Some(idx);
                    break;
                }
            }
            idx += ch.len_utf8();
        }
        let Some(end_i) = end_i else { continue };
        let sig = &u[after..end_i];

        // Split by top-level commas only (respects nested parentheses).
        let mut toks: Vec<&str> = Vec::new();
        let mut d2 = 0i32;
        let mut ts = 0usize;
        let mut pos = 0usize;
        for ch in sig.chars() {
            match ch {
                '(' => d2 += 1,
                ')' => d2 -= 1,
                ',' if d2 == 0 => {
                    toks.push(&sig[ts..pos]);
                    ts = pos + ch.len_utf8();
                }
                _ => {}
            }
            pos += ch.len_utf8();
        }
        if ts < sig.len() {
            toks.push(&sig[ts..]);
        }

        for tok in toks {
            let tok = tok.trim();
            let pname = eq_re.replace(tok, "");
            let pname = pname.trim();
            if pname.is_empty() || pname == "..." {
                continue;
            }
            let first_ok = pname
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic() || c == '_' || c == '.')
                .unwrap_or(false);
            if !first_ok {
                continue;
            }
            params.push(pname.to_string());
        }
    }

    let mut seen = std::collections::HashSet::new();
    params.retain(|p| seen.insert(p.clone()));
    params
}

/// Parameter names documented via \item{name}{} in \arguments content.
/// Port of docs.R's `.inames`.
fn rd_arg_names(args_text: &str) -> Vec<String> {
    let re = regex::Regex::new(r"\\item\s*\{([^{}]*)\}").unwrap();
    re.captures_iter(args_text)
        .map(|c| c[1].trim().to_string())
        .collect()
}

/// Component-wise version comparison (numeric_version semantics: shorter
/// versions are zero-padded for comparison).
fn version_ge(a: &[u64], b: &[u64]) -> bool {
    let len = a.len().max(b.len());
    for i in 0..len {
        let ai = a.get(i).copied().unwrap_or(0);
        let bi = b.get(i).copied().unwrap_or(0);
        if ai != bi {
            return ai > bi;
        }
    }
    true
}

fn metrics_docs(
    _desc: &BTreeMap<String, String>,
    root: &Path,
    files: &[String],
    exports: &[String],
) -> Docs {
    let rd_files = find_files(files, r"^man/.*\.Rd$");
    let n_rd = rd_files.len();

    let exports_filtered: Vec<&String> =
        exports.iter().filter(|e| !e.starts_with("pattern:")).collect();

    // Symbols declared per Rd file (\name + \alias).
    let mut rd_syms: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for f in &rd_files {
        let text = read(root, f).unwrap_or_default();
        let mut syms: Vec<String> = rd_all_blocks(&text, "name");
        syms.extend(rd_all_blocks(&text, "alias"));
        let syms: Vec<String> = syms.iter().map(|s| s.trim().to_string()).collect();
        rd_syms.insert(f, syms);
    }

    // Rd files that document at least one exported symbol.
    let exports_set: std::collections::HashSet<&str> =
        exports_filtered.iter().map(|s| s.as_str()).collect();
    let ex_fn_rd: Vec<&str> = rd_files
        .iter()
        .filter(|f| {
            !exports_set.is_empty()
                && rd_syms
                    .get(*f)
                    .map(|v| v.iter().any(|s| exports_set.contains(s.as_str())))
                    .unwrap_or(false)
        })
        .cloned()
        .collect();

    // ---- 1. dontrun_example_ratio -------------------------------------------
    let dontrun_example_ratio = {
        let rd_ex: Vec<&str> = rd_files
            .iter()
            .filter(|f| rd_has_block(&read(root, f).unwrap_or_default(), "examples"))
            .cloned()
            .collect();
        let n_ex = rd_ex.len();
        if n_ex == 0 {
            None
        } else {
            let dontrun_re = regex::Regex::new(r"^\\don(trun|ttest)\s*\{").unwrap();
            let n_wrap = rd_ex
                .iter()
                .filter(|f| {
                    let text = read(root, f).unwrap_or_default();
                    let Some((content, _)) = rd_first_block(&text, "examples") else {
                        return false;
                    };
                    let body = content.trim();
                    let Some(m2) = dontrun_re.find(body) else {
                        return false;
                    };
                    let Some((_, inner_end)) = rd_brace_content(body, m2.end()) else {
                        return false;
                    };
                    body[(inner_end + 1)..].trim().is_empty()
                })
                .count();
            Some(n_wrap as f64 / n_ex as f64)
        }
    };

    // ---- 2. undocumented_params_rate ----------------------------------------
    let undocumented_params_rate = if ex_fn_rd.is_empty() || exports_filtered.is_empty() {
        None
    } else {
        let rates: Vec<f64> = ex_fn_rd
            .iter()
            .filter_map(|f| {
                let text = read(root, f).unwrap_or_default();
                let ublk = rd_first_block(&text, "usage")?;
                let params = rd_usage_params(&ublk.0);
                if params.is_empty() {
                    return None;
                }
                let ablk = rd_first_block(&text, "arguments");
                let dnames: Vec<String> =
                    ablk.map(|(c, _)| rd_arg_names(&c)).unwrap_or_default();
                let undoc = params.iter().filter(|p| !dnames.contains(p)).count();
                Some(undoc as f64 / params.len() as f64)
            })
            .collect();
        if rates.is_empty() {
            None
        } else {
            Some(rates.iter().sum::<f64>() / rates.len() as f64)
        }
    };

    // ---- 3. value_doc_rate ---------------------------------------------------
    let value_doc_rate = if ex_fn_rd.is_empty() {
        None
    } else {
        let n = ex_fn_rd
            .iter()
            .filter(|f| rd_has_block(&read(root, f).unwrap_or_default(), "value"))
            .count();
        Some(n as f64 / ex_fn_rd.len() as f64)
    };

    // ---- 4. references_coverage ----------------------------------------------
    let references_coverage = if n_rd == 0 {
        None
    } else {
        let n = rd_files
            .iter()
            .filter(|f| rd_has_block(&read(root, f).unwrap_or_default(), "references"))
            .count();
        Some(n as f64 / n_rd as f64)
    };

    // ---- 5. roxygen_doc_coverage ----------------------------------------------
    let roxygen_doc_coverage = if exports_filtered.is_empty() {
        None
    } else {
        let mut all_syms: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for v in rd_syms.values() {
            for s in v {
                all_syms.insert(s.as_str());
            }
        }
        let n = exports_filtered
            .iter()
            .filter(|e| all_syms.contains(e.as_str()))
            .count();
        Some(n as f64 / exports_filtered.len() as f64)
    };

    // ---- 6. has_readme ----------------------------------------------------------
    let has_readme = exists(files, "README.md") || exists(files, "README.Rmd");

    // ---- 7. readme_prose_length --------------------------------------------------
    let readme_prose_length = {
        let rpath = if exists(files, "README.md") {
            Some("README.md")
        } else if exists(files, "README.Rmd") {
            Some("README.Rmd")
        } else {
            None
        };
        rpath.map(|p| {
            let text = read(root, p).unwrap_or_default();
            if text.is_empty() {
                0i64
            } else {
                let fence_re = regex::Regex::new(r"(?s)```[^\n]*\n.*?```").unwrap();
                let stripped = fence_re.replace_all(&text, "");
                let badge_re = regex::Regex::new(r"^\s*(\[!\[|<img\s|\[\[img)").unwrap();
                let text2: String = stripped
                    .lines()
                    .filter(|l| !badge_re.is_match(l))
                    .collect::<Vec<_>>()
                    .join(" ");
                text2.split_whitespace().count() as i64
            }
        })
    };

    // ---- 8. has_pkgdown -----------------------------------------------------------
    let has_pkgdown = exists(files, "_pkgdown.yml") || exists(files, "pkgdown/_pkgdown.yml");

    // ---- 9. news_present ------------------------------------------------------------
    let news_present = exists(files, "NEWS") || exists(files, "NEWS.md");

    // ---- 10. news_structure_quality ---------------------------------------------------
    let news_structure_quality = {
        let npath = if exists(files, "NEWS.md") {
            Some("NEWS.md")
        } else if exists(files, "NEWS") {
            Some("NEWS")
        } else {
            None
        };
        npath.map(|p| {
            let text = read(root, p).unwrap_or_default();
            if text.trim().is_empty() {
                0.0
            } else {
                let lns: Vec<&str> = text.lines().collect();
                let mut n_met = 0i64;

                let ver_hd_re = regex::Regex::new(
                    r"^(#{1,4}\s[^\n]*\d+\.\d+|[Vv]ersion\s+\d+\.\d+|[Cc]hanges?\s+(in|for)\s+(version\s+)?\d+\.\d+|\d+\.\d+(?:\.\d+)?\s*([-_(]|$))",
                )
                .unwrap();
                let hd_lines: Vec<&str> =
                    lns.iter().filter(|l| ver_hd_re.is_match(l)).cloned().collect();
                if !hd_lines.is_empty() {
                    n_met += 1;
                }

                let bullet_re = regex::Regex::new(r"^\s*[-*+]\s+\S").unwrap();
                if lns.iter().any(|l| bullet_re.is_match(l)) {
                    n_met += 1;
                }

                if hd_lines.len() >= 2 {
                    let ver_num_re = regex::Regex::new(r"\d+\.\d+(?:\.\d+)*").unwrap();
                    let ver_strs: Vec<&str> = hd_lines
                        .iter()
                        .flat_map(|l| ver_num_re.find_iter(l).map(|m| m.as_str()))
                        .collect();
                    if ver_strs.len() >= 2 {
                        let parsed: Option<Vec<Vec<u64>>> = ver_strs
                            .iter()
                            .map(|v| {
                                v.split('.')
                                    .map(|p| p.parse::<u64>().ok())
                                    .collect::<Option<Vec<u64>>>()
                            })
                            .collect();
                        if let Some(vers) = parsed {
                            let all_desc = vers.windows(2).all(|w| version_ge(&w[0], &w[1]));
                            if all_desc {
                                n_met += 1;
                            }
                        }
                    }
                }

                n_met as f64 / 3.0
            }
        })
    };

    Docs {
        dontrun_example_ratio,
        undocumented_params_rate,
        value_doc_rate,
        references_coverage,
        roxygen_doc_coverage,
        has_readme,
        readme_prose_length,
        has_pkgdown,
        news_present,
        news_structure_quality,
    }
}

// ---- health -----------------------------------------------------------------

struct Health {
    on_exit_coverage_rate: Option<f64>,
    global_state_write_density: Option<f64>,
    deprecated_idiom_density: Option<f64>,
    debug_artifact_density: Option<f64>,
    has_code_of_conduct: bool,
    has_contributing_guide: bool,
}

/// Strip a single-line comment (rough: ignores '#' inside strings), matching
/// R's `sub("#.*$", "", line)`.
fn strip_comment_line(line: &str) -> String {
    match line.find('#') {
        Some(idx) => line[..idx].to_string(),
        None => line.to_string(),
    }
}

fn n_open(s: &str) -> i64 {
    s.matches('{').count() as i64
}

fn n_close(s: &str) -> i64 {
    s.matches('}').count() as i64
}

/// Extract top-level function bodies from raw lines (comments stripped
/// internally). Brace tracking scans up to 15 lines ahead for the opening
/// '{', handling multi-line argument lists. Nested function bodies are
/// absorbed into the enclosing body and are NOT separately extracted. This
/// is a conservative heuristic (rare false positives/negatives possible),
/// ported faithfully from extract_function_bodies() in health.R.
fn extract_function_bodies(lines: &[String]) -> Vec<Vec<String>> {
    let fn_re = regex::Regex::new(r"\bfunction\s*\(").unwrap();
    let stripped: Vec<String> = lines.iter().map(|l| strip_comment_line(l)).collect();
    let n = stripped.len();
    let mut bodies: Vec<Vec<String>> = Vec::new();
    let mut i = 0usize;

    while i < n {
        if !fn_re.is_match(&stripped[i]) {
            i += 1;
            continue;
        }

        // Locate the opening '{' (handles multi-line signatures).
        let end_k = (i + 15).min(n - 1);
        let mut brace_at: Option<usize> = None;
        for k in i..=end_k {
            if n_open(&stripped[k]) > 0 {
                brace_at = Some(k);
                break;
            }
        }

        let Some(brace_at) = brace_at else {
            // No '{' found within look-ahead: single-expression body.
            bodies.push(vec![stripped[i].clone()]);
            i += 1;
            continue;
        };

        // Collect signature lines plus body until brace depth returns to 0.
        let mut body_lines: Vec<String> = stripped[i..=brace_at].to_vec();
        let mut depth: i64 = body_lines.iter().map(|l| n_open(l) - n_close(l)).sum();
        let mut j = brace_at + 1;

        while j < n && depth > 0 {
            let l = stripped[j].clone();
            depth += n_open(&l) - n_close(&l);
            body_lines.push(l);
            j += 1;
        }

        bodies.push(body_lines);
        i = j; // continue from the first line after the closing '}'
    }
    bodies
}

// Patterns that indicate a function body mutates shared/global state.
// options()/par() with a named argument (=) are setters; bare calls are getters.
// Connection-opening functions are included because unclosed connections
// affect global file-descriptor state.
const MUTATOR_PATS: &[&str] = &[
    "<<-",
    r"\boptions\s*\([^)]*=",
    r"\bpar\s*\([^)]*=",
    r"\bsetwd\s*\(",
    r"\bSys\.setenv\s*\(",
    r"\bsink\s*\(",
    r"\bfile\s*\(",
    r"\burl\s*\(",
    r"\bpipe\s*\(",
    r"\bgzfile\s*\(",
    r"\bbzfile\s*\(",
    r"\bxzfile\s*\(",
    r"\btextConnection\s*\(",
];

fn body_has_mutator(body: &[String], mutator_res: &[regex::Regex]) -> bool {
    let txt = body.join("\n");
    mutator_res.iter().any(|re| re.is_match(&txt))
}

fn body_has_on_exit(body: &[String], on_exit_re: &regex::Regex) -> bool {
    on_exit_re.is_match(&body.join("\n"))
}

fn metrics_health(_desc: &BTreeMap<String, String>, root: &Path, files: &[String]) -> Health {
    // ---- R/ file inventory ----
    let r_files = find_files(files, r"^R/.*\.R$");
    let mut r_loc: i64 = 0;
    let mut file_lines: Vec<Vec<String>> = Vec::new();
    for &f in &r_files {
        let content = read(root, f).unwrap_or_default();
        r_loc += loc(&content) as i64;
        file_lines.push(content.lines().map(|l| l.to_string()).collect());
    }
    let kloc_r = if r_loc > 0 { r_loc as f64 / 1000.0 } else { 0.0 };

    // ---- on_exit_coverage_rate ----
    // Fraction of R/ function bodies that both mutate global/shared state AND
    // call on.exit(). None when no function mutates state (denominator = 0).
    let mutator_res: Vec<regex::Regex> =
        MUTATOR_PATS.iter().map(|p| regex::Regex::new(p).unwrap()).collect();
    let on_exit_re = regex::Regex::new(r"\bon\.exit\s*\(").unwrap();

    let mut n_mutating: i64 = 0;
    let mut n_on_exit: i64 = 0;
    for lns in &file_lines {
        if lns.is_empty() {
            continue;
        }
        for body in extract_function_bodies(lns) {
            if body_has_mutator(&body, &mutator_res) {
                n_mutating += 1;
                if body_has_on_exit(&body, &on_exit_re) {
                    n_on_exit += 1;
                }
            }
        }
    }
    let on_exit_coverage_rate =
        (n_mutating != 0).then(|| n_on_exit as f64 / n_mutating as f64);

    // ---- global_state_write_density ----
    // Per KLOC of R/: <<-, assign()-to-global, options() setters, Sys.setenv().
    // Each grepl() sum below counts matching LINES (not total occurrences).
    let superassign_re = regex::Regex::new("<<-").unwrap();
    let assign_global_re = regex::Regex::new(
        r"\bassign\s*\([^)]*(?:\.GlobalEnv|globalenv\s*\(|baseenv\s*\()",
    )
    .unwrap();
    let options_setter_re = regex::Regex::new(r"\boptions\s*\([^)]*=").unwrap();
    let sys_setenv_re = regex::Regex::new(r"\bSys\.setenv\s*\(").unwrap();

    let global_state_write_density = (r_loc != 0).then(|| {
        let mut cnt: i64 = 0;
        for lns in &file_lines {
            let stripped: Vec<String> = lns.iter().map(|l| strip_comment_line(l)).collect();
            cnt += stripped.iter().filter(|l| superassign_re.is_match(l)).count() as i64;
            cnt += stripped.iter().filter(|l| assign_global_re.is_match(l)).count() as i64;
            cnt += stripped.iter().filter(|l| options_setter_re.is_match(l)).count() as i64;
            cnt += stripped.iter().filter(|l| sys_setenv_re.is_match(l)).count() as i64;
        }
        cnt as f64 / kloc_r
    });

    // ---- deprecated_idiom_density ----
    // Per KLOC of R/: bare T/F, 1:length/nrow/ncol, require()/library() inside
    // function bodies (indentation heuristic), .Internal(). Line-count based,
    // like above. The bare T/F pattern needs lookaround -> fancy_regex.
    let bare_tf_re =
        fancy_regex::Regex::new(r"(?<![A-Za-z0-9_.])[TF](?![A-Za-z0-9_.=])").unwrap();
    let seq_re = regex::Regex::new(r"\b1:(?:length|nrow|ncol)\s*\(").unwrap();
    let indent_re = regex::Regex::new(r"^[ \t]{2,}").unwrap();
    let req_lib_re = regex::Regex::new(r"\b(?:require|library)\s*\(").unwrap();
    let internal_re = regex::Regex::new(r"\.Internal\s*\(").unwrap();

    let deprecated_idiom_density = (r_loc != 0).then(|| {
        let mut cnt: i64 = 0;
        for lns in &file_lines {
            let stripped: Vec<String> = lns.iter().map(|l| strip_comment_line(l)).collect();
            cnt += stripped
                .iter()
                .filter(|l| bare_tf_re.is_match(l).unwrap_or(false))
                .count() as i64;
            cnt += stripped.iter().filter(|l| seq_re.is_match(l)).count() as i64;
            cnt += stripped
                .iter()
                .filter(|l| indent_re.is_match(l) && req_lib_re.is_match(l))
                .count() as i64;
            cnt += stripped.iter().filter(|l| internal_re.is_match(l)).count() as i64;
        }
        cnt as f64 / kloc_r
    });

    // ---- debug_artifact_density ----
    // Per KLOC of R/ (test files live in tests/, not R/). Detects browser()
    // and stray print()/cat() at statement position; lines that also contain
    // message()/warning()/stop() are excluded (intentional output contexts).
    let browser_re = regex::Regex::new(r"^\s*browser\s*\(\s*\)").unwrap();
    let warn_ctx_re = regex::Regex::new(r"\b(?:message|warning|stop)\s*\(").unwrap();
    let print_cat_re = regex::Regex::new(r"^\s*(?:print|cat)\s*\(").unwrap();

    let debug_artifact_density = (r_loc != 0).then(|| {
        let mut cnt: i64 = 0;
        for lns in &file_lines {
            let stripped: Vec<String> = lns.iter().map(|l| strip_comment_line(l)).collect();
            cnt += stripped.iter().filter(|l| browser_re.is_match(l)).count() as i64;
            cnt += stripped
                .iter()
                .filter(|l| print_cat_re.is_match(l) && !warn_ctx_re.is_match(l))
                .count() as i64;
        }
        cnt as f64 / kloc_r
    });

    // ---- community health files ----
    let has_code_of_conduct =
        exists(files, "CODE_OF_CONDUCT.md") || exists(files, ".github/CODE_OF_CONDUCT.md");
    let has_contributing_guide =
        exists(files, "CONTRIBUTING.md") || exists(files, ".github/CONTRIBUTING.md");

    Health {
        on_exit_coverage_rate,
        global_state_write_density,
        deprecated_idiom_density,
        debug_artifact_density,
        has_code_of_conduct,
        has_contributing_guide,
    }
}

/// One parsed `person()` call (Authors@R) or one parsed free-text Author entry.
struct Person {
    given: Option<String>,
    family: Option<String>,
    roles: Vec<String>,
}

struct Meta {
    maintainer: Option<String>,
    maintainer_email: Option<String>,
    n_authors: Option<i64>,
    /// Pre-serialized JSON array of {given,family,roles} objects, matching the
    /// `as.character(jsonlite::toJSON(parsed, auto_unbox = TRUE))`. we store this
    /// as a character scalar containing JSON text (double-encoded when embedded in
    /// the outer summary object), so this is a String, not a Vec<String>.
    authors: Option<String>,
}

/// Extract the inner content of each top-level `person(...)` call in an
/// Authors@R string, tracking balanced parens/quotes (port of .meta_person_inners).
fn meta_person_inners(text: &str) -> Vec<String> {
    let t = text.trim();
    if t.is_empty() {
        return Vec::new();
    }
    let person_re = regex::Regex::new(r"\bperson\s*\(").unwrap();
    let mut result = Vec::new();
    let mut remaining: Vec<char> = t.chars().collect();
    loop {
        let remaining_str: String = remaining.iter().collect();
        let Some(m) = person_re.find(&remaining_str) else { break };
        let m_start_chars = remaining_str[..m.start()].chars().count();
        let m_len_chars = remaining_str[m.start()..m.end()].chars().count();
        let n = remaining.len();
        let open_pos = m_start_chars + m_len_chars - 1; // index of "("
        let start = open_pos + 1; // first char inside "("
        let mut depth: i32 = 1;
        let mut pos = start;
        let mut in_dq = false;
        let mut in_sq = false;
        while pos < n && depth > 0 {
            let ch = remaining[pos];
            if !in_sq && ch == '"' {
                in_dq = !in_dq;
            } else if !in_dq && ch == '\'' {
                in_sq = !in_sq;
            } else if !in_dq && !in_sq {
                if ch == '(' {
                    depth += 1;
                } else if ch == ')' {
                    depth -= 1;
                }
            }
            pos += 1;
        }
        let inner: String = if pos > start {
            remaining[start..(pos - 1)].iter().collect()
        } else {
            String::new()
        };
        result.push(inner);
        remaining = remaining[pos.min(n)..].to_vec();
    }
    result
}

/// First capture group of the first match, or None (port of .cap1).
fn cap1(re: &regex::Regex, text: &str) -> Option<String> {
    re.captures(text).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())
}

/// All quoted (single- or double-) string contents in `text`, outer quotes
/// stripped (port of .extract_quoted).
fn extract_quoted(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r#""[^"]*"|'[^']*'"#).unwrap();
    re.find_iter(text)
        .map(|m| {
            let s = m.as_str();
            s[1..s.len() - 1].to_string()
        })
        .collect()
}

/// Parse the inner content of a single person() call: named given/family/role
/// first, falling back to positional quoted strings (port of .meta_parse_person).
fn meta_parse_person(inner: &str) -> Person {
    let mut given: Option<String> = None;
    let mut family: Option<String> = None;
    let mut roles: Vec<String> = Vec::new();

    let gd_re = regex::Regex::new(r#"(?:given|first)\s*=\s*"([^"]*)""#).unwrap();
    let gs_re = regex::Regex::new(r"(?:given|first)\s*=\s*'([^']*)'").unwrap();
    if let Some(g) = cap1(&gd_re, inner).or_else(|| cap1(&gs_re, inner)) {
        given = Some(g);
    }

    let fd_re = regex::Regex::new(r#"(?:family|last)\s*=\s*"([^"]*)""#).unwrap();
    let fs_re = regex::Regex::new(r"(?:family|last)\s*=\s*'([^']*)'").unwrap();
    if let Some(f) = cap1(&fd_re, inner).or_else(|| cap1(&fs_re, inner)) {
        family = Some(f);
    }

    let role_c_re = regex::Regex::new(r"role\s*=\s*c\(([^)]*)\)").unwrap();
    if let Some(c) = role_c_re.captures(inner) {
        let role_content = c.get(1).map(|m| m.as_str()).unwrap_or("");
        roles = extract_quoted(role_content);
    } else {
        let rd_re = regex::Regex::new(r#"role\s*=\s*"([^"]*)""#).unwrap();
        let rs_re = regex::Regex::new(r"role\s*=\s*'([^']*)'").unwrap();
        if let Some(r) = cap1(&rd_re, inner).or_else(|| cap1(&rs_re, inner)) {
            roles = vec![r];
        }
    }

    if given.is_none() || family.is_none() {
        let named_c_re = regex::Regex::new(r"[A-Za-z_.][A-Za-z0-9_.]*\s*=\s*c\([^)]*\)").unwrap();
        let named_val_re =
            regex::Regex::new(r#"[A-Za-z_.][A-Za-z0-9_.]*\s*=\s*(?:"[^"]*"|'[^']*')"#).unwrap();
        let cleaned = named_c_re.replace_all(inner, "");
        let cleaned = named_val_re.replace_all(&cleaned, "");
        let pos_strs: Vec<String> =
            extract_quoted(&cleaned).into_iter().filter(|s| !s.is_empty()).collect();
        if given.is_none() && !pos_strs.is_empty() {
            given = Some(pos_strs[0].clone());
        }
        if family.is_none() && pos_strs.len() >= 2 {
            family = Some(pos_strs[1].clone());
        }
    }

    Person { given, family, roles }
}

/// Parse the free-text Author field into person entries: split on commas/"and"
/// (protecting commas inside `[roles]`), strip email, pull `[roles]`, then split
/// remaining words into given/family (port of .meta_parse_author_text).
fn meta_parse_author_text(text: &str) -> Vec<Person> {
    let t = text.trim();
    if t.is_empty() {
        return Vec::new();
    }

    // Protect commas inside [...] blocks before splitting on comma/and.
    let bracket_re = regex::Regex::new(r"\[[^\]]+\]").unwrap();
    let mut protected = String::with_capacity(t.len());
    let mut last = 0usize;
    for m in bracket_re.find_iter(t) {
        protected.push_str(&t[last..m.start()]);
        protected.push_str(&m.as_str().replace(',', "\u{1}"));
        last = m.end();
    }
    protected.push_str(&t[last..]);

    let split_re = regex::Regex::new(r"\s*,\s*|\s+and\s+").unwrap();
    let parts: Vec<String> = split_re
        .split(&protected)
        .map(|p| p.trim().replace('\u{1}', ","))
        .filter(|p| !p.is_empty())
        .collect();

    let email_re = regex::Regex::new(r"\s*<[^>]*>").unwrap();
    let role_re = regex::Regex::new(r"\[([^\]]+)\]").unwrap();
    let bracket_strip_re = regex::Regex::new(r"\s*\[[^\]]*\]").unwrap();
    let ws_re = regex::Regex::new(r"\s+").unwrap();

    parts
        .iter()
        .map(|raw_entry| {
            let mut entry = email_re.replace(raw_entry, "").into_owned();
            let mut roles: Vec<String> = Vec::new();
            if let Some(c) = role_re.captures(&entry) {
                let role_str = c.get(1).unwrap().as_str().to_string();
                roles = role_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                entry = bracket_strip_re.replace(&entry, "").trim().to_string();
            }
            let name_parts: Vec<&str> =
                ws_re.split(entry.trim()).filter(|s| !s.is_empty()).collect();
            let n = name_parts.len();
            let (given, family) = if n == 0 {
                (None, None)
            } else if n == 1 {
                (None, Some(name_parts[0].to_string()))
            } else {
                (Some(name_parts[..n - 1].join(" ")), Some(name_parts[n - 1].to_string()))
            };
            Person { given, family, roles }
        })
        .collect()
}

/// A JSON string literal for `v`, or the bare token `null` (jsonlite na="null").
fn json_str_or_null(v: &Option<String>) -> String {
    match v {
        Some(s) => serde_json::to_string(s).unwrap(),
        None => "null".to_string(),
    }
}

/// Compact JSON array of person objects, field order given/family/roles, matching
/// jsonlite::toJSON(parsed, auto_unbox = TRUE) byte-for-byte (roles always an
/// array via I(); given/family unboxed strings or null).
fn persons_to_json(persons: &[Person]) -> String {
    let items: Vec<String> = persons
        .iter()
        .map(|p| {
            let given = json_str_or_null(&p.given);
            let family = json_str_or_null(&p.family);
            let roles = serde_json::to_string(&p.roles).unwrap();
            format!("{{\"given\":{given},\"family\":{family},\"roles\":{roles}}}")
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Compute maintainer/author metadata metrics for a package version
/// (port of metrics_meta, minus n_deps_direct/dep_list which main.rs already
/// computes directly off the DESCRIPTION DCF map).
fn metrics_meta(desc: &BTreeMap<String, String>, _root: &Path, _files: &[String]) -> Meta {
    // NB: whether DESCRIPTION exists at all vs. exists-but-lacks-these-fields
    // converges to the same NA outcome below, so no `exists(files, "DESCRIPTION")`
    // check is needed here (unlike n_deps_direct, where empty-vs-absent differ).

    // ---- maintainer --------------------------------------------------------
    let maint_raw = desc.get("Maintainer").map(|s| s.trim()).unwrap_or("");
    let (maintainer, maintainer_email) = if maint_raw.is_empty() {
        (None, None)
    } else {
        let email_re = regex::Regex::new(r"<([^>]+)>").unwrap();
        if let Some(c) = email_re.captures(maint_raw) {
            let email = c.get(1).unwrap().as_str().to_string();
            let strip_re = regex::Regex::new(r"\s*<[^>]*>.*").unwrap();
            let name_part = strip_re.replace(maint_raw, "").trim().to_string();
            let maintainer = if name_part.is_empty() { None } else { Some(name_part) };
            (maintainer, Some(email))
        } else {
            (Some(maint_raw.to_string()), None)
        }
    };

    // ---- authors ------------------------------------------------------------
    let ar_text = desc.get("Authors@R").map(|s| s.trim()).unwrap_or("");
    let (n_authors, authors) = if !ar_text.is_empty() {
        let inners = meta_person_inners(ar_text);
        if inners.is_empty() {
            (None, None)
        } else {
            let persons: Vec<Person> = inners.iter().map(|inner| meta_parse_person(inner)).collect();
            (Some(persons.len() as i64), Some(persons_to_json(&persons)))
        }
    } else {
        let au_text = desc.get("Author").map(|s| s.trim()).unwrap_or("");
        if au_text.is_empty() {
            (None, None)
        } else {
            let persons = meta_parse_author_text(au_text);
            if persons.is_empty() {
                (None, None)
            } else {
                (Some(persons.len() as i64), Some(persons_to_json(&persons)))
            }
        }
    };

    Meta { maintainer, maintainer_email, n_authors, authors }
}


// ---- security -----------------------------------------------------------

#[derive(serde::Serialize)]
struct InstallTimeSideEffectSurface {
    configure_files: Vec<String>,
    configure_loc: i64,
    #[serde(rename = "onLoad_file_write")]
    onload_file_write: bool,
    #[serde(rename = "onLoad_network")]
    onload_network: bool,
}

#[derive(serde::Serialize)]
struct NonRegistryRemotes {
    count: i64,
    schemes: Vec<String>,
}

#[derive(serde::Serialize)]
struct BundledThirdPartyCode {
    detected: bool,
    files: Vec<String>,
}

struct Security {
    unsafe_pattern_score: i64,
    install_time_side_effect_surface: InstallTimeSideEffectSurface,
    dep_constraint_coverage: Option<f64>,
    non_registry_remotes: NonRegistryRemotes,
    secret_pattern_count: i64,
    compiled_external_lib_exposure: serde_json::Value,
    bundled_third_party_code: BundledThirdPartyCode,
}

/// Unbox a plain (non `as.list`-wrapped) character vector the way
/// jsonlite::toJSON(x, auto_unbox = TRUE) does: empty -> [], one element ->
/// a bare JSON string, two or more -> a JSON array.
fn unbox_str_vec(v: Vec<String>) -> serde_json::Value {
    match v.len() {
        0 => serde_json::Value::Array(vec![]),
        1 => serde_json::Value::String(v.into_iter().next().unwrap()),
        _ => serde_json::Value::Array(v.into_iter().map(serde_json::Value::String).collect()),
    }
}

fn metrics_security(desc: &BTreeMap<String, String>, root: &Path, files: &[String]) -> Security {
    let r_files = find_files(files, r"^R/.*\.R$");

    // shared regexes for (1) unsafe_pattern_score and (2) install-time surface
    let eval_parse_re = regex::Regex::new(r"eval\s*\(\s*parse\s*\(\s*text\s*=").unwrap();
    let system_paste_re = regex::Regex::new(r"system\s*\(\s*paste\s*\(").unwrap();
    let system2_paste_re = regex::Regex::new(r"system2\s*\([^\n]*paste\s*\(").unwrap();
    let setenv_re = regex::Regex::new(r"Sys\.setenv\s*\(").unwrap();
    let sq_re = regex::Regex::new(r"'[^']*'").unwrap();
    let dq_re = regex::Regex::new(r#""[^"]*""#).unwrap();
    let assign_re = regex::Regex::new(r"=\s*[A-Za-z_.][A-Za-z0-9_.]*").unwrap();
    let onload_re = regex::Regex::new(r"\.on(?:Load|Attach)\s*<-\s*function").unwrap();
    // narrow network pattern used for the score (weight 2)
    let net_pat_re = regex::Regex::new(r"download\.file\s*\(|\burl\s*\(|\bcurl\s*\(").unwrap();
    // file-write pattern needs a negative lookbehind (bare file(...) but not tempfile(...) etc)
    let file_write_re = fancy_regex::Regex::new(
        r"writeLines?\s*\(|writeBin\s*\(|\bcat\s*\(\s*[^)]*,\s*(?:file|con)\s*=|(?<![A-Za-z0-9_.])file\s*\(|\bsink\s*\(|write\.csv\s*\(|write\.table\s*\(",
    )
    .unwrap();
    // broader network pattern (adds httr::/RCurl::/curl::) used for the side-effect surface
    let network_re =
        regex::Regex::new(r"download\.file\s*\(|\burl\s*\(|\bcurl\s*\(|httr::|RCurl::|curl::").unwrap();

    let mut unsafe_pattern_score: i64 = 0;
    let mut onload_file_write = false;
    let mut onload_network = false;

    for f in &r_files {
        let Some(content) = read(root, f) else { continue };
        if content.is_empty() {
            continue;
        }

        // 1. unsafe_pattern_score
        unsafe_pattern_score += 3 * eval_parse_re.find_iter(&content).count() as i64;
        unsafe_pattern_score += 2 * system_paste_re.find_iter(&content).count() as i64;
        unsafe_pattern_score += 2 * system2_paste_re.find_iter(&content).count() as i64;

        for ln in content.lines() {
            if !setenv_re.is_match(ln) {
                continue;
            }
            let tmp = sq_re.replace_all(ln, "''").into_owned();
            let stripped = dq_re.replace_all(&tmp, "\"\"");
            if assign_re.is_match(&stripped) {
                unsafe_pattern_score += 2;
            }
        }

        let is_onload_file = onload_re.is_match(&content);
        if is_onload_file {
            unsafe_pattern_score += 2 * net_pat_re.find_iter(&content).count() as i64;

            // 2. install_time_side_effect_surface (onLoad/onAttach side effects)
            if file_write_re.is_match(&content).unwrap_or(false) {
                onload_file_write = true;
            }
            if network_re.is_match(&content) {
                onload_network = true;
            }
        }
    }

    let cfg_names = ["configure", "configure.win", "cleanup", "cleanup.win"];
    let cfg_present: Vec<String> = cfg_names
        .iter()
        .filter(|n| exists(files, n))
        .map(|s| s.to_string())
        .collect();
    let configure_loc: i64 = cfg_present
        .iter()
        .filter_map(|f| read(root, f))
        .map(|c| loc(&c) as i64)
        .sum();

    let install_time_side_effect_surface = InstallTimeSideEffectSurface {
        configure_files: cfg_present,
        configure_loc,
        onload_file_write,
        onload_network,
    };

    // 3. dep_constraint_coverage: fraction of Imports+Depends (excl. "R") with an explicit >= bound
    let parse_dep_entries = |raw: Option<&String>| -> Vec<String> {
        match raw {
            Some(r) if !r.trim().is_empty() => r.split(',').map(|s| s.trim().to_string()).collect(),
            _ => vec![],
        }
    };
    let mut dep_entries: Vec<String> = Vec::new();
    dep_entries.extend(parse_dep_entries(desc.get("Imports")));
    dep_entries.extend(parse_dep_entries(desc.get("Depends")));
    dep_entries.retain(|s| !s.is_empty());

    let paren_re = regex::Regex::new(r"\s*\(.*").unwrap();
    let mut qualifying: Vec<String> = Vec::new();
    for e in &dep_entries {
        let name = paren_re.replace(e, "").trim().to_string();
        if !name.is_empty() && name != "R" {
            qualifying.push(e.clone());
        }
    }
    let dep_constraint_coverage = if qualifying.is_empty() {
        None
    } else {
        let n_ge = qualifying.iter().filter(|e| e.contains(">=")).count();
        Some(n_ge as f64 / qualifying.len() as f64)
    };

    // 4. non_registry_remotes
    let raw_remotes = desc.get("Remotes").map(|s| s.as_str()).unwrap_or("");
    let non_registry_remotes = if raw_remotes.trim().is_empty() {
        NonRegistryRemotes { count: 0, schemes: vec![] }
    } else {
        let entries: Vec<String> = raw_remotes
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let schemes: Vec<String> = entries
            .iter()
            .map(|e| match e.find("::") {
                Some(idx) => e[..idx].trim().to_string(),
                None => "github".to_string(),
            })
            .collect();
        NonRegistryRemotes { count: entries.len() as i64, schemes }
    };

    // 5. secret_pattern_count
    let nonbinary_re = regex::Regex::new(
        r"(?i)\.(rda|rdata|rds|pdf|png|jpg|jpeg|gif|bmp|svg|ico|woff|woff2|eot|ttf|otf|gz|zip|tar|bz2|xz|7z|dll|so|dylib|o|a|lib|pyd|class|jar|pyc|xlsx|xls|docx|doc|pptx|ppt|mp3|mp4|ogg|wav|avi|mov|sam|bam|bai|cram|fasta|fa|fastq|fq|vcf|bcf|bed|wig|bedgraph|bigwig|bw|bigbed|bb)$",
    )
    .unwrap();
    let md5_re = regex::Regex::new(r"(^|/)MD5$").unwrap();
    let akia_re = regex::Regex::new(r"AKIA[0-9A-Z]{16}").unwrap();
    let gh_re = regex::Regex::new(r"gh[pous]_[A-Za-z0-9_]{36,}|github_pat_[A-Za-z0-9_]{36,}").unwrap();
    let api_key_re = regex::Regex::new(
        r#"(?i)(api[_-]?key|api[_-]?secret|secret[_-]?key|access[_-]?token)\s*[=:]\s*["'][A-Za-z0-9+/=_-]{16,}["']"#,
    )
    .unwrap();
    let b64_re = regex::Regex::new(
        r#"(?i)(password|passwd|api_?key|auth_?token|secret)\s*=\s*["'][A-Za-z0-9+/]{40,}={0,2}["']"#,
    )
    .unwrap();

    let mut secret_pattern_count: i64 = 0;
    for f in files {
        if nonbinary_re.is_match(f) || md5_re.is_match(f) {
            continue;
        }
        let Some(content) = read(root, f) else { continue };
        if content.is_empty() || content.len() > 1_000_000 {
            continue;
        }
        secret_pattern_count += akia_re.find_iter(&content).count() as i64;
        secret_pattern_count += gh_re.find_iter(&content).count() as i64;
        secret_pattern_count += api_key_re.find_iter(&content).count() as i64;
        secret_pattern_count += b64_re.find_iter(&content).count() as i64;
    }

    // 6. compiled_external_lib_exposure
    let src_cfg_files = [
        "src/Makevars",
        "src/Makevars.win",
        "src/Makevars.in",
        "src/Makevars.ucrt",
        "configure",
        "configure.ac",
        "configure.in",
    ];
    let flag_re = regex::Regex::new(r"-l[A-Za-z][A-Za-z0-9_-]*").unwrap();
    let ac_re = regex::Regex::new(r"AC_CHECK_LIB\s*\(\s*([A-Za-z][A-Za-z0-9_-]*)").unwrap();
    let mut all_libs: Vec<String> = Vec::new();
    for f in src_cfg_files {
        if !exists(files, f) {
            continue;
        }
        let Some(content) = read(root, f) else { continue };
        if content.is_empty() {
            continue;
        }
        for m in flag_re.find_iter(&content) {
            let lib = m.as_str().strip_prefix("-l").unwrap_or(m.as_str());
            all_libs.push(lib.to_string());
        }
        for c in ac_re.captures_iter(&content) {
            all_libs.push(c[1].to_string());
        }
    }
    let mut seen = std::collections::HashSet::new();
    all_libs.retain(|l| seen.insert(l.clone()));
    let compiled_external_lib_exposure = unbox_str_vec(all_libs);

    // 7. bundled_third_party_code
    const KNOWN_VENDORED: &[&str] = &[
        "sqlite3.c", "sqlite3.h", "json.hpp", "miniz.c", "miniz.h", "stb_image.h",
        "stb_image_write.h", "nanosvg.h", "nanosvgrast.h", "xxhash.h", "xxhash.c",
        "tinyxml2.cpp", "tinyxml2.h", "pugixml.cpp", "pugixml.hpp",
    ];
    let license_re = regex::Regex::new(r"^src/.+/(LICENSE|COPYING)(\.[A-Za-z]+)?$").unwrap();
    let mut found: Vec<String> = Vec::new();
    for f in files {
        if !(f.starts_with("src/") || f.starts_with("inst/")) {
            continue;
        }
        let base = f.rsplit('/').next().unwrap_or(f.as_str());
        if KNOWN_VENDORED.contains(&base) {
            found.push(f.clone());
        }
    }
    for f in files {
        if license_re.is_match(f) {
            found.push(f.clone());
        }
    }
    let mut seen2 = std::collections::HashSet::new();
    found.retain(|f| seen2.insert(f.clone()));
    let detected = !found.is_empty();
    let bundled_third_party_code = BundledThirdPartyCode { detected, files: found };

    Security {
        unsafe_pattern_score,
        install_time_side_effect_surface,
        dep_constraint_coverage,
        non_registry_remotes,
        secret_pattern_count,
        compiled_external_lib_exposure,
        bundled_third_party_code,
    }
}

// ---- AST: per-function stats + OO kinds -------------------------------------

struct FnStat {
    name: String,
    exported: bool,
    file: String,
    line: usize,
    loc: usize,
    n_params: i64,
    cyclocomp: i64,
}

#[derive(Default)]
struct Oo {
    s4_classes: i64,
    s4_generics: i64,
    s4_methods: i64,
    r6_classes: i64,
    rc_classes: i64,
    s7_classes: i64,
    uses_usemethod: bool,
}

fn call_fn_name(call: &tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    let f = call.child_by_field_name("function")?;
    match f.kind() {
        "identifier" => f.utf8_text(bytes).ok().map(str::to_string),
        // e.g. R6::R6Class -> take the rhs symbol
        "namespace_operator" => f
            .child_by_field_name("rhs")
            .and_then(|r| r.utf8_text(bytes).ok())
            .map(str::to_string),
        _ => None,
    }
}

/// Cyclomatic complexity: 1 + decision points (if/for/while/repeat, && and ||).
fn cyclocomp(node: tree_sitter::Node, bytes: &[u8]) -> i64 {
    let mut c = 1i64;
    let mut st = vec![node];
    while let Some(n) = st.pop() {
        match n.kind() {
            "if_statement" | "for_statement" | "while_statement" | "repeat_statement" => c += 1,
            "binary_operator" => {
                if let Some(op) = n.child_by_field_name("operator").or_else(|| n.child(1)) {
                    let t = op.utf8_text(bytes).unwrap_or("");
                    if t == "&&" || t == "||" {
                        c += 1;
                    }
                }
            }
            _ => {}
        }
        let mut cur = n.walk();
        for ch in n.children(&mut cur) {
            st.push(ch);
        }
    }
    c
}

fn mean_i(v: &[i64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<i64>() as f64 / v.len() as f64)
}
fn median_i(v: &[i64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let n = s.len();
    Some(if n % 2 == 1 {
        s[n / 2] as f64
    } else {
        (s[n / 2 - 1] + s[n / 2]) as f64 / 2.0
    })
}

fn metrics_ast(root: &Path, files: &[String], exports: &[String], parser: &mut Parser) -> (Vec<FnStat>, Oo) {
    let r_files = find_files(files, r"^R/.*\.[Rr]$");
    let exported: std::collections::HashSet<&str> = exports.iter().map(String::as_str).collect();
    let mut fns = Vec::new();
    let mut oo = Oo::default();
    for f in &r_files {
        let Some(content) = read(root, f) else { continue };
        let Some(tree) = parser.parse(&content, None) else { continue };
        let bytes = content.as_bytes();
        let rootn = tree.root_node();

        // OO kinds: scan every call in the file.
        let mut st = vec![rootn];
        while let Some(n) = st.pop() {
            if n.kind() == "call" {
                if let Some(name) = call_fn_name(&n, bytes) {
                    match name.as_str() {
                        "setClass" => oo.s4_classes += 1,
                        "setGeneric" => oo.s4_generics += 1,
                        "setMethod" => oo.s4_methods += 1,
                        "setRefClass" => oo.rc_classes += 1,
                        "R6Class" => oo.r6_classes += 1,
                        "new_class" => oo.s7_classes += 1,
                        "UseMethod" => oo.uses_usemethod = true,
                        _ => {}
                    }
                }
            }
            let mut cur = n.walk();
            for ch in n.children(&mut cur) {
                st.push(ch);
            }
        }

        // Per-function: top-level `name <- function(...)` assignments.
        let mut cur = rootn.walk();
        for child in rootn.children(&mut cur) {
            if child.kind() != "binary_operator" {
                continue;
            }
            let Some(rhs) = child.child_by_field_name("rhs") else { continue };
            if rhs.kind() != "function_definition" {
                continue;
            }
            let name = child
                .child_by_field_name("lhs")
                .and_then(|l| l.utf8_text(bytes).ok())
                .unwrap_or("")
                .to_string();
            let n_params = rhs
                .child_by_field_name("parameters")
                .map(|p| {
                    let mut pc = p.walk();
                    p.children(&mut pc).filter(|c| c.kind() == "parameter").count() as i64
                })
                .unwrap_or(0);
            fns.push(FnStat {
                exported: exported.contains(name.as_str()),
                name,
                file: f.to_string(),
                line: rhs.start_position().row + 1,
                loc: rhs.end_position().row - rhs.start_position().row + 1,
                n_params,
                cyclocomp: cyclocomp(rhs, bytes),
            });
        }
    }
    (fns, oo)
}

// ---- main -------------------------------------------------------------------

fn main() {
    let dir = std::env::args().nth(1).expect("usage: rpkg-analyzer <package_dir>");

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .expect("load tree-sitter-r");

    // Debug: --sexp <file.R> prints the parse tree, for learning node kinds.
    if dir == "--sexp" {
        let f = std::env::args().nth(2).expect("--sexp <file>");
        let src = std::fs::read_to_string(&f).expect("read");
        let tree = parser.parse(&src, None).expect("parse");
        println!("{}", tree.root_node().to_sexp());
        return;
    }

    let root = PathBuf::from(&dir);
    let files = list_files(&root);

    // --- structure ---
    let n_files = files.len();
    let (mut loc_r, mut loc_src, mut loc_tests, mut loc_docs, mut loc_vignettes) = (0, 0, 0, 0, 0);
    let mut lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut has_src = false;

    for f in &files {
        let in_r = f.starts_with("R/");
        let in_src = is_src_file(f);
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
    // the meta.R rule combines c(Imports, Depends) in that order.
    for field in ["Imports", "Depends"] {
        deps.extend(dep_names(&get(field)));
    }
    // Distinct direct dependencies, declaration order. some tools do not dedupe, so a
    // package listed in both Imports and Depends is double-counted there; we keep
    // the distinct count.
    let mut seen = std::collections::HashSet::new();
    deps.retain(|d| seen.insert(d.clone()));
    let n_deps_direct = deps.len();

    // Additional_repositories: non-CRAN repo URLs declared for dependencies.
    // its presence signals a source outside the CRAN registry.
    let additional_repositories: Vec<String> = get("Additional_repositories")
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    // Config/* fields as a map (keys stripped of the "Config/" prefix).
    let config: BTreeMap<String, String> = desc
        .iter()
        .filter(|(k, _)| k.starts_with("Config/"))
        .map(|(k, v)| (k.trim_start_matches("Config/").to_string(), v.clone()))
        .collect();
    let date_publication = desc.get("Date/Publication").cloned().filter(|s| !s.is_empty());
    let encoding = desc.get("Encoding").cloned().filter(|s| !s.is_empty());

    // High-value DESCRIPTION fields promoted to explicit slots.
    let field = |k: &str| desc.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let priority = field("Priority");
    let url = field("URL");
    let bug_reports = field("BugReports");
    let vignette_builder = field("VignetteBuilder");
    let os_type = field("OS_type");
    let pkg_type = field("Type");
    let language = field("Language");
    let copyright = field("Copyright");
    let biocviews = field("biocViews");
    let needs_compilation = field("NeedsCompilation");

    // Data sets: files under data/.
    let mut data_names: Vec<String> = Vec::new();
    let mut data_sizes: Vec<u64> = Vec::new();
    for f in &files {
        if f.starts_with("data/") {
            if let Ok(m) = std::fs::metadata(root.join(f)) {
                if m.is_file() {
                    data_names.push(f.trim_start_matches("data/").to_string());
                    data_sizes.push(m.len());
                }
            }
        }
    }
    let num_data_files = data_names.len();
    let data_size_total: u64 = data_sizes.iter().sum();
    let data_size_median = median_u64(&data_sizes);

    let package = get("Package");
    let version = get("Version");

    // --- NAMESPACE ---
    let ns = read(&root, "NAMESPACE")
        .map(|t| parse_namespace(&t, &mut parser))
        .unwrap_or_default();
    let has_ns = exists(&files, "NAMESPACE");
    let funcs = metrics_functions(&root, &files, &ns, has_ns, &package);

    // AST-derived per-function stats + OO kinds (new v2 metrics).
    let (fn_stats, oo) = metrics_ast(&root, &files, &ns.exports, &mut parser);
    let fn_locs: Vec<i64> = fn_stats.iter().map(|f| f.loc as i64).collect();
    let fn_cyclos: Vec<i64> = fn_stats.iter().map(|f| f.cyclocomp).collect();
    let exp_params: Vec<i64> =
        fn_stats.iter().filter(|f| f.exported).map(|f| f.n_params).collect();
    let n_fns_r = fn_stats.len();
    let n_fns_r_exported = fn_stats.iter().filter(|f| f.exported).count();

    // --- legal + portability + tests ---
    let legal = metrics_legal(&desc, &root, &files);
    let port = metrics_portability(&desc, &root, &files);
    let tests = metrics_tests(&desc, &root, &files, &ns.exports);
    let docs = metrics_docs(&desc, &root, &files, &ns.exports);
    let health = metrics_health(&desc, &root, &files);
    let meta = metrics_meta(&desc, &root, &files);
    let security = metrics_security(&desc, &root, &files);

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
        "dontrun_example_ratio": docs.dontrun_example_ratio,
        "undocumented_params_rate": docs.undocumented_params_rate,
        "value_doc_rate": docs.value_doc_rate,
        "references_coverage": docs.references_coverage,
        "roxygen_doc_coverage": docs.roxygen_doc_coverage,
        "has_readme": docs.has_readme,
        "readme_prose_length": docs.readme_prose_length,
        "has_pkgdown": docs.has_pkgdown,
        "news_present": docs.news_present,
        "news_structure_quality": docs.news_structure_quality,
        "on_exit_coverage_rate": health.on_exit_coverage_rate,
        "global_state_write_density": health.global_state_write_density,
        "deprecated_idiom_density": health.deprecated_idiom_density,
        "debug_artifact_density": health.debug_artifact_density,
        "has_code_of_conduct": health.has_code_of_conduct,
        "has_contributing_guide": health.has_contributing_guide,
        "maintainer": meta.maintainer,
        "maintainer_email": meta.maintainer_email,
        "n_authors": meta.n_authors,
        "authors": meta.authors,
        "unsafe_pattern_score": security.unsafe_pattern_score,
        "install_time_side_effect_surface": security.install_time_side_effect_surface,
        "dep_constraint_coverage": security.dep_constraint_coverage,
        "non_registry_remotes": security.non_registry_remotes,
        "secret_pattern_count": security.secret_pattern_count,
        "compiled_external_lib_exposure": security.compiled_external_lib_exposure,
        "bundled_third_party_code": security.bundled_third_party_code,
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
        "n_fns_r": n_fns_r,
        "n_fns_r_exported": n_fns_r_exported,
        "n_fns_r_not_exported": n_fns_r - n_fns_r_exported,
        "loc_per_fn_mean": mean_i(&fn_locs),
        "loc_per_fn_median": median_i(&fn_locs),
        "npars_exported_mean": mean_i(&exp_params),
        "npars_exported_median": median_i(&exp_params),
        "cyclocomp_mean": mean_i(&fn_cyclos),
        "cyclocomp_median": median_i(&fn_cyclos),
        "cyclocomp_max": fn_cyclos.iter().max().copied(),
        "n_s4_classes": oo.s4_classes,
        "n_s4_generics": oo.s4_generics,
        "n_s4_methods": oo.s4_methods,
        "n_r6_classes": oo.r6_classes,
        "n_rc_classes": oo.rc_classes,
        "n_s7_classes": oo.s7_classes,
        "n_s3_methods": ns.s3_methods,
        "uses_usemethod": oo.uses_usemethod,
        "n_deps_direct": n_deps_direct,
        "dep_list": deps,
        "has_additional_repositories": !additional_repositories.is_empty(),
        "additional_repositories": additional_repositories,
        "config": config,
        "date_publication": date_publication,
        "encoding": encoding,
        "priority": priority,
        "url": url,
        "bug_reports": bug_reports,
        "vignette_builder": vignette_builder,
        "os_type": os_type,
        "type": pkg_type,
        "language": language,
        "copyright": copyright,
        "biocviews": biocviews,
        "needs_compilation": needs_compilation,
        "num_data_files": num_data_files,
        "data_size_total": data_size_total,
        "data_size_median": data_size_median,
        "data_files": data_names,
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
    for fnst in &fn_stats {
        println!(
            "{}",
            serde_json::json!({
                "rec": "function", "name": fnst.name, "exported": fnst.exported,
                "file": fnst.file, "line": fnst.line, "loc": fnst.loc,
                "n_params": fnst.n_params, "cyclocomp": fnst.cyclocomp,
            })
        );
    }
    // Full parsed DESCRIPTION as a raw intermediate: every field is preserved,
    // modeled or not, so future metrics derive from stored data without re-cloning.
    let mut dcf = serde_json::Map::new();
    dcf.insert("rec".into(), serde_json::Value::String("dcf".into()));
    for (k, v) in &desc {
        dcf.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    println!("{}", serde_json::Value::Object(dcf));
}
