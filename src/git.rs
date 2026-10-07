//! Blocking git queries — every function here runs a `git` subprocess and
//! belongs on the background pool. All of them are guarded by
//! `git_usable()`, which keeps Apple's "Install Developer Tools?" dialog
//! from ever being triggered by a status poll.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Branch and dirtiness for the status bar.
#[derive(Default, Clone, PartialEq, Debug)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
}

/// Whether invoking `git` is safe. On a Mac without the Command Line Tools,
/// /usr/bin/git is a shim that pops Apple's "Install Developer Tools?" GUI —
/// our 3-second status poll must never be the thing that triggers it.
pub fn git_usable() -> bool {
    static USABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *USABLE.get_or_init(|| {
        let Ok(out) = Command::new("/bin/sh")
            .args(["-c", "command -v git"])
            .output()
        else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if cfg!(target_os = "macos") && path == "/usr/bin/git" {
            return Command::new("/usr/bin/xcode-select")
                .arg("-p")
                .output()
                .is_ok_and(|o| o.status.success());
        }
        true
    })
}

/// `core.fsmonitor` names a command that `git status` runs, and a repo's own
/// `.git/config` can set it. These polls run in whatever directory the user
/// lands in — an unpacked archive included — so never honour it.
const NO_FSMONITOR: [&str; 2] = ["-c", "core.fsmonitor=false"];

