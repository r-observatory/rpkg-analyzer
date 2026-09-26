// inst/CITATION read without evaluating it: what it cites, its DOIs, and whether
// a static read can be trusted to have seen everything.

use std::collections::BTreeMap;
use tree_sitter::{Node, Parser};

const ENTRY_CALLS: [&str; 2] = ["bibentry", "citEntry"];
/// Calls a static read can follow: entries, headers, persons and joins of literals.
const ALLOWED_CALLS: [&str; 13] = [
    "bibentry", "citEntry", "citHeader", "citFooter", "person", "as.person", "personList", "c", "list", "paste",
    "paste0", "packageName", "structure",
];
const PUBLICATION_TYPES: [&str; 12] = [
    "article", "book", "booklet", "inbook", "incollection", "inproceedings", "conference", "phdthesis",
    "mastersthesis", "techreport", "proceedings", "unpublished",
];
const ASSIGNMENTS: [&str; 5] = ["<-", "<<-", "=", "->", "->>"];

pub struct Citation {
    pub read: &'static str,
    pub kind: Option<&'static str>,
    pub n_entries: Option<i64>,
    pub bibtypes: Option<Vec<String>>,
    pub dois: Option<Vec<String>>,
    pub venues: Option<Vec<String>>,
}

impl Citation {
    pub fn parse_error() -> Citation {
        Citation { read: "parse_error", kind: None, n_entries: None, bibtypes: None, dois: None, venues: None }
    }
}

/// UTF-8, or Latin-1 when DESCRIPTION declares it, as readCitationFile does.
pub fn decode(bytes: &[u8], encoding: Option<&str>) -> Option<String> {
    let latin1 = encoding.is_some_and(|e| e.trim().eq_ignore_ascii_case("latin1") || e.trim().eq_ignore_ascii_case("ISO-8859-1"));
    if latin1 {
        Some(bytes.iter().map(|&b| b as char).collect())
    } else {
        String::from_utf8(bytes.to_vec()).ok()
    }
}

/// A DOI lowercased, without a resolver prefix or trailing punctuation.
pub fn normalize_doi(raw: &str) -> Option<String> {
    let lower = raw.trim().to_lowercase();
    let bare = ["https://doi.org/", "http://doi.org/", "https://dx.doi.org/", "http://dx.doi.org/", "doi:"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        .unwrap_or(&lower);
    let doi = bare.trim().trim_end_matches(['.', ',', ';', ':', ')', ']', '}']);
    doi.starts_with("10.").then(|| doi.to_string())
}

/// The DOI in a doi.org URL, if there is one.
fn doi_in_url(url: &str) -> Option<String> {
    let lower = url.to_lowercase();
    let at = lower.find("doi.org/10.")?;
    // Slice the string the offset came from: lowercasing can change byte lengths.
    normalize_doi(&lower[at + "doi.org/".len()..])
}

pub fn venue(doi: &str) -> Option<&'static str> {
    if doi.starts_with("10.32614/cran.package.") || doi.starts_with("10.5281/zenodo.") {
        None
    } else if doi.starts_with("10.18637/jss") {
        Some("jss")
    } else if doi.starts_with("10.32614/rj") {
        Some("rjournal")
    } else if doi.starts_with("10.21105/joss") {
        Some("joss")
    } else {
        Some("other")
    }
}

fn text<'a>(n: Node, src: &'a str) -> &'a str {
    n.utf8_text(src.as_bytes()).unwrap_or("")
}

/// The content of a string literal node, or None when the node is not one.
fn string_value(n: Node, src: &str) -> Option<String> {
    if n.kind() != "string" {
        return None;
    }
    let mut c = n.walk();
    let content = n.children(&mut c).find(|ch| ch.kind() == "string_content");
    Some(content.map(|ch| text(ch, src).to_string()).unwrap_or_default())
}

#[derive(Default)]
struct Walk {
    needs_eval: bool,
    uses_meta: bool,
    entries: i64,
    bibtypes: Vec<String>,
    dois: Vec<String>,
}

fn read_entry(call: Node, src: &str, name: &str, w: &mut Walk) {
    let Some(args) = call.child_by_field_name("arguments") else { return };
    let mut bibtype: Option<String> = None;
    let mut first_positional: Option<String> = None;
    let mut c = args.walk();
    for arg in args.children(&mut c).filter(|a| a.kind() == "argument") {
        let value = arg.child_by_field_name("value");
        match arg.child_by_field_name("name").map(|n| text(n, src)) {
            Some(k) if (k == "bibtype" && name == "bibentry") || (k == "entry" && name == "citEntry") => {
                bibtype = value.and_then(|v| string_value(v, src));
            }
            Some("doi") => {
                if let Some(d) = value.and_then(|v| string_value(v, src)).and_then(|s| normalize_doi(&s)) {
                    w.dois.push(d);
                }
            }
            Some("url") => {
                if let Some(d) = value.and_then(|v| string_value(v, src)).and_then(|s| doi_in_url(&s)) {
                    w.dois.push(d);
                }
            }
            None if first_positional.is_none() => {
                first_positional = Some(value.and_then(|v| string_value(v, src)).unwrap_or_default());
            }
            _ => {}
        }
    }
    let bt = bibtype.or(first_positional.filter(|s| !s.is_empty()));
    if let Some(bt) = bt {
        w.bibtypes.push(bt.to_lowercase());
    }
}

