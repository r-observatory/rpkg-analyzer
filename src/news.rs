// The NEWS file R's readers would use, and the section of it for one version.

use std::sync::LazyLock;

/// The order R's NEWS readers look in.
pub const NEWS_FILES: [&str; 5] = ["inst/NEWS.Rd", "NEWS.md", "inst/NEWS.md", "NEWS", "inst/NEWS"];
pub const CHANGELOG_FILES: [&str; 3] = ["ChangeLog", "CHANGELOG", "CHANGES"];

pub fn news_file(files: &[String]) -> Option<&'static str> {
    NEWS_FILES.iter().copied().find(|p| files.iter().any(|f| f == p))
}

pub fn changelog_file(files: &[String]) -> Option<&'static str> {
    CHANGELOG_FILES.iter().copied().find(|p| files.iter().any(|f| f == p))
}

/// Release notes are cut here, at a character boundary.
pub const RELEASE_NOTES_CAP: usize = 16_384;

pub struct ReleaseNotes {
    pub source: &'static str,
    pub text: String,
    pub truncated: bool,
}

/// The longest prefix of `s` within `max` bytes that ends on a character boundary.
pub fn cap_utf8(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// `version` as a whole token, optionally prefixed with v.
fn version_token(version: &str) -> Option<fancy_regex::Regex> {
    fancy_regex::Regex::new(&format!(r"(?<![\w.])v?{}(?![\w]|[.-]\d)", regex::escape(version))).ok()
}

static VERSION_LIKE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\d+\.\d+").unwrap());
static MD_HEADING: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^(#{1,6})\s+(.*)$").unwrap());

fn trim_blank_lines(lines: &[&str]) -> String {
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(lines.len());
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map(|i| i + 1).unwrap_or(start);
    lines[start..end.max(start)].iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
}

/// Line ends made \n, and a leading byte order mark dropped, as R's readers drop it.
fn normalize_newlines(text: &str) -> String {
    text.strip_prefix('\u{feff}').unwrap_or(text).replace("\r\n", "\n").replace('\r', "\n")
}

static SETEXT_UNDERLINE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^ {0,3}(=+|-+)\s*$").unwrap());

/// Markdown headings as (heading line, first body line, level, text), ATX and Setext
/// alike, outside code fences and a leading YAML front matter, as R's reader sees them.
fn md_headings(lines: &[&str]) -> Vec<(usize, usize, usize, String)> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut i = 0;
    if lines.first() == Some(&"---") {
        if let Some(end) = lines.iter().skip(1).position(|l| *l == "---") {
            i = end + 2;
        }
    }
    while i < lines.len() {
        let l = lines[i];
        if l.trim_start().starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence {
            if let Some(c) = MD_HEADING.captures(l) {
                out.push((i, i + 1, c[1].len(), c[2].trim_end_matches('#').trim().to_string()));
            } else if !l.trim().is_empty() && i + 1 < lines.len() {
                if let Some(c) = SETEXT_UNDERLINE.captures(lines[i + 1]) {
                    let level = if c[1].starts_with('=') { 1 } else { 2 };
                    out.push((i, i + 2, level, l.trim().to_string()));
                    i += 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// NEWS.md: from the heading naming the version to the next heading of the same
/// or a higher level that names any version, kept as Markdown.
pub fn md_section(text: &str, version: &str) -> Option<String> {
    let tok = version_token(version)?;
    let text = normalize_newlines(text);
    let lines: Vec<&str> = text.lines().collect();
    let heads = md_headings(&lines);
    let at = heads.iter().position(|(_, _, _, t)| tok.is_match(t).unwrap_or(false))?;
    let (_, body, level, _) = heads[at];
    let end = heads[at + 1..]
        .iter()
        .find(|(_, _, lv, t)| *lv <= level && VERSION_LIKE.is_match(t))
        .map(|(i, _, _, _)| *i)
        .unwrap_or(lines.len());
    Some(trim_blank_lines(&lines[body..end]))
}

static RD_SECTION: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\\section\s*\{").unwrap());

/// inst/NEWS.Rd: the first \section whose title names "version <version>", the rule
/// R's own reader uses, so "Changes in pkg version X" and "Version X" both count.
pub fn rd_section(text: &str, version: &str) -> Option<String> {
    let title_re = fancy_regex::Regex::new(&format!(
        r"(?i)\bversion\s+v?{}(?![\w]|[.-]\d)",
        regex::escape(version)
    ))
    .ok()?;
    let text = crate::strip_rd_comments(&normalize_newlines(text));
    for m in RD_SECTION.find_iter(&text) {
        let Some((title, title_end)) = crate::rd_brace_content(&text, m.end()) else { continue };
        if !title_re.is_match(&title).unwrap_or(false) {
            continue;
        }
        let rest = &text[title_end + 1..];
        let open = rest.len() - rest.trim_start().len();
        if !rest[open..].starts_with('{') {
            return None;
        }
        let body_start = title_end + 1 + open + 1;
        let (body, _) = crate::rd_brace_content(&text, body_start)?;
        return Some(rd_news_text(&body));
    }
    None
}

/// The content of a brace group starting at or after `pos` (whitespace allowed),
/// and the index of its closing brace.
fn group_at(s: &str, pos: usize) -> Option<(String, usize)> {
    let rest = s.get(pos..)?;
    let skip = rest.len() - rest.trim_start().len();
    if !rest[skip..].starts_with('{') {
        return None;
    }
    crate::rd_brace_content(s, pos + skip + 1)
}

/// Rd NEWS markup as plain lines: one "- " line per \item, subsection titles on
/// their own line, inline markup rendered the way dataset titles are.
pub fn rd_news_text(body: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    rd_news_walk(body, &mut lines, &mut cur);
    flush(&mut lines, &mut cur);
    lines.join("\n")
}

fn flush(lines: &mut Vec<String>, cur: &mut String) {
    let text = crate::rds::rd_inline_text(cur);
    if !text.is_empty() && text != "-" {
        lines.push(text);
    }
    cur.clear();
}

fn rd_news_walk(s: &str, lines: &mut Vec<String>, cur: &mut String) {
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'\\' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_alphabetic() {
                j += 1;
            }
            match &s[i + 1..j] {
                "itemize" | "enumerate" | "describe" => {
                    if let Some((inner, end)) = group_at(s, j) {
                        flush(lines, cur);
                        rd_news_walk(&inner, lines, cur);
                        flush(lines, cur);
                        i = end + 1;
                        continue;
                    }
                }
                "item" => {
                    flush(lines, cur);
                    cur.push_str("- ");
                    // \item{label}{text} inside \describe.
                    if let Some((label, end)) = group_at(s, j) {
                        cur.push_str(&label);
                        cur.push_str(": ");
                        if let Some((text, end2)) = group_at(s, end + 1) {
                            cur.push_str(&text);
                            i = end2 + 1;
                        } else {
                            i = end + 1;
                        }
                        continue;
                    }
                    i = j;
                    continue;
                }
                "subsection" | "section" => {
                    if let Some((title, end)) = group_at(s, j) {
                        flush(lines, cur);
                        cur.push_str(&title);
                        flush(lines, cur);
                        match group_at(s, end + 1) {
                            Some((inner, end2)) => {
                                rd_news_walk(&inner, lines, cur);
                                flush(lines, cur);
                                i = end2 + 1;
                            }
                            None => i = end + 1,
                        }
                        continue;
                    }
                }
                _ => {}
            }
            // Other markup is inline: copy the command and its group for rendering.
            if let Some((inner, end)) = group_at(s, j).filter(|_| j < b.len() && b[j] == b'{') {
                cur.push_str(&s[i..j]);
                cur.push('{');
                cur.push_str(&inner);
                cur.push('}');
                i = end + 1;
                continue;
            }
            let next = s[i + 1..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
            let end = if j > i + 1 { j } else { (i + 1 + next).min(s.len()) };
            cur.push_str(&s[i..end]);
            i = end;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        cur.push(ch);
        i += ch.len_utf8();
    }
}

/// A plain NEWS version header: "Version X", "Changes in version X", "<Package> X", or X at the start of a line.
fn plain_header(version_pat: &str, package: &str) -> Option<regex::Regex> {
    regex::Regex::new(&format!(
        r"(?i)^(?:\s*(?:changes\s+in\s+(?:r\s+)?)?version\s+v?{v}|\s*{p}\s+(?:version\s+)?v?{v}|v?{v})(?:[^\w.-]|[.-]\D|$)",
        v = version_pat,
        p = regex::escape(package)
    ))
    .ok()
}

/// Plain NEWS: from the header carrying the version to the next version header.
pub fn plain_section(text: &str, version: &str, package: &str) -> Option<String> {
    let this = plain_header(&regex::escape(version), package)?;
    let any = plain_header(r"\d+\.\d+(?:[.-]\d+)*", package)?;
    let text = normalize_newlines(text);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|l| this.is_match(l))?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| any.is_match(l))
        .map(|k| start + 1 + k)
        .unwrap_or(lines.len());
    Some(trim_blank_lines(&lines[start + 1..end]))
}

/// The section of `news_file` for `version`, capped, or None when there is none.
pub fn release_notes(news_file: &str, text: &str, version: &str, package: &str) -> Option<ReleaseNotes> {
    // A DESCRIPTION that could not be read leaves no version, and any section would be a guess.
    if version.trim().is_empty() {
        return None;
    }
    let (source, section) = match news_file {
        "NEWS.md" | "inst/NEWS.md" => ("news_md", md_section(text, version)?),
        "inst/NEWS.Rd" => ("news_rd", rd_section(text, version)?),
        _ => ("news_plain", plain_section(text, version, package)?),
    };
    let (text, truncated) = cap_utf8(&section, RELEASE_NOTES_CAP);
    Some(ReleaseNotes { source, text, truncated })
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
        assert_eq!(changelog_file(&files(&["CHANGELOG"])), Some("CHANGELOG"));
        assert_eq!(changelog_file(&files(&["changelog", "Changes", "inst/ChangeLog"])), None, "exact case, at the root");
        assert_eq!(changelog_file(&files(&["CHANGES", "ChangeLog"])), Some("ChangeLog"));
    }

    #[test]
    fn a_markdown_section_runs_to_the_next_version_heading() {
        let md = "# fixpkg 1.2.0.9000\n\n- dev\n\n# fixpkg 1.2.0\n\n## Bug fixes\n\n- Fixed `f()`.\n\n```\n# not a heading 9.9\n```\n\n# fixpkg 1.1.0\n\n- old\n";
        let s = md_section(md, "1.2.0").unwrap();
        assert!(s.starts_with("## Bug fixes"), "the heading line itself is left out: {s}");
        assert!(s.contains("- Fixed `f()`."));
        assert!(s.contains("# not a heading 9.9"), "a fenced line is not a heading");
        assert!(!s.contains("old") && !s.contains("dev"));
        assert_eq!(md_section("# pkg v2.0\n- a\n", "2.0").as_deref(), Some("- a"));
        assert_eq!(md_section("# pkg 1.0-2\n- a\n", "1.0"), None, "1.0 is not a token of 1.0-2");
        assert_eq!(md_section("# pkg 1.1\n", "1.2"), None);
    }

    #[test]
    fn setext_headings_and_front_matter_read_as_r_reads_them() {
        let md = "---\ntitle: 1.2.0\n---\n\npkg 1.2.0\n=========\n\nBUG FIX\n-------\n- one\n\npkg 1.1.0\n=========\n- old\n";
        assert_eq!(md_section(md, "1.2.0").as_deref(), Some("BUG FIX\n-------\n- one"));
        let h2 = "What's new in version 1.2.5\n---------------------------\n\n- fixed\n";
        assert_eq!(md_section(h2, "1.2.5").as_deref(), Some("- fixed"));
    }

    #[test]
    fn an_rd_section_becomes_plain_lines() {
        let rd = "% comment\n\\name{NEWS}\n\\section{Changes in version 1.2.0 (2024-01-01)}{\n  \\subsection{BUG FIXES}{\n    \\itemize{\n      \\item Fixed \\code{f()} with 50\\% less work. % why\n      \\item Second.\n    }\n  }\n}\n\\section{Changes in version 1.1.0}{\\itemize{\\item Old.}}\n";
        assert_eq!(
            rd_section(rd, "1.2.0").as_deref(),
            Some("BUG FIXES\n- Fixed `f()` with 50% less work.\n- Second.")
        );
        assert_eq!(rd_section(rd, "1.3.0"), None);
        let named = "\\section{Changes in fixpkg version 0.1.2}{\\itemize{\\item A.}}\n\\section{Version 0.1.1}{\\itemize{\\item B.}}";
        assert_eq!(rd_section(named, "0.1.2").as_deref(), Some("- A."));
        assert_eq!(rd_section(named, "0.1.1").as_deref(), Some("- B."));
    }

    #[test]
    fn a_plain_section_runs_to_the_next_version_header() {
        let t = "fixpkg 1.2.0\n  * one\n  * two\n\nfixpkg 1.1.0\n  * old\n";
        assert_eq!(plain_section(t, "1.2.0", "fixpkg").as_deref(), Some("  * one\n  * two"));
        let t2 = "CHANGES IN VERSION 2.0\n- a\nVersion 1.9\n- b\n";
        assert_eq!(plain_section(t2, "2.0", "fixpkg").as_deref(), Some("- a"));
        let t3 = "2.0 (2024)\n- a\n1.9\n- b\n";
        assert_eq!(plain_section(t3, "2.0", "fixpkg").as_deref(), Some("- a"));
    }

    #[test]
    fn notes_are_cut_at_a_character_boundary_and_flagged() {
        let long = format!("# pkg 1.0\n{}", "é".repeat(RELEASE_NOTES_CAP));
        let n = release_notes("NEWS.md", &long, "1.0", "pkg").unwrap();
        assert!(n.truncated);
        assert!(n.text.len() <= RELEASE_NOTES_CAP);
        assert_eq!(n.source, "news_md");
        assert_eq!(cap_utf8("abc", 3), ("abc".to_string(), false));
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_first_heading() {
        assert_eq!(md_section("\u{feff}# pkg 1.0.0\n- a\n# pkg 0.9.0\n- b\n", "1.0.0").as_deref(), Some("- a"));
        assert_eq!(plain_section("\u{feff}Version 1.0.0\n- a\nVersion 0.9.0\n- b\n", "1.0.0", "pkg").as_deref(), Some("- a"));
        let rd = "\u{feff}\\section{Changes in version 1.0.0}{\\itemize{\\item A.}}";
        assert_eq!(rd_section(rd, "1.0.0").as_deref(), Some("- A."));
    }

    #[test]
    fn no_version_selects_no_section() {
        let md = "# pkg (development version)\n- unreleased\n\n# pkg 1.0.0\n- a\n";
        assert!(release_notes("NEWS.md", md, "", "pkg").is_none());
        assert!(release_notes("NEWS", "  * a\n  * b\n", " ", "pkg").is_none());
    }
}
