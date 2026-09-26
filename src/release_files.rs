// The files R CMD build keeps from a git branch, reproducing R 4.6.1's
// tools:::.build_packages and tools:::inRbuildignore.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

/// tools:::get_exclude_patterns() in R 4.6.1.
pub const DEFAULT_EXCLUDE_PATTERNS: [&str; 18] = [
    r"^\.Rbuildignore$",
    r"(^|/)\.DS_Store$",
    r"^\.(RData|Rhistory)$",
    r"~$",
    r"\.bak$",
    r"\.sw.$",
    r"(^|/)\.#[^/]*$",
    r"(^|/)#[^/]*#$",
    r"^TITLE$",
    r"^data/00Index$",
    r"^inst/doc/00Index\.dcf$",
    r"^config\.(cache|log|status)$",
    r"(^|/)autom4te\.cache$",
    r"^src/.*\.d$",
    r"^src/Makedeps$",
    r"^src/so_locations$",
    r"^inst/doc/Rplots\.(ps|pdf)$",
    r"^(GPATH|GRTAGS|GTAGS)$",
];

/// tools:::.vc_dir_names in R 4.6.1.
pub const VC_DIR_NAMES: [&str; 8] = ["CVS", ".svn", ".arch-ids", ".bzr", ".git", ".hg", "_darcs", ".metadata"];

/// tools:::.hidden_file_exclusions in R 4.6.1.
pub const HIDDEN_FILE_EXCLUSIONS: [&str; 28] = [
    ".Renviron", ".Rprofile", ".Rproj.user", ".Rhistory", ".Rapp.history", ".tex", ".log", ".aux",
    ".pdf", ".png", ".backups", ".cvsignore", ".cproject", ".directory", ".dropbox", ".exrc",
    ".gdb.history", ".gitattributes", ".gitignore", ".gitmodules", ".hgignore", ".hgtags",
    ".htaccess", ".latex2html-init", ".project", ".seed", ".settings", ".tm_properties",
];

pub enum RbuildignoreText {
    Absent,
    Unreadable,
    Text(String),
}

pub fn read_rbuildignore(root: &Path) -> RbuildignoreText {
    let p = root.join(".Rbuildignore");
    if !p.exists() {
        return RbuildignoreText::Absent;
    }
    match std::fs::read(&p) {
        Ok(bytes) => RbuildignoreText::Text(String::from_utf8_lossy(&bytes).into_owned()),
        Err(_) => RbuildignoreText::Unreadable,
    }
}

/// Lines as readLines returns them: split on \r\n, \n or \r, untrimmed, and no
/// empty line after a final terminator.
pub fn readlines(text: &str) -> Vec<String> {
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = norm.split('\n').map(String::from).collect();
    if norm.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Every ancestor directory of every file, relative to the package root.
pub fn ancestor_dirs(files: &[String]) -> BTreeSet<String> {
    let mut dirs = BTreeSet::new();
    for f in files {
        let mut p = f.as_str();
        while let Some((parent, _)) = p.rsplit_once('/') {
            if !dirs.insert(parent.to_string()) {
                break;
            }
            p = parent;
        }
    }
    dirs
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

static OLD_DIR: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"([Oo]ld|\.Rcheck)$").unwrap());
static DEPS_DIR: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^src.*/[.]deps$").unwrap());
static BUILD_INDEX: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^.Rbuildindex[.]").unwrap());

/// The exclusions .build_packages applies beside the patterns.
pub struct Structural {
    res_rc: String,
    tarball: Option<regex::Regex>,
}

impl Structural {
    pub fn new(package: &str) -> Structural {
        // R pastes the package name into the pattern unescaped, and so does this.
        let tarball = regex::Regex::new(&format!(
            r"^{package}_[0-9.-]+\.(tar\.gz|tar|tar\.bz2|tar\.xz|tgz|zip)$"
        ))
        .ok();
        Structural { res_rc: format!("src/{package}_res.rc"), tarball }
    }

    pub fn excludes(&self, path: &str, is_dir: bool) -> bool {
        let base = basename(path);
        (is_dir && (base == "check" || base == "chm" || VC_DIR_NAMES.contains(&base)))
            || (is_dir && OLD_DIR.is_match(base))
            || base == "Read-and-delete-me"
            || base == "GNUMakefile"
            || base.starts_with("._")
            || (is_dir && DEPS_DIR.is_match(path))
            || path == self.res_rc
            || path.ends_with("inst/doc/.Rinstignore")
            || path.ends_with("inst/doc/.build.timestamp")
            || path.ends_with("vignettes/.Rinstignore")
            || BUILD_INDEX.is_match(path)
            || HIDDEN_FILE_EXCLUSIONS.contains(&base)
            || self.tarball.as_ref().is_some_and(|re| re.is_match(path))
    }
}

