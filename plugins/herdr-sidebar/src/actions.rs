//! The file context menu's model and effects: which entries a target offers,
//! and the filesystem/clipboard/shell operations behind them. UI-free so it is
//! unit-testable; `app.rs` owns the popup rendering and input routing.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuAction {
    NewFile,
    NewFolder,
    CopyPath,
    CopyRelativePath,
    Rename,
    Delete,
    /// `git add` the target — the Explorer's staging entry (issue #20).
    Stage,
    OpenExternal,
    Reveal,
    ChangeFolder,
    ChangeFolderTyped,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuEntry {
    Action(MenuAction, &'static str),
    Separator,
}

/// VS Code-style context menu for a tree row (`target` = `Some(is_dir)` for a
/// row; `None` for a right-click on empty space, which targets the workspace
/// root: creation only). "Open with Default App" is offered for files only —
/// a directory's shell association is the file manager, which is what
/// "Reveal in File Explorer" already does. `in_repo` adds "Stage Changes"
/// (issue #20) — it stays out of the menu entirely when the target is not
/// inside a git repository, so the entry never offers an action that can only
/// fail.
pub fn menu_entries(target: Option<bool>, in_repo: bool) -> Vec<MenuEntry> {
    let mut entries = vec![
        MenuEntry::Action(MenuAction::NewFile, "New File…"),
        MenuEntry::Action(MenuAction::NewFolder, "New Folder…"),
    ];
    if target == Some(false) {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::OpenExternal, "Open with Default App"),
        ]);
    }
    if target.is_some() && in_repo {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::Stage, "Stage Changes"),
        ]);
    }
    if target.is_some() {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
            MenuEntry::Action(MenuAction::CopyRelativePath, "Copy Relative Path"),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::Rename, "Rename…"),
            MenuEntry::Action(MenuAction::Delete, "Delete"),
        ]);
    }
    entries.extend([
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::ChangeFolder, "Change Folder…"),
        MenuEntry::Action(MenuAction::ChangeFolderTyped, "Change Folder (Type Path)…"),
    ]);
    entries
}

/// A usable file name from prompt input: trimmed, non-empty, no path
/// separators or drive colons (a name, not a path).
pub fn validate_name(input: &str) -> Option<&str> {
    let name = input.trim();
    (!name.is_empty()
        && !name.contains(['/', '\\', ':'])
        && name != "."
        && name != "..")
        .then_some(name)
}

fn fresh_path(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = dir.join(name);
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{name} already exists"),
        ));
    }
    Ok(path)
}

pub fn create_file(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = fresh_path(dir, name)?;
    std::fs::write(&path, b"")?;
    Ok(path)
}

pub fn create_folder(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = fresh_path(dir, name)?;
    std::fs::create_dir(&path)?;
    Ok(path)
}

pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    let target = fresh_path(parent, new_name)?;
    std::fs::rename(path, &target)?;
    Ok(target)
}

pub fn delete(path: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Copy text to the system clipboard by piping to the platform's clipboard
/// tool (a console child of the TUI's own pty — no window is created). These
/// tools only ever reach the clipboard of the machine this process runs on,
/// so over SSH they either fail (no `xclip`/`wl-copy` on a headless remote)
/// or silently write to a clipboard the user can never see. Falling back to
/// an OSC 52 escape sequence hands the job to the terminal emulator itself,
/// which runs on the user's machine and receives the sequence over the same
/// SSH stream, so it reaches the clipboard the user actually expects.
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    #[cfg(windows)]
    let candidates: &[&[&str]] = &[&["clip"]];
    #[cfg(not(windows))]
    let candidates: &[&[&str]] = &[&["pbcopy"], &["wl-copy"], &["xclip", "-selection", "clipboard"]];

    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no clipboard tool found");
    for argv in candidates {
        match copy_with(argv, text) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = err,
        }
    }
    osc52_copy(text).or(Err(last_err))
}

/// Ask the terminal itself to set the clipboard via OSC 52. `TMUX` selects
/// tmux's DCS passthrough wrapping (plain OSC 52 never reaches the outer
/// terminal through tmux); other multiplexers/terminals take the sequence
/// as-is, and one that doesn't understand OSC 52 simply ignores it.
fn osc52_copy(text: &str) -> io::Result<()> {
    use std::io::Write;

    let payload = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let sequence = if std::env::var_os("TMUX").is_some() {
        format!("\x1bPtmux;{}\x1b\\", payload.replace('\x1b', "\x1b\x1b"))
    } else {
        payload
    };
    let mut stdout = io::stdout();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()
}

