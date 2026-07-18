//! Git plumbing: repo discovery, `status --porcelain -z` parsing, and the
//! stage / unstage / commit operations, all via the `git` CLI (no libgit2).
//! Parsing is pure and unit-tested; commands run with the repo toplevel as cwd
//! so the repo-relative paths porcelain reports resolve even when the pane's
//! cwd is a subdirectory.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One file in the staged or unstaged list.
#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    /// Repo-relative path (the new path, for renames), `/`-separated as git reports it.
    pub path: String,
    /// Rename/copy source, when there is one — unstaging a rename must reset both.
    pub orig: Option<String>,
    /// The VS Code-style status letter to display: M, A, D, R, C, U (untracked),
    /// or `!` for merge conflicts.
    pub letter: char,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub branch: String,
    pub staged: Vec<FileEntry>,
    pub unstaged: Vec<FileEntry>,
}

pub struct Git {
    root: PathBuf,
}

impl Git {
    /// Locate the repository containing `dir`; Err with git's message when there
    /// is none (or git itself is missing).
    pub fn discover(dir: &Path) -> Result<Git, String> {
        let out = run_in(dir, &["rev-parse", "--show-toplevel"])?;
        let root = out.trim();
        if root.is_empty() {
            return Err("not inside a git repository".to_string());
        }
        Ok(Git { root: PathBuf::from(root) })
    }

    pub fn status(&self) -> Result<Status, String> {
        let out = run_in(
            &self.root,
            &["status", "--porcelain", "-z", "--branch", "--untracked-files=all"],
        )?;
        Ok(parse_status(&out))
    }

    /// Stage one entry: `add -A` records modifications, additions, and deletions alike.
    pub fn stage(&self, entry: &FileEntry) -> Result<(), String> {
        run_in(&self.root, &["add", "-A", "--", &entry.path]).map(drop)
    }

    pub fn stage_all(&self) -> Result<(), String> {
        run_in(&self.root, &["add", "-A"]).map(drop)
    }

    /// Unstage one entry. `reset` needs a HEAD to reset against; on an unborn
    /// branch (no commits yet) fall back to dropping the path from the index.
    pub fn unstage(&self, entry: &FileEntry) -> Result<(), String> {
        let mut args = vec!["reset", "-q", "--", entry.path.as_str()];
        if let Some(orig) = &entry.orig {
            args.push(orig);
        }
        if run_in(&self.root, &args).is_ok() {
            return Ok(());
        }
        run_in(&self.root, &["rm", "--cached", "-r", "-q", "--", &entry.path]).map(drop)
    }

    pub fn unstage_all(&self) -> Result<(), String> {
        if run_in(&self.root, &["reset", "-q"]).is_ok() {
            return Ok(());
        }
        run_in(&self.root, &["rm", "--cached", "-r", "-q", "--", "."]).map(drop)
    }

    /// Commit the staged changes; returns git's summary line ("[branch abc1234] …").
    pub fn commit(&self, message: &str) -> Result<String, String> {
        let out = run_in(&self.root, &["commit", "-m", message])?;
        Ok(out.lines().next().unwrap_or("committed").to_string())
    }

    /// Throw away a file's working-tree changes: untracked files are deleted,
    /// tracked ones restored from HEAD (the caller confirms first).
    pub fn discard(&self, entry: &FileEntry) -> Result<(), String> {
        if entry.letter == 'U' {
            return run_in(&self.root, &["clean", "-fd", "--", &entry.path]).map(drop);
        }
        run_in(&self.root, &["checkout", "--", &entry.path]).map(drop)
    }

    /// The diff a commit-message suggestion should describe: the staged diff
    /// when something is staged (that is what would be committed), else the
    /// working-tree diff. Untracked files only appear as names, so they ride
    /// along in the returned path list either way.
    pub fn diff_for_message(&self) -> Result<(String, Vec<String>), String> {
        let staged = run_in(&self.root, &["diff", "--cached", "--stat", "--patch"])?;
        let (diff, names_args): (String, &[&str]) = if staged.trim().is_empty() {
            let unstaged = run_in(&self.root, &["diff", "--stat", "--patch"])?;
            (unstaged, &["diff", "--name-only"])
        } else {
            (staged, &["diff", "--cached", "--name-only"])
        };
        let mut files: Vec<String> = run_in(&self.root, names_args)?
            .lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty())
            .collect();
        if files.is_empty() {
            // Nothing tracked changed: describe the untracked files instead.
            files = run_in(&self.root, &["ls-files", "--others", "--exclude-standard"])?
                .lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect();
        }
        Ok((diff, files))
    }

    // ---- Drawer queries (display-only lists, VS Code Git-Graph style) ----

    pub fn graph(&self, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        lines(run_in(&self.root, &["log", "--graph", "--oneline", "--decorate=short", &n])?)
    }

    pub fn commits(&self, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        lines(run_in(
            &self.root,
            &["log", "--oneline", "--decorate=short", "--date=short", &n],
        )?)
    }

    pub fn file_history(&self, path: &str, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        lines(run_in(&self.root, &["log", "--oneline", "--follow", &n, "--", path])?)
    }

    /// Local + remote branches, the current one first and starred.
    pub fn branches(&self) -> Result<Vec<String>, String> {
        lines(run_in(
            &self.root,
            &["branch", "-a", "--sort=-committerdate", "--format=%(HEAD) %(refname:short)"],
        )?)
    }

    pub fn remotes(&self) -> Result<Vec<String>, String> {
        let out = run_in(&self.root, &["remote", "-v"])?;
        // `remote -v` lists fetch and push separately; one line per remote reads better.
        let mut seen = Vec::new();
        for line in out.lines() {
            if let Some(rest) = line.strip_suffix(" (fetch)") {
                seen.push(rest.replace('\t', "  "));
            }
        }
        Ok(seen)
    }

    pub fn stashes(&self) -> Result<Vec<String>, String> {
        lines(run_in(&self.root, &["stash", "list"])?)
    }

    pub fn tags(&self) -> Result<Vec<String>, String> {
        lines(run_in(&self.root, &["tag", "--sort=-creatordate"])?)
    }
}