fn walk(n: Node, src: &str, desc: &BTreeMap<String, String>, w: &mut Walk) {
    match n.kind() {
        "call" => {
            let f = n.child_by_field_name("function");
            let name = crate::call_fn_name(&n, src.as_bytes());
            match (f.map(|f| f.kind()), name.as_deref()) {
                (Some("identifier") | Some("namespace_operator"), Some(nm)) if ALLOWED_CALLS.contains(&nm) => {
                    if ENTRY_CALLS.contains(&nm) {
                        w.entries += 1;
                        read_entry(n, src, nm, w);
                    }
                }
                _ => w.needs_eval = true,
            }
            // The function name is not a free symbol; its arguments are walked below.
            if let Some(args) = n.child_by_field_name("arguments") {
                walk(args, src, desc, w);
            }
            return;
        }
        "binary_operator" => {
            let op = n.child_by_field_name("operator").map(|o| text(o, src)).unwrap_or("");
            if ASSIGNMENTS.contains(&op) {
                w.needs_eval = true;
            }
        }
        "extract_operator" => {
            let lhs = n.child_by_field_name("lhs");
            let rhs = n.child_by_field_name("rhs");
            let op = n.child_by_field_name("operator").map(|o| text(o, src)).unwrap_or("");
            if lhs.is_some_and(|l| l.kind() == "identifier" && text(l, src) == "meta") && op == "$" {
                let field = rhs.map(|r| string_value(r, src).unwrap_or_else(|| text(r, src).to_string())).unwrap_or_default();
                if desc.contains_key(&field) {
                    w.uses_meta = true;
                } else {
                    w.needs_eval = true;
                }
                return;
            }
            if let Some(l) = lhs {
                walk(l, src, desc, w);
            }
            return;
        }
        "subset2" => {
            // meta[["Field"]] reads the same DESCRIPTION field as meta$Field.
            let f = n.child_by_field_name("function");
            if f.is_some_and(|f| f.kind() == "identifier" && text(f, src) == "meta") {
                let field = n
                    .child_by_field_name("arguments")
                    .and_then(|a| {
                        let mut c = a.walk();
                        let found = a.children(&mut c).find(|ch| ch.kind() == "argument");
                        found
                    })
                    .and_then(|a| a.child_by_field_name("value"))
                    .and_then(|v| string_value(v, src));
                match field {
                    Some(fl) if desc.contains_key(&fl) => w.uses_meta = true,
                    _ => w.needs_eval = true,
                }
                return;
            }
            w.needs_eval = true;
        }
        "identifier" => {
            let parent_kind = n.parent().map(|p| p.kind()).unwrap_or("");
            let is_arg_name = n.parent().and_then(|p| p.child_by_field_name("name")).is_some_and(|nm| nm.id() == n.id());
            let t = text(n, src);
            if !(is_arg_name || parent_kind == "namespace_operator" || t == "T" || t == "F") {
                w.needs_eval = true;
            }
        }
        "function_definition" | "if_statement" | "for_statement" | "while_statement" | "repeat_statement" => {
            w.needs_eval = true;
        }
        _ => {}
    }
    let mut c = n.walk();
    for ch in n.children(&mut c) {
        walk(ch, src, desc, w);
    }
}

