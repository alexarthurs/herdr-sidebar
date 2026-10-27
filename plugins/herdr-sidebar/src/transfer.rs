//! Moving and copying files between folders: the Explorer's drag and drop,
//! its Cut / Copy / Paste, and files dropped onto the pane from a file
//! manager. UI-free so it is unit-testable; `explorer_app.rs` owns the
//! gestures, the confirmation and the worker thread.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Move,
    Copy,
}

impl Op {
    pub fn verb(self) -> &'static str {
        match self {
            Op::Move => "move",
            Op::Copy => "copy",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Op::Move => Op::Copy,
            Op::Copy => Op::Move,
        }
    }
}

/// Why `sources` cannot land in `dest` — `None` when the drop is valid.
/// A folder can never go into itself or a descendant (a copy would recurse
/// forever), and moving something into the folder it already lives in is a
/// no-op worth saying so rather than silently "succeeding".
pub fn drop_problem(sources: &[PathBuf], dest: &Path, op: Op) -> Option<String> {
    if sources.is_empty() {
        return Some("nothing to drop".into());
    }
    for source in sources {
        if dest.starts_with(source) {
            return Some(format!("can't drop '{}' into itself", display_name(source)));
        }
        if op == Op::Move && source.parent() == Some(dest) {
            return Some(format!("'{}' is already there", display_name(source)));
        }
    }
    None
}

/// What a finished transfer produced: every new path, plus the first failure
/// (the transfer stops there — later sources are left untouched).
#[derive(Debug)]
pub struct Outcome {
    pub op: Op,
    pub landed: Vec<PathBuf>,
    pub failed: Option<(PathBuf, io::Error)>,
}

/// Carry each source into `dest`. A move never overwrites: a name already
/// taken in `dest` is an error. A copy instead picks a free "… copy" name,
/// so Copy then Paste into the same folder duplicates like VS Code.
pub fn transfer(sources: &[PathBuf], dest: &Path, op: Op) -> Outcome {
    let mut landed = Vec::new();
    for source in sources {
        let result = match op {
            Op::Move => move_one(source, dest),
            Op::Copy => copy_one(source, dest),
        };
        match result {
            Ok(target) => landed.push(target),
            Err(error) => {
                return Outcome {
                    op,
                    landed,
                    failed: Some((source.clone(), error)),
                };
            }
        }
    }
    Outcome {
        op,
        landed,
        failed: None,
    }
}

fn move_one(source: &Path, dest: &Path) -> io::Result<PathBuf> {
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let target = dest.join(name);
    if target.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists in {}",
                name.to_string_lossy(),
                display_name(dest)
            ),
        ));
    }
    match std::fs::rename(source, &target) {
        Ok(()) => Ok(target),
        // Another filesystem: copy, then remove the original only once the
        // copy is complete. A half-written copy is removed instead.
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            if let Err(error) = copy_tree(source, &target) {
                let _ = remove_any(&target);
                return Err(error);
            }
            remove_any(source)?;
            Ok(target)
        }
        Err(error) => Err(error),
    }
}

fn copy_one(source: &Path, dest: &Path) -> io::Result<PathBuf> {
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let is_dir = source.symlink_metadata()?.is_dir();
    let target = free_copy_target(dest, &name.to_string_lossy(), is_dir);
    if let Err(error) = copy_tree(source, &target) {
        let _ = remove_any(&target);
        return Err(error);
    }
    Ok(target)
}

/// `dest/name` when free, else `name copy`, `name copy 2`, … (before the
/// extension for files: `notes copy.txt`).
fn free_copy_target(dest: &Path, name: &str, is_dir: bool) -> PathBuf {
    let plain = dest.join(name);
    if plain.symlink_metadata().is_err() {
        return plain;
    }
    let (stem, ext) = split_extension(name, is_dir);
    (1..)
        .map(|n| {
            let suffix = if n == 1 {
                " copy".to_string()
            } else {
                format!(" copy {n}")
            };
            dest.join(format!("{stem}{suffix}{ext}"))
        })
        .find(|candidate| candidate.symlink_metadata().is_err())
        .expect("an unbounded range always finds a free name")
}