fn lines(out: String) -> Result<Vec<String>, String> {
    Ok(out.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
}

fn run_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-c")
        .arg("color.ui=false")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("git failed")
        .trim()
        .to_string())
}

/// Parse `git status --porcelain -z --branch` output. Entries are NUL-separated
/// `XY path`; a rename/copy is followed by a second NUL-separated field holding
/// the source path. X is the index (staged) state, Y the worktree state.
pub fn parse_status(raw: &str) -> Status {
    let mut status = Status::default();
    let mut parts = raw.split('\0');
    while let Some(entry) = parts.next() {
        if entry.is_empty() {
            continue;
        }
        if let Some(header) = entry.strip_prefix("## ") {
            status.branch = parse_branch(header);
            continue;
        }
        let Some((xy, path)) = split_entry(entry) else {
            continue;
        };
        let (x, y) = xy;
        let orig = if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            parts.next().filter(|s| !s.is_empty()).map(str::to_string)
        } else {
            None
        };
        let path = path.to_string();
        if x == '?' && y == '?' {
            status.unstaged.push(FileEntry { path, orig: None, letter: 'U' });
            continue;
        }
        if x == '!' {
            continue; // ignored file
        }
        if is_conflict(x, y) {
            status.unstaged.push(FileEntry { path, orig, letter: '!' });
            continue;
        }
        if x != ' ' {
            status.staged.push(FileEntry {
                path: path.clone(),
                orig: orig.clone(),
                letter: display_letter(x),
            });
        }
        if y != ' ' {
            status.unstaged.push(FileEntry { path, orig, letter: display_letter(y) });
        }
    }
    status
}

/// `("XY", path)` from one porcelain entry; the XY columns are always ASCII.
fn split_entry(entry: &str) -> Option<((char, char), &str)> {
    let bytes = entry.as_bytes();
    if bytes.len() < 4 || bytes[2] != b' ' {
        return None;
    }
    Some(((bytes[0] as char, bytes[1] as char), &entry[3..]))
}

fn is_conflict(x: char, y: char) -> bool {
    matches!(
        (x, y),
        ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
    )
}

/// Type changes (T) read as plain modifications, matching VS Code.
fn display_letter(c: char) -> char {
    if c == 'T' { 'M' } else { c }
}

/// Branch from the `## …` header: `main...origin/main [ahead 1]`, bare `main`,
/// `No commits yet on main`, or `HEAD (no branch)` when detached.
fn parse_branch(header: &str) -> String {
    let head = header.split("...").next().unwrap_or(header);
    head.strip_prefix("No commits yet on ")
        .unwrap_or(head)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, letter: char, orig: Option<&str>) -> FileEntry {
        FileEntry { path: path.to_string(), orig: orig.map(str::to_string), letter }
    }

    #[test]
    fn parses_branch_variants() {
        assert_eq!(parse_status("## main...origin/main [ahead 1]\0").branch, "main");
        assert_eq!(parse_status("## git-panel\0").branch, "git-panel");
        assert_eq!(parse_status("## No commits yet on trunk\0").branch, "trunk");
        assert_eq!(parse_status("## HEAD (no branch)\0").branch, "HEAD (no branch)");
    }

    #[test]
    fn splits_staged_and_unstaged_sides() {
        let s = parse_status("## main\0MM src/app.rs\0A  new.rs\0 D gone.rs\0");
        assert_eq!(
            s.staged,
            vec![entry("src/app.rs", 'M', None), entry("new.rs", 'A', None)]
        );
        assert_eq!(
            s.unstaged,
            vec![entry("src/app.rs", 'M', None), entry("gone.rs", 'D', None)]
        );
    }

    #[test]
    fn untracked_shows_as_u() {
        let s = parse_status("?? docs/notes.md\0");
        assert_eq!(s.staged, vec![]);
        assert_eq!(s.unstaged, vec![entry("docs/notes.md", 'U', None)]);
    }

    #[test]
    fn rename_consumes_the_source_field() {
        let s = parse_status("R  new_name.rs\0old_name.rs\0?? after.txt\0");
        assert_eq!(s.staged, vec![entry("new_name.rs", 'R', Some("old_name.rs"))]);
        assert_eq!(s.unstaged, vec![entry("after.txt", 'U', None)]);
    }

    #[test]
    fn type_change_reads_as_modified() {
        let s = parse_status("T  link.sh\0 T other.sh\0");
        assert_eq!(s.staged, vec![entry("link.sh", 'M', None)]);
        assert_eq!(s.unstaged, vec![entry("other.sh", 'M', None)]);
    }

    #[test]
    fn conflicts_land_unstaged_with_bang() {
        let s = parse_status("UU merge.rs\0AA both.rs\0");
        assert_eq!(s.staged, vec![]);
        assert_eq!(
            s.unstaged,
            vec![entry("merge.rs", '!', None), entry("both.rs", '!', None)]
        );
    }

    #[test]
    fn garbage_and_ignored_entries_are_skipped() {
        let s = parse_status("!! target\0x\0\0 M ok.rs\0");
        assert_eq!(s.staged, vec![]);
        assert_eq!(s.unstaged, vec![entry("ok.rs", 'M', None)]);
    }

    #[test]
    fn paths_with_spaces_survive() {
        let s = parse_status("M  my docs/read me.md\0");
        assert_eq!(s.staged, vec![entry("my docs/read me.md", 'M', None)]);
    }
}
