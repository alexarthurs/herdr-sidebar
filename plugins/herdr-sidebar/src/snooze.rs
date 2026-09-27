//! Per-tab "the user closed/hid the sidebar here" markers: hide (« or b) and
//! the toggle CLOSE write one, the quiet ensure hook honors it — otherwise the
//! very next focus event would reopen what the user just closed. Toggle OPEN
//! clears it. Markers for tabs that no longer exist are swept each ensure run
//! (tab ids can be recycled).

use std::path::{Path, PathBuf};

pub fn dir() -> PathBuf {
    crate::rundir::dir("snooze")
}

fn legacy_dir() -> PathBuf {
    std::env::temp_dir().join("herdr-sidebar-snooze")
}

pub fn migrate_legacy(dir: &Path, live_tabs: &std::collections::BTreeSet<String>) {
    let legacy = legacy_dir();
    migrate_legacy_from(&legacy, dir, live_tabs);
}

fn migrate_legacy_from(legacy: &Path, dir: &Path, live_tabs: &std::collections::BTreeSet<String>) {
    if !crate::rundir::is_owned_non_writable(legacy) || crate::rundir::ensure_private(dir).is_err()
    {
        return;
    }
    for tab in live_tabs {
        let Some(name) = marker_name(tab) else {
            continue;
        };
        let old = legacy.join(&name);
        if old.is_file() && std::fs::write(dir.join(&name), b"").is_ok() {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// Sanitized marker filename for a non-empty `tab`, or `None` if the id is
/// unsafe. `tab` ids are pane-supplied identifiers (`workspace:tab`);
/// rejecting path separators and `.`/`..` keeps a marker from ever landing
/// outside `dir` or colliding with `.`/`..` themselves. Callers decide
/// separately how to treat an empty id — that is not "unsafe", it is "no tab
/// to snooze" and every caller here already no-ops on it.
fn marker_name(tab: &str) -> Option<String> {
    let name = tab.replace(':', "_");
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        None
    } else {
        Some(name)
    }
}

fn invalid_tab_id(tab: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("unsafe tab id {tab:?}"),
    )
}

/// Mark `tab` snoozed. Creates `dir` as a private directory first (tests
/// pass their own already-owned temp dir, so `ensure_private` is a no-op
/// there); refuses to write through a directory that fails that check, and
/// refuses a tab id that cannot be turned into a safe filename.
pub fn set(dir: &Path, tab: &str) -> std::io::Result<()> {
    if tab.is_empty() {
        return Ok(());
    }
    let Some(name) = marker_name(tab) else {
        return Err(invalid_tab_id(tab));
    };
    crate::rundir::ensure_private(dir)?;
    std::fs::write(dir.join(name), b"")
}

pub fn clear(dir: &Path, tab: &str) -> std::io::Result<()> {
    if tab.is_empty() {
        return Ok(());
    }
    let Some(name) = marker_name(tab) else {
        return Err(invalid_tab_id(tab));
    };
    if !crate::rundir::is_private(dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("snooze dir {} is not private", dir.display()),
        ));
    }
    match std::fs::remove_file(dir.join(name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn is_set(dir: &Path, tab: &str) -> bool {
    !tab.is_empty()
        && crate::rundir::is_private(dir)
        && marker_name(tab).is_some_and(|name| dir.join(name).exists())
}

pub fn sweep(dir: &Path, live_tabs: &std::collections::BTreeSet<String>) {
    if !crate::rundir::is_private(dir) {
        return;
    }
    let live: std::collections::BTreeSet<String> =
        live_tabs.iter().map(|t| t.replace(':', "_")).collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !live.contains(&entry.file_name().to_string_lossy().into_owned()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("herdr-snooze-test-{label}-{}", std::process::id()))
    }

    #[test]
    fn bad_tab_ids_are_rejected() {
        for bad in ["a/b", "a\\b", ".", ".."] {
            assert_eq!(marker_name(bad), None, "{bad:?}");
        }
        assert_eq!(marker_name("w1:t1"), Some("w1_t1".to_string()));
    }

    #[test]
    fn unsafe_tab_ids_error_rather_than_silently_no_op() {
        let dir = scratch("unsafe-ids");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(set(&dir, "../escape").is_err());
        assert!(clear(&dir, "../escape").is_err());
        // Empty stays a legitimate no-op, not an error.
        assert!(set(&dir, "").is_ok());
        assert!(clear(&dir, "").is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn is_set_is_false_and_sweep_is_a_no_op_on_a_non_private_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("not-private");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("w1_t1"), b"").unwrap();

        assert!(!is_set(&dir, "w1:t1"));
        sweep(&dir, &std::collections::BTreeSet::new());
        // Sweep must not have touched anything either.
        assert!(dir.join("w1_t1").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn clear_refuses_a_non_private_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("clear-refuses");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(clear(&dir, "w1:t1").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_clear_and_sweep_round_trip_on_an_owned_dir() {
        let dir = scratch("round-trip");
        let _ = std::fs::remove_dir_all(&dir);

        set(&dir, "w1:t1").unwrap();
        set(&dir, "w1:t2").unwrap();
        assert!(is_set(&dir, "w1:t1"));
        assert!(!is_set(&dir, "w1:t9"));
        assert!(!is_set(&dir, ""), "empty tab id never snoozes");

        clear(&dir, "w1:t1").unwrap();
        assert!(!is_set(&dir, "w1:t1"));
        // Clearing an already-cleared marker is not an error.
        clear(&dir, "w1:t1").unwrap();

        let live = std::collections::BTreeSet::from(["w1:t3".to_string()]);
        sweep(&dir, &live);
        assert!(!is_set(&dir, "w1:t2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_moves_only_live_legacy_markers() {
        let legacy = scratch("legacy");
        let dir = scratch("migrated");
        let _ = std::fs::remove_dir_all(&legacy);
        let _ = std::fs::remove_dir_all(&dir);
        crate::rundir::ensure_private(&legacy).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(legacy.join("w1_t1"), b"").unwrap();
        std::fs::write(legacy.join("w1_old"), b"").unwrap();

        let live = std::collections::BTreeSet::from(["w1:t1".to_string()]);
        migrate_legacy_from(&legacy, &dir, &live);

        assert!(is_set(&dir, "w1:t1"));
        assert!(!legacy.join("w1_t1").exists());
        assert!(legacy.join("w1_old").exists());
        let _ = std::fs::remove_dir_all(legacy);
        let _ = std::fs::remove_dir_all(dir);
    }
}
