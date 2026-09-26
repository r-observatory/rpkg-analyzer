// Vignette sources as tools::pkgVignettes finds them, and whether building one
// runs any code.

use std::path::Path;
use std::sync::LazyLock;

/// Sweave sources need no engine declaration.
const SWEAVE_EXTS: [&str; 4] = ["Rnw", "rnw", "Snw", "snw"];
/// These count only when the text declares \VignetteEngine{.
const ENGINE_EXTS: [&str; 9] = ["Rmd", "rmd", "Rtex", "Rhtml", "Rrst", "qmd", "md", "asis", "rsp"];
/// Names in an eval= expression that make evaluation depend on the environment.
const GATE_NAMES: [&str; 6] = ["NOT_CRAN", "Sys.getenv", "identical", "nzchar", "requireNamespace", "interactive"];

static MD_HEADER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?m)^[ \t]*```+[ \t]*\{[ \t]*[rR](?:[ \t,][^}\n]*)?\}").unwrap());
static RNW_HEADER: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?m)^[ \t]*<<([^>\n]*)>>=").unwrap());
static RTEX_HEADER: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?m)^[ \t]*%+[ \t]*begin\.rcode(.*)$").unwrap());
static RHTML_HEADER: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"<!--[ \t]*begin\.rcode(.*)").unwrap());
static RRST_HEADER: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?m)^\.\.[ \t]*\{r(.*)\}").unwrap());
static RSP_CODE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"<%(?:=|\s)").unwrap());
static OPTS_SET: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"opts_chunk\$set\s*\(").unwrap());
static SWEAVE_OPTS: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\\SweaveOpts\s*\{([^}]*)\}").unwrap());
static EVAL_ARG: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\beval\s*=\s*").unwrap());

fn ext(path: &str) -> &str {
    path.rsplit_once('.').map(|(_, e)| e).unwrap_or("")
}

/// A file directly under vignettes/ whose extension can be a vignette source.
pub fn is_candidate(path: &str) -> bool {
    let Some(name) = path.strip_prefix("vignettes/") else { return false };
    !name.contains('/') && (SWEAVE_EXTS.contains(&ext(name)) || ENGINE_EXTS.contains(&ext(name)))
}

/// Whether R would build this candidate as a vignette.
pub fn is_source(path: &str, text: &str) -> bool {
    SWEAVE_EXTS.contains(&ext(path)) || text.contains("\\VignetteEngine{")
}

/// The vignette sources among `files`, in file order.
pub fn vignette_sources(root: &Path, files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|f| is_candidate(f))
        // An unreadable Sweave source still counts by its extension, so vignette_facts can say unknown.
        .filter(|f| is_source(f, &super::read_lossy(root, f).unwrap_or_default()))
        .cloned()
        .collect()
}

/// The value of the eval= argument in an argument list, up to the next top-level comma.
fn eval_value(args: &str) -> Option<String> {
    let m = EVAL_ARG.find(args)?;
    let mut depth = 0i32;
    let mut out = String::new();
    for ch in args[m.end()..].chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => break,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => break,
            _ => {}
        }
        out.push(ch);
    }
    Some(out.trim().to_string())
}

/// The argument text of each opts_chunk$set(...) call that is not commented out.
fn opts_chunk_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for m in OPTS_SET.find_iter(text) {
        let line_start = text[..m.start()].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..m.start()].contains('#') {
            continue;
        }
        let mut depth = 1i32;
        let mut args = String::new();
        for ch in text[m.end()..].chars() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            args.push(ch);
        }
        out.push(args);
    }
    out
}

fn is_off(v: &str) -> bool {
    v == "FALSE" || v == "F"
}

fn is_on(v: &str) -> bool {
    v == "TRUE" || v == "T"
}

fn is_gate(v: &str) -> bool {
    GATE_NAMES.iter().any(|n| v.contains(n))
}

/// Chunk header argument text, or None for an engine with no R chunks.
fn chunk_headers(path: &str, text: &str) -> Option<Vec<String>> {
    let caps = |re: &regex::Regex| -> Vec<String> {
        re.captures_iter(text)
            .map(|c| c.get(1).map(|m| m.as_str()).unwrap_or_else(|| c.get(0).unwrap().as_str()).to_string())
            .collect()
    };
    Some(match ext(path) {
        "Rnw" | "rnw" | "Snw" | "snw" => caps(&RNW_HEADER),
        "Rmd" | "rmd" | "qmd" | "md" => MD_HEADER.find_iter(text).map(|m| m.as_str().to_string()).collect(),
        "Rtex" => caps(&RTEX_HEADER),
        "Rhtml" => caps(&RHTML_HEADER),
        "Rrst" => caps(&RRST_HEADER),
        _ => return None,
    })
}

/// A Quarto front matter that turns evaluation off (execute: eval: false).
fn quarto_eval_off(text: &str) -> bool {
    let t = text.trim_start_matches('\u{feff}');
    let Some(rest) = t.strip_prefix("---") else { return false };
    let Some(end) = rest.find("\n---") else { return false };
    let mut in_execute = false;
    for line in rest[..end].lines() {
        if line.trim_start() == line {
            in_execute = line.trim_end() == "execute:";
            continue;
        }
        if in_execute && line.trim() == "eval: false" {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VignetteRun {
    /// Building it runs no R code.
    Static,
    /// Whether it runs code depends on the environment (NOT_CRAN and the like).
    Gated,
    /// Building it runs R code.
    Dynamic,
}

impl VignetteRun {
    pub fn as_str(self) -> &'static str {
        match self {
            VignetteRun::Static => "static",
            VignetteRun::Gated => "gated",
            VignetteRun::Dynamic => "dynamic",
        }
    }
}

pub fn classify(path: &str, text: &str, has_orig_sibling: bool) -> VignetteRun {
    let headers = chunk_headers(path, text);
    let evals: Vec<String> = headers.iter().flatten().filter_map(|h| eval_value(h)).collect();
    let globals: Vec<String> = opts_chunk_args(text).iter().filter_map(|a| eval_value(a)).collect();
    if evals.iter().chain(&globals).any(|v| is_gate(v)) {
        return VignetteRun::Gated;
    }
    if has_orig_sibling {
        return VignetteRun::Static;
    }
    let Some(headers) = headers else {
        // asis and rsp: rsp runs R inside <% %>, asis never does.
        return if ext(path) == "rsp" && RSP_CODE.is_match(text) { VignetteRun::Dynamic } else { VignetteRun::Static };
    };
    if headers.is_empty() {
        return VignetteRun::Static;
    }
    let each_off = headers.iter().all(|h| eval_value(h).is_some_and(|v| is_off(&v)));
    // A later global opts_chunk$set(eval = TRUE) turns evaluation back on like a header does.
    let any_on = evals.iter().chain(&globals).any(|v| is_on(v));
    let sweave_off = SWEAVE_OPTS
        .captures_iter(text)
        .any(|c| eval_value(&c[1]).is_some_and(|v| is_off(&v)));
    let global_off = globals.iter().any(|v| is_off(v)) || sweave_off;
    if each_off || (global_off && !any_on) || quarto_eval_off(text) {
        VignetteRun::Static
    } else {
        VignetteRun::Dynamic
    }
}

pub struct VignetteFacts {
    pub sources: Vec<String>,
    /// 1 when any vignette runs code or may; None with no vignettes or an unreadable source.
    pub dynamic: Option<bool>,
    pub eval_gated: Option<i64>,
}

pub fn vignette_facts(root: &Path, files: &[String]) -> VignetteFacts {
    let sources = vignette_sources(root, files);
    let mut runs = Vec::new();
    for s in &sources {
        let Some(text) = super::read_lossy(root, s) else {
            return VignetteFacts { sources, dynamic: None, eval_gated: None };
        };
        let orig = format!("{s}.orig");
        runs.push(classify(s, &text, files.iter().any(|f| *f == orig)));
    }
    if runs.is_empty() {
        return VignetteFacts { sources, dynamic: None, eval_gated: None };
    }
    VignetteFacts {
        dynamic: Some(runs.iter().any(|r| *r != VignetteRun::Static)),
        eval_gated: Some(runs.iter().filter(|r| **r == VignetteRun::Gated).count() as i64),
        sources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_sit_directly_under_vignettes() {
        assert!(is_candidate("vignettes/intro.Rmd"));
        assert!(is_candidate("vignettes/guide.qmd"));
        assert!(is_candidate("vignettes/old.Rnw"));
        assert!(is_candidate("vignettes/paper.asis"));
        assert!(!is_candidate("vignettes/articles/deep.Rmd"), "subdirectories are not vignettes");
        assert!(!is_candidate("vignettes/refs.bib"));
        assert!(!is_candidate("inst/doc/intro.Rmd"));
    }

    #[test]
    fn only_sweave_needs_no_engine_line() {
        assert!(is_source("vignettes/a.Rnw", "no engine line"));
        assert!(!is_source("vignettes/README.md", "# notes"));
        assert!(is_source("vignettes/a.qmd", "%\\VignetteEngine{quarto::html}"));
        assert!(is_source("vignettes/a.asis", "%\\VignetteEngine{R.rsp::asis}"));
        let missing = Path::new("/nonexistent/rpkg-analyzer-vignette-root");
        let facts = vignette_facts(missing, &["vignettes/a.Rnw".to_string(), "vignettes/b.Rmd".to_string()]);
        assert_eq!(facts.sources, vec!["vignettes/a.Rnw".to_string()], "an unreadable Sweave source still counts");
        assert_eq!(facts.dynamic, None, "an unreadable source leaves vignette_dynamic unknown");
        assert_eq!(facts.eval_gated, None);
    }

    #[test]
    fn each_static_shape_is_static() {
        let rmd = |body: &str| format!("---\ntitle: x\n---\n%\\VignetteEngine{{knitr::rmarkdown}}\n{body}");
        assert_eq!(classify("vignettes/a.Rmd", &rmd("text only"), false), VignetteRun::Static);
        assert_eq!(classify("vignettes/a.Rmd", &rmd("```{r}\n1\n```"), true), VignetteRun::Static, ".Rmd.orig sibling");
        assert_eq!(classify("vignettes/a.Rmd", &rmd("```{r a, eval=FALSE}\n1\n```\n```{r b, eval = F}\n2\n```"), false), VignetteRun::Static);
        assert_eq!(
            classify("vignettes/a.Rmd", &rmd("```{r}\nknitr::opts_chunk$set(collapse = TRUE, eval = FALSE)\n```\n```{r}\n2\n```"), false),
            VignetteRun::Static
        );
        assert_eq!(classify("vignettes/a.Rnw", "<<eval=FALSE>>=\n1\n@\n", false), VignetteRun::Static);
        assert_eq!(classify("vignettes/a.Rnw", "\\SweaveOpts{echo=TRUE, eval=FALSE}\n<<a>>=\n1\n@\n", false), VignetteRun::Static);
        assert_eq!(
            classify("vignettes/a.qmd", "---\ntitle: x\nexecute:\n  echo: true\n  eval: false\n---\n```{r}\n1\n```\n", false),
            VignetteRun::Static
        );
        assert_eq!(classify("vignettes/a.asis", "%\\VignetteEngine{R.rsp::asis}", false), VignetteRun::Static);
        assert_eq!(
            classify("vignettes/a.Rmd", &rmd("```{r}\n# knitr::opts_chunk$set(eval = TRUE)\nknitr::opts_chunk$set(eval = FALSE)\n```\n```{r}\n2\n```"), false),
            VignetteRun::Static,
            "a commented-out call does not turn evaluation back on"
        );
        assert_eq!(classify("vignettes/a.Rtex", "% begin.rcode a, eval=FALSE\n1\n% end.rcode\n", false), VignetteRun::Static);
        assert_eq!(classify("vignettes/a.Rhtml", "<!--begin.rcode a, eval=FALSE\n1\nend.rcode-->\n", false), VignetteRun::Static);
        assert_eq!(classify("vignettes/a.Rrst", ".. {r a, eval=FALSE}\n1\n.. ..\n", false), VignetteRun::Static);
        assert_eq!(
            classify("vignettes/a.rsp", "<%@meta language=\"R-vignette\" content=\"\n%\\VignetteEngine{R.rsp::rsp}\n\"%>\nplain text\n", false),
            VignetteRun::Static,
            "an rsp directive runs no R code"
        );
    }

    #[test]
    fn a_header_that_turns_evaluation_back_on_is_dynamic() {
        let t = "```{r}\nopts_chunk$set(eval = FALSE)\n```\n```{r, eval = TRUE}\n1\n```\n";
        assert_eq!(classify("vignettes/a.Rmd", t, false), VignetteRun::Dynamic);
        assert_eq!(classify("vignettes/a.Rmd", "```{r setup}\n1\n```\n", false), VignetteRun::Dynamic);
        assert_eq!(classify("vignettes/a.Rnw", "<<a, echo=TRUE>>=\n1\n@\n", false), VignetteRun::Dynamic);
        let commented = "```{r}\n#knitr::opts_chunk$set(eval = FALSE)\nx <- 1 # opts_chunk$set(eval = FALSE)\n```\n```{r}\n1\n```\n";
        assert_eq!(classify("vignettes/a.Rmd", commented, false), VignetteRun::Dynamic, "a commented-out call sets nothing");
        let toggled = "```{r}\nknitr::opts_chunk$set(eval = FALSE)\n```\n```{r}\n1\n```\n```{r}\nknitr::opts_chunk$set(eval = TRUE)\n```\n```{r}\n2\n```\n";
        assert_eq!(classify("vignettes/a.Rmd", toggled, false), VignetteRun::Dynamic, "a later global call turns evaluation back on");
        assert_eq!(classify("vignettes/a.Rtex", "% begin.rcode a\n1\n% end.rcode\n", false), VignetteRun::Dynamic);
        assert_eq!(classify("vignettes/a.Rhtml", "<!--begin.rcode a\n1\nend.rcode-->\n", false), VignetteRun::Dynamic);
        assert_eq!(classify("vignettes/a.Rrst", ".. {r a}\n1\n.. ..\n", false), VignetteRun::Dynamic);
        assert_eq!(classify("vignettes/a.rsp", "%\\VignetteEngine{R.rsp::rsp}\n<% x <- 1 %>\n", false), VignetteRun::Dynamic);
        assert_eq!(classify("vignettes/a.rsp", "%\\VignetteEngine{R.rsp::rsp}\nValue: <%=pi%>\n", false), VignetteRun::Dynamic);
    }

    #[test]
    fn evaluation_that_depends_on_the_environment_is_gated() {
        let t = "```{r}\nknitr::opts_chunk$set(eval = identical(Sys.getenv(\"NOT_CRAN\"), \"true\"))\n```\n";
        assert_eq!(classify("vignettes/a.Rmd", t, false), VignetteRun::Gated);
        assert_eq!(classify("vignettes/a.Rmd", "```{r, eval = NOT_CRAN}\n1\n```\n", false), VignetteRun::Gated);
        assert_eq!(classify("vignettes/a.Rmd", "```{r, eval=requireNamespace(\"sf\")}\n1\n```\n", false), VignetteRun::Gated);
    }
}
