// Help pages: which files are pages, how their examples run, and what they document.

use std::sync::LazyLock;

static RD_PAGE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)^man/(?:(?:unix|windows)/)?[^/]+\.rd$").unwrap());

/// Help pages as R installs them: man/*.Rd in any case, plus the unix/ and windows/
/// subdirectories. man/macros holds Rd macro definitions, which are not pages.
pub fn rd_page_files(files: &[String]) -> Vec<&str> {
    files.iter().filter(|f| RD_PAGE.is_match(f)).map(String::as_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_pages_count_and_macros_do_not() {
        let files: Vec<String> = [
            "man/a.Rd", "man/b.rd", "man/C.RD", "man/unix/u.Rd", "man/windows/w.rd",
            "man/macros/m.Rd", "man/figures/x.png", "inst/man/y.Rd",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            rd_page_files(&files),
            vec!["man/a.Rd", "man/b.rd", "man/C.RD", "man/unix/u.Rd", "man/windows/w.rd"]
        );
    }
}