/// Minimal RFC 4648 base64 encoder — OSC 52's payload is the only user, so a
/// dependency isn't worth pulling in for it.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(match b1 {
            Some(b1) => TABLE[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            None => '=',
        });
        out.push(match b2 {
            Some(b2) => TABLE[(b2 & 0x3f) as usize] as char,
            None => '=',
        });
    }
    out
}

fn copy_with(argv: &[&str], text: &str) -> io::Result<()> {
    use std::io::Write;

    let mut child = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("{} opened without stdin", argv[0]),
        ));
    };
    stdin.write_all(text.as_bytes())?;
    drop(stdin);
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{} exited with {status}", argv[0])))
    }
}

/// Read text from the system clipboard when a platform clipboard command is
/// available. Keeping this best-effort matches [`copy_to_clipboard`]: the
/// editor remains useful over SSH/headless sessions where no clipboard exists.
pub fn paste_from_clipboard() -> io::Result<String> {
    #[cfg(windows)]
    let candidates: &[&[&str]] = &[&[
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); Get-Clipboard -Raw",
    ]];
    #[cfg(target_os = "macos")]
    let candidates: &[&[&str]] = &[&["pbpaste"]];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[&[&str]] = &[
        &["wl-paste", "--no-newline"],
        &["xclip", "-selection", "clipboard", "-o"],
    ];

    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no clipboard tool found");
    for argv in candidates {
        match std::process::Command::new(argv[0]).args(&argv[1..]).output() {
            Ok(output) if output.status.success() => {
                return String::from_utf8(output.stdout)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            Ok(output) => {
                last_err = io::Error::other(format!(
                    "{} exited with {}",
                    argv[0], output.status
                ));
            }
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

/// Open the platform file manager with the path selected (best-effort).
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(parent) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

/// Open a path with the OS-associated application (VS Code's "Open with
/// Default App" / a double click in the file manager).
///
/// Windows goes through `explorer.exe <path>` rather than `cmd /c start`:
/// explorer is a GUI-subsystem process, so no console is created for it and
/// Windows 11 doesn't flash a Windows Terminal window (the same reason the
/// [[events]] hooks use the windowless sidecar). It resolves the shell
/// association exactly like a double click. Its exit code is unreliable
/// (explorer routinely returns 1 on success), so only the spawn is checked.
pub fn open_external(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    std::process::Command::new(program)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Quote text embedded in a double-quoted AppleScript string literal.
#[cfg(any(test, target_os = "macos"))]
fn applescript_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Parse a completed osascript picker invocation. Keeping this separate from
/// process launch makes cancel/error and path normalization testable on every
/// platform, including CI hosts that do not provide osascript.
#[cfg(any(test, target_os = "macos"))]
fn parse_osascript_folder(success: bool, stdout: &[u8]) -> Option<PathBuf> {
    if !success {
        return None;
    }
    let picked = String::from_utf8_lossy(stdout).trim().to_string();
    if picked.is_empty() {
        return None;
    }
    let trimmed = picked.trim_end_matches('/');
    Some(if trimmed.is_empty() { PathBuf::from("/") } else { PathBuf::from(trimmed) })
}

/// Native "choose a folder" dialog. The apps call this from a worker thread
/// so their heartbeat continues while the dialog is open.
///
/// macOS deliberately uses an osascript subprocess: rfd's Cocoa backend needs
/// the main thread or a running NSApplication and aborts a terminal TUI when
/// invoked from the worker. Windows keeps its existing rfd/IFileDialog path.
#[cfg(any(windows, target_os = "macos"))]
pub fn pick_folder(start: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // An invalid default location makes choose-folder fail instead of
        // opening, so omit it if the former root has disappeared.
        let location = if start.is_dir() {
            format!(
                " default location POSIX file \"{}\"",
                applescript_escape(&start.display().to_string())
            )
        } else {
            String::new()
        };
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "POSIX path of (choose folder with prompt \"Open Folder\"{location})"
            ))
            .output()
            .ok()?;
        parse_osascript_folder(output.status.success(), &output.stdout)
    }
    #[cfg(windows)]
    {
        rfd::FileDialog::new()
            .set_title("Open Folder")
            .set_directory(start)
            .pick_folder()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-ft-actions-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn has(entries: &[MenuEntry], action: MenuAction) -> bool {
        entries.iter().any(|e| matches!(e, MenuEntry::Action(a, _) if *a == action))
    }

    #[test]
    fn menu_shape_for_rows_and_root() {
        let row = menu_entries(Some(false), true);
        assert!(matches!(row[0], MenuEntry::Action(MenuAction::NewFile, _)));
        assert!(has(&row, MenuAction::Delete));
        let root = menu_entries(None, true);
        assert!(!has(&root, MenuAction::Rename));
        assert!(has(&root, MenuAction::Reveal));
    }

    #[test]
    fn open_external_is_offered_for_files_only() {
        assert!(has(&menu_entries(Some(false), true), MenuAction::OpenExternal), "file row");
        assert!(!has(&menu_entries(Some(true), true), MenuAction::OpenExternal), "directory row");
        assert!(!has(&menu_entries(None, true), MenuAction::OpenExternal), "empty space");
        // Directories keep everything else they had.
        assert!(has(&menu_entries(Some(true), true), MenuAction::Rename));
    }

    #[test]
    fn stage_is_offered_for_rows_inside_a_repo_only() {
        assert!(has(&menu_entries(Some(false), true), MenuAction::Stage), "file row");
        assert!(has(&menu_entries(Some(true), true), MenuAction::Stage), "directory row");
        assert!(!has(&menu_entries(None, true), MenuAction::Stage), "empty space");
        assert!(!has(&menu_entries(Some(false), false), MenuAction::Stage), "outside a repo");
        assert!(!has(&menu_entries(Some(true), false), MenuAction::Stage));
    }

    #[test]
    fn name_validation_rejects_paths_and_blanks() {
        assert_eq!(validate_name("  notes.md "), Some("notes.md"));
        assert_eq!(validate_name(""), None);
        assert_eq!(validate_name("   "), None);
        assert_eq!(validate_name("a/b"), None);
        assert_eq!(validate_name("a\\b"), None);
        assert_eq!(validate_name("C:"), None);
        assert_eq!(validate_name(".."), None);
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_commands_must_accept_input_and_exit_successfully() {
        assert!(copy_with(&["sh", "-c", "cat >/dev/null"], "copied").is_ok());
        let error = copy_with(&["sh", "-c", "cat >/dev/null; exit 7"], "not copied")
            .unwrap_err()
            .to_string();
        assert!(error.contains("exited with"), "{error}");
    }

    #[test]
    fn base64_matches_rfc_4648_padding_cases() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode("/path/héllo".as_bytes()), "L3BhdGgvaMOpbGxv");
    }

    #[test]
    fn applescript_literals_escape_backslashes_before_quotes() {
        assert_eq!(applescript_escape("/tmp/plain"), "/tmp/plain");
        assert_eq!(applescript_escape(r#"/tmp/a"b"#), r#"/tmp/a\"b"#);
        assert_eq!(applescript_escape(r"/tmp/a\b"), r"/tmp/a\\b");
        assert_eq!(applescript_escape(r#"/tmp/a\"b"#), r#"/tmp/a\\\"b"#);
    }

    #[test]
    fn osascript_picker_output_parsing_handles_cancel_root_and_trailing_slash() {
        assert_eq!(parse_osascript_folder(false, b"/ignored/\n"), None);
        assert_eq!(parse_osascript_folder(true, b"\n"), None);
        assert_eq!(parse_osascript_folder(true, b"/\n"), Some(PathBuf::from("/")));
        assert_eq!(
            parse_osascript_folder(true, b"/Users/alex/My Folder/\n"),
            Some(PathBuf::from("/Users/alex/My Folder"))
        );
    }

    #[test]
    fn create_rename_delete_roundtrip() {
        let dir = tmp("roundtrip");
        let file = create_file(&dir, "a.txt").unwrap();
        assert!(file.exists());
        assert!(create_file(&dir, "a.txt").is_err(), "no overwrite");
        let folder = create_folder(&dir, "sub").unwrap();
        assert!(folder.is_dir());
        let renamed = rename(&file, "b.txt").unwrap();
        assert!(renamed.exists() && !file.exists());
        assert!(rename(&renamed, "sub").is_err(), "no clobbering existing");
        delete(&renamed, false).unwrap();
        delete(&folder, true).unwrap();
        assert!(!renamed.exists() && !folder.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