pub struct BuildFilter {
    /// Files R CMD build keeps.
    pub kept: Vec<String>,
    pub excluded_files: BTreeSet<String>,
    pub excluded_dirs: BTreeSet<String>,
    /// .Rbuildignore lines that did not compile; None when there is no .Rbuildignore.
    pub bad_lines: Option<i64>,
}

pub enum ReleaseList {
    /// Release input: the tree is the release.
    AsGiven,
    /// Git input: what R CMD build would keep.
    Filtered(BuildFilter),
    /// Git input whose .Rbuildignore exists but could not be read.
    Unknown,
}

impl ReleaseList {
    pub fn for_input(root: &Path, tree_files: &[String], package: &str, kind: crate::cli::InputKind) -> ReleaseList {
        match kind {
            crate::cli::InputKind::Release => ReleaseList::AsGiven,
            crate::cli::InputKind::Git => filter_release(tree_files, &read_rbuildignore(root), package),
        }
    }

    /// The files release-content columns read. With an unknown list the tree stands in,
    /// and the caller nulls every release-content value before printing.
    pub fn files(&self, tree_files: &[String]) -> Vec<String> {
        match self {
            ReleaseList::Filtered(f) => f.kept.clone(),
            _ => tree_files.to_vec(),
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, ReleaseList::Unknown)
    }

    pub fn excluded_files(&self) -> BTreeSet<String> {
        match self {
            ReleaseList::Filtered(f) => f.excluded_files.clone(),
            _ => BTreeSet::new(),
        }
    }

    pub fn bad_lines(&self) -> Option<i64> {
        match self {
            ReleaseList::Filtered(f) => f.bad_lines,
            _ => None,
        }
    }
}

pub fn filter_release(tree_files: &[String], rbi: &RbuildignoreText, package: &str) -> ReleaseList {
    let lines = match rbi {
        RbuildignoreText::Unreadable => return ReleaseList::Unknown,
        RbuildignoreText::Absent => None,
        RbuildignoreText::Text(t) => Some(readlines(t)),
    };
    // perl = TRUE, ignore.case = TRUE, as inRbuildignore calls grepl.
    let compile = |p: &str| fancy_regex::Regex::new(&format!("(?i){p}"));
    let mut patterns: Vec<fancy_regex::Regex> = DEFAULT_EXCLUDE_PATTERNS
        .iter()
        .map(|p| compile(p).expect("R's default exclude patterns compile"))
        .collect();
    let mut bad = 0i64;
    for line in lines.iter().flatten().filter(|l| !l.is_empty()) {
        match compile(line) {
            Ok(re) => patterns.push(re),
            Err(_) => bad += 1,
        }
    }
    let hit = |p: &str| patterns.iter().any(|re| re.is_match(p).unwrap_or(false));
    let structural = Structural::new(package);

    let excluded_dirs: BTreeSet<String> = ancestor_dirs(tree_files)
        .into_iter()
        .filter(|d| hit(d) || structural.excludes(d, true))
        .collect();
    let mut kept = Vec::new();
    let mut excluded_files = BTreeSet::new();
    for f in tree_files {
        // An excluded directory takes everything under it.
        let mut under_excluded = false;
        let mut p = f.as_str();
        while let Some((parent, _)) = p.rsplit_once('/') {
            if excluded_dirs.contains(parent) {
                under_excluded = true;
                break;
            }
            p = parent;
        }
        if under_excluded || hit(f) || structural.excludes(f, false) {
            excluded_files.insert(f.clone());
        } else {
            kept.push(f.clone());
        }
    }
    ReleaseList::Filtered(BuildFilter { kept, excluded_files, excluded_dirs, bad_lines: lines.map(|_| bad) })
}

/// Summary keys that keep their value when the release file list is unknown.
pub const KEPT_WHEN_UNKNOWN: [&str; 12] = [
    "rec",
    "analyzer_version",
    "input_kind",
    "package",
    "version",
    "ci_present",
    "ci_type",
    "ci_matrix_breadth",
    "ci_pr_gated",
    "has_pkgdown",
    "has_code_of_conduct",
    "has_contributing_guide",
];

