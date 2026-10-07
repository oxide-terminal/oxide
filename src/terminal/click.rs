//! What's under the pointer when you cmd-click terminal text: a URL, or a
//! `path:line:col` that resolves to a real file. Pure functions so the
//! token parsing and resolution order are unit-testable; the pane supplies
//! the grid row and the existence check.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickTarget {
    Url(String),
    Path {
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// A token's span within its row, for hover underlining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub start: usize,
    /// Exclusive.
    pub end: usize,
    pub text: String,
}

fn is_break(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}'
        )
}

/// The whitespace/bracket-delimited token covering column `ix`, with
/// trailing sentence punctuation trimmed (`foo.rs:12,` in a traceback).
pub fn token_at(chars: &[char], ix: usize) -> Option<Token> {
    if chars.is_empty() {
        return None;
    }
    let ix = ix.min(chars.len() - 1);
    if is_break(chars[ix]) {
        return None;
    }
    let start = (0..=ix)
        .rev()
        .find(|&i| is_break(chars[i]))
        .map_or(0, |i| i + 1);
    let mut end = (ix..chars.len())
        .find(|&i| is_break(chars[i]))
        .unwrap_or(chars.len());
    while end > start && matches!(chars[end - 1], ',' | '.' | ';' | ':' | '!' | '?') {
        end -= 1;
    }
    if end <= start {
        return None;
    }
    Some(Token {
        start,
        end,
        text: chars[start..end].iter().collect(),
    })
}

/// Split `src/main.rs:42:8` into the path text and the line/column suffix.
/// Only trailing all-digit components count, so `foo:bar` stays whole and a
/// Windows drive letter is never mistaken for a line number.
pub fn split_line_col(token: &str) -> (&str, Option<u32>, Option<u32>) {
    let mut path = token;
    let mut numbers: Vec<u32> = Vec::new();
    while numbers.len() < 2 {
        let Some((head, tail)) = path.rsplit_once(':') else {
            break;
        };
        if head.is_empty() || tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        let Ok(n) = tail.parse::<u32>() else { break };
        numbers.push(n);
        path = head;
    }
    // Parsed right to left: the last pushed is the line.
    match numbers.as_slice() {
        [] => (token, None, None),
        [line] => (path, Some(*line), None),
        [col, line] => (path, Some(*line), Some(*col)),
        _ => unreachable!(),
    }
}

/// Where a relative path may be anchored, in precedence order.
pub struct Roots<'a> {
    pub cwd: Option<&'a Path>,
    pub git_root: Option<&'a Path>,
    pub tree_root: Option<&'a Path>,
    pub home: Option<&'a Path>,
}