/// Split `notes.txt` into (`notes`, `.txt`). Folders and dotfiles such as
/// `.env` keep their whole name as the stem.
fn split_extension(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}

/// Recursive copy that recreates symlinks instead of following them, so a
/// link to a big directory (or to an ancestor) is not expanded.
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let meta = source.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        return copy_symlink(source, target);
    }
    if meta.is_dir() {
        std::fs::create_dir(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        // After the children: a read-only folder would refuse them.
        std::fs::set_permissions(target, meta.permissions())?;
        return Ok(());
    }
    std::fs::copy(source, target).map(|_| ())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(source)?, target)
}

#[cfg(windows)]
fn copy_symlink(source: &Path, target: &Path) -> io::Result<()> {
    let link = std::fs::read_link(source)?;
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(link, target)
    } else {
        // Creating a file symlink needs a privilege most accounts lack;
        // copying what it points at is the useful fallback.
        std::os::windows::fs::symlink_file(&link, target)
            .or_else(|_| std::fs::copy(source, target).map(|_| ()))
    }
}

fn remove_any(path: &Path) -> io::Result<()> {
    let meta = path.symlink_metadata()?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Files dropped on the pane from a file manager. Terminals type a drop as
/// pasted text — shell-quoted paths (`'/a b/c' /d`), backslash-escaped
/// paths, or `file://` URIs one per line, depending on the emulator. The
/// text only counts as a drop when EVERY item is an existing absolute path;
/// anything else is an ordinary paste.
pub fn parse_dropped(text: &str) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let tokens = if text.lines().all(|line| {
        let line = line.trim();
        line.is_empty() || line.starts_with('#') || line.starts_with("file://")
    }) {
        parse_uri_list(text)?
    } else {
        split_shell_words(text)?
            .into_iter()
            .map(|word| {
                if word.starts_with("file://") {
                    file_uri_to_path(&word)
                } else {
                    Some(PathBuf::from(word))
                }
            })
            .collect::<Option<Vec<_>>>()?
    };
    let valid = !tokens.is_empty()
        && tokens
            .iter()
            .all(|path| path.is_absolute() && path.symlink_metadata().is_ok());
    valid.then_some(tokens)
}

