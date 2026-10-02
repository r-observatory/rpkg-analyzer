#![recursion_limit = "2048"]
// rpkg-analyzer: a pure function of one extracted R package source tree.
// Reads a directory, emits newline-delimited JSON metric records on stdout.
// This first cut covers the structure, DESCRIPTION (DCF), and NAMESPACE groups,
// which map one-to-one onto the current pipeline's structure.R / parse_dcf /
// parse_namespace.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tree_sitter::Parser;

mod authors;
#[cfg(test)]
mod build_id;
mod cache;
mod citation;
mod cli;
mod memory;
mod news;
mod rd_pages;
mod rds;
mod release_files;
mod repo_practices;
mod test_suite;
mod vignettes;

#[global_allocator]
static ALLOCATOR: memory::EndsRun = memory::EndsRun;

// ---- regexes ----------------------------------------------------------------

/// The regex of a pattern written out in the source, compiled the first time
/// the line is reached and kept for the rest of the run.
macro_rules! regex {
    ($pattern:expr) => {{
        static RE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new($pattern).unwrap());
        &*RE
    }};
}

/// The same for a pattern with look-around, which takes the other engine.
macro_rules! fancy_regex {
    ($pattern:expr) => {{
        static RE: std::sync::LazyLock<fancy_regex::Regex> =
            std::sync::LazyLock::new(|| fancy_regex::Regex::new($pattern).unwrap());
        &*RE
    }};
}

/// How many regexes `kept_regex` keeps.
const KEPT_REGEXES: usize = 64;

/// Regexes by the text of their pattern. A pattern past `KEPT_REGEXES` is
/// compiled for the call that asked and not kept, so text that came out of a
/// package cannot make the list grow.
struct KeptRegexes(BTreeMap<String, std::sync::Arc<regex::Regex>>);

impl KeptRegexes {
    fn get(&mut self, pattern: &str) -> Result<std::sync::Arc<regex::Regex>, regex::Error> {
        if let Some(re) = self.0.get(pattern) {
            return Ok(re.clone());
        }
        let re = std::sync::Arc::new(regex::Regex::new(pattern)?);
        if self.0.len() < KEPT_REGEXES {
            self.0.insert(pattern.to_string(), re.clone());
        }
        Ok(re)
    }
}

/// The regex of a pattern the program puts together from its own fixed parts,
/// compiled the first time the text is asked for.
fn kept_regex(pattern: &str) -> Result<std::sync::Arc<regex::Regex>, regex::Error> {
    static KEPT: std::sync::Mutex<KeptRegexes> = std::sync::Mutex::new(KeptRegexes(BTreeMap::new()));
    KEPT.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(pattern)
}

// ---- file walking -----------------------------------------------------------

/// All files under `root`, relative to it, excluding the .git directory.
/// Mirrors what `git archive` would hand the pipeline (tracked tree, no .git).
fn list_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn rec(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(rd) = memory::read_dir(dir) else { return };
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

/// A file's text with any invalid UTF-8 replaced, so a Latin-1 byte cannot hide it.
fn read_lossy(root: &Path, rel: &str) -> Option<String> {
    std::fs::read(root.join(rel)).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
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

/// The tree-sitter grammar for a compiled-source file, by extension.
fn language_for_ext(path: &str) -> Option<tree_sitter::Language> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    Some(match ext.as_str() {
        "c" | "h" => tree_sitter_c::LANGUAGE.into(),
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" => tree_sitter_cpp::LANGUAGE.into(),
        "f" | "f90" | "f95" | "f03" | "f08" => tree_sitter_fortran::LANGUAGE.into(),
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        _ => return None,
    })
}

static VENDORED_SRC: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^src/rust/vendor[^/]*/|^src/(?:[^/]+/)*target/").unwrap());

/// Vendored crates and cargo build output, which are not the package's own code.
fn is_vendored_src(path: &str) -> bool {
    VENDORED_SRC.is_match(path)
}

/// the is_src: src/ files with a compiled-language extension only (structure.R).
/// Excludes Makevars, configure, .in, etc.
fn is_src_file(path: &str) -> bool {
    if !path.starts_with("src/") || is_vendored_src(path) {
        return false;
    }
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hxx" | "f" | "f90" | "f95" | "rs"
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
}

/// Split a DESCRIPTION License string into canonical tokens (port of .legal_tokenize).
fn legal_tokenize(lic: &str) -> Vec<String> {
    if lic.trim().is_empty() {
        return vec![];
    }
    let ws = regex!(r"[ \t]+");
    let strip = regex!(r"\s*\+\s*file\s+LICEN[SC]E\s*$");
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

/// A value CRAN's template leaves unfilled: blank, a bare <...>, YEAR, COPYRIGHT HOLDER or "your name".
fn is_license_placeholder(v: &str) -> bool {
    let v = v.trim();
    v.is_empty()
        || (v.starts_with('<') && v.ends_with('>') && !v[1..v.len() - 1].contains('>'))
        || v.eq_ignore_ascii_case("YEAR")
        || v.eq_ignore_ascii_case("COPYRIGHT HOLDER")
        || v.to_lowercase().contains("your name")
}

/// A filled MIT/BSD template (both lines, real values), or a full license text (neither line).
fn license_template_complete(content: &str) -> bool {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    // Only spaces and tabs after the colon, so an empty value never takes the next line.
    let year_re = regex!(r"(?m)^\s*YEAR:[ \t]*(.*)$");
    let holder_re = regex!(r"(?m)^\s*COPYRIGHT HOLDER:[ \t]*(.*)$");
    let year = year_re.captures(content).map(|c| c[1].to_string());
    let holder = holder_re.captures(content).map(|c| c[1].to_string());
    match (year, holder) {
        (Some(y), Some(h)) => !is_license_placeholder(&y) && !is_license_placeholder(&h),
        (None, None) => !content.trim().is_empty(),
        _ => false,
    }
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
        .map(|l| regex!(r"\bfile\s+LICEN[SC]E\b").is_match(l))
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
                let content = read_lossy(root, p).unwrap_or_default();
                if content.trim().is_empty() {
                    Some(false)
                } else if tokens.iter().any(|t| TEMPLATE_TOKENS.contains(&t.as_str())) {
                    Some(license_template_complete(&content))
                } else {
                    Some(true)
                }
            }
        }
    };

    Legal { license, spdx_valid, osi_approved, license_file_completeness }
}

// ---- portability ------------------------------------------------------------

struct Port {
    system_requirements_count: Option<i64>,
    cxx_standard_required: Option<String>,
    nonportable_compiler_flags: i64,
    nonportable_compiler_flags_json: Vec<String>,
    min_r_version: Option<String>,
}

fn find_files<'a>(files: &'a [String], pat: &str) -> Vec<&'a str> {
    let re = kept_regex(pat).unwrap();
    files.iter().filter(|f| re.is_match(f)).map(|s| s.as_str()).collect()
}

