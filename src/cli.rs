// Command-line parsing. The package directory comes first so callers that pass
// it as the only positional argument keep working; the input kind follows it.

/// What the tree handed to the analyzer is: a built release, or a git branch
/// that R CMD build has not filtered yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    Release,
    Git,
}

impl InputKind {
    pub fn as_str(self) -> &'static str {
        match self {
            InputKind::Release => "release",
            InputKind::Git => "git",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Version,
    Datasets(String),
    Sexp(String),
    Kinds(String),
    Analyze { dir: String, kind: InputKind },
}

pub const USAGE: &str = "usage: rpkg-analyzer <package_dir> --input-kind release|git";

fn kind_after(args: &[String]) -> Option<InputKind> {
    match args {
        [flag, value] if flag == "--input-kind" => match value.as_str() {
            "release" => Some(InputKind::Release),
            "git" => Some(InputKind::Git),
            _ => None,
        },
        _ => None,
    }
}

/// Reads the arguments that follow the program name.
pub fn parse_args(args: &[String]) -> Result<Mode, String> {
    let file_arg = |usage: &str| args.get(1).cloned().ok_or_else(|| usage.to_string());
    match args.first().map(String::as_str) {
        None => Err(USAGE.to_string()),
        Some("--version") | Some("-V") => Ok(Mode::Version),
        Some("--datasets") => file_arg("usage: rpkg-analyzer --datasets <package_dir>").map(Mode::Datasets),
        Some("--sexp") => file_arg("usage: rpkg-analyzer --sexp <file>").map(Mode::Sexp),
        Some("--kinds") => file_arg("usage: rpkg-analyzer --kinds <file>").map(Mode::Kinds),
        Some(dir) => {
            let kind = kind_after(&args[1..]).ok_or_else(|| USAGE.to_string())?;
            Ok(Mode::Analyze { dir: dir.to_string(), kind })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_input_kind_follows_the_directory() {
        assert_eq!(
            parse_args(&args(&["pkg", "--input-kind", "release"])),
            Ok(Mode::Analyze { dir: "pkg".into(), kind: InputKind::Release })
        );
        assert_eq!(
            parse_args(&args(&["pkg", "--input-kind", "git"])),
            Ok(Mode::Analyze { dir: "pkg".into(), kind: InputKind::Git })
        );
    }

    #[test]
    fn a_missing_or_unknown_input_kind_is_refused() {
        assert_eq!(parse_args(&args(&["pkg"])), Err(USAGE.to_string()));
        assert_eq!(parse_args(&args(&["pkg", "--input-kind"])), Err(USAGE.to_string()));
        assert_eq!(parse_args(&args(&["pkg", "--input-kind", "tarball"])), Err(USAGE.to_string()));
        assert_eq!(parse_args(&args(&["pkg", "--input-kind", "git", "extra"])), Err(USAGE.to_string()));
        assert_eq!(parse_args(&args(&[])), Err(USAGE.to_string()));
    }

    #[test]
    fn the_other_modes_need_no_input_kind() {
        assert_eq!(parse_args(&args(&["--version"])), Ok(Mode::Version));
        assert_eq!(parse_args(&args(&["-V"])), Ok(Mode::Version));
        assert_eq!(parse_args(&args(&["--datasets", "pkg"])), Ok(Mode::Datasets("pkg".into())));
        assert_eq!(parse_args(&args(&["--sexp", "a.R"])), Ok(Mode::Sexp("a.R".into())));
        assert_eq!(parse_args(&args(&["--kinds", "a.R"])), Ok(Mode::Kinds("a.R".into())));
    }
}