/// Shell-style word splitting: whitespace separates, '…' is literal, "…"
/// honours backslash escapes, and outside quotes a backslash escapes the
/// next character — except on Windows, where it is the path separator.
/// `None` on an unterminated quote.
fn split_shell_words(text: &str) -> Option<Vec<String>> {
    let backslash_escapes = !cfg!(windows);
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => current.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' if backslash_escapes => {
                            let next = chars.next()?;
                            if !matches!(next, '"' | '\\' | '$' | '`') {
                                current.push('\\');
                            }
                            current.push(next);
                        }
                        c => current.push(c),
                    }
                }
            }
            '\\' if backslash_escapes => {
                in_word = true;
                current.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                current.push(c);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

/// `text/uri-list`: one URI per line, `#` comments. Only local `file://`
/// URIs make sense here.
fn parse_uri_list(text: &str) -> Option<Vec<PathBuf>> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(file_uri_to_path)
        .collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///p` and `file://localhost/p` are local; another host is not.
    let path = if let Some(stripped) = rest.strip_prefix("localhost") {
        stripped
    } else {
        rest
    };
    if !path.starts_with('/') {
        return None;
    }
    let bytes = percent_decode(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        // `/C:/Users/…` → `C:/Users/…`.
        let text = String::from_utf8(bytes).ok()?;
        let text = match text.as_bytes() {
            [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => text[1..].to_string(),
            _ => text,
        };
        Some(PathBuf::from(OsString::from(text)))
    }
}

fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// `file://` URI for an absolute path, percent-encoding everything outside
/// the unreserved set (and `/`).
pub fn path_to_file_uri(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = {
        let text = path.to_string_lossy().replace('\\', "/");
        if text.starts_with('/') {
            text
        } else {
            format!("/{text}")
        }
        .into_bytes()
    };
    let mut uri = String::from("file://");
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':') {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// `x-special/gnome-copied-files`: the clipboard format Nautilus, Nemo and
/// Caja paste files from — `copy` or `cut`, then one URI per line.
fn gnome_copied_files(paths: &[PathBuf], op: Op) -> String {
    let mut text = String::from(match op {
        Op::Copy => "copy",
        Op::Move => "cut",
    });
    for path in paths {
        text.push('\n');
        text.push_str(&path_to_file_uri(path));
    }
    text
}

/// Parse either `x-special/gnome-copied-files` or a plain `text/uri-list`
/// read back from the clipboard. A `cut` header means the paste moves.
fn parse_clipboard_files(text: &str) -> Option<(Op, Vec<PathBuf>)> {
    let mut lines = text.lines().map(str::trim).peekable();
    let op = match lines.peek() {
        Some(&"cut") => Some(Op::Move),
        Some(&"copy") => Some(Op::Copy),
        _ => None,
    };
    if op.is_some() {
        lines.next();
    }
    let paths: Vec<PathBuf> = lines
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(file_uri_to_path)
        .collect::<Option<_>>()?;
    (!paths.is_empty()).then_some((op.unwrap_or(Op::Copy), paths))
}

/// Put files (not their paths as text) on the system clipboard, so a paste
/// in the file manager copies — or, for Cut, moves — them. Needs a native
/// clipboard tool: OSC 52 only carries text, so over SSH this fails.
pub fn copy_files_to_clipboard(paths: &[PathBuf], op: Op) -> io::Result<()> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // A single helper can only serve one format. KDE's Dolphin reads
        // text/uri-list; the GNOME family reads the gnome-copied-files
        // format, which alone carries Cut.
        let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.contains("KDE"));
        let (mime, payload) = if kde {
            let mut list = String::new();
            for path in paths {
                list.push_str(&path_to_file_uri(path));
                list.push_str("\r\n");
            }
            ("text/uri-list", list)
        } else {
            (
                "x-special/gnome-copied-files",
                gnome_copied_files(paths, op),
            )
        };
        let mut candidates: Vec<Vec<&str>> = Vec::new();
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            candidates.push(vec!["wl-copy", "--type", mime]);
        }
        candidates.push(vec!["xclip", "-selection", "clipboard", "-t", mime]);
        let mut last_err = io::Error::new(
            io::ErrorKind::NotFound,
            "needs xclip (X11) or wl-clipboard (Wayland)",
        );
        for argv in &candidates {
            match crate::actions::copy_with(argv, &payload) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => last_err = error,
            }
        }
        Err(last_err)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = op;
        let items: Vec<String> = paths
            .iter()
            .map(|p| {
                format!(
                    "POSIX file \"{}\"",
                    crate::actions::applescript_escape(&p.to_string_lossy())
                )
            })
            .collect();
        let script = format!("set the clipboard to {{{}}}", items.join(", "));
        let status = std::process::Command::new("osascript")
            .args(["-e", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("osascript exited with {status}")))
        }
    }
    #[cfg(windows)]
    {
        let _ = op;
        let list: Vec<String> = paths
            .iter()
            .map(|p| format!("'{}'", p.to_string_lossy().replace('\'', "''")))
            .collect();
        let script = format!("Set-Clipboard -LiteralPath {}", list.join(","));
        let status = crate::actions::clipboard_command(&[
            "powershell",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("powershell exited with {status}")))
        }
    }
}

