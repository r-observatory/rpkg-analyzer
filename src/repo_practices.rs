// Facts about the repository rather than the release: CI, the pkgdown config and
// the community files. A release can show one of these only when it carries it.

use crate::cli::InputKind;
use std::path::Path;

/// Code of conduct spellings, checked at the root and in .github/ (same list as vcs-signals).
pub const COC_FILES: [&str; 8] = [
    "CODE_OF_CONDUCT.md",
    "CODE_OF_CONDUCT",
    "CODE_OF_CONDUCT.Rmd",
    "CODE_OF_CONDUCT.rst",
    "code_of_conduct.md",
    "Code_of_conduct.md",
    "CODE-OF-CONDUCT.md",
    "CONDUCT.md",
];

/// Contributing guide spellings, checked at the root and in .github/ (same list as vcs-signals).
pub const CONTRIBUTING_FILES: [&str; 7] = [
    "CONTRIBUTING.md",
    "CONTRIBUTING",
    "CONTRIBUTING.Rmd",
    "CONTRIBUTING.rst",
    "contributing.md",
    "Contributing.md",
    "CONTRIBUTING.MD",
];

/// The config paths pkgdown 2.2.0 reads (pkgdown:::pkgdown_config_path).
pub const PKGDOWN_CONFIG_PATHS: [&str; 6] = [
    "_pkgdown.yml",
    "_pkgdown.yaml",
    "pkgdown/_pkgdown.yml",
    "pkgdown/_pkgdown.yaml",
    "inst/_pkgdown.yml",
    "inst/_pkgdown.yaml",
];

pub struct RepoPractices {
    pub ci_present: Option<bool>,
    pub ci_type: Option<Vec<String>>,
    pub ci_matrix_breadth: Option<i64>,
    pub ci_pr_gated: Option<bool>,
    pub has_pkgdown: Option<bool>,
    pub has_code_of_conduct: Option<bool>,
    pub has_contributing_guide: Option<bool>,
}

/// Whether one of `names` sits at the package root or directly in .github/.
pub fn at_root_or_github(files: &[String], names: &[&str]) -> bool {
    names.iter().any(|n| {
        let gh = format!(".github/{n}");
        files.iter().any(|f| f == n || *f == gh)
    })
}

/// Computed on the unfiltered tree. Release input reports a file it carries and
/// leaves the rest NULL, because a release cannot show what the repository keeps.
pub fn metrics_repo_practices(root: &Path, tree_files: &[String], kind: InputKind) -> RepoPractices {
    let ci_yml = super::find_files(tree_files, r"^\.github/workflows/.*\.ya?ml$");
    let has_travis = super::exists(tree_files, ".travis.yml");
    let has_appveyor = super::exists(tree_files, "appveyor.yml");
    let has_circleci = !super::find_files(tree_files, r"^\.circleci/").is_empty();
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
    let ci_matrix_breadth = super::gha_matrix_breadth(root, &ci_yml);
    let ci_pr_gated = ci_yml
        .iter()
        .any(|f| super::read(root, f).map(|c| c.contains("pull_request")).unwrap_or(false));

    let has_pkgdown = PKGDOWN_CONFIG_PATHS.iter().any(|p| super::exists(tree_files, p));
    let has_coc = at_root_or_github(tree_files, &COC_FILES);
    let has_contributing = at_root_or_github(tree_files, &CONTRIBUTING_FILES);

    let presence = |found: bool| match kind {
        InputKind::Git => Some(found),
        InputKind::Release => found.then_some(true),
    };
    let ci_known = kind == InputKind::Git || ci_present;
    RepoPractices {
        ci_present: presence(ci_present),
        ci_type: ci_known.then_some(ci_type),
        ci_matrix_breadth: ci_known.then_some(ci_matrix_breadth),
        ci_pr_gated: ci_known.then_some(ci_pr_gated),
        has_pkgdown: presence(has_pkgdown),
        has_code_of_conduct: presence(has_coc),
        has_contributing_guide: presence(has_contributing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These lists are shared with vcs-signals (config.R COC_TREE_PATHS and
    // CONTRIBUTING_TREE_PATHS); a change here needs the same change there.
    #[test]
    fn the_file_lists_match_the_ones_vcs_signals_reads() {
        assert_eq!(
            COC_FILES,
            [
                "CODE_OF_CONDUCT.md", "CODE_OF_CONDUCT", "CODE_OF_CONDUCT.Rmd", "CODE_OF_CONDUCT.rst",
                "code_of_conduct.md", "Code_of_conduct.md", "CODE-OF-CONDUCT.md", "CONDUCT.md",
            ]
        );
        assert_eq!(
            CONTRIBUTING_FILES,
            [
                "CONTRIBUTING.md", "CONTRIBUTING", "CONTRIBUTING.Rmd", "CONTRIBUTING.rst",
                "contributing.md", "Contributing.md", "CONTRIBUTING.MD",
            ]
        );
        assert_eq!(
            PKGDOWN_CONFIG_PATHS,
            [
                "_pkgdown.yml", "_pkgdown.yaml", "pkgdown/_pkgdown.yml", "pkgdown/_pkgdown.yaml",
                "inst/_pkgdown.yml", "inst/_pkgdown.yaml",
            ]
        );
    }

    #[test]
    fn a_community_file_counts_at_the_root_or_in_github_only() {
        let files = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(at_root_or_github(&files(&["CONDUCT.md"]), &COC_FILES));
        assert!(at_root_or_github(&files(&[".github/CODE_OF_CONDUCT.md"]), &COC_FILES));
        assert!(!at_root_or_github(&files(&["inst/CODE_OF_CONDUCT.md"]), &COC_FILES));
        assert!(!at_root_or_github(&files(&["conduct.md"]), &COC_FILES));
        assert!(at_root_or_github(&files(&["CONTRIBUTING.MD"]), &CONTRIBUTING_FILES));
    }
}