/// With no way to know what the release holds, every release-content key is NULL.
pub fn null_release_content(summary: &mut serde_json::Value) {
    if let Some(map) = summary.as_object_mut() {
        for (k, v) in map.iter_mut() {
            if !KEPT_WHEN_UNKNOWN.contains(&k.as_str()) {
                *v = serde_json::Value::Null;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn filtered(tree: &[&str], rbi: &str) -> BuildFilter {
        match filter_release(&files(tree), &RbuildignoreText::Text(rbi.to_string()), "fixpkg") {
            ReleaseList::Filtered(f) => f,
            _ => panic!("expected a filtered list"),
        }
    }

    #[test]
    fn lines_split_the_way_readlines_splits_them() {
        assert_eq!(readlines("a\r\nb\rc\nd"), vec!["a", "b", "c", "d"]);
        assert_eq!(readlines("a \n\n"), vec!["a ", ""]);
        assert_eq!(readlines(""), vec![""]);
    }

    #[test]
    fn a_comment_line_is_a_pattern_and_trailing_space_is_kept() {
        let f = filtered(&["#notes#", "README.md", "cran-comments.md"], "#notes#\n^README\\.md \n^cran-comments\\.md$\n");
        assert!(f.excluded_files.contains("#notes#"), "a # line is used as a pattern");
        assert!(f.kept.contains(&"README.md".to_string()), "the trailing space stays in the pattern");
        assert!(f.excluded_files.contains("cran-comments.md"));
    }

    #[test]
    fn patterns_ignore_case_and_directories_take_their_contents() {
        let f = filtered(&["Vignettes/a.Rmd", "vignettes/articles/b.Rmd", "vignettes/c.Rmd"], "^vignettes/articles$\n");
        assert!(f.excluded_files.contains("vignettes/articles/b.Rmd"));
        assert!(f.kept.contains(&"vignettes/c.Rmd".to_string()));
        let g = filtered(&["Vignettes/a.Rmd"], "^vignettes$\n");
        assert!(g.excluded_files.contains("Vignettes/a.Rmd"), "matching ignores case");
    }

    #[test]
    fn a_pattern_that_does_not_compile_is_counted_and_skipped() {
        let f = filtered(&["a.txt", "b.txt"], "^a\\.txt$\n(unclosed\n");
        assert_eq!(f.bad_lines, Some(1));
        assert!(f.excluded_files.contains("a.txt"));
        assert!(f.kept.contains(&"b.txt".to_string()));
    }

    #[test]
    fn no_rbuildignore_still_applies_the_default_patterns() {
        let f = match filter_release(&files(&["R/a.R", "src/a.d", ".DS_Store", "notes~"]), &RbuildignoreText::Absent, "fixpkg") {
            ReleaseList::Filtered(f) => f,
            _ => panic!("expected a filtered list"),
        };
        assert_eq!(f.bad_lines, None);
        assert_eq!(f.kept, vec!["R/a.R".to_string()]);
    }

    #[test]
    fn an_unreadable_rbuildignore_makes_the_release_unknown() {
        assert!(matches!(
            filter_release(&files(&["R/a.R"]), &RbuildignoreText::Unreadable, "fixpkg"),
            ReleaseList::Unknown
        ));
    }

    #[test]
    fn each_structural_exclusion_applies() {
        let tree = [
            "check/x.R", "chm/x.html", ".svn/entries", "CVS/Root", "R/old/x.R", "R/Old/y.R", "pkg.Rcheck/00check.log",
            "Read-and-delete-me", "src/GNUMakefile", "._DESCRIPTION", "src/sub/.deps/x.Po", "src/fixpkg_res.rc",
            "inst/doc/.Rinstignore", "inst/doc/.build.timestamp", "vignettes/.Rinstignore", ".Rbuildindex.x",
            "R/.Rhistory", ".gitignore", "fixpkg_1.0.tar.gz", "R/bold/keep.R", "R/a.R", "otherpkg_1.0.tar.gz",
        ];
        let f = filtered(&tree, "");
        assert_eq!(
            f.kept,
            vec!["R/a.R".to_string(), "otherpkg_1.0.tar.gz".to_string()],
            "only R/a.R and another package's tarball survive (R/bold ends in old)"
        );
    }
}