/// Files a file manager put on the system clipboard (Copy or Cut there),
/// with the operation a paste should perform. `None` when the clipboard
/// holds no files or no clipboard tool answers.
pub fn files_from_clipboard() -> Option<(Op, Vec<PathBuf>)> {
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[&[&str]] = &[
        &[
            "wl-paste",
            "--no-newline",
            "--type",
            "x-special/gnome-copied-files",
        ],
        &["wl-paste", "--no-newline", "--type", "text/uri-list"],
        &[
            "xclip",
            "-selection",
            "clipboard",
            "-t",
            "x-special/gnome-copied-files",
            "-o",
        ],
        &[
            "xclip",
            "-selection",
            "clipboard",
            "-t",
            "text/uri-list",
            "-o",
        ],
    ];
    #[cfg(target_os = "macos")]
    let candidates: &[&[&str]] = &[&[
        "osascript",
        "-e",
        "POSIX path of (the clipboard as «class furl»)",
    ]];
    #[cfg(windows)]
    let candidates: &[&[&str]] = &[&[
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); \
         Get-Clipboard -Format FileDropList | ForEach-Object { $_.FullName }",
    ]];
    for argv in candidates {
        if argv[0] == "wl-paste" && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            continue;
        }
        let Ok(output) = crate::actions::clipboard_command(argv)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        if let Some(found) = parse_clipboard_files(&text) {
            return Some(found);
        }
        // macOS / Windows print plain paths, one per line.
        let paths: Vec<PathBuf> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.symlink_metadata().is_ok())
            .collect();
        if !paths.is_empty() {
            return Some((Op::Copy, paths));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-ft-transfer-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_folder_cannot_be_dropped_into_itself_or_a_descendant() {
        let src = PathBuf::from("/w/src");
        assert!(drop_problem(std::slice::from_ref(&src), Path::new("/w/src"), Op::Move).is_some());
        assert!(
            drop_problem(
                std::slice::from_ref(&src),
                Path::new("/w/src/deep/er"),
                Op::Copy
            )
            .is_some()
        );
        // A sibling that merely shares the prefix is fine.
        assert!(drop_problem(std::slice::from_ref(&src), Path::new("/w/src2"), Op::Move).is_none());
        assert!(
            drop_problem(&[src], Path::new("/w"), Op::Move).is_some(),
            "already there"
        );
        assert!(drop_problem(&[PathBuf::from("/w/a.txt")], Path::new("/w"), Op::Copy).is_none());
        assert!(drop_problem(&[], Path::new("/w"), Op::Copy).is_some());
    }

    #[test]
    fn move_relocates_and_never_overwrites() {
        let dir = tmp("move");
        let dest = dir.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, "a").unwrap();
        let outcome = transfer(std::slice::from_ref(&a), &dest, Op::Move);
        assert!(outcome.failed.is_none());
        assert_eq!(outcome.landed, vec![dest.join("a.txt")]);
        assert!(!a.exists());

        std::fs::write(&a, "new").unwrap();
        let outcome = transfer(std::slice::from_ref(&a), &dest, Op::Move);
        let (path, error) = outcome.failed.expect("collision must fail");
        assert_eq!(path, a);
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "a");
        assert!(a.exists(), "the source survives a refused move");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_duplicates_with_free_names_and_recurses_folders() {
        let dir = tmp("copy");
        let a = dir.join("notes.txt");
        std::fs::write(&a, "x").unwrap();
        let first = transfer(std::slice::from_ref(&a), &dir, Op::Copy);
        let second = transfer(std::slice::from_ref(&a), &dir, Op::Copy);
        assert_eq!(first.landed, vec![dir.join("notes copy.txt")]);
        assert_eq!(second.landed, vec![dir.join("notes copy 2.txt")]);

        let folder = dir.join("pkg.d");
        std::fs::create_dir_all(folder.join("inner")).unwrap();
        std::fs::write(folder.join("inner/f"), "deep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("inner", folder.join("link")).unwrap();
        let dest = dir.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let outcome = transfer(std::slice::from_ref(&folder), &dest, Op::Copy);
        assert!(outcome.failed.is_none());
        assert_eq!(
            std::fs::read_to_string(dest.join("pkg.d/inner/f")).unwrap(),
            "deep"
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(dest.join("pkg.d/link")).unwrap(),
            PathBuf::from("inner"),
            "symlinks are recreated, not followed"
        );
        let again = transfer(std::slice::from_ref(&folder), &dir, Op::Copy);
        assert_eq!(
            again.landed,
            vec![dir.join("pkg.d copy")],
            "folders keep dots"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_stops_the_batch_and_reports_what_landed() {
        let dir = tmp("batch");
        let dest = dir.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let a = dir.join("a");
        std::fs::write(&a, "").unwrap();
        let missing = dir.join("missing");
        let c = dir.join("c");
        std::fs::write(&c, "").unwrap();
        let outcome = transfer(&[a, missing.clone(), c.clone()], &dest, Op::Move);
        assert_eq!(outcome.landed, vec![dest.join("a")]);
        assert_eq!(outcome.failed.unwrap().0, missing);
        assert!(c.exists(), "later sources are untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_names_split_the_extension_but_not_dotfiles() {
        assert_eq!(split_extension("a.txt", false), ("a", ".txt"));
        assert_eq!(split_extension(".env", false), (".env", ""));
        assert_eq!(split_extension("Makefile", false), ("Makefile", ""));
        assert_eq!(split_extension("v1.2", true), ("v1.2", ""));
    }

    #[cfg(unix)]
    #[test]
    fn dropped_text_parses_quoted_escaped_and_uri_forms() {
        let dir = tmp("drop");
        let spaced = dir.join("with space.txt");
        let plain = dir.join("plain");
        std::fs::write(&spaced, "").unwrap();
        std::fs::write(&plain, "").unwrap();
        let (s, p) = (spaced.display().to_string(), plain.display().to_string());

        // GNOME Terminal / VTE: single-quoted, space separated.
        assert_eq!(
            parse_dropped(&format!("'{s}' '{p}' ")),
            Some(vec![spaced.clone(), plain.clone()])
        );
        // Backslash-escaped spaces.
        assert_eq!(
            parse_dropped(&s.replace(' ', "\\ ")),
            Some(vec![spaced.clone()])
        );
        // Double quotes.
        assert_eq!(
            parse_dropped(&format!("\"{s}\"")),
            Some(vec![spaced.clone()])
        );
        // URI list.
        let uris = format!(
            "{}\r\n{}\r\n",
            path_to_file_uri(&spaced),
            path_to_file_uri(&plain)
        );
        assert_eq!(
            parse_dropped(&uris),
            Some(vec![spaced.clone(), plain.clone()])
        );

        // Ordinary pastes are not drops.
        assert_eq!(parse_dropped("hello world"), None);
        assert_eq!(parse_dropped(&format!("'{s}' relative.txt")), None);
        assert_eq!(parse_dropped(&format!("{}/nope", dir.display())), None);
        assert_eq!(parse_dropped("'unterminated"), None);
        assert_eq!(parse_dropped("   "), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_uris_roundtrip_through_percent_encoding() {
        let path = PathBuf::from("/tmp/a b/ç%#.txt");
        let uri = path_to_file_uri(&path);
        assert_eq!(uri, "file:///tmp/a%20b/%C3%A7%25%23.txt");
        assert_eq!(file_uri_to_path(&uri), Some(path));
        assert_eq!(
            file_uri_to_path("file://localhost/x"),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(file_uri_to_path("file://otherhost/x"), None);
        assert_eq!(file_uri_to_path("file:///bad%zz"), None);
    }

    #[cfg(unix)]
    #[test]
    fn gnome_clipboard_format_carries_cut_and_copy() {
        let paths = vec![PathBuf::from("/a b"), PathBuf::from("/c")];
        let text = gnome_copied_files(&paths, Op::Move);
        assert_eq!(text, "cut\nfile:///a%20b\nfile:///c");
        assert_eq!(
            parse_clipboard_files(&text),
            Some((Op::Move, paths.clone()))
        );
        assert_eq!(
            parse_clipboard_files(&gnome_copied_files(&paths, Op::Copy)),
            Some((Op::Copy, paths.clone()))
        );
        // A bare uri-list (KDE) pastes as a copy.
        assert_eq!(
            parse_clipboard_files("file:///a%20b\r\nfile:///c\r\n"),
            Some((Op::Copy, paths))
        );
        assert_eq!(parse_clipboard_files("just text"), None);
        assert_eq!(parse_clipboard_files("copy\n"), None);
    }
}