/// Resolve `text` to an existing path. Absolute and `~` paths are taken as
/// is; relative ones are tried against the pane's cwd, then the repo root
/// (tools print repo-relative paths from subdirectories), then the tree
/// root. No basename search: opening the wrong `mod.rs` is worse than
/// opening nothing.
pub fn resolve_path(
    text: &str,
    roots: &Roots,
    exists: &mut dyn FnMut(&Path) -> bool,
) -> Option<PathBuf> {
    if text.is_empty() || text.contains("://") {
        return None;
    }
    if let Some(rest) = text.strip_prefix("~/") {
        let candidate = roots.home?.join(rest);
        return exists(&candidate).then_some(candidate);
    }
    if text == "~" {
        return None;
    }
    let path = Path::new(text);
    if path.is_absolute() {
        return exists(path).then(|| path.to_path_buf());
    }
    for base in [roots.cwd, roots.git_root, roots.tree_root]
        .into_iter()
        .flatten()
    {
        let candidate = base.join(path);
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Classify a token: URLs win, then `path[:line[:col]]` that resolves.
pub fn classify(
    token: &str,
    roots: &Roots,
    exists: &mut dyn FnMut(&Path) -> bool,
) -> Option<ClickTarget> {
    if token.starts_with("http://") || token.starts_with("https://") || token.starts_with("file://")
    {
        return Some(ClickTarget::Url(token.to_string()));
    }
    let (text, line, col) = split_line_col(token);
    // A bare word is rarely a path someone wants opened; require a
    // separator, an extension, or a line number to even try.
    let plausible = text.contains('/') || text.contains('.') || line.is_some();
    if !plausible {
        return None;
    }
    let path = resolve_path(text, roots, exists)?;
    Some(ClickTarget::Path { path, line, col })
}

/// Whether a path is safe to type at a prompt. Quoting protects the parsed
/// line, but the line editor acts on control bytes as they arrive: a name
/// holding `^U` and `\r` wipes the line and runs whatever follows.
pub fn typeable(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// Single-quote for any Bourne-family shell, so a space in a name stays one
/// argument. A leading `-` gets `./` so the shell can't read it as a flag.
/// Check `typeable` first when the text will be typed rather than exec'd.
pub fn shell_quote(text: &str) -> String {
    let text = if text.starts_with('-') {
        format!("./{text}")
    } else {
        text.to_string()
    };
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn token_extraction_trims_punctuation_and_respects_breaks() {
        let row = chars("error --> src/main.rs:12:9, then (x/y.rs) end");
        let t = token_at(&row, 12).unwrap();
        assert_eq!(t.text, "src/main.rs:12:9");
        assert_eq!((t.start, t.end), (10, 26));
        assert_eq!(token_at(&row, 34).unwrap().text, "x/y.rs");
        assert!(token_at(&row, 5).is_none(), "whitespace is not a token");
        assert_eq!(
            token_at(&chars("at foo.rs:88,"), 5).unwrap().text,
            "foo.rs:88"
        );
        assert!(token_at(&[], 0).is_none());
    }

    #[test]
    fn line_and_column_split() {
        assert_eq!(
            split_line_col("src/main.rs:42:8"),
            ("src/main.rs", Some(42), Some(8))
        );
        assert_eq!(
            split_line_col("src/main.rs:42"),
            ("src/main.rs", Some(42), None)
        );
        assert_eq!(split_line_col("src/main.rs"), ("src/main.rs", None, None));
        assert_eq!(split_line_col("foo:bar"), ("foo:bar", None, None));
        assert_eq!(split_line_col("C:\\foo"), ("C:\\foo", None, None));
        assert_eq!(split_line_col("a:1:2:3"), ("a:1", Some(2), Some(3)));
        assert_eq!(split_line_col(":42"), (":42", None, None));
    }

    fn roots<'a>(cwd: &'a Path, git: &'a Path, tree: &'a Path, home: &'a Path) -> Roots<'a> {
        Roots {
            cwd: Some(cwd),
            git_root: Some(git),
            tree_root: Some(tree),
            home: Some(home),
        }
    }

    #[test]
    fn resolution_precedence_cwd_then_git_then_tree() {
        let (cwd, git, tree, home) = (
            Path::new("/w/sub"),
            Path::new("/w"),
            Path::new("/t"),
            Path::new("/home/me"),
        );
        let r = roots(cwd, git, tree, home);
        let existing: HashSet<PathBuf> = [
            "/w/sub/a.rs",
            "/w/a.rs",
            "/w/b.rs",
            "/t/c.rs",
            "/home/me/d.rs",
            "/abs.rs",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let mut exists = |p: &Path| existing.contains(p);
        assert_eq!(
            resolve_path("a.rs", &r, &mut exists),
            Some(PathBuf::from("/w/sub/a.rs")),
            "cwd wins"
        );
        assert_eq!(
            resolve_path("b.rs", &r, &mut exists),
            Some(PathBuf::from("/w/b.rs")),
            "then git root"
        );
        assert_eq!(
            resolve_path("c.rs", &r, &mut exists),
            Some(PathBuf::from("/t/c.rs")),
            "then tree root"
        );
        assert_eq!(
            resolve_path("~/d.rs", &r, &mut exists),
            Some(PathBuf::from("/home/me/d.rs"))
        );
        assert_eq!(
            resolve_path("/abs.rs", &r, &mut exists),
            Some(PathBuf::from("/abs.rs"))
        );
        assert_eq!(resolve_path("nope.rs", &r, &mut exists), None);
        assert_eq!(resolve_path("/nope.rs", &r, &mut exists), None);
    }

    #[test]
    fn classify_prefers_urls_and_rejects_non_paths() {
        let r = Roots {
            cwd: Some(Path::new("/w")),
            git_root: None,
            tree_root: None,
            home: None,
        };
        let mut exists = |p: &Path| p == Path::new("/w/src/main.rs");
        assert_eq!(
            classify("https://x.y/z:12", &r, &mut exists),
            Some(ClickTarget::Url("https://x.y/z:12".into()))
        );
        assert_eq!(
            classify("src/main.rs:42:8", &r, &mut exists),
            Some(ClickTarget::Path {
                path: PathBuf::from("/w/src/main.rs"),
                line: Some(42),
                col: Some(8)
            })
        );
        assert_eq!(classify("foo:bar", &r, &mut exists), None);
        assert_eq!(
            classify("missing.rs:3", &r, &mut exists),
            None,
            "a line number on a non-file is nothing"
        );
        assert_eq!(
            classify("main", &r, &mut exists),
            None,
            "bare words are not tried"
        );
    }

    #[test]
    fn quoting_handles_spaces_quotes_newlines_and_dashes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("x\ny"), "'x\ny'");
        assert_eq!(shell_quote("-rf"), "'./-rf'");
        assert_eq!(shell_quote("plain"), "'plain'");
        assert!(typeable("a b/it's.txt"));
        assert!(!typeable("a\x15curl evil|sh\r"), "C0 controls");
        assert!(!typeable("Icon\r"));
        assert!(!typeable("x\x7fy"), "DEL");
        assert!(!typeable("x\u{85}y"), "C1 controls");
    }
}