fn git_text(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(NO_FSMONITOR)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn read_git_status(cwd: &Path) -> GitStatus {
    if !git_usable() {
        return GitStatus::default();
    }
    let git = |args: &[&str]| git_text(cwd, args);
    let Some(branch) = git(&["symbolic-ref", "--short", "HEAD"])
        .or_else(|| git(&["rev-parse", "--short", "HEAD"]))
        .filter(|b| !b.is_empty())
    else {
        return GitStatus::default();
    };
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let (ahead, behind) = git(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .and_then(|s| {
            let (a, b) = s.split_once('\t')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((0, 0));
    GitStatus {
        branch: Some(branch),
        dirty,
        ahead,
        behind,
    }
}

/// The repository root containing `cwd`, or `None` outside a repo.
pub fn toplevel(cwd: &Path) -> Option<PathBuf> {
    if !git_usable() {
        return None;
    }
    let root = git_text(cwd, &["rev-parse", "--show-toplevel"])?;
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// Per-file state for tree decorations, in rollup priority order: a
/// directory takes the most urgent state found beneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitFileStatus {
    Untracked,
    Added,
    Renamed,
    Modified,
    Deleted,
    Conflicted,
}

/// More changed files than the decorations can usefully show. A monorepo
/// with tens of thousands of dirty paths gets no colours, not a hang.
pub const MAX_STATUS_ENTRIES: usize = 20_000;

#[derive(Debug, PartialEq)]
pub enum StatusError {
    NotARepo,
    TooLarge,
}

/// Every changed or untracked path under `root`, keyed by absolute path,
/// with directories rolled up from their contents.
pub fn file_statuses(root: &Path) -> Result<HashMap<PathBuf, GitFileStatus>, StatusError> {
    if !git_usable() {
        return Err(StatusError::NotARepo);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(NO_FSMONITOR)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=no",
        ])
        .output()
        .map_err(|_| StatusError::NotARepo)?;
    if !out.status.success() {
        return Err(StatusError::NotARepo);
    }
    let files = parse_porcelain_z(&out.stdout, root)?;
    Ok(rollup(files, root))
}

/// Parse `git status --porcelain=v1 -z`. Records are `XY path\0`, with a
/// second `orig\0` record following renames and copies. NUL separation is
/// what makes filenames with spaces and newlines safe.
pub fn parse_porcelain_z(
    bytes: &[u8],
    root: &Path,
) -> Result<HashMap<PathBuf, GitFileStatus>, StatusError> {
    use std::os::unix::ffi::OsStrExt;
    let mut map = HashMap::new();
    let mut records = bytes.split(|&b| b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let (x, y) = (record[0] as char, record[1] as char);
        let rel = std::ffi::OsStr::from_bytes(&record[3..]);
        let status = classify(x, y);
        if matches!(x, 'R' | 'C') {
            // The original path follows as its own record; it no longer
            // exists at that name, so only the new path is decorated.
            let _original = records.next();
        }
        if let Some(status) = status {
            map.insert(root.join(rel), status);
            if map.len() > MAX_STATUS_ENTRIES {
                return Err(StatusError::TooLarge);
            }
        }
    }
    Ok(map)
}

fn classify(x: char, y: char) -> Option<GitFileStatus> {
    use GitFileStatus::*;
    match (x, y) {
        ('?', '?') => Some(Untracked),
        ('!', '!') => None,
        ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') => Some(Conflicted),
        ('D', _) | (_, 'D') => Some(Deleted),
        ('R', _) | ('C', _) => Some(Renamed),
        ('A', _) => Some(Added),
        ('M', _) | (_, 'M') | ('T', _) | (_, 'T') => Some(Modified),
        _ => None,
    }
}

/// Mark every ancestor of a changed path (up to `root`) with the most urgent
/// state beneath it, so a collapsed directory still shows something changed.
pub fn rollup(
    mut map: HashMap<PathBuf, GitFileStatus>,
    root: &Path,
) -> HashMap<PathBuf, GitFileStatus> {
    let files: Vec<(PathBuf, GitFileStatus)> = map.iter().map(|(p, s)| (p.clone(), *s)).collect();
    for (path, status) in files {
        let mut dir = path.parent();
        while let Some(d) = dir {
            if !d.starts_with(root) || d == root {
                break;
            }
            let entry = map.entry(d.to_path_buf()).or_insert(status);
            if *entry < status {
                *entry = status;
            }
            dir = d.parent();
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &[u8]) -> HashMap<PathBuf, GitFileStatus> {
        parse_porcelain_z(s, Path::new("/r")).unwrap()
    }

    #[test]
    fn parses_common_statuses() {
        let m = parse(
            b" M src/a.rs\0A  src/new.rs\0?? notes.txt\0 D gone.rs\0UU merge.rs\0MM both.rs\0",
        );
        assert_eq!(m[Path::new("/r/src/a.rs")], GitFileStatus::Modified);
        assert_eq!(m[Path::new("/r/src/new.rs")], GitFileStatus::Added);
        assert_eq!(m[Path::new("/r/notes.txt")], GitFileStatus::Untracked);
        assert_eq!(m[Path::new("/r/gone.rs")], GitFileStatus::Deleted);
        assert_eq!(m[Path::new("/r/merge.rs")], GitFileStatus::Conflicted);
        assert_eq!(
            m[Path::new("/r/both.rs")],
            GitFileStatus::Modified,
            "staged and modified"
        );
    }

    #[test]
    fn renames_consume_the_original_record() {
        let m = parse(b"R  new.rs\0old.rs\0 M other.rs\0");
        assert_eq!(m[Path::new("/r/new.rs")], GitFileStatus::Renamed);
        assert!(!m.contains_key(Path::new("/r/old.rs")));
        assert_eq!(m[Path::new("/r/other.rs")], GitFileStatus::Modified);
    }

    #[test]
    fn nul_separation_keeps_odd_filenames_intact() {
        let m = parse(b"?? has space.txt\0?? line\nbreak.txt\0");
        assert!(m.contains_key(Path::new("/r/has space.txt")));
        assert!(m.contains_key(Path::new("/r/line\nbreak.txt")));
    }

    #[test]
    fn ignored_entries_are_skipped_and_size_is_capped() {
        assert!(parse(b"!! target/\0").is_empty());
        let mut big = Vec::new();
        for i in 0..=MAX_STATUS_ENTRIES {
            big.extend_from_slice(format!("?? f{i}\0").as_bytes());
        }
        assert_eq!(
            parse_porcelain_z(&big, Path::new("/r")),
            Err(StatusError::TooLarge)
        );
    }

    #[test]
    fn rollup_marks_every_ancestor_with_the_worst_state() {
        let mut m = HashMap::new();
        m.insert(
            PathBuf::from("/r/src/tree/deep/x.rs"),
            GitFileStatus::Untracked,
        );
        m.insert(PathBuf::from("/r/src/tree/y.rs"), GitFileStatus::Modified);
        m.insert(PathBuf::from("/r/src/z.rs"), GitFileStatus::Conflicted);
        let m = rollup(m, Path::new("/r"));
        assert_eq!(m[Path::new("/r/src/tree/deep")], GitFileStatus::Untracked);
        assert_eq!(
            m[Path::new("/r/src/tree")],
            GitFileStatus::Modified,
            "modified outranks untracked"
        );
        assert_eq!(m[Path::new("/r/src")], GitFileStatus::Conflicted);
        assert!(
            !m.contains_key(Path::new("/r")),
            "the root itself isn't decorated"
        );
        assert!(!m.contains_key(Path::new("/")));
    }
}
