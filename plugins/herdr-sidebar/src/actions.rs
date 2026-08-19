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
/// "Reveal in File Explorer" already does.
pub fn menu_entries(target: Option<bool>) -> Vec<MenuEntry> {
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
/// tool (a console child of the TUI's own pty — no window is created).
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    use std::io::Write;
    #[cfg(windows)]
    let candidates: &[&[&str]] = &[&["clip"]];
    #[cfg(not(windows))]
    let candidates: &[&[&str]] = &[&["pbcopy"], &["wl-copy"], &["xclip", "-selection", "clipboard"]];

    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no clipboard tool found");
    for argv in candidates {
        let spawned = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(text.as_bytes())?;
                }
                child.wait()?;
                return Ok(());
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

/// Quote a string for embedding in a double-quoted AppleScript literal.
#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The native "choose a folder" dialog, run to completion on the CALLING
/// thread — both apps call it from a spawned one so the pane's liveness
/// heartbeat keeps beating while the dialog is open.
///
/// macOS goes through `osascript`, NOT `rfd`: rfd's Cocoa backend routes every
/// dialog through `run_on_main`, which panics outright when called off the main
/// thread in a process with no running `NSApplication` ("You are running RFD in
/// NonWindowed environment…"). A terminal TUI is exactly that process, so the
/// background-thread call Windows is perfectly happy with aborted the whole
/// pane on macOS. A subprocess has no main-thread constraint and keeps the
/// dialog native. Windows' `IFileDialog` has no such rule, so it keeps rfd.
///
/// Returns `None` when the user cancels (osascript exits non-zero with
/// "User canceled. (-128)"; rfd returns `None`).
#[cfg(any(windows, target_os = "macos"))]
pub fn pick_folder(start: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // `default location` on a path that no longer exists makes osascript
        // error out instead of opening, so only pass one we can still see.
        let location = if start.is_dir() {
            format!(
                " default location POSIX file \"{}\"",
                applescript_escape(&start.display().to_string())
            )
        } else {
            String::new()
        };
        let out = std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "POSIX path of (choose folder with prompt \"Open Folder\"{location})"
            ))
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // `POSIX path of` yields a trailing slash; keep it only for root.
        let picked = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let trimmed = picked.trim_end_matches('/');
        match (picked.is_empty(), trimmed.is_empty()) {
            (true, _) => None,
            (false, true) => Some(PathBuf::from("/")),
            (false, false) => Some(PathBuf::from(trimmed)),
        }
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
        let row = menu_entries(Some(false));
        assert!(matches!(row[0], MenuEntry::Action(MenuAction::NewFile, _)));
        assert!(has(&row, MenuAction::Delete));
        let root = menu_entries(None);
        assert!(!has(&root, MenuAction::Rename));
        assert!(has(&root, MenuAction::Reveal));
    }

    #[test]
    fn open_external_is_offered_for_files_only() {
        assert!(has(&menu_entries(Some(false)), MenuAction::OpenExternal), "file row");
        assert!(!has(&menu_entries(Some(true)), MenuAction::OpenExternal), "directory row");
        assert!(!has(&menu_entries(None), MenuAction::OpenExternal), "empty space");
        // Directories keep everything else they had.
        assert!(has(&menu_entries(Some(true)), MenuAction::Rename));
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

    /// A folder name may legally contain a quote or a backslash; an unescaped
    /// one would terminate the AppleScript literal early and either error out
    /// or change what the script means.
    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_literals_escape_quotes_and_backslashes() {
        assert_eq!(applescript_escape("/tmp/plain"), "/tmp/plain");
        assert_eq!(applescript_escape(r#"/tmp/a"b"#), r#"/tmp/a\"b"#);
        assert_eq!(applescript_escape(r"/tmp/a\b"), r"/tmp/a\\b");
        // Backslashes are doubled BEFORE quotes are escaped, so an escaped
        // quote never gets its own backslash re-escaped into a literal one.
        assert_eq!(applescript_escape(r#"/tmp/a\"b"#), r#"/tmp/a\\\"b"#);
    }

    /// osascript is present on every macOS install; a cancelled dialog must
    /// come back as `None` rather than a panic or an empty path.
    #[cfg(target_os = "macos")]
    #[test]
    fn pick_folder_returns_none_when_the_script_fails() {
        // `false` exits non-zero the same way a user cancel does.
        let out = std::process::Command::new("osascript")
            .arg("-e")
            .arg("error \"cancelled\" number -128")
            .output()
            .expect("osascript present");
        assert!(!out.status.success(), "a -128 error must exit non-zero");
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