/// Reads the CITATION bytes; `desc` gives the Encoding and the fields meta$ can name.
pub fn read_citation(bytes: &[u8], desc: &BTreeMap<String, String>) -> Citation {
    let Some(src) = decode(bytes, desc.get("Encoding").map(String::as_str)) else {
        return Citation::parse_error();
    };
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_r::LANGUAGE.into()).expect("load tree-sitter-r");
    let Some(tree) = parser.parse(&src, None) else { return Citation::parse_error() };
    if tree.root_node().has_error() {
        return Citation::parse_error();
    }
    let mut w = Walk::default();
    walk(tree.root_node(), &src, desc, &mut w);

    let read = if w.needs_eval {
        "needs_eval"
    } else if w.uses_meta {
        "meta"
    } else {
        "literal"
    };
    let mut bibtypes: Vec<String> = Vec::new();
    for b in w.bibtypes {
        if !bibtypes.contains(&b) {
            bibtypes.push(b);
        }
    }
    let mut dois: Vec<String> = Vec::new();
    for d in w.dois {
        if !dois.contains(&d) {
            dois.push(d);
        }
    }
    let kind = if bibtypes.iter().any(|b| PUBLICATION_TYPES.contains(&b.as_str())) {
        Some("publication")
    } else if !bibtypes.is_empty() && bibtypes.iter().all(|b| b == "manual" || b == "misc") {
        Some("software_only")
    } else {
        None
    };
    let venues: Vec<String> = ["jss", "rjournal", "joss", "other"]
        .iter()
        .filter(|v| dois.iter().any(|d| venue(d) == Some(**v)))
        .map(|v| v.to_string())
        .collect();
    // An empty list asserts "no DOI" only when the read saw everything.
    let (dois, venues) = if read == "needs_eval" && dois.is_empty() { (None, None) } else { (Some(dois), Some(venues)) };
    Citation { read, kind, n_entries: Some(w.entries), bibtypes: Some(bibtypes), dois, venues }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(fields: &[(&str, &str)]) -> BTreeMap<String, String> {
        fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_literal_citation_is_read_whole() {
        let src = r#"citHeader("To cite fixpkg in publications use:")
bibentry(bibtype = "Article",
  title = "A paper", author = c(person("Ann", "Lee"), as.person("Bo Chen")),
  journal = "Journal of Statistical Software", year = "2020",
  doi = "https://doi.org/10.18637/JSS.v001.i01.")
bibentry("Manual", title = paste0("fixpkg: ", "tools"), url = "https://doi.org/10.32614/CRAN.package.fixpkg")
"#;
        let c = read_citation(src.as_bytes(), &desc(&[]));
        assert_eq!(c.read, "literal");
        assert_eq!(c.kind, Some("publication"));
        assert_eq!(c.n_entries, Some(2));
        assert_eq!(c.bibtypes, Some(vec!["article".to_string(), "manual".to_string()]));
        assert_eq!(c.dois, Some(vec!["10.18637/jss.v001.i01".to_string(), "10.32614/cran.package.fixpkg".to_string()]));
        assert_eq!(c.venues, Some(vec!["jss".to_string()]), "the CRAN package DOI is not a venue");
    }

    #[test]
    fn meta_fields_the_description_declares_read_as_meta() {
        let src = r#"bibentry("Manual", title = meta$Title, note = paste("R package version", meta$Version), year = meta[["Date"]])"#;
        let c = read_citation(src.as_bytes(), &desc(&[("Title", "x"), ("Version", "1.0"), ("Date", "2020")]));
        assert_eq!(c.read, "meta");
        assert_eq!(c.kind, Some("software_only"));
        assert_eq!(c.dois, Some(vec![]), "a full read may say there is no DOI");
        let missing = read_citation(src.as_bytes(), &desc(&[("Title", "x")]));
        assert_eq!(missing.read, "needs_eval", "a field DESCRIPTION lacks needs evaluation");
    }

    #[test]
    fn anything_that_needs_evaluating_is_flagged_and_claims_no_absence() {
        for src in [
            r#"year <- sub("-.*", "", meta$Date); bibentry("Manual", title = "x", year = year)"#,
            r#"bibentry("Manual", title = "x", note = sprintf("version %s", packageVersion("fixpkg")))"#,
            r#"citation(auto = meta)"#,
            r#"bibentry("Manual", title = "x", year = format(Sys.Date(), "%Y"))"#,
        ] {
            let c = read_citation(src.as_bytes(), &desc(&[("Date", "2020-01-01")]));
            assert_eq!(c.read, "needs_eval", "{src}");
            assert_eq!(c.dois, None, "{src}");
            assert_eq!(c.venues, None, "{src}");
        }
        let with_doi = read_citation(br#"bibentry("Article", title = "x", doi = "10.21105/joss.01234", year = format(Sys.Date()))"#, &desc(&[]));
        assert_eq!(with_doi.read, "needs_eval");
        assert_eq!(with_doi.dois, Some(vec!["10.21105/joss.01234".to_string()]));
        assert_eq!(with_doi.venues, Some(vec!["joss".to_string()]));
    }

    #[test]
    fn a_url_that_grows_when_lowercased_still_gives_its_doi() {
        let url = "\u{130}\u{130}\u{130}\u{130}\u{130}\u{130} https://doi.org/10.1/\u{fc}\u{fc}\u{fc}\u{fc}";
        assert_eq!(doi_in_url(url), Some("10.1/\u{fc}\u{fc}\u{fc}\u{fc}".to_string()));
    }

    #[test]
    fn citentry_uses_entry_for_its_type() {
        let c = read_citation(br#"citEntry(entry = "Book", title = "T", author = "A", year = "1999")"#, &desc(&[]));
        assert_eq!(c.bibtypes, Some(vec!["book".to_string()]));
        assert_eq!(c.kind, Some("publication"));
    }

    #[test]
    fn encoding_and_parse_errors() {
        let latin1 = b"bibentry(\"Manual\", title = \"M\xfcller\")";
        let declared = read_citation(latin1, &desc(&[("Encoding", "latin1")]));
        assert_eq!(declared.read, "literal", "Latin-1 decodes when DESCRIPTION says so");
        let undeclared = read_citation(latin1, &desc(&[]));
        assert_eq!(undeclared.read, "parse_error", "without the Encoding field the bytes are not UTF-8");
        assert_eq!(undeclared.bibtypes, None);
        let broken = read_citation(b"bibentry(\"Manual\", title = ", &desc(&[]));
        assert_eq!(broken.read, "parse_error");
        assert_eq!(broken.n_entries, None);
    }
}
