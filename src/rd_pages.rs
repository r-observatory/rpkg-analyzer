// Help pages: which files are pages, how their examples run, and what they document.

use std::path::Path;
use std::sync::LazyLock;

static RD_PAGE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)^man/(?:(?:unix|windows)/)?[^/]+\.rd$").unwrap());

/// Help pages as R installs them: man/*.Rd in any case, plus the unix/ and windows/
/// subdirectories. man/macros holds Rd macro definitions, which are not pages.
pub fn rd_page_files(files: &[String]) -> Vec<&str> {
    files.iter().filter(|f| RD_PAGE.is_match(f)).map(String::as_str).collect()
}

static EXAMPLES_OPEN: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\\examples\s*\{").unwrap());
// roxygen2 7.3.3 writes withAutoprint(\{; earlier releases wrote (if (...) withAutoprint else force)(\{.
static EXAMPLES_IF: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\\dontshow\s*\{\s*if\s*\(.*withAutoprint[^\n]*\(\s*\\?\{").unwrap());
static IF_GUARD: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^if\s*\(\s*!?\s*(?:interactive|requireNamespace|Sys\.getenv)\s*\(").unwrap()
});

/// The content of every \examples{...} block in comment-stripped Rd text. A block
/// whose braces never close runs to the end of the page.
pub fn example_blocks(stripped: &str) -> Vec<String> {
    EXAMPLES_OPEN
        .find_iter(stripped)
        .map(|m| match crate::rd_brace_content(stripped, m.end()) {
            Some((c, _)) => c,
            None => stripped[m.end()..].to_string(),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExampleClass {
    /// Some code runs in a plain R CMD check.
    Run,
    /// Code runs only when donttest examples are run.
    DonttestOnly,
    /// Code sits only in \dontrun, \dontshow or \testonly.
    NeverRun,
    /// Comments or whitespace only.
    Empty,
}

#[derive(Default)]
struct ExampleText {
    plain: String,
    donttest: String,
    dontrun: String,
    dontshow: String,
}

/// Splits example text by the wrapper it sits in. \dontdiff code runs, so it stays in place.
fn split_examples(s: &str, ctx: &[&str], acc: &mut ExampleText) {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut chunk_start = 0usize;
    let push = |acc: &mut ExampleText, text: &str| {
        let slot = if ctx.is_empty() {
            &mut acc.plain
        } else if ctx.contains(&"dontrun") {
            &mut acc.dontrun
        } else if ctx.contains(&"dontshow") || ctx.contains(&"testonly") {
            &mut acc.dontshow
        } else {
            &mut acc.donttest
        };
        slot.push_str(text);
    };
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_alphabetic() {
            j += 1;
        }
        let cmd = &s[i + 1..j];
        let wrapper = matches!(cmd, "dontrun" | "donttest" | "dontshow" | "testonly" | "dontdiff");
        let rest = &s[j..];
        let open = rest.len() - rest.trim_start().len();
        if wrapper && rest[open..].starts_with('{') {
            push(acc, &s[chunk_start..i]);
            let body_start = j + open + 1;
            let (inner, end) = match crate::rd_brace_content(s, body_start) {
                Some(x) => x,
                None => (s[body_start..].to_string(), s.len() - 1),
            };
            let mut nctx = ctx.to_vec();
            if cmd != "dontdiff" {
                nctx.push(cmd);
            }
            split_examples(&inner, &nctx, acc);
            i = end + 1;
            chunk_start = i;
            continue;
        }
        // An escape or an unknown command is example text.
        i = if j > i + 1 { j } else { (i + 2).min(b.len()) };
    }
    if chunk_start < s.len() {
        push(acc, &s[chunk_start..]);
    }
}

/// Whether text holds R code: a line that is neither blank nor an R comment.
fn has_code(text: &str) -> bool {
    text.lines().map(str::trim).any(|l| !l.is_empty() && !l.starts_with('#'))
}

pub struct ExamplePage {
    pub class: ExampleClass,
    /// A run page whose examples sit behind @examplesIf or an if guard.
    pub conditional: bool,
}

/// None when the page has no \examples block.
pub fn classify_examples(blocks: &[String]) -> Option<ExamplePage> {
    if blocks.is_empty() {
        return None;
    }
    let mut acc = ExampleText::default();
    for block in blocks {
        split_examples(block, &[], &mut acc);
    }
    let class = if has_code(&acc.plain) {
        ExampleClass::Run
    } else if has_code(&acc.donttest) {
        ExampleClass::DonttestOnly
    } else if has_code(&acc.dontrun) || has_code(&acc.dontshow) {
        ExampleClass::NeverRun
    } else {
        ExampleClass::Empty
    };
    let first_code = acc.plain.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'));
    let conditional = class == ExampleClass::Run
        && (blocks.iter().any(|b| EXAMPLES_IF.is_match(b)) || first_code.is_some_and(|l| IF_GUARD.is_match(l)));
    Some(ExamplePage { class, conditional })
}

/// What one help page says, read once.
pub struct PageFacts {
    pub examples: Option<ExamplePage>,
}

pub fn page_facts(root: &Path, files: &[String]) -> Vec<PageFacts> {
    rd_page_files(files)
        .into_iter()
        .map(|f| {
            let text = crate::strip_rd_comments(&crate::read_lossy(root, f).unwrap_or_default());
            PageFacts { examples: classify_examples(&example_blocks(&text)) }
        })
        .collect()
}

pub struct ExampleCounts {
    pub pages: i64,
    pub run: i64,
    pub donttest_only: i64,
    pub never_run: i64,
    pub empty: i64,
    pub conditional: i64,
}

pub fn example_counts(pages: &[PageFacts]) -> ExampleCounts {
    let ex: Vec<&ExamplePage> = pages.iter().filter_map(|p| p.examples.as_ref()).collect();
    let n = |c: ExampleClass| ex.iter().filter(|e| e.class == c).count() as i64;
    ExampleCounts {
        pages: ex.len() as i64,
        run: n(ExampleClass::Run),
        donttest_only: n(ExampleClass::DonttestOnly),
        never_run: n(ExampleClass::NeverRun),
        empty: n(ExampleClass::Empty),
        conditional: ex.iter().filter(|e| e.conditional).count() as i64,
    }
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

    fn class_of(rd: &str) -> Option<(ExampleClass, bool)> {
        let stripped = crate::strip_rd_comments(rd);
        classify_examples(&example_blocks(&stripped)).map(|p| (p.class, p.conditional))
    }

    #[test]
    fn each_example_class_is_told_apart() {
        assert_eq!(class_of("\\examples{\nf(1)\n}"), Some((ExampleClass::Run, false)));
        assert_eq!(class_of("\\examples{\n\\dontdiff{f(1)}\n}"), Some((ExampleClass::Run, false)), "dontdiff code runs");
        assert_eq!(class_of("\\examples{\n\\donttest{f(1)}\n}"), Some((ExampleClass::DonttestOnly, false)));
        assert_eq!(
            class_of("\\examples{\n\\donttest{\\dontrun{f(1)}}\n}"),
            Some((ExampleClass::NeverRun, false)),
            "dontrun inside donttest never runs"
        );
        assert_eq!(class_of("\\examples{\n\\dontrun{f(1)}\n}"), Some((ExampleClass::NeverRun, false)));
        assert_eq!(class_of("\\examples{\n\\dontshow{f(1)}\n}"), Some((ExampleClass::NeverRun, false)));
        assert_eq!(class_of("\\examples{\n# just a comment\n\n}"), Some((ExampleClass::Empty, false)));
        assert_eq!(class_of("\\name{f}\n\\title{x}"), None);
    }

    #[test]
    fn rd_comments_and_escapes_are_respected() {
        // The % starts an Rd comment, so the only code is the escaped 50\% line.
        assert_eq!(class_of("\\examples{\n% f(1)\nx <- \"50\\%\"\n}"), Some((ExampleClass::Run, false)));
        assert_eq!(class_of("\\examples{\n% f(1)\n}"), Some((ExampleClass::Empty, false)));
        // An escaped brace does not close the block early.
        assert_eq!(class_of("\\examples{\n\\dontrun{g(\"\\}\")}\n}"), Some((ExampleClass::NeverRun, false)));
    }

    #[test]
    fn examples_if_and_if_guards_are_conditional() {
        let roxygen = "\\examples{\n\\dontshow{if (requireNamespace(\"sf\", quietly = TRUE)) withAutoprint(\\{ # examplesIf}\nf(1)\n\\dontshow{\\}) # examplesIf}\n}";
        assert_eq!(class_of(roxygen), Some((ExampleClass::Run, true)));
        assert_eq!(class_of("\\examples{\nif (interactive()) {\n  f(1)\n}\n}"), Some((ExampleClass::Run, true)));
        assert_eq!(class_of("\\examples{\nx <- 1\nif (interactive()) f(x)\n}"), Some((ExampleClass::Run, false)));
        // roxygen2 before 7.3.3 wrote the wrapper as (if (getRversion() >= "3.4") withAutoprint else force)(...).
        let roxygen_71 = "\\examples{\n\\dontshow{if (interactive()) (if (getRversion() >= \"3.4\") withAutoprint else force)(\\{ # examplesIf}\nf(1)\n\\dontshow{\\}) # examplesIf}\n}";
        assert_eq!(class_of(roxygen_71), Some((ExampleClass::Run, true)));
    }
}
