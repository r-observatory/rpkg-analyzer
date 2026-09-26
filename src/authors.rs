// What an author entry carries besides a name: ORCID and ROR identifiers, and a
// short comment.

use std::sync::LazyLock;

static ORCID_FORM: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^(\d{4})-?(\d{4})-?(\d{4})-?(\d{3}[\dX])$").unwrap());
static ORCID_URL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)orcid\.org/(\d{4}-\d{4}-\d{4}-\d{3}[\dX])").unwrap());
static ORCID_PREFIX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)^(?:https?://)?(?:www\.)?orcid\.org/").unwrap());
static ROR_FORM: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^0[a-hj-km-np-tv-z0-9]{6}[0-9]{2}$").unwrap());
static EMAIL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[^\s<>()\[\]@,;]+@[^\s<>()\[\]@,;]+\.[^\s<>()\[\]@,;]+").unwrap());
static EMAIL_BRACKETED: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\s*<[^<>]*@[^<>]*>").unwrap());
static EMAIL_ONLY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[<(]?[^\s<>()@,;]+@[^\s<>()@,;]+\.[^\s<>()@,;]+[>)]?$").unwrap());
// One part of a comment = c(...) body: an optional name (bare, quoted or backticked), then a
// quoted value. Every quoted value is consumed from its opening quote, so text inside it is never a name.
static COMMENT_PART: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?:([A-Za-z_.][A-Za-z0-9_.]*)|`([^`]*)`|"([^"]*)"|'([^']*)')\s*=\s*(?:"([^"]*)"|'([^']*)')|"([^"]*)"|'([^']*)'"#,
    )
    .unwrap()
});

/// A bare ORCID iD, kept only when its ISO 7064 MOD 11-2 check digit holds.
pub fn orcid_checked(raw: &str) -> Option<String> {
    let v = raw.trim();
    // Only a leading orcid.org URL is stripped, so text before it means the value is not an iD.
    let bare = ORCID_PREFIX.replace(v, "").trim_end_matches('/').to_ascii_uppercase();
    let c = ORCID_FORM.captures(&bare)?;
    let digits = format!("{}{}{}{}", &c[1], &c[2], &c[3], &c[4]);
    let mut total = 0u32;
    for ch in digits.chars().take(15) {
        total = (total + ch.to_digit(10)?) * 2;
    }
    let r = (12 - total % 11) % 11;
    let check = if r == 10 { 'X' } else { char::from_digit(r, 10)? };
    (digits.chars().last() == Some(check)).then(|| format!("{}-{}-{}-{}", &c[1], &c[2], &c[3], &c[4]))
}

/// A bare ROR id when the value is one.
pub fn ror_checked(raw: &str) -> Option<String> {
    let lower = raw.trim().to_lowercase();
    let bare = ["https://ror.org/", "http://ror.org/", "ror.org/"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        .unwrap_or(&lower)
        .trim_end_matches('/');
    ROR_FORM.is_match(bare).then(|| bare.to_string())
}

/// The first checked ORCID in an orcid.org URL anywhere in the text.
pub fn orcid_in_text(text: &str) -> Option<String> {
    ORCID_URL.captures_iter(text).find_map(|c| orcid_checked(&c[1]))
}

/// An email address, or a handle starting with @, is never a name.
pub fn is_email_like(v: &str) -> bool {
    let v = v.trim();
    v.starts_with('@') || EMAIL_ONLY.is_match(v)
}

/// Comment parts joined, emails removed, whitespace collapsed, at most 120 characters.
pub fn clean_comment(parts: &[String]) -> Option<String> {
    let joined = parts.join("; ");
    let unbracketed = EMAIL_BRACKETED.replace_all(&joined, "");
    let no_email = EMAIL.replace_all(&unbracketed, "");
    let collapsed = no_email.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| c == ';' || c == ',' || c.is_whitespace());
    (!trimmed.is_empty()).then(|| trimmed.chars().take(120).collect())
}

/// The quoted parts of a comment = c(...) body in order, each with its name when it has one.
pub fn comment_parts(body: &str) -> Vec<(Option<String>, String)> {
    COMMENT_PART
        .captures_iter(body)
        .map(|c| {
            let name = (1..=4).find_map(|i| c.get(i)).map(|m| m.as_str().to_string());
            let value = (5..=8).find_map(|i| c.get(i)).map_or("", |m| m.as_str()).to_string();
            (name, value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_orcid_needs_its_check_digit() {
        assert_eq!(orcid_checked("0000-0002-1825-0097").as_deref(), Some("0000-0002-1825-0097"));
        assert_eq!(orcid_checked("https://orcid.org/0000-0002-1694-233x").as_deref(), Some("0000-0002-1694-233X"));
        assert_eq!(orcid_checked("0000-0002-1825-0098"), None);
        assert_eq!(orcid_checked("0000-0002-1825"), None);
        assert_eq!(orcid_in_text("see https://orcid.org/0000-0001-5109-3700.").as_deref(), Some("0000-0001-5109-3700"));
        assert_eq!(orcid_checked("http://www.orcid.org/0000-0002-1825-0097/").as_deref(), Some("0000-0002-1825-0097"));
    }

    #[test]
    fn only_a_leading_orcid_url_is_read_as_an_id() {
        // Lower-casing changes the byte length of these letters, which must not move a slice.
        assert_eq!(orcid_checked("\u{130}T\u{dc}, orcid.org/"), None);
        assert_eq!(orcid_checked("\u{212a}\u{212a}\u{212a} orcid.org/0000-0002-1825-0097"), None);
        assert_eq!(orcid_checked("\u{130} https://orcid.org/0000-0002-1825-0097"), None);
        assert_eq!(orcid_checked("Univ of X, https://orcid.org/0000-0002-1825-0097"), None);
        assert_eq!(
            orcid_in_text("\u{130}T\u{dc}, https://orcid.org/0000-0002-1825-0097").as_deref(),
            Some("0000-0002-1825-0097")
        );
    }

    #[test]
    fn comment_parts_come_in_order_with_their_names() {
        let parts = comment_parts(r#"affiliation = "Univ (X)", 'ORCID'="0000-0002-1825-0097", "King's College", `grant` = 'G-1', "a = 'b'""#);
        let want: Vec<(Option<&str>, &str)> = vec![
            (Some("affiliation"), "Univ (X)"),
            (Some("ORCID"), "0000-0002-1825-0097"),
            (None, "King's College"),
            (Some("grant"), "G-1"),
            (None, "a = 'b'"),
        ];
        let got: Vec<(Option<&str>, &str)> = parts.iter().map(|(n, v)| (n.as_deref(), v.as_str())).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn a_ror_id_has_one_shape() {
        assert_eq!(ror_checked("https://ror.org/05dxps055").as_deref(), Some("05dxps055"));
        assert_eq!(ror_checked("03vek6s52").as_deref(), Some("03vek6s52"));
        assert_eq!(ror_checked("05dxps05"), None);
        assert_eq!(ror_checked("15dxps055"), None, "a ROR id starts with 0");
        assert_eq!(ror_checked("0ldxps055"), None, "l is not in the alphabet");
    }

    #[test]
    fn a_comment_loses_emails_and_stops_at_120_characters() {
        assert_eq!(clean_comment(&["Wrote it <ann@x.org>".into()]).as_deref(), Some("Wrote it"));
        assert_eq!(clean_comment(&["a".repeat(200)]).map(|c| c.chars().count()), Some(120));
        assert_eq!(clean_comment(&["  ".into()]), None);
        assert!(is_email_like("ann@x.org"));
        assert!(is_email_like("<ann@x.org>"));
        assert!(is_email_like("@ann"));
        assert!(!is_email_like("Ann"));
    }
}