fn metrics_portability(desc: &BTreeMap<String, String>, root: &Path, files: &[String]) -> Port {
    // system_requirements_count: split on comma or the word "and", unique lowercased
    let system_requirements_count = desc
        .get("SystemRequirements")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|sr| {
            let re = regex!(r"[,]|\band\b");
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
    let cxx_re = regex!(r"^\s*CXX_STD\s*=\s*CXX(\d+)");
    let abs_re = regex!(r"-[IL]/\S+");
    let comment_re = regex!(r"^\s*#");
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
            let re = regex!(r"\bR\s*\(\s*>=\s*([0-9]+\.[0-9]+(?:\.[0-9]+)?)\s*\)");
            re.captures(dep).map(|c| c[1].to_string())
        });

    Port {
        system_requirements_count,
        cxx_standard_required,
        nonportable_compiler_flags,
        nonportable_compiler_flags_json: found_flags,
        min_r_version,
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
    let re = kept_regex(&format!(r"(?m)^\s+{key}:\s*\[([^\]]+)\]")).unwrap();
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
    let pair_re = regex!(r"\{[^}\n]*\bos:\s*[^}\n]+\}");
    let os_re = regex!(r#"\bos:\s*['"]?([^,'"{}\s]+)"#);
    let r_re = regex!(r#"\br(?:-version)?:\s*['"]?([^,'"{}\s]+)"#);
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
    let snap_re = regex!(r"expect_snapshot\s*\(");
    let snapf_re = regex!(r"expect_snapshot_file\s*\(");
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
        if kept_regex(&pat).unwrap().is_match(&test_content) {
            test_isolation_libs.push(lib.to_string());
        }
    }
    if regex!(r"\blocal_mocked_bindings\s*\(").is_match(&test_content) {
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

    let stoch_re = regex!(r"\b(?:sample|runif|rnorm|rbinom)\s*\(");
    let seed_re = regex!(r"\bset\.seed\s*\(");
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

    Tests {
        has_tests,
        test_to_code_ratio,
        testthat_edition,
        snapshot_test_count,
        test_isolation_libs,
        exported_fn_test_linkage,
        stochastic_seed_discipline,
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
    let re = regex!(r"^([A-Za-z.][A-Za-z0-9_.]*)\s*(?:<<?-|=)\s*function\s*\(");
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

    let nse_re = regex!(r"\b(eval|substitute|quote|bquote|match\.call|sys\.call)\s*\(");
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
    let tc_re = regex!(r"([A-Za-z.][A-Za-z0-9.]*):::([A-Za-z.][A-Za-z0-9._]*)");
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

/// Text with Rd `%` comments removed; an escaped `\%` stays.
fn strip_rd_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let b = line.as_bytes();
        let mut cut = line.len();
        let mut j = 0;
        while j < b.len() {
            if b[j] == b'\\' {
                j += 2;
                continue;
            }
            if b[j] == b'%' {
                cut = j;
                break;
            }
            j += 1;
        }
        out.push_str(&line[..cut]);
        if cut < line.len() && line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// First \cmd{...} block in text. Port of docs.R's `.fb`.
fn rd_first_block(text: &str, cmd: &str) -> Option<(String, usize)> {
    let pat = format!(r"\\{}\s*\{{", regex::escape(cmd));
    let re = kept_regex(&pat).ok()?;
    let m = re.find(text)?;
    rd_brace_content(text, m.end())
}

/// All \cmd{...} block contents in text (only successful parses). Port of docs.R's `.ab`.
fn rd_all_blocks(text: &str, cmd: &str) -> Vec<String> {
    let pat = format!(r"\\{}\s*\{{", regex::escape(cmd));
    let Ok(re) = kept_regex(&pat) else { return vec![] };
    re.find_iter(text)
        .filter_map(|m| rd_brace_content(text, m.end()).map(|(c, _)| c))
        .collect()
}

/// Whether text contains at least one \cmd{ marker. Port of docs.R's `.hc`.
fn rd_has_block(text: &str, cmd: &str) -> bool {
    let pat = format!(r"\\{}\s*\{{", regex::escape(cmd));
    kept_regex(&pat).map(|re| re.is_match(text)).unwrap_or(false)
}

/// Extract parameter names from \usage block content (approximate).
/// Port of docs.R's `.uparams`.
fn rd_usage_params(u: &str) -> Vec<String> {
    if u.trim().is_empty() {
        return vec![];
    }
    let comment_re = regex!(r"%[^\n]*");
    let u = comment_re.replace_all(u, "");
    let dots_re = regex!(r"\\dots|\\ldots");
    let u = dots_re.replace_all(&u, "...");
    let sig_re = regex!(r"[A-Za-z_.][A-Za-z0-9_.]*\s*\(");
    let eq_re = regex!(r"\s*=.*$");

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
    let re = regex!(r"\\item\s*\{([^{}]*)\}");
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

/// README names in the order they are tried, matched case-insensitively at the root.
const README_NAMES: [&str; 6] = ["README.md", "README.markdown", "README.Rmd", "README.qmd", "README", "README.txt"];

fn readme_file(files: &[String]) -> Option<&str> {
    README_NAMES
        .iter()
        .find_map(|n| files.iter().find(|f| !f.contains('/') && f.eq_ignore_ascii_case(n)))
        .map(String::as_str)
}

fn metrics_docs(
    _desc: &BTreeMap<String, String>,
    root: &Path,
    files: &[String],
    exports: &[String],
    news_file: Option<&str>,
) -> Docs {
    let rd_files = rd_pages::rd_page_files(files);
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
            let dontrun_re = regex!(r"^\\don(trun|ttest)\s*\{");
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
    let readme = readme_file(files);
    let has_readme = readme.is_some();

    // ---- 7. readme_prose_length --------------------------------------------------
    let readme_prose_length = {
        readme.map(|p| {
            let text = read(root, p).unwrap_or_default();
            if text.is_empty() {
                0i64
            } else {
                let fence_re = regex!(r"(?s)```[^\n]*\n.*?```");
                let stripped = fence_re.replace_all(&text, "");
                let badge_re = regex!(r"^\s*(\[!\[|<img\s|\[\[img)");
                let text2: String = stripped
                    .lines()
                    .filter(|l| !badge_re.is_match(l))
                    .collect::<Vec<_>>()
                    .join(" ");
                text2.split_whitespace().count() as i64
            }
        })
    };

    // ---- 9. news_present ------------------------------------------------------------
    let news_present = news_file.is_some();

    // ---- 10. news_structure_quality ---------------------------------------------------
    let news_structure_quality = {
        // Its Markdown and bullet heuristics say nothing about Rd.
        let npath = news_file.filter(|p| *p != "inst/NEWS.Rd");
        npath.map(|p| {
            let text = read(root, p).unwrap_or_default();
            if text.trim().is_empty() {
                0.0
            } else {
                let lns: Vec<&str> = text.lines().collect();
                let mut n_met = 0i64;

                let ver_hd_re = regex!(
                    r"^(#{1,4}\s[^\n]*\d+\.\d+|[Vv]ersion\s+\d+\.\d+|[Cc]hanges?\s+(in|for)\s+(version\s+)?\d+\.\d+|\d+\.\d+(?:\.\d+)?\s*([-_(]|$))"
                );
                let hd_lines: Vec<&str> =
                    lns.iter().filter(|l| ver_hd_re.is_match(l)).cloned().collect();
                if !hd_lines.is_empty() {
                    n_met += 1;
                }

                let bullet_re = regex!(r"^\s*[-*+]\s+\S");
                if lns.iter().any(|l| bullet_re.is_match(l)) {
                    n_met += 1;
                }

                if hd_lines.len() >= 2 {
                    let ver_num_re = regex!(r"\d+\.\d+(?:\.\d+)*");
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
    let fn_re = regex!(r"\bfunction\s*\(");
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
    static MUTATORS: std::sync::LazyLock<Vec<regex::Regex>> =
        std::sync::LazyLock::new(|| MUTATOR_PATS.iter().map(|p| regex::Regex::new(p).unwrap()).collect());
    let mutator_res: &[regex::Regex] = &MUTATORS;
    let on_exit_re = regex!(r"\bon\.exit\s*\(");

    let mut n_mutating: i64 = 0;
    let mut n_on_exit: i64 = 0;
    for lns in &file_lines {
        if lns.is_empty() {
            continue;
        }
        for body in extract_function_bodies(lns) {
            if body_has_mutator(&body, mutator_res) {
                n_mutating += 1;
                if body_has_on_exit(&body, on_exit_re) {
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
    let superassign_re = regex!("<<-");
    let assign_global_re = regex!(r"\bassign\s*\([^)]*(?:\.GlobalEnv|globalenv\s*\(|baseenv\s*\()");
    let options_setter_re = regex!(r"\boptions\s*\([^)]*=");
    let sys_setenv_re = regex!(r"\bSys\.setenv\s*\(");

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
    let bare_tf_re = fancy_regex!(r"(?<![A-Za-z0-9_.])[TF](?![A-Za-z0-9_.=])");
    let seq_re = regex!(r"\b1:(?:length|nrow|ncol)\s*\(");
    let indent_re = regex!(r"^[ \t]{2,}");
    let req_lib_re = regex!(r"\b(?:require|library)\s*\(");
    let internal_re = regex!(r"\.Internal\s*\(");

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
    let browser_re = regex!(r"^\s*browser\s*\(\s*\)");
    let warn_ctx_re = regex!(r"\b(?:message|warning|stop)\s*\(");
    let print_cat_re = regex!(r"^\s*(?:print|cat)\s*\(");

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

    Health {
        on_exit_coverage_rate,
        global_state_write_density,
        deprecated_idiom_density,
        debug_artifact_density,
    }
}

/// One parsed `person()` call (Authors@R) or one parsed free-text Author entry.
struct Person {
    given: Option<String>,
    family: Option<String>,
    roles: Vec<String>,
    comment: Option<String>,
    orcid: Option<String>,
    ror: Option<String>,
}

struct Meta {
    maintainer: Option<String>,
    maintainer_email: Option<String>,
    n_authors: Option<i64>,
    /// Pre-serialized JSON array of {given,family,roles} objects, each followed by
    /// comment, orcid and ror keys only when declared, matching the
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
    let person_re = regex!(r"\bperson\s*\(");
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
    let re = regex!(r#""[^"]*"|'[^']*'"#);
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

    let gd_re = regex!(r#"(?:given|first)\s*=\s*"([^"]*)""#);
    let gs_re = regex!(r"(?:given|first)\s*=\s*'([^']*)'");
    if let Some(g) = cap1(gd_re, inner).or_else(|| cap1(gs_re, inner)) {
        given = Some(g);
    }

    let fd_re = regex!(r#"(?:family|last)\s*=\s*"([^"]*)""#);
    let fs_re = regex!(r"(?:family|last)\s*=\s*'([^']*)'");
    if let Some(f) = cap1(fd_re, inner).or_else(|| cap1(fs_re, inner)) {
        family = Some(f);
    }

    let role_c_re = regex!(r"role\s*=\s*c\(([^)]*)\)");
    if let Some(c) = role_c_re.captures(inner) {
        let role_content = c.get(1).map(|m| m.as_str()).unwrap_or("");
        roles = extract_quoted(role_content);
    } else {
        let rd_re = regex!(r#"role\s*=\s*"([^"]*)""#);
        let rs_re = regex!(r"role\s*=\s*'([^']*)'");
        if let Some(r) = cap1(rd_re, inner).or_else(|| cap1(rs_re, inner)) {
            roles = vec![r];
        }
    }

    let named_c_re = regex!(r"[A-Za-z_.][A-Za-z0-9_.]*\s*=\s*c\([^)]*\)");
    let named_val_re = regex!(r#"[A-Za-z_.][A-Za-z0-9_.]*\s*=\s*(?:"[^"]*"|'[^']*')"#);

    // comment = c(ORCID = "...", ROR = "...", "free text"), or comment = "free text". A
    // parenthesis inside a quoted part does not end the c(...).
    let comment_c_re = regex!(r#"comment\s*=\s*c\(((?:"[^"]*"|'[^']*'|[^)"'])*)\)"#);
    let comment_s_re = regex!(r#"comment\s*=\s*(?:"([^"]*)"|'([^']*)')"#);
    let (mut orcid, mut ror, mut free) = (None, None, Vec::new());
    let mut comment_text = String::new();
    if let Some(c) = comment_c_re.captures(inner) {
        let body = c.get(1).map(|m| m.as_str()).unwrap_or("");
        // A part's name may be quoted: c('ORCID' = "...") reads as c(ORCID = "...").
        let parts = authors::comment_parts(body);
        let is_key = |name: &Option<String>, key: &str| name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(key));
        orcid = parts.iter().filter(|(n, _)| is_key(n, "ORCID")).find_map(|(_, v)| authors::orcid_checked(v));
        ror = parts.iter().filter(|(n, _)| is_key(n, "ROR")).find_map(|(_, v)| authors::ror_checked(v));
        // Other named parts keep their value without the name, as CRAN keeps them.
        free = parts
            .into_iter()
            .filter(|(n, _)| !is_key(n, "ORCID") && !is_key(n, "ROR"))
            .map(|(_, v)| v)
            .collect();
        comment_text = body.to_string();
    } else if let Some(c) = comment_s_re.captures(inner) {
        let v = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()).unwrap_or("");
        free = vec![v.to_string()];
        comment_text = v.to_string();
    }
    if orcid.is_none() {
        orcid = authors::orcid_in_text(&comment_text);
    }
    // A part that is only the kept ORCID is the identifier; any other iD stays in the comment.
    free.retain(|f| orcid.is_none() || authors::orcid_checked(f) != orcid);

    if given.is_none() || family.is_none() {
        let cleaned = named_c_re.replace_all(inner, "");
        let cleaned = named_val_re.replace_all(&cleaned, "");
        // Empty slots keep their place; an email or @handle is never a name.
        let pos_strs: Vec<String> = extract_quoted(&cleaned);
        let usable = |s: &&String| !s.is_empty() && !authors::is_email_like(s);
        if given.is_none() {
            given = pos_strs.first().filter(usable).cloned();
        }
        if family.is_none() {
            family = pos_strs.get(1).filter(usable).cloned();
        }
    }

    Person { given, family, roles, comment: authors::clean_comment(&free), orcid, ror }
}

/// The free-text Author field as persons: split on , ; "and" "&" "with contributions from", keeping
/// [roles] and (notes) whole, dropping emails, keeping ORCIDs, and moving notes and "... by" into comments.
fn meta_parse_author_text(text: &str) -> Vec<Person> {
    let t = text.trim();
    if t.is_empty() {
        return Vec::new();
    }
    // Some Author fields hold person() calls; those read as Authors@R.
    if regex!(r"\bperson\s*\(").is_match(t) {
        let persons: Vec<Person> = meta_person_inners(t).iter().map(|inner| meta_parse_person(inner)).collect();
        // Prose such as "person(s)" parses to no name, so that text is split as prose instead.
        if persons.iter().any(|p| p.given.is_some() || p.family.is_some()) {
            return persons;
        }
    }

    // ORCIDs come out first so the split cannot cut them; each leaves a marker.
    let orcid_re = regex!(
        r"(?i)<\s*https?://orcid\.org/([0-9X-]{16,19})\s*>|\(\s*ORCID:?\s*(?:<?\s*https?://orcid\.org/)?([0-9X-]{16,19})\s*>?\s*\)"
    );
    let mut ids: Vec<Option<String>> = Vec::new();
    let marked = orcid_re
        .replace_all(t, |c: &regex::Captures| {
            let raw = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()).unwrap_or("");
            ids.push(authors::orcid_checked(raw));
            format!(" \u{2}{}\u{2} ", ids.len() - 1)
        })
        .into_owned();
    // Separators inside [...] and (...) belong to one entry.
    let group_re = regex!(r"\[[^\]]*\]|\([^)]*\)");
    let mut groups: Vec<String> = Vec::new();
    let protected = group_re
        .replace_all(&marked, |c: &regex::Captures| {
            groups.push(c[0].to_string());
            format!("\u{3}{}\u{3}", groups.len() - 1)
        })
        .into_owned();

    let split_re = regex!(r"(?i)\s*[,;]\s*|\s+and\s+|\s+&\s+|\s+with\s+contributions\s+from\s+");
    let restore_re = regex!(r"\u{3}(\d+)\u{3}");
    let marker_re = regex!(r"\u{2}(\d+)\u{2}");
    let lead_and_re = regex!(r"(?i)^(?:and|&)\s+");
    let email_re = regex!(r"\s*<[^>]*>");
    let rd_email_re = regex!(r"\\email\{[^}]*\}");
    let role_re = regex!(r"\[([^\]]+)\]");
    let bracket_strip_re = regex!(r"\s*\[[^\]]*\]");
    let paren_re = regex!(r"\(([^)]*)\)");
    let label_re = regex!(r"(?i)^(?:authors?|contributors?|maintainer)\s*:\s*");
    let preamble_re = regex!(r"(?i)^(.+?\bby)\s+(\S.*)$");
    let ws_re = regex!(r"\s+");

    split_re
        .split(&protected)
        .filter_map(|raw| {
            let part = restore_re.replace_all(raw, |c: &regex::Captures| groups[c[1].parse::<usize>().unwrap()].clone());
            let mut entry = lead_and_re.replace(part.trim(), "").into_owned();
            let orcid = marker_re
                .captures_iter(&entry)
                .find_map(|c| ids.get(c[1].parse::<usize>().unwrap()).cloned().flatten());
            entry = marker_re.replace_all(&entry, " ").into_owned();
            entry = email_re.replace_all(&entry, "").into_owned();
            entry = rd_email_re.replace_all(&entry, "").into_owned();
            let mut roles: Vec<String> = Vec::new();
            if let Some(c) = role_re.captures(&entry) {
                roles = c[1].split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                entry = bracket_strip_re.replace_all(&entry, "").into_owned();
            }
            let mut notes: Vec<String> =
                paren_re.captures_iter(&entry).map(|c| c[1].trim().to_string()).filter(|s| !s.is_empty()).collect();
            entry = paren_re.replace_all(&entry, " ").into_owned();
            entry = label_re.replace(entry.trim(), "").into_owned();
            if let Some(c) = preamble_re.captures(entry.trim()) {
                notes.insert(0, c[1].to_string());
                entry = c[2].to_string();
            }
            // Bare emails and stray brackets are never part of a name.
            let words: Vec<String> = ws_re
                .split(entry.trim())
                .filter(|w| !w.is_empty() && !authors::is_email_like(w))
                .map(|w| w.trim_matches(|c| "()<>[]".contains(c)).to_string())
                .filter(|w| !w.is_empty())
                .collect();
            if words.is_empty() {
                return None;
            }
            let n = words.len();
            let (given, family) = if n == 1 {
                (None, Some(words[0].clone()))
            } else {
                (Some(words[..n - 1].join(" ")), Some(words[n - 1].clone()))
            };
            Some(Person { given, family, roles, comment: authors::clean_comment(&notes), orcid, ror: None })
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

/// Compact JSON array of persons as jsonlite::toJSON(auto_unbox = TRUE) writes them: given, family,
/// roles, then comment, orcid and ror only when declared, so given and family stay adjacent.
fn persons_to_json(persons: &[Person]) -> String {
    let items: Vec<String> = persons
        .iter()
        .map(|p| {
            let given = json_str_or_null(&p.given);
            let family = json_str_or_null(&p.family);
            let roles = serde_json::to_string(&p.roles).unwrap();
            let mut item = format!("{{\"given\":{given},\"family\":{family},\"roles\":{roles}");
            for (key, value) in [("comment", &p.comment), ("orcid", &p.orcid), ("ror", &p.ror)] {
                if let Some(v) = value {
                    item.push_str(&format!(",\"{key}\":{}", serde_json::to_string(v).unwrap()));
                }
            }
            item.push('}');
            item
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
        let email_re = regex!(r"<([^>]+)>");
        if let Some(c) = email_re.captures(maint_raw) {
            let email = c.get(1).unwrap().as_str().to_string();
            let strip_re = regex!(r"\s*<[^>]*>.*");
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
    let eval_parse_re = regex!(r"eval\s*\(\s*parse\s*\(\s*text\s*=");
    let system_paste_re = regex!(r"system\s*\(\s*paste\s*\(");
    let system2_paste_re = regex!(r"system2\s*\([^\n]*paste\s*\(");
    let setenv_re = regex!(r"Sys\.setenv\s*\(");
    let sq_re = regex!(r"'[^']*'");
    let dq_re = regex!(r#""[^"]*""#);
    let assign_re = regex!(r"=\s*[A-Za-z_.][A-Za-z0-9_.]*");
    let onload_re = regex!(r"\.on(?:Load|Attach)\s*<-\s*function");
    // narrow network pattern used for the score (weight 2)
    let net_pat_re = regex!(r"download\.file\s*\(|\burl\s*\(|\bcurl\s*\(");
    // file-write pattern needs a negative lookbehind (bare file(...) but not tempfile(...) etc)
    let file_write_re = fancy_regex!(
        r"writeLines?\s*\(|writeBin\s*\(|\bcat\s*\(\s*[^)]*,\s*(?:file|con)\s*=|(?<![A-Za-z0-9_.])file\s*\(|\bsink\s*\(|write\.csv\s*\(|write\.table\s*\("
    );
    // broader network pattern (adds httr::/RCurl::/curl::) used for the side-effect surface
    let network_re = regex!(r"download\.file\s*\(|\burl\s*\(|\bcurl\s*\(|httr::|RCurl::|curl::");

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

    let paren_re = regex!(r"\s*\(.*");
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
    let nonbinary_re = regex!(
        r"(?i)\.(rda|rdata|rds|pdf|png|jpg|jpeg|gif|bmp|svg|ico|woff|woff2|eot|ttf|otf|gz|zip|tar|bz2|xz|7z|dll|so|dylib|o|a|lib|pyd|class|jar|pyc|xlsx|xls|docx|doc|pptx|ppt|mp3|mp4|ogg|wav|avi|mov|sam|bam|bai|cram|fasta|fa|fastq|fq|vcf|bcf|bed|wig|bedgraph|bigwig|bw|bigbed|bb)$"
    );
    let md5_re = regex!(r"(^|/)MD5$");
    let akia_re = regex!(r"AKIA[0-9A-Z]{16}");
    let gh_re = regex!(r"gh[pous]_[A-Za-z0-9_]{36,}|github_pat_[A-Za-z0-9_]{36,}");
    let api_key_re = regex!(
        r#"(?i)(api[_-]?key|api[_-]?secret|secret[_-]?key|access[_-]?token)\s*[=:]\s*["'][A-Za-z0-9+/=_-]{16,}["']"#
    );
    let b64_re = regex!(
        r#"(?i)(password|passwd|api_?key|auth_?token|secret)\s*=\s*["'][A-Za-z0-9+/]{40,}={0,2}["']"#
    );

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
    let flag_re = regex!(r"-l[A-Za-z][A-Za-z0-9_-]*");
    let ac_re = regex!(r"AC_CHECK_LIB\s*\(\s*([A-Za-z][A-Za-z0-9_-]*)");
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
    let license_re = regex!(r"^src/.+/(LICENSE|COPYING)(\.[A-Za-z]+)?$");
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

// ---- documentation-coverage + source-repository signals ---------------------
// (exports-help coverage is intentionally omitted: roxygen_doc_coverage in the
// docs group already measures the fraction of exports with a \name/\alias page.)

struct Extra {
    has_recognized_repo: bool,
    repo_host: Option<String>,
    repo_url: Option<String>,
    help_pages_with_examples: i64,
    examples_coverage: Option<f64>,
    news_up_to_date: Option<bool>,
}

/// Reduce a forge URL to `host/owner/repo`, dropping trailing paths (/issues,
/// /tree/..., .git) that would break a derived pak spec.
fn normalize_repo(p: &str, domain: &str) -> String {
    if let Some(idx) = p.find(domain) {
        let after = &p[idx + domain.len()..];
        let segs: Vec<&str> = after
            .trim_start_matches(['/', ':'])
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if segs.len() >= 2 {
            let repo = segs[1].trim_end_matches(".git");
            return format!("{domain}/{}/{repo}", segs[0]);
        }
    }
    p.to_string()
}

/// Detect a source repository on a recognized forge from URL / BugReports.
fn detect_repo(fields: &[Option<String>]) -> Option<(String, String)> {
    let forges = [
        ("github.com", "GitHub"),
        ("gitlab.com", "GitLab"),
        ("codeberg.org", "Codeberg"),
        ("bitbucket.org", "Bitbucket"),
        ("git.sr.ht", "SourceHut"),
    ];
    for field in fields.iter().flatten() {
        for part in field.split([',', ' ', '\n', '\t']) {
            let p = part.trim().trim_end_matches('/');
            if p.is_empty() {
                continue;
            }
            for (domain, label) in forges {
                if p.contains(domain) {
                    return Some((label.to_string(), normalize_repo(p, domain)));
                }
            }
            if p.contains("gitea") {
                return Some(("Gitea".to_string(), p.to_string()));
            }
        }
    }
    None
}

fn metrics_extra(desc: &BTreeMap<String, String>, root: &Path, files: &[String], news_file: Option<&str>) -> Extra {
    let repo = detect_repo(&[desc.get("URL").cloned(), desc.get("BugReports").cloned()]);

    // Rd help pages with an \examples section.
    let rd_files = rd_pages::rd_page_files(files);
    let mut help_pages_with_examples = 0i64;
    for f in &rd_files {
        if rd_has_block(&read(root, f).unwrap_or_default(), "examples") {
            help_pages_with_examples += 1;
        }
    }
    let examples_coverage =
        (!rd_files.is_empty()).then(|| help_pages_with_examples as f64 / rd_files.len() as f64);

    // NEWS synced to version: the first version token in NEWS equals the package Version.
    let news_up_to_date = news_file
        .and_then(|p| read(root, p))
        .map(|news| {
            let ver_re = regex!(r"\d+\.\d+(?:[.-]\d+)*");
            let latest = ver_re.find(&news).map(|m| m.as_str().to_string());
            let pkg_ver = desc.get("Version").map(|s| s.trim().to_string());
            latest.is_some() && latest == pkg_ver
        });

    Extra {
        has_recognized_repo: repo.is_some(),
        repo_host: repo.as_ref().map(|(h, _)| h.clone()),
        repo_url: repo.map(|(_, u)| u),
        help_pages_with_examples,
        examples_coverage,
        news_up_to_date,
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
    calls: Vec<String>,
    native_calls: Vec<String>,
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

/// nexpr: median count of expression nodes starting per code line (R only).
/// A tree-sitter approximation of a getParseData-based measure.
fn metrics_nexpr(root: &Path, files: &[String], parser: &mut Parser) -> Option<f64> {
    let expr_kinds = [
        "call",
        "binary_operator",
        "unary_operator",
        "if_statement",
        "for_statement",
        "while_statement",
        "repeat_statement",
        "function_definition",
        "braced_expression",
        "extract_operator",
        "subset",
        "subset2",
        "namespace_operator",
    ];
    let mut counts: Vec<i64> = Vec::new();
    for f in find_files(files, r"^R/.*\.[Rr]$") {
        let Some(content) = read(root, &f) else { continue };
        let Some(tree) = parser.parse(&content, None) else { continue };
        let n_lines = content.lines().count().max(1);
        let mut per_line = vec![0i64; n_lines];
        let mut st = vec![tree.root_node()];
        while let Some(x) = st.pop() {
            if x.is_named() && expr_kinds.contains(&x.kind()) {
                let row = x.start_position().row;
                if row < per_line.len() {
                    per_line[row] += 1;
                }
            }
            let mut c = x.walk();
            for ch in x.children(&mut c) {
                st.push(ch);
            }
        }
        counts.extend(per_line.into_iter().filter(|&c| c > 0));
    }
    median_i(&counts)
}

fn count_kinds(node: tree_sitter::Node, kinds: &[&str]) -> i64 {
    let mut n = 0i64;
    let mut st = vec![node];
    while let Some(x) = st.pop() {
        // named nodes only: Fortran's `function`/`subroutine` keyword tokens share
        // the kind name of the definition node but are anonymous.
        if x.is_named() && kinds.contains(&x.kind()) {
            n += 1;
        }
        let mut c = x.walk();
        for ch in x.children(&mut c) {
            st.push(ch);
        }
    }
    n
}

#[derive(Debug, PartialEq)]
struct SrcFns {
    total: i64,
    c: i64,
    cpp: i64,
    fortran: i64,
    rust: i64,
    n_files: i64,
}

/// Function counts in src/ across C, C++, Fortran, and Rust. The oracle scan_src is tested against.
#[cfg(test)]
fn count_src_functions(root: &Path, files: &[String]) -> SrcFns {
    let mut parser = Parser::new();
    let (mut c, mut cpp, mut fortran, mut rust, mut nf) = (0i64, 0i64, 0i64, 0i64, 0i64);
    for f in files {
        if !f.starts_with("src/") || is_vendored_src(f) {
            continue;
        }
        let Some(lang) = language_for_ext(f) else { continue };
        if parser.set_language(&lang).is_err() {
            continue;
        }
        let Some(content) = read(root, f) else { continue };
        let Some(tree) = parser.parse(&content, None) else { continue };
        nf += 1;
        let ext = f.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "c" | "h" => c += count_kinds(tree.root_node(), &["function_definition"]),
            "cc" | "cpp" | "cxx" | "hpp" | "hxx" => {
                cpp += count_kinds(tree.root_node(), &["function_definition"])
            }
            "f" | "f90" | "f95" | "f03" | "f08" => {
                fortran += count_kinds(tree.root_node(), &["function", "subroutine"])
            }
            "rs" => rust += count_kinds(tree.root_node(), &["function_item"]),
            _ => {}
        }
    }
    SrcFns { total: c + cpp + fortran + rust, c, cpp, fortran, rust, n_files: nf }
}

// ---- cross-language (R -> native) call graph --------------------------------

/// The C/C++ function name from a function_definition, unwrapping pointer and
/// parenthesized declarators down to the identifier.
fn c_fn_name(def: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    let mut st = vec![def];
    while let Some(n) = st.pop() {
        if n.kind() == "function_declarator" {
            let mut d = n.child_by_field_name("declarator");
            for _ in 0..8 {
                let dd = d?;
                if matches!(dd.kind(), "identifier" | "field_identifier" | "qualified_identifier") {
                    return dd.utf8_text(bytes).ok().map(String::from);
                }
                d = dd.child_by_field_name("declarator");
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            st.push(ch);
        }
    }
    None
}

/// Names of every function defined in src/ (C, C++, Fortran, Rust). The oracle scan_src is tested against.
#[cfg(test)]
fn collect_src_function_names(root: &Path, files: &[String]) -> std::collections::HashSet<String> {
    let mut parser = Parser::new();
    let mut names = std::collections::HashSet::new();
    for f in files {
        if !f.starts_with("src/") {
            continue;
        }
        let Some(lang) = language_for_ext(f) else { continue };
        if parser.set_language(&lang).is_err() {
            continue;
        }
        let Some(content) = read(root, f) else { continue };
        let Some(tree) = parser.parse(&content, None) else { continue };
        let bytes = content.as_bytes();
        let ext = f.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        let mut st = vec![tree.root_node()];
        while let Some(n) = st.pop() {
            let name = match ext.as_str() {
                "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hxx"
                    if n.kind() == "function_definition" =>
                {
                    c_fn_name(n, bytes)
                }
                "f" | "f90" | "f95" | "f03" | "f08"
                    if n.is_named() && matches!(n.kind(), "function" | "subroutine") =>
                {
                    let mut c = n.walk();
                    n.children(&mut c)
                        .find(|ch| ch.kind().ends_with("_statement"))
                        .and_then(|s| s.child_by_field_name("name"))
                        .and_then(|nm| nm.utf8_text(bytes).ok())
                        .map(String::from)
                }
                "rs" if n.kind() == "function_item" => n
                    .child_by_field_name("name")
                    .and_then(|nm| nm.utf8_text(bytes).ok())
                    .map(String::from),
                _ => None,
            };
            if let Some(nm) = name {
                names.insert(nm);
            }
            let mut c = n.walk();
            for ch in n.children(&mut c) {
                st.push(ch);
            }
        }
    }
    names
}

/// The R-visible routine name / symbol from the first argument of a native call.
fn first_arg_symbol(args: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    let mut c = args.walk();
    for ch in args.children(&mut c) {
        if ch.kind() != "argument" {
            continue;
        }
        let val = ch.child_by_field_name("value")?;
        return match val.kind() {
            "string" => {
                let mut cc = val.walk();
                val.children(&mut cc)
                    .find(|x| x.kind() == "string_content")
                    .and_then(|x| x.utf8_text(bytes).ok())
                    .map(String::from)
            }
            "identifier" => val.utf8_text(bytes).ok().map(String::from),
            "namespace_operator" => val
                .child_by_field_name("rhs")
                .and_then(|x| x.utf8_text(bytes).ok())
                .map(String::from),
            _ => None,
        };
    }
    None
}

struct NativeGraph {
    n_native_call_sites: i64,
    n_native_edges: i64,
    n_native_targets: i64,
    native_resolution_rate: Option<f64>,
    edges: Vec<(String, String)>,
}

fn metrics_native_graph(
    root: &Path,
    files: &[String],
    fns: &[FnStat],
    c_functions: &std::collections::HashSet<String>,
) -> NativeGraph {
    // Registration table: {"rname", (DL_FUNC) &cfunc, n} across the *MethodDef arrays.
    let reg_re = regex!(r#"\{\s*"([^"]+)"\s*,\s*\(DL_FUNC\)\s*&?\s*(\w+)"#);
    let mut reg: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for f in files {
        if !is_src_file(f) {
            continue;
        }
        let Some(content) = read(root, f) else { continue };
        for cap in reg_re.captures_iter(&content) {
            reg.insert(cap[1].to_string(), cap[2].to_string());
        }
    }

    // Resolve a routine symbol to a compiled function, via the registration table
    // or a direct / C_-stripped name match.
    let resolve = |sym: &str| -> Option<String> {
        let stripped = sym.strip_prefix("C_").or_else(|| sym.strip_prefix("F_")).unwrap_or(sym);
        for cand in [sym, stripped] {
            if let Some(c) = reg.get(cand) {
                return Some(c.clone());
            }
            if c_functions.contains(cand) {
                return Some(cand.to_string());
            }
        }
        None
    };

    // Each R function's native calls become R -> compiled edges (the bridge that
    // unites the R graph with the compiled graphs).
    let mut n_native_call_sites = 0i64;
    let mut resolved = 0i64;
    let mut targets = std::collections::HashSet::new();
    let mut edge_pairs: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for f in fns {
        for sym in &f.native_calls {
            n_native_call_sites += 1;
            if let Some(c) = resolve(sym) {
                resolved += 1;
                edge_pairs.insert((f.name.clone(), c.clone()));
                targets.insert(c);
            }
        }
    }
    // Sorted, so the native edge records come out in one order on every run.
    let mut edges: Vec<(String, String)> = edge_pairs.into_iter().collect();
    edges.sort_unstable();
    NativeGraph {
        n_native_call_sites,
        n_native_edges: edges.len() as i64,
        n_native_targets: targets.len() as i64,
        native_resolution_rate: (n_native_call_sites > 0)
            .then(|| resolved as f64 / n_native_call_sites as f64),
        edges,
    }
}

/// The rightmost identifier of a call target (`foo`, `obj.foo`, `ns::foo`).
fn rightmost_ident(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" => {
            node.utf8_text(bytes).ok().map(String::from)
        }
        "field_expression" => node.child_by_field_name("field").and_then(|f| rightmost_ident(f, bytes)),
        "scoped_identifier" | "qualified_identifier" => {
            node.child_by_field_name("name").and_then(|f| rightmost_ident(f, bytes))
        }
        _ => None,
    }
}

fn callee_name(call: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    rightmost_ident(call.child_by_field_name("function")?, bytes)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SrcLang {
    CFamily,
    Rust,
    Fortran,
}

/// Is `n` a function definition for this language?
fn is_def(n: tree_sitter::Node, lang: SrcLang) -> bool {
    match lang {
        SrcLang::CFamily => n.kind() == "function_definition",
        SrcLang::Rust => n.kind() == "function_item",
        SrcLang::Fortran => n.is_named() && matches!(n.kind(), "function" | "subroutine"),
    }
}

/// The defined function's name.
fn def_name(n: tree_sitter::Node, lang: SrcLang, bytes: &[u8]) -> Option<String> {
    match lang {
        SrcLang::CFamily => c_fn_name(n, bytes),
        SrcLang::Rust => n
            .child_by_field_name("name")
            .and_then(|nm| nm.utf8_text(bytes).ok())
            .map(String::from),
        SrcLang::Fortran => {
            let mut c = n.walk();
            n.children(&mut c)
                .find(|ch| ch.kind().ends_with("_statement"))
                .and_then(|s| s.child_by_field_name("name"))
                .and_then(|nm| nm.utf8_text(bytes).ok())
                .map(String::from)
        }
    }
}

/// The callee name at a call-site node, or None if the node is not a call.
fn call_target(b: tree_sitter::Node, lang: SrcLang, bytes: &[u8]) -> Option<String> {
    match (lang, b.kind()) {
        (SrcLang::Fortran, "subroutine_call") => {
            b.child_by_field_name("subroutine").and_then(|x| rightmost_ident(x, bytes))
        }
        // Fortran uses call_expression for function references too (also array
        // indexing, which is filtered out because it will not match a def name).
        (SrcLang::Fortran, "call_expression") => {
            b.named_child(0).and_then(|x| rightmost_ident(x, bytes))
        }
        (_, "call_expression") => callee_name(b, bytes),
        _ => None,
    }
}

/// A compiled function node with metadata, mirroring the R `function` records.
#[derive(Debug, PartialEq)]
struct SrcFn {
    name: String,
    lang: &'static str,
    file: String,
    line: usize,
    loc: usize,
}

fn lang_str_of(ext: &str) -> &'static str {
    match ext {
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" => "cpp",
        "f" | "f90" | "f95" | "f03" | "f08" => "fortran",
        "rs" => "rust",
        _ => "other",
    }
}

/// Internal call graph for one compiled language: nodes are function definitions
/// in src/, edges are calls between them. C and C++ are graphed together.
/// Returns the stats, the edges, and the node records with metadata. The oracle
/// scan_src is tested against.
#[cfg(test)]
fn build_src_graph(
    root: &Path,
    files: &[String],
    lang: SrcLang,
) -> (Network, Vec<(String, String)>, Vec<SrcFn>) {
    let exts: &[&str] = match lang {
        SrcLang::CFamily => &["c", "h", "cc", "cpp", "cxx", "hpp", "hxx"],
        SrcLang::Rust => &["rs"],
        SrcLang::Fortran => &["f", "f90", "f95", "f03", "f08"],
    };
    let mut parser = Parser::new();
    let mut names: Vec<String> = Vec::new();
    let mut calls: Vec<Vec<String>> = Vec::new();
    let mut nodes: Vec<SrcFn> = Vec::new();
    for f in files {
        if !f.starts_with("src/") || (lang == SrcLang::Rust && is_vendored_src(f)) {
            continue;
        }
        let ext = f.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if !exts.contains(&ext.as_str()) {
            continue;
        }
        let Some(l) = language_for_ext(f) else { continue };
        if parser.set_language(&l).is_err() {
            continue;
        }
        let Some(content) = read(root, f) else { continue };
        let Some(tree) = parser.parse(&content, None) else { continue };
        let bytes = content.as_bytes();
        let lang_str = lang_str_of(&ext);
        let mut st = vec![tree.root_node()];
        while let Some(n) = st.pop() {
            if is_def(n, lang) {
                if let Some(nm) = def_name(n, lang, bytes) {
                    let mut callees = Vec::new();
                    let mut bst = vec![n];
                    while let Some(b) = bst.pop() {
                        if let Some(cn) = call_target(b, lang, bytes) {
                            callees.push(cn);
                        }
                        let mut bc = b.walk();
                        for ch in b.children(&mut bc) {
                            bst.push(ch);
                        }
                    }
                    nodes.push(SrcFn {
                        name: nm.clone(),
                        lang: lang_str,
                        file: f.clone(),
                        line: n.start_position().row + 1,
                        loc: n.end_position().row - n.start_position().row + 1,
                    });
                    names.push(nm);
                    calls.push(callees);
                }
                // do not descend further: nested definitions fold into this one
                continue;
            }
            let mut c = n.walk();
            for ch in n.children(&mut c) {
                st.push(ch);
            }
        }
    }
    let (net, edges) = graph_from_calls(&names, &calls);
    (net, edges, nodes)
}

/// One compiled function definition, as the call graph needs it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct SrcDef {
    name: String,
    line: usize,
    loc: usize,
    callees: Vec<String>,
}

/// One parse of a compiled file, from its bytes and extension alone; consumers apply their path rules.
/// A change to it or to SrcDef bumps cache::CACHE_FORMAT.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
struct SrcFacts {
    parsed: bool,
    n_defs: i64,
    names: Vec<String>,
    defs: Vec<SrcDef>,
}

/// The call-graph language of a lowercased compiled-source extension.
fn src_lang_of(ext: &str) -> Option<SrcLang> {
    match ext {
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hxx" => Some(SrcLang::CFamily),
        "rs" => Some(SrcLang::Rust),
        "f" | "f90" | "f95" | "f03" | "f08" => Some(SrcLang::Fortran),
        _ => None,
    }
}

/// The walks of count_src_functions, collect_src_function_names and build_src_graph, on one tree.
fn src_facts(parser: &mut Parser, ext: &str, content: &str) -> SrcFacts {
    let (Some(grammar), Some(lang)) = (language_for_ext(ext), src_lang_of(ext)) else {
        return SrcFacts::default();
    };
    if parser.set_language(&grammar).is_err() {
        return SrcFacts::default();
    }
    let Some(tree) = parser.parse(content, None) else { return SrcFacts::default() };
    let bytes = content.as_bytes();
    let root = tree.root_node();
    // Counts, nested definitions included.
    let kinds: &[&str] = match lang {
        SrcLang::CFamily => &["function_definition"],
        SrcLang::Fortran => &["function", "subroutine"],
        SrcLang::Rust => &["function_item"],
    };
    let n_defs = count_kinds(root, kinds);
    // Names, nested definitions included.
    let mut names = Vec::new();
    let mut st = vec![root];
    while let Some(n) = st.pop() {
        if is_def(n, lang) {
            if let Some(nm) = def_name(n, lang, bytes) {
                names.push(nm);
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            st.push(ch);
        }
    }
    // Graph nodes: a nested definition folds into the one around it.
    let mut defs = Vec::new();
    let mut st = vec![root];
    while let Some(n) = st.pop() {
        if is_def(n, lang) {
            if let Some(nm) = def_name(n, lang, bytes) {
                let mut callees = Vec::new();
                let mut bst = vec![n];
                while let Some(b) = bst.pop() {
                    if let Some(cn) = call_target(b, lang, bytes) {
                        callees.push(cn);
                    }
                    let mut bc = b.walk();
                    for ch in b.children(&mut bc) {
                        bst.push(ch);
                    }
                }
                defs.push(SrcDef {
                    name: nm,
                    line: n.start_position().row + 1,
                    loc: n.end_position().row - n.start_position().row + 1,
                    callees,
                });
            }
            continue;
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            st.push(ch);
        }
    }
    SrcFacts { parsed: true, n_defs, names, defs }
}

/// One language's call-graph input, in file order.
#[derive(Default)]
struct SrcGraphIn {
    names: Vec<String>,
    calls: Vec<Vec<String>>,
    nodes: Vec<SrcFn>,
}

impl SrcGraphIn {
    fn graph(self) -> (Network, Vec<(String, String)>, Vec<SrcFn>) {
        let (net, edges) = graph_from_calls(&self.names, &self.calls);
        (net, edges, self.nodes)
    }
}

/// Everything the compiled-source metrics read, from one pass over src/.
struct SrcScan {
    fns: SrcFns,
    names: std::collections::HashSet<String>,
    c_family: SrcGraphIn,
    rust: SrcGraphIn,
    fortran: SrcGraphIn,
    files: u64,
}

/// Parses each compiled file under src/ once. Counts skip vendored files, names take
/// every file, and the Rust graph skips vendored Rust. Facts for bytes seen before come from `cache`.
fn scan_src(root: &Path, files: &[String], cache: &mut cache::Cache) -> SrcScan {
    let mut parser = Parser::new();
    let (mut c, mut cpp, mut fortran, mut rust, mut nf) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut names = std::collections::HashSet::new();
    let mut c_family = SrcGraphIn::default();
    let mut rust_g = SrcGraphIn::default();
    let mut fortran_g = SrcGraphIn::default();
    let mut taken = 0u64;
    for f in files {
        if !f.starts_with("src/") || language_for_ext(f).is_none() {
            continue;
        }
        taken += 1;
        let Some(content) = read(root, f) else { continue };
        let ext = f.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        let facts = cache.get_or_compute("src", &ext, content.as_bytes(), || src_facts(&mut parser, &ext, &content));
        drop(content);
        if !facts.parsed {
            continue;
        }
        let vendored = is_vendored_src(f);
        if !vendored {
            nf += 1;
            match ext.as_str() {
                "c" | "h" => c += facts.n_defs,
                "cc" | "cpp" | "cxx" | "hpp" | "hxx" => cpp += facts.n_defs,
                "f" | "f90" | "f95" | "f03" | "f08" => fortran += facts.n_defs,
                "rs" => rust += facts.n_defs,
                _ => {}
            }
        }
        names.extend(facts.names);
        let Some(lang) = src_lang_of(&ext) else { continue };
        if lang == SrcLang::Rust && vendored {
            continue;
        }
        let g = match lang {
            SrcLang::CFamily => &mut c_family,
            SrcLang::Rust => &mut rust_g,
            SrcLang::Fortran => &mut fortran_g,
        };
        let lang_str = lang_str_of(&ext);
        for d in facts.defs {
            g.nodes.push(SrcFn { name: d.name.clone(), lang: lang_str, file: f.clone(), line: d.line, loc: d.loc });
            g.names.push(d.name);
            g.calls.push(d.callees);
        }
    }
    SrcScan {
        fns: SrcFns { total: c + cpp + fortran + rust, c, cpp, fortran, rust, n_files: nf },
        names,
        c_family,
        rust: rust_g,
        fortran: fortran_g,
        files: taken,
    }
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
                .trim_matches(|c| c == '"' || c == '\'' || c == '`')
                .to_string();
            let n_params = rhs
                .child_by_field_name("parameters")
                .map(|p| {
                    let mut pc = p.walk();
                    p.children(&mut pc).filter(|c| c.kind() == "parameter").count() as i64
                })
                .unwrap_or(0);
            // call sites within this function's body (for the call-network)
            let mut calls = Vec::new();
            let mut native_calls = Vec::new();
            let mut bst = vec![rhs];
            while let Some(n) = bst.pop() {
                if n.kind() == "call" {
                    if let Some(cn) = call_fn_name(&n, bytes) {
                        // a native call also carries its routine symbol (the R->C bridge)
                        if matches!(cn.as_str(), ".Call" | ".C" | ".Fortran" | ".External" | ".External2") {
                            if let Some(args) = n.child_by_field_name("arguments") {
                                if let Some(sym) = first_arg_symbol(args, bytes) {
                                    native_calls.push(sym);
                                }
                            }
                        }
                        calls.push(cn);
                    }
                }
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    bst.push(ch);
                }
            }
            fns.push(FnStat {
                exported: exported.contains(name.as_str()),
                name,
                file: f.to_string(),
                line: rhs.start_position().row + 1,
                loc: rhs.end_position().row - rhs.start_position().row + 1,
                n_params,
                cyclocomp: cyclocomp(rhs, bytes),
                calls,
                native_calls,
            });
        }
    }
    (fns, oo)
}

// ---- per-subdir whitespace / comment metrics --------------------------------

#[derive(Clone, Copy)]
enum Lang {
    R,
    C,
    Fortran,
    Rd,
    Other,
}

/// Family for a file extension, so extensions roll up into meaningful groups.
fn ext_category(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "r" | "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hxx" | "f" | "f90" | "f95" | "f03"
        | "f08" | "rs" | "py" | "java" | "scala" | "jl" | "go" | "sh" | "bash" | "pl" | "rb"
        | "lua" | "sql" | "stan" | "jags" | "bug" | "s" | "asm" => "code",
        "rmd" | "qmd" | "rnw" | "rd" | "md" | "markdown" | "tex" | "ltx" | "rst" | "org"
        | "adoc" | "asciidoc" | "ipynb" | "texi" | "pod" => "documentation",
        "csv" | "tsv" | "rda" | "rdata" | "rds" | "json" | "ndjson" | "xml" | "parquet"
        | "feather" | "txt" | "dat" | "tab" | "fwf" => "data",
        "yml" | "yaml" | "toml" | "ini" | "cfg" | "conf" | "dcf" | "lock" => "config",
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "css" | "scss" | "sass" | "less" | "html"
        | "htm" | "vue" | "svelte" => "web",
        _ => "other",
    }
}

fn lang_of(path: &str) -> Lang {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "r" => Lang::R,
        "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hxx" => Lang::C,
        "f" | "f90" | "f95" => Lang::Fortran,
        "rd" => Lang::Rd,
        _ => Lang::Other,
    }
}

/// (blank, comment) line counts for one file, language-aware. Approximate:
/// comment detection handles line comments plus C block comments.
fn line_stats(content: &str, lang: Lang) -> (i64, i64) {
    let mut blank = 0i64;
    let mut comment = 0i64;
    let mut in_block = false;
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank += 1;
            continue;
        }
        let is_comment = match lang {
            Lang::R => t.starts_with('#'),
            Lang::Rd => t.starts_with('%'),
            Lang::Fortran => {
                t.starts_with('!') || matches!(line.chars().next(), Some('c' | 'C' | '*'))
            }
            Lang::C => {
                if in_block {
                    if t.contains("*/") {
                        in_block = false;
                    }
                    true
                } else if t.starts_with("//") {
                    true
                } else if t.starts_with("/*") {
                    if !t.contains("*/") {
                        in_block = true;
                    }
                    true
                } else {
                    false
                }
            }
            Lang::Other => false,
        };
        if is_comment {
            comment += 1;
        }
    }
    (blank, comment)
}

/// Indentation style over R/ files: -1 for tabs, else the modal space width, 0 if none.
fn detect_indentation(root: &Path, r_files: &[&str]) -> i64 {
    let mut has_tab = false;
    let mut widths: BTreeMap<usize, usize> = BTreeMap::new();
    for f in r_files {
        let Some(c) = read(root, f) else { continue };
        for line in c.lines() {
            let leading: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            if leading.contains('\t') {
                has_tab = true;
            } else {
                let n = leading.len();
                if n > 0 {
                    *widths.entry(n).or_insert(0) += 1;
                }
            }
        }
    }
    if has_tab {
        return -1;
    }
    widths.into_iter().max_by_key(|&(_, count)| count).map(|(w, _)| w as i64).unwrap_or(0)
}

struct WhiteSpace {
    blank_lines_r: i64,
    comment_lines_r: i64,
    rel_space_r: Option<f64>,
    blank_lines_src: i64,
    comment_lines_src: i64,
    rel_space_src: Option<f64>,
    blank_lines_tests: i64,
    comment_lines_tests: i64,
    rel_space_tests: Option<f64>,
    indentation: i64,
}

fn metrics_whitespace(root: &Path, files: &[String]) -> WhiteSpace {
    let subdir = |prefix: &str| -> (i64, i64, i64) {
        let (mut blank, mut comment, mut total) = (0i64, 0i64, 0i64);
        for f in files {
            if !f.starts_with(prefix) || is_noncode(f) {
                continue;
            }
            let Some(c) = read(root, f) else { continue };
            let (b, cm) = line_stats(&c, lang_of(f));
            blank += b;
            comment += cm;
            total += loc(&c) as i64;
        }
        (blank, comment, total)
    };
    let rel = |blank: i64, total: i64| (total > 0).then(|| blank as f64 / total as f64);
    let (br, cr, tr) = subdir("R/");
    let (bs, cs, ts) = subdir("src/");
    let (bt, ct, tt) = subdir("tests/");
    WhiteSpace {
        blank_lines_r: br,
        comment_lines_r: cr,
        rel_space_r: rel(br, tr),
        blank_lines_src: bs,
        comment_lines_src: cs,
        rel_space_src: rel(bs, ts),
        blank_lines_tests: bt,
        comment_lines_tests: ct,
        rel_space_tests: rel(bt, tt),
        indentation: detect_indentation(root, &find_files(files, r"^R/.*\.[Rr]$")),
    }
}

// ---- call network -----------------------------------------------------------
// A syntactic call graph: nodes are the package's top-level R functions, a
// directed edge A -> B means A's body calls B (B also a package function).
// Metrics are our own definitions.

#[derive(Debug, PartialEq)]
struct Network {
    n_nodes: i64,
    n_edges: i64,
    n_clusters: i64,     // connected components among functions that have >=1 edge
    n_isolated: i64,     // functions with no internal call edge
    node_degree_mean: Option<f64>,
    node_degree_median: Option<f64>,
    node_degree_max: i64,
    n_terminal_nodes: i64, // sinks: functions that call no other package function
    betweenness_mean: Option<f64>,
    betweenness_median: Option<f64>,
    betweenness_max: Option<f64>,
}

fn mean_f(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}
fn median_f(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    Some(if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 })
}

fn uf_find(p: &mut [usize], x: usize) -> usize {
    if p[x] != x {
        let r = uf_find(p, p[x]);
        p[x] = r;
    }
    p[x]
}

/// Brandes betweenness centrality, directed and unweighted (raw, not normalized).
fn brandes(n: usize, adj: &[Vec<usize>]) -> Vec<f64> {
    let mut bc = vec![0.0f64; n];
    for s in 0..n {
        let mut stack = Vec::new();
        let mut pred: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut sigma = vec![0.0f64; n];
        sigma[s] = 1.0;
        let mut dist = vec![-1i64; n];
        dist[s] = 0;
        let mut q = std::collections::VecDeque::new();
        q.push_back(s);
        while let Some(v) = q.pop_front() {
            stack.push(v);
            for &w in &adj[v] {
                if dist[w] < 0 {
                    dist[w] = dist[v] + 1;
                    q.push_back(w);
                }
                if dist[w] == dist[v] + 1 {
                    sigma[w] += sigma[v];
                    pred[w].push(v);
                }
            }
        }
        let mut delta = vec![0.0f64; n];
        while let Some(w) = stack.pop() {
            let dw = delta[w];
            for &v in &pred[w] {
                delta[v] += (sigma[v] / sigma[w]) * (1.0 + dw);
            }
            if w != s {
                bc[w] += delta[w];
            }
        }
    }
    bc
}

/// Build a call graph from named nodes and their (possibly external) callees,
/// returning the network statistics and the resolved edges as name pairs.
/// Callees not in `names` are dropped.
fn graph_from_calls(
    names: &[String],
    per_node_calls: &[Vec<String>],
) -> (Network, Vec<(String, String)>) {
    let idx: std::collections::HashMap<&str, usize> =
        names.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
    let mut edge_set: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    for (i, calls) in per_node_calls.iter().enumerate() {
        for callee in calls {
            if let Some(&j) = idx.get(callee.as_str()) {
                if i != j {
                    edge_set.insert((i, j));
                }
            }
        }
    }
    // Node order, so the edge records and the betweenness sums never depend on a hash seed.
    let mut edges: Vec<(usize, usize)> = edge_set.into_iter().collect();
    edges.sort_unstable();
    let named: Vec<(String, String)> =
        edges.iter().map(|&(a, b)| (names[a].clone(), names[b].clone())).collect();
    (graph_stats(names.len(), &edges), named)
}

fn metrics_network(fns: &[FnStat]) -> (Network, Vec<(String, String)>) {
    let names: Vec<String> = fns.iter().map(|f| f.name.clone()).collect();
    let calls: Vec<Vec<String>> = fns.iter().map(|f| f.calls.clone()).collect();
    graph_from_calls(&names, &calls)
}

fn graph_stats(n: usize, edges: &[(usize, usize)]) -> Network {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut out_deg = vec![0usize; n];
    let mut deg = vec![0usize; n];
    let mut parent: Vec<usize> = (0..n).collect();
    for &(a, b) in edges {
        adj[a].push(b);
        out_deg[a] += 1;
        deg[a] += 1;
        deg[b] += 1;
        let (ra, rb) = (uf_find(&mut parent, a), uf_find(&mut parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    }
    // clusters among connected nodes (degree >= 1); isolated counted separately
    let mut roots = std::collections::HashSet::new();
    let mut n_isolated = 0i64;
    for i in 0..n {
        if deg[i] >= 1 {
            roots.insert(uf_find(&mut parent, i));
        } else {
            n_isolated += 1;
        }
    }
    let degrees: Vec<i64> = deg.iter().map(|&d| d as i64).collect();
    // a terminal (sink) is a connected function that calls no package function
    let n_terminal_nodes =
        (0..n).filter(|&i| deg[i] >= 1 && out_deg[i] == 0).count() as i64;
    let bc = brandes(n, &adj);
    Network {
        n_nodes: n as i64,
        n_edges: edges.len() as i64,
        n_clusters: roots.len() as i64,
        n_isolated,
        node_degree_mean: mean_i(&degrees),
        node_degree_median: median_i(&degrees),
        node_degree_max: degrees.iter().copied().max().unwrap_or(0),
        n_terminal_nodes,
        betweenness_mean: mean_f(&bc),
        betweenness_median: median_f(&bc),
        betweenness_max: bc.iter().copied().fold(None::<f64>, |m, x| Some(m.map_or(x, |v| v.max(x)))),
    }
}

// ---- main -------------------------------------------------------------------

/// The version that produced a record. A consumer storing results needs to know
/// which build wrote them, so it can tell data it has already collected from
/// data a newer build would describe differently.
const ANALYZER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where one run's time went, for the line RPKG_ANALYZER_STATS asks for.
#[derive(Default)]
struct RunStats {
    ms_compiled: f64,
    ms_r: f64,
    ms_tests: f64,
    ms_data: f64,
    files_compiled: u64,
    files_r: u64,
    files_tests: u64,
    files_data: u64,
    hits_compiled: u64,
    cache_errors: u64,
    verify_mismatch: u64,
}

/// Milliseconds since `t`, to the microsecond.
fn ms_since(t: std::time::Instant) -> f64 {
    (t.elapsed().as_secs_f64() * 1e6).round() / 1e3
}

/// The figure in kB on the line of a process status that starts with `key`.
fn status_kb(status: &str, key: &str) -> Option<u64> {
    let rest = status.lines().find_map(|l| l.strip_prefix(key))?;
    rest.split_whitespace().next()?.parse().ok()
}

/// The most memory this process has had resident and the most address space
/// it has had, in kB, where the system keeps both: Linux does, in a file. The
/// file is the process's, whichever thread reads it.
fn peak_memory_kb() -> (Option<u64>, Option<u64>) {
    match std::fs::read_to_string("/proc/self/status") {
        Ok(status) => (status_kb(&status, "VmHWM:"), status_kb(&status, "VmPeak:")),
        Err(_) => (None, None),
    }
}

/// Appends one JSON line to the file RPKG_ANALYZER_STATS names, after the records.
/// Unset or empty writes nothing, and any error is ignored.
fn write_run_stats(s: &RunStats, total_ms: f64) {
    let Some(path) = std::env::var_os("RPKG_ANALYZER_STATS").filter(|p| !p.is_empty()) else {
        return;
    };
    let other = total_ms - s.ms_compiled - s.ms_r - s.ms_tests - s.ms_data;
    let (peak_rss_kb, peak_vm_kb) = peak_memory_kb();
    let line = serde_json::json!({
        "build": ANALYZER_VERSION,
        "ms": total_ms,
        "ms_compiled": s.ms_compiled,
        "ms_r": s.ms_r,
        "ms_tests": s.ms_tests,
        "ms_data": s.ms_data,
        "ms_other": (other.max(0.0) * 1e3).round() / 1e3,
        "compiled": {"files": s.files_compiled, "hits": s.hits_compiled},
        "r": {"files": s.files_r, "hits": 0},
        "tests": {"files": s.files_tests, "hits": 0},
        "data": {"files": s.files_data, "hits": 0},
        "cache_errors": s.cache_errors,
        "verify_mismatch": s.verify_mismatch,
        // Null where the system keeps no such figure, so the keys are the
        // same everywhere.
        "peak_rss_kb": peak_rss_kb,
        "peak_vm_kb": peak_vm_kb,
    });
    let appended = std::fs::OpenOptions::new().create(true).append(true).open(path);
    if let Ok(mut f) = appended {
        let _ = std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes());
    }
}

/// Debug output: the parse tree of one file, or a histogram of its node kinds.
fn print_parse_tree(f: &str, histogram: bool) {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .expect("load tree-sitter-r");
    let src = std::fs::read_to_string(f).expect("read");
    if let Some(lang) = language_for_ext(f) {
        parser.set_language(&lang).expect("load grammar");
    }
    let tree = parser.parse(&src, None).expect("parse");
    if !histogram {
        println!("{}", tree.root_node().to_sexp());
        return;
    }
    let mut hist: BTreeMap<String, i64> = BTreeMap::new();
    let mut st = vec![tree.root_node()];
    while let Some(x) = st.pop() {
        *hist.entry(x.kind().to_string()).or_insert(0) += 1;
        let mut c = x.walk();
        for ch in x.children(&mut c) {
            st.push(ch);
        }
    }
    for (k, v) in hist {
        println!("{v}\t{k}");
    }
}

/// Debug output: the per-file decisions behind the summary (release boundary, help
/// pages, vignettes), one record per file.
fn explain(dir: &str, kind: cli::InputKind) {
    let root = PathBuf::from(dir);
    let tree_files = list_files(&root);
    let desc = read(&root, "DESCRIPTION").map(|t| parse_dcf(&t)).unwrap_or_default();
    let package = desc.get("Package").map(|s| s.trim().to_string()).unwrap_or_default();
    let release = release_files::ReleaseList::for_input(&root, &tree_files, &package, kind);
    let files = release.files(&tree_files);
    let known = release.is_known();
    for f in &tree_files {
        let kept = known.then(|| files.contains(f));
        println!("{}", serde_json::json!({"rec": "release_file", "path": f, "kept": kept}));
    }
    for p in rd_pages::page_facts(&root, &files, &package) {
        println!(
            "{}",
            serde_json::json!({
                "rec": "rd_page", "file": p.file,
                "examples": p.examples.as_ref().map(|e| e.class.as_str()),
                "conditional": p.examples.as_ref().map(|e| e.conditional),
                "internal": p.internal, "doc_type": p.doc_type, "package_overview": p.package_overview,
            })
        );
    }
    for v in vignettes::vignette_sources(&root, &files) {
        let run = read_lossy(&root, &v).map(|t| {
            let orig = format!("{v}.orig");
            vignettes::classify(&v, &t, files.contains(&orig)).as_str()
        });
        println!("{}", serde_json::json!({"rec": "vignette", "file": v, "run": run}));
    }
}

fn main() {
    memory::on_a_deep_stack(run);
}

/// One run of the analyzer, from its arguments to its last record.
fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match cli::parse_args(&args) {
        Ok(mode) => mode,
        Err(usage) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    };
    let (dir, kind) = match mode {
        cli::Mode::Version => {
            println!("rpkg-analyzer {ANALYZER_VERSION}");
            return;
        }
        // The dataset records only, read from the tree as given.
        cli::Mode::Datasets(f) => {
            for rec in rds::scan_package(Path::new(&f), &BTreeSet::new()) {
                println!("{rec}");
            }
            return;
        }
        cli::Mode::Sexp(f) => return print_parse_tree(&f, false),
        cli::Mode::Kinds(f) => return print_parse_tree(&f, true),
        cli::Mode::Explain { dir, kind } => return explain(&dir, kind),
        cli::Mode::Analyze { dir, kind } => (dir, kind),
    };
    let t_run = std::time::Instant::now();
    let mut stats = RunStats::default();

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .expect("load tree-sitter-r");

    let root = PathBuf::from(&dir);
    let tree_files = list_files(&root);
    let desc = read(&root, "DESCRIPTION").map(|t| parse_dcf(&t)).unwrap_or_default();
    let release = release_files::ReleaseList::for_input(
        &root,
        &tree_files,
        desc.get("Package").map(|s| s.trim()).unwrap_or(""),
        kind,
    );
    let files = release.files(&tree_files);
    let excluded: BTreeSet<String> = release.excluded_files();
    let content_known = release.is_known();

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

    // --- extended languages: LOC by extension over the WHOLE package (incl.
    // inst/ web assets), distinct from the code-dir lang_breakdown. Flags
    // likely-minified/generated files (a very long single line).
    let mut all_lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut minified_asset_files = 0i64;
    for f in &files {
        if is_noncode(f) {
            continue;
        }
        let Some(content) = read(&root, f) else { continue };
        *all_lang.entry(file_ext(f)).or_insert(0) += loc(&content);
        if content.lines().any(|ln| ln.chars().count() > 2000) {
            minified_asset_files += 1;
        }
    }
    let has_web_assets = all_lang.keys().any(|e| {
        matches!(
            e.as_str(),
            "js" | "html" | "htm" | "css" | "ts" | "jsx" | "tsx" | "vue" | "scss" | "sass"
        )
    });
    // Roll extensions up into families (code / documentation / data / config / web).
    let mut language_categories: BTreeMap<String, usize> = BTreeMap::new();
    for (ext, l) in &all_lang {
        *language_categories.entry(ext_category(ext).to_string()).or_insert(0) += l;
    }

    // --- DESCRIPTION ---
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

    // Total expanded source size on disk (static proxy for install footprint;
    // the true installed size needs a build / the r-universe API).
    let total_source_size: u64 = files
        .iter()
        .filter_map(|f| std::fs::metadata(root.join(f)).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum();

    // Test cases across frameworks. Each test file is classified by its content
    // and counted with that framework's unit; the framework set is also reported.
    let tf_test_files =
        find_files(&files, r"^(?:tests|inst/tinytest|inst/unitTests)/.*\.[Rr]$");
    let re_testthat = regex!(r"\b(?:test_that|describe|it)\s*\(");
    let re_expect = regex!(r"\bexpect_\w+\s*\(");
    let re_unittest = regex!(r"\bok(?:_group)?\s*\(");
    let re_runit_fn = regex!(r"(?m)^\s*test[.\w]*\s*(?:<-|=)\s*function");
    let re_runit_check = regex!(r"\bcheck(?:Equals|True|Identical|Exception)\w*\s*\(");
    let re_testit = regex!(r"\bassert\s*\(");
    let mut n_test_cases = 0i64;
    let mut testing_frameworks: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for f in &tf_test_files {
        let Some(c) = read(&root, f) else { continue };
        if re_testthat.is_match(&c) {
            testing_frameworks.insert("testthat".into());
            n_test_cases += re_testthat.find_iter(&c).count() as i64;
        } else if re_unittest.is_match(&c) {
            testing_frameworks.insert("unittest".into());
            n_test_cases += re_unittest.find_iter(&c).count() as i64;
        } else if f.starts_with("inst/tinytest") || re_expect.is_match(&c) {
            testing_frameworks.insert("tinytest".into());
            n_test_cases += re_expect.find_iter(&c).count() as i64;
        } else if re_runit_fn.is_match(&c) || re_runit_check.is_match(&c) {
            testing_frameworks.insert("RUnit".into());
            let n = re_runit_fn.find_iter(&c).count();
            n_test_cases += if n > 0 { n as i64 } else { re_runit_check.find_iter(&c).count() as i64 };
        } else if re_testit.is_match(&c) {
            testing_frameworks.insert("testit".into());
            n_test_cases += re_testit.find_iter(&c).count() as i64;
        }
    }
    // unitizer is expression-based (no discrete case token); flag by its dir.
    if files.iter().any(|f| f.starts_with("tests/unitizer/")) {
        testing_frameworks.insert("unitizer".into());
    }
    // frameworks can be declared without our parser recognizing a case.
    let suggests_lc = desc.get("Suggests").map(|s| s.to_lowercase()).unwrap_or_default();
    for (dep, name) in [
        ("testthat", "testthat"),
        ("tinytest", "tinytest"),
        ("runit", "RUnit"),
        ("testit", "testit"),
        ("unittest", "unittest"),
        ("unitizer", "unitizer"),
        ("svunit", "svUnit"),
        ("quickcheck", "quickcheck"),
        ("hedgehog", "hedgehog"),
    ] {
        if suggests_lc.split(',').any(|d| d.trim().starts_with(dep)) {
            testing_frameworks.insert(name.into());
        }
    }
    let testing_frameworks: Vec<String> = testing_frameworks.into_iter().collect();

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

    // Dataset object names: data/datalist is authoritative (a line is either
    // `name` or `file: obj1 obj2`); otherwise fall back to file stems. A single
    // .rda without a datalist can still hold several objects we cannot see here.
    let datalist = exists(&files, "data/datalist").then(|| read(&root, "data/datalist")).flatten();
    let datasets: Vec<String> = if let Some(dl) = datalist {
        let mut names = Vec::new();
        for line in dl.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match line.split_once(':') {
                Some((_, objs)) => names.extend(objs.split_whitespace().map(String::from)),
                None => names.push(line.to_string()),
            }
        }
        names
    } else {
        let mut names: Vec<String> = data_names
            .iter()
            .map(|f| f.rsplit_once('.').map(|(s, _)| s).unwrap_or(f).to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    };

    let package = get("Package");
    let version = get("Version");

    // --- NAMESPACE ---
    let ns = exists(&files, "NAMESPACE")
        .then(|| read(&root, "NAMESPACE"))
        .flatten()
        .map(|t| parse_namespace(&t, &mut parser))
        .unwrap_or_default();
    let has_ns = exists(&files, "NAMESPACE");
    let t_r = std::time::Instant::now();
    let funcs = metrics_functions(&root, &files, &ns, has_ns, &package);

    // AST-derived per-function stats + OO kinds (new v2 metrics).
    let (fn_stats, oo) = metrics_ast(&root, &files, &ns.exports, &mut parser);
    let nexpr = metrics_nexpr(&root, &files, &mut parser);
    stats.ms_r += ms_since(t_r);
    stats.files_r = find_files(&files, r"^R/.*\.[Rr]$").len() as u64;
    let fn_locs: Vec<i64> = fn_stats.iter().map(|f| f.loc as i64).collect();
    let fn_cyclos: Vec<i64> = fn_stats.iter().map(|f| f.cyclocomp).collect();
    let exp_params: Vec<i64> =
        fn_stats.iter().filter(|f| f.exported).map(|f| f.n_params).collect();
    let n_fns_r = fn_stats.len();
    let n_fns_r_exported = fn_stats.iter().filter(|f| f.exported).count();
    let news_file = news::news_file(&files);
    let release_notes = news_file.and_then(|p| {
        let text = read_lossy(&root, p)?;
        news::release_notes(p, &text, &version, &package)
    });
    let ex = metrics_extra(&desc, &root, &files, news_file);
    let t_net = std::time::Instant::now();
    let (net, r_edges) = metrics_network(&fn_stats);
    stats.ms_r += ms_since(t_net);
    let ws = metrics_whitespace(&root, &files);
    let t_src = std::time::Instant::now();
    let mut src_cache = cache::Cache::from_env();
    let scan = scan_src(&root, &files, &mut src_cache);
    let src_fns = scan.fns;
    let ng = metrics_native_graph(&root, &files, &fn_stats, &scan.names);
    let (cnet, c_edges, c_nodes) = scan.c_family.graph();
    let (rnet, rust_edges, rust_nodes) = scan.rust.graph();
    let (fnet, fortran_edges, fortran_nodes) = scan.fortran.graph();
    stats.ms_compiled += ms_since(t_src);
    stats.files_compiled = scan.files;
    stats.hits_compiled = src_cache.stats.hits;
    stats.cache_errors = src_cache.stats.errors;
    stats.verify_mismatch = src_cache.stats.verify_mismatch;

    // Deprecated R functions: body calls .Deprecated/.Defunct or lifecycle::deprecate_*.
    let dep_calls = [".Deprecated", ".Defunct", "deprecate_soft", "deprecate_warn", "deprecate_stop"];
    let n_deprecated_functions = fn_stats
        .iter()
        .filter(|f| f.calls.iter().any(|c| dep_calls.contains(&c.as_str())))
        .count() as i64;
    // LOC per function split by exported vs internal.
    let exp_locs: Vec<i64> = fn_stats.iter().filter(|f| f.exported).map(|f| f.loc as i64).collect();
    let int_locs: Vec<i64> = fn_stats.iter().filter(|f| !f.exported).map(|f| f.loc as i64).collect();

    // Documentation lines per help page (Rd file LOC).
    let rd_locs: Vec<i64> = rd_pages::rd_page_files(&files)
        .iter()
        .filter_map(|f| read(&root, f).map(|c| loc(&c) as i64))
        .collect();

    // Structured SystemRequirements.
    let sysreq = desc.get("SystemRequirements").cloned().unwrap_or_default();
    let sysreq_lc = sysreq.to_lowercase();
    let sysreq_has_java = sysreq_lc.contains("java");
    let sysreq_has_gnu_make = sysreq_lc.contains("gnu make") || sysreq_lc.contains("gnumake");
    let sysreq_components: Vec<String> = sysreq
        .split([',', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Static-check-style signals over R source (native interface, library() in
    // package code, .Internal, super-assignment).
    let native_re = regex!(r"\.(Call|C|Fortran|External2?)\s*\(");
    let library_re = regex!(r"\b(?:library|require)\s*\(");
    let internal_re = regex!(r"\.Internal\s*\(");
    let ga_re = regex!(r"<<-");
    let (mut n_native_calls, mut n_library_calls, mut n_internal_calls, mut n_global_assign) =
        (0i64, 0i64, 0i64, 0i64);
    for f in &find_files(&files, r"^R/.*\.[Rr]$") {
        let Some(c) = read(&root, f) else { continue };
        n_native_calls += native_re.find_iter(&c).count() as i64;
        n_library_calls += library_re.find_iter(&c).count() as i64;
        n_internal_calls += internal_re.find_iter(&c).count() as i64;
        n_global_assign += ga_re.find_iter(&c).count() as i64;
    }

    // --- legal + portability + tests ---
    let legal = metrics_legal(&desc, &root, &files);
    let port = metrics_portability(&desc, &root, &files);
    let t_tests = std::time::Instant::now();
    let tests = metrics_tests(&desc, &root, &files, &ns.exports);
    let suite = test_suite::test_suite(&root, &files, &dep_names(&get("Suggests")));
    stats.ms_tests += ms_since(t_tests);
    stats.files_tests = tf_test_files.len() as u64;
    let blocks = suite.blocks.as_ref();
    let docs = metrics_docs(&desc, &root, &files, &ns.exports, news_file);
    let rd = rd_pages::page_facts(&root, &files, &package);
    let exc = rd_pages::example_counts(&rd);
    let topics = rd_pages::topic_counts(&rd, &ns.exports);
    // With no example pages there is nothing to break down, so the buckets stay NULL.
    let bucket = |v: i64| (exc.pages > 0).then_some(v);
    let health = metrics_health(&desc, &root, &files);
    let meta = metrics_meta(&desc, &root, &files);
    let security = metrics_security(&desc, &root, &files);
    let practices = repo_practices::metrics_repo_practices(&root, &tree_files, kind);
    let cit = exists(&files, "inst/CITATION").then(|| match std::fs::read(root.join("inst/CITATION")) {
        Ok(bytes) => citation::read_citation(&bytes, &desc),
        Err(_) => citation::Citation::parse_error(),
    });

    // --- additional static signals ---
    // OpenMP: SystemRequirements, Makevars -fopenmp, or a src #pragma omp / _OPENMP.
    let uses_openmp = desc
        .get("SystemRequirements")
        .map(|s| s.to_lowercase().contains("openmp"))
        .unwrap_or(false)
        || find_files(&files, r"^src/Makevars(\.win)?$")
            .iter()
            .any(|f| read(&root, f).map(|c| c.contains("-fopenmp")).unwrap_or(false))
        || files.iter().filter(|f| is_src_file(f)).any(|f| {
            read(&root, f)
                .map(|c| c.contains("#pragma omp") || c.contains("_OPENMP"))
                .unwrap_or(false)
        });

    let vig = vignettes::vignette_facts(&root, &files);
    let num_demos = find_files(&files, r"^demo/.*\.[Rr]$").len();
    let count_prefix = |p: &str| files.iter().filter(|f| f.starts_with(p)).count();
    let files_r = count_prefix("R/");
    let files_src = count_prefix("src/");
    let files_tests = count_prefix("tests/");
    let files_inst = count_prefix("inst/include/"); // inst counts only inst/include
    let files_vignettes = count_prefix("vignettes/");

    // Translations: language codes from po/*.po|.pot filenames (e.g. R-de.po -> de).
    let mut translations: Vec<String> = files
        .iter()
        .filter(|f| f.starts_with("po/") && (f.ends_with(".po") || f.ends_with(".pot")))
        .filter_map(|f| {
            let base = f.rsplit('/').next()?;
            let stem = base.trim_end_matches(".pot").trim_end_matches(".po");
            Some(stem.rsplit('-').next().unwrap_or(stem).to_string())
        })
        .collect();
    translations.sort();
    translations.dedup();

    // Author role counts from Authors@R.
    let authors_r = desc.get("Authors@R").cloned().unwrap_or_default();
    let role_count = |role: &str| {
        kept_regex(&format!(r#"["']{role}["']"#))
            .unwrap()
            .find_iter(&authors_r)
            .count() as i64
    };
    let (desc_n_aut, desc_n_cre, desc_n_ctb, desc_n_cph, desc_n_fnd, desc_n_rev, desc_n_trl) = (
        role_count("aut"),
        role_count("cre"),
        role_count("ctb"),
        role_count("cph"),
        role_count("fnd"),
        role_count("rev"),
        role_count("trl"),
    );

    // --- emit NDJSON ---
    let mut summary = serde_json::json!({
        "rec": "summary",
        "analyzer_version": ANALYZER_VERSION,
        "input_kind": kind.as_str(),
        "extdata": rds::extdata_inventory(&root, &excluded),
        "package": package,
        "version": version,
        "license": legal.license,
        "spdx_valid": legal.spdx_valid,
        "osi_approved": legal.osi_approved,
        "license_file_completeness": legal.license_file_completeness,
        "has_citation": cit.is_some(),
        "citation_read": cit.as_ref().map(|c| c.read),
        "citation_kind": cit.as_ref().and_then(|c| c.kind),
        "citation_n_entries": cit.as_ref().and_then(|c| c.n_entries),
        "citation_bibtype": cit.as_ref().and_then(|c| c.bibtypes.clone()),
        "citation_dois": cit.as_ref().and_then(|c| c.dois.clone()),
        "citation_venue": cit.as_ref().and_then(|c| c.venues.clone()),
        "has_rd_bibliography": exists(&files, "inst/REFERENCES.bib") || exists(&files, "inst/REFERENCES.R"),
        "min_r_version": port.min_r_version,
        "system_requirements_count": port.system_requirements_count,
        "cxx_standard_required": port.cxx_standard_required,
        "nonportable_compiler_flags": port.nonportable_compiler_flags,
        "nonportable_compiler_flags_json": port.nonportable_compiler_flags_json,
        "has_vignettes": !vig.sources.is_empty(),
        "vignette_dynamic": vig.dynamic,
        "vignette_eval_gated": vig.eval_gated,
        "has_tests": tests.has_tests,
        "test_to_code_ratio": tests.test_to_code_ratio,
        "testthat_edition": tests.testthat_edition,
        "snapshot_test_count": tests.snapshot_test_count,
        "test_isolation_libs": tests.test_isolation_libs,
        "exported_fn_test_linkage": tests.exported_fn_test_linkage,
        "stochastic_seed_discipline": tests.stochastic_seed_discipline,
        "test_framework_primary": suite.primary,
        "test_frameworks_used": suite.used,
        "test_frameworks_declared": suite.declared,
        "n_test_units": suite.n_units,
        "test_unit": suite.unit,
        "n_rout_save": suite.n_rout_save,
        "n_test_blocks": blocks.map(|b| b.total),
        "n_test_blocks_cran_skipped": blocks.map(|b| b.skipped),
        "tests_gated_not_cran": blocks.map(|b| b.gated),
        "ci_present": practices.ci_present,
        "ci_type": practices.ci_type,
        "ci_matrix_breadth": practices.ci_matrix_breadth,
        "ci_pr_gated": practices.ci_pr_gated,
        "dontrun_example_ratio": docs.dontrun_example_ratio,
        "n_help_topics": topics.pages,
        "n_help_topics_internal": topics.internal,
        "n_help_topics_data": topics.data,
        "n_help_topics_package": topics.package,
        "examples_coverage_fn": topics.examples_coverage_fn,
        "examples_coverage_fn_basis": topics.basis,
        "rd_example_pages": exc.pages,
        "rd_example_pages_run": bucket(exc.run),
        "rd_example_pages_donttest_only": bucket(exc.donttest_only),
        "rd_example_pages_never_run": bucket(exc.never_run),
        "rd_example_pages_empty": bucket(exc.empty),
        "rd_example_pages_conditional": bucket(exc.conditional),
        "undocumented_params_rate": docs.undocumented_params_rate,
        "value_doc_rate": docs.value_doc_rate,
        "references_coverage": docs.references_coverage,
        "roxygen_doc_coverage": docs.roxygen_doc_coverage,
        "has_readme": docs.has_readme,
        "readme_prose_length": docs.readme_prose_length,
        "has_pkgdown": practices.has_pkgdown,
        "news_present": docs.news_present,
        "news_structure_quality": docs.news_structure_quality,
        "news_file": news_file,
        "release_notes_source": release_notes.as_ref().map(|n| n.source),
        "changelog_file": news::changelog_file(&files),
        "on_exit_coverage_rate": health.on_exit_coverage_rate,
        "global_state_write_density": health.global_state_write_density,
        "deprecated_idiom_density": health.deprecated_idiom_density,
        "debug_artifact_density": health.debug_artifact_density,
        "has_code_of_conduct": practices.has_code_of_conduct,
        "has_contributing_guide": practices.has_contributing_guide,
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
        "all_languages": all_lang,
        "language_categories": language_categories,
        "has_web_assets": has_web_assets,
        "minified_asset_files": minified_asset_files,
        "nexpr": nexpr,
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
        "n_fns_src": src_fns.total,
        "n_fns_c": src_fns.c,
        "n_fns_cpp": src_fns.cpp,
        "n_fns_fortran": src_fns.fortran,
        "n_fns_rust": src_fns.rust,
        "n_fns_per_file_src": (src_fns.n_files > 0).then(|| src_fns.total as f64 / src_fns.n_files as f64),
        "n_deprecated_functions": n_deprecated_functions,
        "loc_per_fn_exported_mean": mean_i(&exp_locs),
        "loc_per_fn_exported_median": median_i(&exp_locs),
        "loc_per_fn_internal_mean": mean_i(&int_locs),
        "loc_per_fn_internal_median": median_i(&int_locs),
        "doclines_per_fn_mean": mean_i(&rd_locs),
        "doclines_per_fn_median": median_i(&rd_locs),
        "sysreq_has_java": sysreq_has_java,
        "sysreq_has_gnu_make": sysreq_has_gnu_make,
        "sysreq_components": sysreq_components,
        "n_native_calls": n_native_calls,
        "n_native_call_sites": ng.n_native_call_sites,
        "n_native_edges": ng.n_native_edges,
        "n_native_targets": ng.n_native_targets,
        "native_resolution_rate": ng.native_resolution_rate,
        "cnet_n_nodes": cnet.n_nodes,
        "cnet_n_edges": cnet.n_edges,
        "cnet_n_clusters": cnet.n_clusters,
        "cnet_node_degree_max": cnet.node_degree_max,
        "cnet_betweenness_max": cnet.betweenness_max,
        "rnet_n_nodes": rnet.n_nodes,
        "rnet_n_edges": rnet.n_edges,
        "rnet_n_clusters": rnet.n_clusters,
        "rnet_node_degree_max": rnet.node_degree_max,
        "rnet_betweenness_max": rnet.betweenness_max,
        "fnet_n_nodes": fnet.n_nodes,
        "fnet_n_edges": fnet.n_edges,
        "fnet_n_clusters": fnet.n_clusters,
        "fnet_node_degree_max": fnet.node_degree_max,
        "fnet_betweenness_max": fnet.betweenness_max,
        "n_library_calls_in_r": n_library_calls,
        "n_internal_calls": n_internal_calls,
        "n_global_assign": n_global_assign,
        "n_s4_classes": oo.s4_classes,
        "n_s4_generics": oo.s4_generics,
        "n_s4_methods": oo.s4_methods,
        "n_r6_classes": oo.r6_classes,
        "n_rc_classes": oo.rc_classes,
        "n_s7_classes": oo.s7_classes,
        "n_s3_methods": ns.s3_methods,
        "uses_usemethod": oo.uses_usemethod,
        "has_recognized_repo": ex.has_recognized_repo,
        "repo_host": ex.repo_host,
        "repo_url": ex.repo_url,
        "help_pages_with_examples": ex.help_pages_with_examples,
        "examples_coverage": ex.examples_coverage,
        "news_up_to_date": ex.news_up_to_date,
        "net_n_nodes": net.n_nodes,
        "net_n_edges": net.n_edges,
        "net_n_clusters": net.n_clusters,
        "net_n_isolated": net.n_isolated,
        "net_node_degree_mean": net.node_degree_mean,
        "net_node_degree_median": net.node_degree_median,
        "net_node_degree_max": net.node_degree_max,
        "net_n_terminal_nodes": net.n_terminal_nodes,
        "net_betweenness_mean": net.betweenness_mean,
        "net_betweenness_median": net.betweenness_median,
        "net_betweenness_max": net.betweenness_max,
        "blank_lines_r": ws.blank_lines_r,
        "comment_lines_r": ws.comment_lines_r,
        "rel_space_r": ws.rel_space_r,
        "blank_lines_src": ws.blank_lines_src,
        "comment_lines_src": ws.comment_lines_src,
        "rel_space_src": ws.rel_space_src,
        "blank_lines_tests": ws.blank_lines_tests,
        "comment_lines_tests": ws.comment_lines_tests,
        "rel_space_tests": ws.rel_space_tests,
        "indentation": ws.indentation,
        "uses_openmp": uses_openmp,
        "num_vignettes": vig.sources.len(),
        "num_demos": num_demos,
        "files_r": files_r,
        "files_src": files_src,
        "files_tests": files_tests,
        "files_inst": files_inst,
        "files_vignettes": files_vignettes,
        "translations": translations,
        "desc_n_aut": desc_n_aut,
        "desc_n_cre": desc_n_cre,
        "desc_n_ctb": desc_n_ctb,
        "desc_n_cph": desc_n_cph,
        "desc_n_fnd": desc_n_fnd,
        "desc_n_rev": desc_n_rev,
        "desc_n_trl": desc_n_trl,
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
        "datasets": datasets,
        "total_source_size": total_source_size,
        "n_test_cases": n_test_cases,
        "testing_frameworks": testing_frameworks,
        "export_patterns": ns.export_patterns,
        "s3_methods": ns.s3_methods,
        "export_classes": ns.export_classes,
        "export_methods": ns.export_methods,
        "import_from": ns.import_from,
        "imports_whole": ns.imports_whole,
        "use_dyn_lib": ns.use_dyn_lib,
        "build_ignore_bad_lines": release.bad_lines(),
        "build_ignored": match &release {
            release_files::ReleaseList::Filtered(f) => Some(release_files::build_ignored_items(
                &tree_files,
                f,
                &vignettes::vignette_sources(&root, &tree_files),
            )),
            _ => None,
        },
    });
    if !content_known {
        release_files::null_release_content(&mut summary);
    }
    println!("{summary}");
    // Detail records describe release contents, which an unknown file list cannot give.
    if content_known {
        if let (Some(p), Some(n)) = (news_file, &release_notes) {
            println!(
                "{}",
                serde_json::json!({
                    "rec": "release_notes", "package_version": version, "news_file": p,
                    "release_notes_source": n.source, "release_notes": n.text,
                    "release_notes_truncated": n.truncated,
                })
            );
        }
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
                    "rec": "function", "lang": "r", "name": fnst.name, "exported": fnst.exported,
                    "file": fnst.file, "line": fnst.line, "loc": fnst.loc,
                    "n_params": fnst.n_params, "cyclocomp": fnst.cyclocomp,
                })
            );
        }
        // Compiled function nodes, so C/C++/Rust/Fortran endpoints in the unified
        // graph carry the same file/line/loc metadata as the R nodes.
        for sf in c_nodes.iter().chain(&rust_nodes).chain(&fortran_nodes) {
            println!(
                "{}",
                serde_json::json!({
                    "rec": "function", "lang": sf.lang, "name": sf.name,
                    "file": sf.file, "line": sf.line, "loc": sf.loc,
                })
            );
        }
        // Call-graph edges: the raw structure behind the network stats. Nodes are the
        // `function` records above (R) or compiled function names.
        for (graph, edges) in [
            ("r", &r_edges),
            ("native", &ng.edges),
            ("c", &c_edges),
            ("rust", &rust_edges),
            ("fortran", &fortran_edges),
        ] {
            for (from, to) in edges {
                println!(
                    "{}",
                    serde_json::json!({"rec": "call_edge", "graph": graph, "from": from, "to": to})
                );
            }
        }
    }
    // Full parsed DESCRIPTION as a raw intermediate: every field is preserved,
    // modeled or not, so future metrics derive from stored data without re-cloning.
    let mut dcf = serde_json::Map::new();
    dcf.insert("rec".into(), serde_json::Value::String("dcf".into()));
    for (k, v) in &desc {
        dcf.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    println!("{}", serde_json::Value::Object(dcf));

    // Datasets under data/ and R/sysdata.rda, read from R serialization with no R
    // runtime.
    let t_data = std::time::Instant::now();
    if content_known {
        rds::scan_package_each(&root, &excluded, &mut |rec| println!("{rec}"));
    }
    stats.ms_data += ms_since(t_data);
    stats.files_data = num_data_files as u64;
    let _ = std::io::Write::flush(&mut std::io::stdout());
    write_run_stats(&stats, ms_since(t_run));
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peak_is_read_off_its_line_of_the_process_status() {
        let status = "Name:\trpkg-analyzer\nVmPeak:\t   71520 kB\nVmSize:\t   71456 kB\nVmHWM:\t    3984 kB\nVmRSS:\t    3984 kB\n";
        assert_eq!(status_kb(status, "VmHWM:"), Some(3984));
        assert_eq!(status_kb(status, "VmPeak:"), Some(71520));
        // A kernel thread has neither line, and a line may not hold a number.
        assert_eq!(status_kb("Name:\tkthreadd\nState:\tS (sleeping)\n", "VmHWM:"), None);
        assert_eq!(status_kb("VmHWM:\n", "VmHWM:"), None);
        assert_eq!(status_kb("VmHWM:\tmany kB\n", "VmHWM:"), None);
        assert_eq!(status_kb("", "VmPeak:"), None);
    }

    /// Both or neither, and what was resident was never more than the address
    /// space there was.
    #[test]
    fn the_peaks_are_the_systems_or_absent() {
        let (rss, vm) = peak_memory_kb();
        if cfg!(target_os = "linux") {
            let (rss, vm) = (rss.expect("VmHWM"), vm.expect("VmPeak"));
            assert!(rss > 0 && vm >= rss, "{rss} kB resident, {vm} kB of address space");
        } else if !Path::new("/proc/self/status").exists() {
            assert_eq!((rss, vm), (None, None));
        }
    }

    /// Rule J: a name the viewer treats as junk.
    fn is_junk(p: &Person) -> bool {
        let g = p.given.as_deref().unwrap_or("");
        let f = p.family.as_deref().unwrap_or("");
        let lead = g.trim().to_lowercase();
        lead.starts_with("and ") || lead.starts_with("& ") || [g, f].iter().any(|s| s.contains(['(', ')', '<', ']', '@']))
    }

    fn names(ps: &[Person]) -> Vec<(Option<&str>, Option<&str>)> {
        ps.iter().map(|p| (p.given.as_deref(), p.family.as_deref())).collect()
    }

    #[test]
    fn authors_r_keeps_orcid_ror_and_a_short_comment() {
        let p = meta_parse_person(
            r#""Ann", "Lee", role = c("aut", "cre"), email = "ann@x.org", comment = c(ORCID = "0000-0002-1825-0097", "Wrote the parser <ann@x.org>")"#,
        );
        assert_eq!((p.given.as_deref(), p.family.as_deref()), (Some("Ann"), Some("Lee")));
        assert_eq!(p.orcid.as_deref(), Some("0000-0002-1825-0097"));
        assert_eq!(p.comment.as_deref(), Some("Wrote the parser"));
        let org = meta_parse_person(r#""ACME Foundation", role = "fnd", comment = c(ROR = "https://ror.org/05dxps055")"#);
        assert_eq!(org.ror.as_deref(), Some("05dxps055"));
        assert_eq!(org.comment, None);
        let bad = meta_parse_person(r#""Bo", "Chen", comment = c(ORCID = "0000-0002-1825-0098")"#);
        assert_eq!(bad.orcid, None, "a failed check digit is dropped");
        let url = meta_parse_person(r#""Cy", "Dunn", comment = "https://orcid.org/0000-0001-5109-3700""#);
        assert_eq!(url.orcid.as_deref(), Some("0000-0001-5109-3700"));
        assert_eq!(url.comment, None, "an ORCID URL is the identifier, not a comment");
    }

    #[test]
    fn a_quoted_part_name_or_a_parenthesis_keeps_the_orcid() {
        let single = meta_parse_person(r#""Zuguang", "Gu", role = c("aut", "cre"), comment = c('ORCID'="0000-0002-7395-8709")"#);
        assert_eq!(single.orcid.as_deref(), Some("0000-0002-7395-8709"));
        assert_eq!(single.comment, None, "the name of a part is not a comment");
        let double = meta_parse_person(r#""Ann", "Lee", comment = c("ORCID" = "0000-0002-1825-0097", "Wrote the parser")"#);
        assert_eq!(double.orcid.as_deref(), Some("0000-0002-1825-0097"));
        assert_eq!(double.comment.as_deref(), Some("Wrote the parser"));
        let paren = meta_parse_person(r#""Ann", "Lee", comment = c("Wrote f() and g()", ORCID = "0000-0002-1825-0097")"#);
        assert_eq!(paren.orcid.as_deref(), Some("0000-0002-1825-0097"));
        assert_eq!(paren.comment.as_deref(), Some("Wrote f() and g()"));
        let ror = meta_parse_person(r#""ACME", role = "fnd", comment = c("ROR" = "05dxps055")"#);
        assert_eq!(ror.ror.as_deref(), Some("05dxps055"));
    }

    #[test]
    fn the_rest_of_the_comment_is_kept() {
        let turkish = meta_parse_person(r#""Ayse", "Kaya", comment = c("İTÜ, orcid.org/")"#);
        assert_eq!((turkish.given.as_deref(), turkish.family.as_deref()), (Some("Ayse"), Some("Kaya")));
        assert_eq!(turkish.orcid, None);
        assert_eq!(turkish.comment.as_deref(), Some("İTÜ, orcid.org/"));
        let mixed = meta_parse_person(r#""Ann", "Lee", comment = "Univ of X, https://orcid.org/0000-0002-1825-0097""#);
        assert_eq!(mixed.orcid.as_deref(), Some("0000-0002-1825-0097"));
        assert_eq!(mixed.comment.as_deref(), Some("Univ of X, https://orcid.org/0000-0002-1825-0097"));
        let named = meta_parse_person(
            r#""Ann", "Lee", comment = c(affiliation = "Univ (X)", 'github' = "annlee", ORCID = "0000-0002-1825-0097", "Wrote it")"#,
        );
        assert_eq!(named.orcid.as_deref(), Some("0000-0002-1825-0097"));
        assert_eq!(named.comment.as_deref(), Some("Univ (X); annlee; Wrote it"));
        let org = meta_parse_person(r#""ACME", role = "fnd", comment = c(ror = "05dxps055", project = "Edicitnet", grant_agreement = "776665")"#);
        assert_eq!(org.ror.as_deref(), Some("05dxps055"));
        assert_eq!(org.comment.as_deref(), Some("Edicitnet; 776665"));
    }

    #[test]
    fn an_id_part_is_dropped_only_when_it_is_the_kept_orcid() {
        assert!(authors::orcid_checked("0000-0001-8715-4771").is_some());
        let bare = meta_parse_person(r#""Ingo", "Rohlfing", comment = c("0000-0001-8715-4771")"#);
        assert_eq!(bare.orcid, None, "an unnamed iD is not declared as an ORCID");
        assert_eq!(bare.comment.as_deref(), Some("0000-0001-8715-4771"));
        let other = meta_parse_person(r#""Ann", "Lee", comment = c(ORCID = "0000-0002-1825-0097", "0000-0001-5109-3700")"#);
        assert_eq!(other.orcid.as_deref(), Some("0000-0002-1825-0097"));
        assert_eq!(other.comment.as_deref(), Some("0000-0001-5109-3700"));
        let same = meta_parse_person(
            r#""Ann", "Lee", comment = c(ORCID = "0000-0002-1825-0097", "https://orcid.org/0000-0002-1825-0097")"#,
        );
        assert_eq!(same.comment, None);
    }

    #[test]
    fn positional_slots_keep_their_place_and_never_hold_an_email() {
        let p = meta_parse_person(r#""", "Smith", role = "aut""#);
        assert_eq!((p.given.as_deref(), p.family.as_deref()), (None, Some("Smith")));
        let e = meta_parse_person(r#""ann@x.org", "Lee""#);
        assert_eq!((e.given.as_deref(), e.family.as_deref()), (None, Some("Lee")));
        let h = meta_parse_person(r#""Ann", "@annlee""#);
        assert_eq!((h.given.as_deref(), h.family.as_deref()), (Some("Ann"), None));
    }

    #[test]
    fn legacy_author_text_splits_into_clean_names() {
        let cases: Vec<(&str, Vec<(Option<&str>, Option<&str>)>)> = vec![
            ("Ann Lee <ann@x.org>, Bob Gray <bob@y.org>", vec![(Some("Ann"), Some("Lee")), (Some("Bob"), Some("Gray"))]),
            ("Ann Lee, and Bob Gray", vec![(Some("Ann"), Some("Lee")), (Some("Bob"), Some("Gray"))]),
            ("Tarn Duong & Matt Wand", vec![(Some("Tarn"), Some("Duong")), (Some("Matt"), Some("Wand"))]),
            ("Ann Lee; Bob Gray", vec![(Some("Ann"), Some("Lee")), (Some("Bob"), Some("Gray"))]),
            (
                "Stefan Wilhelm with contributions from Manjunath B G <bgmanjunath@gmail.com>",
                vec![(Some("Stefan"), Some("Wilhelm")), (Some("Manjunath B"), Some("G"))],
            ),
            ("Ann Lee (University of X, Y) and Bob Gray", vec![(Some("Ann"), Some("Lee")), (Some("Bob"), Some("Gray"))]),
            ("Francois Brun (ACTA) \\email{francois.brun@acta.asso.fr}", vec![(Some("Francois"), Some("Brun"))]),
            ("James Browne (jbrowne6@jhu.edu)", vec![(Some("James"), Some("Browne"))]),
            (
                "Original S code by Richard A. Becker and Allan R. Wilks; R port by Ray Brownrigg",
                vec![(Some("Richard A."), Some("Becker")), (Some("Allan R."), Some("Wilks")), (Some("Ray"), Some("Brownrigg"))],
            ),
        ];
        for (text, want) in cases {
            let ps = meta_parse_author_text(text);
            assert_eq!(names(&ps), want, "{text}");
            assert!(ps.iter().all(|p| !is_junk(p)), "no junk-looking name from {text}");
        }
    }

    #[test]
    fn legacy_notes_and_orcids_are_kept_beside_the_name() {
        let ps = meta_parse_author_text(
            "Ann Lee (University of X) <https://orcid.org/0000-0002-1825-0097>, Bob Gray (ORCID 0000-0001-5109-3700), R port by Cy Dunn",
        );
        assert_eq!(ps[0].comment.as_deref(), Some("University of X"));
        assert_eq!(ps[0].orcid.as_deref(), Some("0000-0002-1825-0097"));
        assert_eq!(ps[1].orcid.as_deref(), Some("0000-0001-5109-3700"));
        assert_eq!(ps[1].comment, None);
        assert_eq!(ps[2].comment.as_deref(), Some("R port by"));
        let as_r = meta_parse_author_text(r#"c(person("Ann", "Lee", role = "aut"), person("Bob", "Gray"))"#);
        assert_eq!(names(&as_r), vec![(Some("Ann"), Some("Lee")), (Some("Bob"), Some("Gray"))]);
    }

    #[test]
    fn person_s_in_prose_is_not_a_person_call() {
        let prose = meta_parse_author_text("Ann Lee and other person(s)");
        assert_eq!(names(&prose), vec![(Some("Ann"), Some("Lee")), (Some("other"), Some("person"))]);
        let noted = meta_parse_author_text("Ann Lee (a person (s) who wrote it), Bob Gray");
        assert_eq!(noted.len(), 2);
        assert_eq!(names(&noted)[1], (Some("Bob"), Some("Gray")));
    }

    #[test]
    fn optional_keys_follow_roles_in_a_fixed_order() {
        let p = Person {
            given: Some("Ann".into()),
            family: Some("Lee".into()),
            roles: vec!["aut".into()],
            comment: Some("x".into()),
            orcid: Some("0000-0002-1825-0097".into()),
            ror: Some("05dxps055".into()),
        };
        assert_eq!(
            persons_to_json(&[p]),
            r#"[{"given":"Ann","family":"Lee","roles":["aut"],"comment":"x","orcid":"0000-0002-1825-0097","ror":"05dxps055"}]"#
        );
        let bare = Person { given: None, family: Some("Lee".into()), roles: vec![], comment: None, orcid: None, ror: None };
        assert_eq!(persons_to_json(&[bare]), r#"[{"given":null,"family":"Lee","roles":[]}]"#);
    }

    #[test]
    fn rust_sources_count_and_vendored_crates_do_not() {
        assert!(is_src_file("src/rust/src/lib.rs"));
        assert!(is_src_file("src/init.c"));
        assert!(!is_src_file("src/rust/vendor/cfg-if/src/lib.rs"));
        assert!(!is_src_file("src/rust/vendor-patched/x/src/lib.rs"));
        assert!(!is_src_file("src/rust/target/release/build/x.rs"));
        assert!(!is_src_file("src/Makevars"));
    }

    #[test]
    fn a_filled_template_or_a_full_text_is_complete() {
        assert!(license_template_complete("YEAR: 2024\nCOPYRIGHT HOLDER: Ann Lee\n"));
        assert!(license_template_complete("YEAR: 2024\r\nCOPYRIGHT HOLDER: fixpkg authors\r\n"));
        assert!(license_template_complete("MIT License\n\nCopyright (c) 2024 Ann Lee\n\nPermission is hereby granted"));
        assert!(!license_template_complete("YEAR: <year>\nCOPYRIGHT HOLDER: Ann Lee\n"));
        assert!(!license_template_complete("YEAR: 2024\nCOPYRIGHT HOLDER: <Your Name>\n"));
        assert!(!license_template_complete("YEAR: 2024\nCOPYRIGHT HOLDER: your name\n"));
        assert!(!license_template_complete("YEAR: YEAR\nCOPYRIGHT HOLDER: COPYRIGHT HOLDER\n"));
        assert!(!license_template_complete("YEAR: 2024\nCOPYRIGHT HOLDER:\n"));
        assert!(!license_template_complete("YEAR: 2024\n"), "one line alone is not complete");
        assert!(!license_template_complete("  \n"));
    }

    // R 4.6.1's share/licenses/MIT byte for byte, as a package would copy it unfilled.
    const R_MIT_TEMPLATE: &str = concat!(
        "Based on <http://opensource.org/licenses/MIT>\n",
        "\n",
        "This is a template.  Provide completed entries of the form\n",
        "\n",
        "YEAR:\n",
        "COPYRIGHT HOLDER: \n",
        "\n",
        "(separated by empty lines) as file LICENSE, and specify\n",
        "\n",
        "License: MIT + file LICENSE\n",
        "\n",
        "in file DESCRIPTION.\n",
        "\n",
        "************************************************************************\n",
        "\n",
        "Copyright (c) <YEAR>, <COPYRIGHT HOLDER>\n",
        "\n",
        "Permission is hereby granted, free of charge, to any person obtaining\n",
        "a copy of this software and associated documentation files (the\n",
        "\"Software\"), to deal in the Software without restriction, including\n",
        "without limitation the rights to use, copy, modify, merge, publish,\n",
        "distribute, sublicense, and/or sell copies of the Software, and to\n",
        "permit persons to whom the Software is furnished to do so, subject to\n",
        "the following conditions:\n",
        "\n",
        "The above copyright notice and this permission notice shall be\n",
        "included in all copies or substantial portions of the Software.\n",
        "\n",
        "THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND,\n",
        "EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF\n",
        "MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND\n",
        "NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE\n",
        "LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION\n",
        "OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION\n",
        "WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.\n",
    );

    #[test]
    fn an_empty_value_does_not_take_the_next_line() {
        assert!(!license_template_complete("YEAR:\nCOPYRIGHT HOLDER: Ann Lee\n"));
        assert!(!license_template_complete("YEAR:\r\nCOPYRIGHT HOLDER: Ann Lee\r\n"));
        assert!(!license_template_complete("YEAR: 2024\nCOPYRIGHT HOLDER: \n\n(separated by empty lines) as file LICENSE\n"));
        assert!(!license_template_complete("YEAR:\nCOPYRIGHT HOLDER: \nORGANIZATION:\n\n(separated by empty lines)\n"));
        assert!(!license_template_complete(R_MIT_TEMPLATE), "an unfilled copy of R's template");
        assert!(license_template_complete("YEAR:\t2024\nCOPYRIGHT HOLDER:\tAnn Lee\n"));
        assert!(license_template_complete("  YEAR: 2024\n  COPYRIGHT HOLDER: Ann Lee\n"));
    }

    #[test]
    fn only_a_whole_bracketed_value_is_a_placeholder() {
        assert!(!is_license_placeholder("<Ann Lee> <ann@x.org>"));
        assert!(license_template_complete("YEAR: 2024\nCOPYRIGHT HOLDER: <Ann Lee> <ann@x.org>\n"));
        assert!(is_license_placeholder("<Your Name>"));
        assert!(is_license_placeholder("<>"));
        assert!(is_license_placeholder("<<name>"));
        assert!(!is_license_placeholder("<"));
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_year_line() {
        assert!(license_template_complete("\u{feff}YEAR: 2024\nCOPYRIGHT HOLDER: Ann Lee\n"));
        assert!(license_template_complete("\u{feff}YEAR: 2024\r\nCOPYRIGHT HOLDER: Ann Lee\r\n"));
        assert!(!license_template_complete("\u{feff}YEAR: <year>\nCOPYRIGHT HOLDER: Ann Lee\n"));
        assert!(!license_template_complete("\u{feff}\n"), "a mark alone is an empty file");
    }

    fn version_parts(v: &str) -> Vec<u64> {
        v.split('.')
            .map(|p| p.parse::<u64>().expect("version component is a number"))
            .collect()
    }

    /// 0.4.0 was the last release under the old column contract, so this build must
    /// report 0.5.0 or later or a pipeline cannot tell the two kinds of row apart.
    #[test]
    fn analyzer_version_is_past_the_last_release_under_the_old_column_contract() {
        let parts = version_parts(ANALYZER_VERSION);
        assert!(
            parts.len() >= 3,
            "ANALYZER_VERSION is {ANALYZER_VERSION}, which a consumer cannot compare \
             component-wise against a released tag"
        );
        assert!(
            version_ge(&parts, &[0, 5, 0]),
            "ANALYZER_VERSION is {ANALYZER_VERSION}, at or below 0.4.0, the last release \
             under the old column contract"
        );
    }

    /// A hash set hands back its items in a new order on every instance, so the
    /// edges are sorted before anything reads them.
    #[test]
    fn a_graph_comes_out_the_same_every_time() {
        let names: Vec<String> = (0..60).map(|i| format!("f{i:02}")).collect();
        let calls: Vec<Vec<String>> = (0..60)
            .map(|i| (0..60).filter(|j| (i * 7 + j * 3) % 4 == 0).map(|j| format!("f{j:02}")).collect())
            .collect();
        let (net, edges) = graph_from_calls(&names, &calls);
        assert!(edges.len() > 500, "enough edges for a hash order to show: {}", edges.len());
        let mut sorted = edges.clone();
        sorted.sort();
        assert_eq!(edges, sorted, "edges come out in node order");
        let bits = |n: &Network| {
            (
                n.betweenness_mean.map(f64::to_bits),
                n.betweenness_median.map(f64::to_bits),
                n.betweenness_max.map(f64::to_bits),
            )
        };
        for _ in 0..50 {
            let (again, again_edges) = graph_from_calls(&names, &calls);
            assert_eq!(again_edges, edges);
            assert_eq!(bits(&again), bits(&net));
        }
    }

    /// Repeated function names (R across two files, a C static helper) keep the
    /// later definition, and the order still never moves.
    #[test]
    fn a_graph_with_repeated_names_comes_out_the_same_every_time() {
        let names: Vec<String> = (0..40).map(|i| format!("g{:02}", i % 25)).collect();
        let calls: Vec<Vec<String>> = (0..40)
            .map(|i| (0..25).filter(|j| (i + j) % 3 == 0).map(|j| format!("g{j:02}")).collect())
            .collect();
        let (net, edges) = graph_from_calls(&names, &calls);
        assert!(edges.len() > 200, "{} edges", edges.len());
        for _ in 0..50 {
            let (again, again_edges) = graph_from_calls(&names, &calls);
            assert_eq!(again_edges, edges);
            assert_eq!(again.betweenness_max.map(f64::to_bits), net.betweenness_max.map(f64::to_bits));
            assert_eq!(again.betweenness_mean.map(f64::to_bits), net.betweenness_mean.map(f64::to_bits));
        }
    }

    /// Compiled files covering every rule the three old passes apply by path or
    /// by extension, and the parse quirks the one pass must keep.
    fn src_pool() -> Vec<(&'static str, Vec<u8>)> {
        let t = |s: &str| s.as_bytes().to_vec();
        vec![
            ("src/a.c", t("static int helper(int x) { return x + 1; }\nint twice(int x) { return helper(helper(x)); }\nvoid takes_cb(int n) { int (*cb)(int) = helper; cb(n); twice(n); }\n")),
            ("src/b.h", t("int shared(int x);\nstatic inline int in_header(int x) { return shared(x); }\n")),
            ("src/c.cpp", t("int shared(int x) { return x; }\nnamespace n { int cpp_fn(int y) { return shared(y) + in_header(y); } }\n")),
            ("src/nested/deep.cc", t("struct S { int m(int v) { return v; } };\nint deep(int x) { S s; return s.m(x); }\n")),
            ("src/upper.C", t("int upper_c(void) { return 0; }\n")),
            ("src/rust/vendor/dep/v.c", t("int vendored_c(void) { return twice(1); }\n")),
            ("src/rust/vendor/dep/src/lib.rs", t("fn vendored_rs() -> i32 { 1 }\nfn more() -> i32 { vendored_rs() }\n")),
            ("src/rust/vendor-patched/p/src/lib.rs", t("fn patched() {}\n")),
            ("src/sub/target/debug/build/gen.c", t("int generated(void) { return 0; }\n")),
            ("src/rust/src/lib.rs", t("fn one() -> i32 { two() }\nfn two() -> i32 { fn nested() -> i32 { 2 } nested() }\n")),
            ("src/f77.f", t("      SUBROUTINE SUB1(X)\n      DOUBLE PRECISION X\n      CALL SUB2(X)\n      RETURN\n      END\n      SUBROUTINE SUB2(X)\n      DOUBLE PRECISION X\n      X = X + 1\n      RETURN\n      END\n")),
            ("src/mod.f90", t("module m\ncontains\n  function sq(x) result(y)\n    real :: x, y\n    y = x * x\n  end function sq\n  subroutine use_sq(a)\n    real :: a\n    a = sq(a)\n    call sub1(a)\n  end subroutine use_sq\nend module m\n")),
            ("src/m95.f95", t("subroutine s95()\n  call s03()\nend subroutine s95\n")),
            ("src/m03.f03", t("subroutine s03()\n  call s08()\nend subroutine s03\n")),
            ("src/m08.f08", t("subroutine s08()\nend subroutine s08\n")),
            ("src/latin1.c", b"/* caf\xe9 */ int latin(void) { return 0; }\n".to_vec()),
            ("src/dup1.c", t("int same(void) { return 1; }\nint user1(void) { return same(); }\n")),
            ("src/dup2.c", t("int same(void) { return 2; }\nint user2(void) { return same(); }\n")),
            ("src/crlf.c", t("int crlf_a(void) {\r\n  return 0;\r\n}\r\nint crlf_b(void) { return crlf_a(); }\r\n")),
            ("src/empty.c", Vec::new()),
            ("src/Makevars", t("PKG_LIBS = -lm\n")),
            ("src/init.hpp", t("inline int hpp_fn(int x) { return x; }\n")),
            ("R/a.R", t("f <- function(x) .Call(C_twice, x)\n")),
        ]
    }

    fn write_src_tree(entries: &[&(&'static str, Vec<u8>)]) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("rpa-scan-src-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, bytes) in entries {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// scan_src against the three passes it replaced, field for field.
    fn assert_scan_matches_the_old_passes(root: &Path) {
        let files = list_files(root);
        let scan = scan_src(root, &files, &mut cache::Cache::new(None, false));
        assert_eq!(scan.fns, count_src_functions(root, &files), "counts");
        assert_eq!(scan.names, collect_src_function_names(root, &files), "names");
        for (got, lang) in [
            (scan.c_family, SrcLang::CFamily),
            (scan.rust, SrcLang::Rust),
            (scan.fortran, SrcLang::Fortran),
        ] {
            let (net, edges, nodes) = got.graph();
            let (old_net, old_edges, old_nodes) = build_src_graph(root, &files, lang);
            assert_eq!(nodes, old_nodes, "{lang:?} nodes");
            assert_eq!(edges, old_edges, "{lang:?} edges");
            assert_eq!(net, old_net, "{lang:?} network");
        }
    }

    #[test]
    fn one_parse_per_file_gives_what_three_passes_gave() {
        let pool = src_pool();
        let all: Vec<&(&'static str, Vec<u8>)> = pool.iter().collect();
        let root = write_src_tree(&all);
        let files = list_files(&root);
        let scan = scan_src(&root, &files, &mut cache::Cache::new(None, false));
        assert!(scan.names.contains("vendored_c") && scan.names.contains("vendored_rs"), "names take vendored files");
        assert!(scan.fns.n_files >= 10, "the pool parses: {:?}", scan.fns);
        assert!(!scan.names.contains("latin"), "a non-UTF-8 file is skipped");
        assert_scan_matches_the_old_passes(&root);
        let _ = std::fs::remove_dir_all(&root);

        // Random subsets, so no pass gets to lean on a file another one brought.
        let mut seed: u64 = 0x5eed_0510;
        for _ in 0..150 {
            let subset: Vec<&(&'static str, Vec<u8>)> = pool
                .iter()
                .filter(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (seed >> 33) & 1 == 1
                })
                .collect();
            let root = write_src_tree(&subset);
            assert_scan_matches_the_old_passes(&root);
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// A pattern gets the regex compiled for it the first time, a pattern that
    /// does not compile is kept out, and the list stops at its limit.
    #[test]
    fn a_kept_regex_is_compiled_once_and_the_list_stops_at_its_limit() {
        use std::sync::Arc;
        let mut kept = KeptRegexes(BTreeMap::new());
        let dirs = kept.get(r"^R/").expect("a pattern");
        assert!(Arc::ptr_eq(&dirs, &kept.get(r"^R/").expect("the same pattern")), "one regex a pattern");
        assert!(dirs.is_match("R/a.R") && !dirs.is_match("man/R/a.R"));
        // Another pattern is another regex, however alike the two are.
        let tests = kept.get(r"^tests/").expect("a pattern");
        assert!(tests.is_match("tests/a.R") && !tests.is_match("R/a.R"));
        assert_eq!(kept.0.len(), 2);
        assert!(kept.get("(").is_err() && kept.get("(").is_err(), "an error each time it is asked for");
        assert_eq!(kept.0.len(), 2);
        for i in 2..KEPT_REGEXES {
            let re = kept.get(&format!("^made{i}$")).expect("a pattern");
            assert!(re.is_match(&format!("made{i}")) && !re.is_match("made"), "pattern {i}");
        }
        assert_eq!(kept.0.len(), KEPT_REGEXES);
        // Past the limit a pattern still gets its regex, compiled for each call.
        let late = kept.get("^late$").expect("a pattern");
        assert!(late.is_match("late") && !late.is_match("later"));
        assert!(!Arc::ptr_eq(&late, &kept.get("^late$").expect("the same pattern")), "not kept");
        assert_eq!(kept.0.len(), KEPT_REGEXES);
        assert!(kept.0.keys().all(|p| p != "^late$" && p != "("));
        assert!(Arc::ptr_eq(&dirs, &kept.get(r"^R/").expect("the first pattern")), "the first is still kept");
    }

    /// The program's own patterns, about thirty, are well inside the limit, so
    /// each is compiled once in a run: the help-page helpers, which put the
    /// same pattern together on every call, are handed one regex.
    #[test]
    fn a_pattern_put_together_twice_is_one_regex() {
        let text = "\\name{a}\n\\alias{a}\n\\alias{b}\n\\usage{a(x)}\n";
        assert_eq!(rd_all_blocks(text, "alias"), ["a", "b"]);
        assert_eq!(rd_first_block(text, "usage").map(|(body, _)| body).as_deref(), Some("a(x)"));
        assert!(rd_has_block(text, "name") && !rd_has_block(text, "value"));
        let pattern = |cmd: &str| format!(r"\\{}\s*\{{", regex::escape(cmd));
        for cmd in ["alias", "usage", "name", "value"] {
            let first = kept_regex(&pattern(cmd)).expect("a pattern");
            assert!(std::sync::Arc::ptr_eq(&first, &kept_regex(&pattern(cmd)).expect("the same pattern")), "{cmd}");
        }
        assert_eq!(find_files(&["R/a.R".to_string(), "man/a.Rd".to_string()], r"^R/"), ["R/a.R"]);
        assert!(std::sync::Arc::ptr_eq(&kept_regex(r"^R/").unwrap(), &kept_regex(r"^R/").unwrap()));
    }
}
