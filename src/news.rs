// The NEWS file R's readers would use, and the section of it for one version.

/// The order R's NEWS readers look in.
pub const NEWS_FILES: [&str; 5] = ["inst/NEWS.Rd", "NEWS.md", "inst/NEWS.md", "NEWS", "inst/NEWS"];
pub const CHANGELOG_FILES: [&str; 3] = ["ChangeLog", "CHANGELOG", "CHANGES"];

pub fn news_file(files: &[String]) -> Option<&'static str> {
    NEWS_FILES.iter().copied().find(|p| files.iter().any(|f| f == p))
}

pub fn changelog_file(files: &[String]) -> Option<&'static str> {
    CHANGELOG_FILES.iter().copied().find(|p| files.iter().any(|f| f == p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_news_file_is_the_first_one_r_reads() {
        assert_eq!(news_file(&files(&["NEWS", "NEWS.md", "inst/NEWS.Rd"])), Some("inst/NEWS.Rd"));
        assert_eq!(news_file(&files(&["inst/NEWS", "NEWS.md"])), Some("NEWS.md"));
        assert_eq!(news_file(&files(&["inst/NEWS"])), Some("inst/NEWS"));
        assert_eq!(news_file(&files(&["news.md", "docs/NEWS.md"])), None);
        assert_eq!(changelog_file(&files(&["CHANGES", "ChangeLog"])), Some("ChangeLog"));
    }
}
