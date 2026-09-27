//! Private per-user runtime directories for the sidebar's small scratch
//! files (preview control files, hide/snooze markers). These are NOT durable
//! state — [`crate::state`] owns that — but they still must not land in a
//! directory another local user can read, write, or plant a symlink in.
//!
//! `std::env::temp_dir()` (unix `/tmp`) is shared and world-writable, and our
//! filenames are predictable (pane ids, sequence numbers), so a prior design
//! that scoped a fixed subdirectory of it and chmod'd it 0700 was still
//! racy: a local attacker who wins the race before the first chmod, or who
//! plants the directory itself before we ever run, can plant a symlink or
//! own the directory outright. The fix is to stop sharing a directory with
//! other users at all — use the plugin's own per-user state dir
//! ([`crate::state::state_dir`]), which every herdr-sidebar process already
//! agrees on and which the OS gives us privately from the start.
//!
//! Not `XDG_RUNTIME_DIR`: it can be absent from some process environments,
//! and logind removes it the moment the last login session ends, while
//! herdr's server/panes can outlive that. The state dir is persistent, but
//! what we put under it here is tiny and already swept (orphan control
//! sweep in `viewer.rs`, snooze sweep on every `ensure` run).

use std::path::{Path, PathBuf};

/// Where a named private runtime directory (`"scratch"`, `"snooze"`) lives.
/// Pure: never touches the filesystem. Callers that are about to WRITE
/// through it must call [`ensure_private`] first.
pub fn dir(name: &str) -> PathBuf {
    match crate::state::state_dir() {
        Some(base) => dir_in(&base, name),
        None => fallback_dir(name),
    }
}

/// `dir()`'s placement logic, minus the env/HOME lookup, so it can be tested
/// without mutating process-global environment state that other tests read
/// concurrently.
fn dir_in(base: &Path, name: &str) -> PathBuf {
    base.join(name)
}

/// No `HOME`/`LOCALAPPDATA` to resolve a state dir from at all — extremely
/// rare (a stripped-down service environment). Falls back to the shared temp
/// dir, scoped per-user on unix by euid. Unlike the state dir, this fallback
/// is not guaranteed to be the same path across every process that might
/// need it (`TMPDIR` can vary by process): a basename-only control token
/// (see `viewer::control_token`) may not resolve back to the writer's
/// directory for a reader in a different process in this rare case. Known
/// limitation, accepted rather than risking herdr's metadata-token
/// truncation (`AGENTS.md`) by carrying a full path instead.
#[cfg(unix)]
fn fallback_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("herdr-sidebar-{name}-{}", euid()))
}

#[cfg(windows)]
fn fallback_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("herdr-sidebar-{name}"))
}

/// Create `dir` (a path that came from [`dir`]) as our own private 0700
/// directory, repairing loose permissions left by an older release. Only
/// ever call this on a path from [`dir`]: the parent is either the user's
/// own state dir or the temp-dir fallback above, never a directory shared
/// with untrusted content, so `create_dir_all` on the parent needs no extra
/// scoping.
#[cfg(unix)]
pub fn ensure_private(dir: &Path) -> std::io::Result<()> {
    use std::fs::{DirBuilder, Permissions};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    let uid = meta.uid();
    let mode = meta.permissions().mode();
    let expected = euid();
    check(
        dir,
        meta.file_type().is_symlink(),
        meta.is_dir(),
        uid,
        mode,
        expected,
        false,
    )?;
    if uid == expected && mode & 0o077 != 0 {
        std::fs::set_permissions(dir, Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Windows' per-user profile directories (`%LOCALAPPDATA%`, `%TEMP%`) are
/// already ACL-restricted to the owning account; there is no POSIX mode bit
/// to narrow further here, so this only has to guarantee the directory
/// exists.
#[cfg(windows)]
pub fn ensure_private(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Read-only check: is `dir` a private directory we own, safe to write
/// through or trust the contents of? Never creates or repairs anything —
/// callers that need a fresh private directory call [`ensure_private`].
#[cfg(unix)]
pub fn is_private(dir: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    check(
        dir,
        meta.file_type().is_symlink(),
        meta.is_dir(),
        meta.uid(),
        meta.permissions().mode(),
        euid(),
        true,
    )
    .is_ok()
}

#[cfg(windows)]
pub fn is_private(dir: &Path) -> bool {
    dir.is_dir()
}

/// Compatibility check for read-only migration from directories created by
/// older releases. Those directories were commonly 0755, but are safe to read
/// and clean when they are real directories owned by us and nobody else can
/// write into them.
#[cfg(unix)]
pub fn is_owned_non_writable(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::symlink_metadata(dir).is_ok_and(|meta| {
        !meta.file_type().is_symlink()
            && meta.is_dir()
            && meta.uid() == euid()
            && meta.permissions().mode() & 0o022 == 0
    })
}

#[cfg(windows)]
pub fn is_owned_non_writable(dir: &Path) -> bool {
    dir.is_dir()
}

/// Pure validation shared by [`ensure_private`] (which repairs a loose mode
/// itself, so it does not ask `check` to enforce one) and [`is_private`]
/// (read-only, so a loose mode is disqualifying). Takes plain values rather
/// than `Metadata` so it can be unit-tested without touching the filesystem.
#[cfg(unix)]
fn check(
    dir: &Path,
    is_symlink: bool,
    is_dir: bool,
    uid: u32,
    mode: u32,
    expected_uid: u32,
    enforce_mode: bool,
) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    if is_symlink {
        return Err(Error::other(format!("{} is a symlink", dir.display())));
    }
    if !is_dir {
        return Err(Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if uid != expected_uid {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {uid}, not {expected_uid} (created by another local user?)",
                dir.display()
            ),
        ));
    }
    if enforce_mode && mode & 0o077 != 0 {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!("{} has group/world permissions", dir.display()),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: geteuid() takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn scratch(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("herdr-rundir-test-{label}-{}", std::process::id()))
    }

    #[test]
    fn dir_in_joins_the_base_and_name() {
        assert_eq!(
            dir_in(Path::new("/state/herdr-sidebar"), "scratch"),
            PathBuf::from("/state/herdr-sidebar/scratch")
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_creates_a_0700_directory_owned_by_us() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = scratch("create");
        let _ = std::fs::remove_dir_all(&dir);

        ensure_private(&dir).unwrap();

        let meta = std::fs::metadata(&dir).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert_eq!(meta.uid(), euid());
        assert!(is_private(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_repairs_a_loosely_permissioned_existing_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("repair");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_tolerates_the_directory_already_existing() {
        let dir = scratch("already-exists");
        let _ = std::fs::remove_dir_all(&dir);
        ensure_private(&dir).unwrap();

        // A second call races nothing here, but exercises the same
        // AlreadyExists path a genuine race between two processes would.
        ensure_private(&dir).unwrap();
        assert!(is_private(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_rejects_a_preexisting_symlink() {
        use std::os::unix::fs::symlink;
        let dir = scratch("symlink");
        let target = scratch("symlink-target");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, &dir).unwrap();

        let err = ensure_private(&dir).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(!is_private(&dir));

        let _ = std::fs::remove_file(&dir);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_rejects_a_preexisting_regular_file() {
        let dir = scratch("regular-file");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dir);
        std::fs::write(&dir, b"not a directory").unwrap();

        let err = ensure_private(&dir).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
        assert!(!is_private(&dir));

        let _ = std::fs::remove_file(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn is_private_rejects_an_owned_but_group_readable_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("loose-mode");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();

        assert!(!is_private(&dir));
        assert!(
            is_owned_non_writable(&dir),
            "legacy migration may read an owned 0750 directory"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_compatibility_rejects_group_writable_directories() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("legacy-writable");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(!is_owned_non_writable(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn check_reports_a_uid_mismatch() {
        let dir = Path::new("/nonexistent/for-message-only");
        let err = check(dir, false, true, 501, 0o700, 1000, false).unwrap_err();
        assert!(
            err.to_string().contains("owned by uid 501, not 1000"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_only_enforces_mode_when_asked() {
        let dir = Path::new("/nonexistent/for-message-only");
        assert!(check(dir, false, true, 1000, 0o755, 1000, false).is_ok());
        let err = check(dir, false, true, 1000, 0o755, 1000, true).unwrap_err();
        assert!(err.to_string().contains("group/world"), "{err}");
    }
}
