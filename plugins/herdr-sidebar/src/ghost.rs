//! The drag "ghost" that follows the mouse pointer ACROSS panes. A terminal
//! pane cannot draw beyond its own cells, so on macOS the sidebar spawns a
//! tiny native click-through overlay (`tools/drag-ghost.swift`, embedded at
//! build time) for the duration of a drag. It ends when we close its stdin.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

#[cfg(target_os = "macos")]
const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/drag-ghost"));
#[cfg(not(target_os = "macos"))]
const HELPER: &[u8] = &[];

/// A running overlay; dropping it closes stdin, which ends the helper.
pub struct Ghost {
    child: Child,
}

impl Drop for Ghost {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        // The helper exits on EOF; reap it, killing a stuck one.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// FNV-1a over the embedded helper: its unpacked file is named by it, so a
/// rebuilt helper of the same length (Mach-O segments are page-aligned)
/// still gets unpacked afresh instead of running the old copy.
fn helper_hash() -> u64 {
    HELPER.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Where the embedded helper is unpacked (private per-user state dir).
fn helper_path() -> Option<PathBuf> {
    let dir = crate::rundir::dir("drag-ghost");
    crate::rundir::ensure_private(&dir).ok()?;
    Some(dir.join(format!("drag-ghost-{:016x}", helper_hash())))
}

/// Unpack the helper once (or when its bytes changed) and make it executable.
fn ensure_helper() -> Option<PathBuf> {
    if HELPER.is_empty() {
        return None;
    }
    let path = helper_path()?;
    let stale = std::fs::metadata(&path).map_or(true, |m| m.len() != HELPER.len() as u64);
    if stale {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, HELPER).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
        std::fs::rename(&tmp, &path).ok()?;
    }
    Some(path)
}

/// Show `label` beside the pointer until the returned handle is dropped.
/// `None` when the overlay is unavailable: not macOS, no swiftc at build,
/// an SSH session (the window would open on the remote Mac's display, not
/// in front of the user), or a helper that died right after launch (dyld
/// minos mismatch, no WindowServer). Callers then fall back to the in-pane
/// chip.
pub fn start(label: &str) -> Option<Ghost> {
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return None;
    }
    let path = ensure_helper()?;
    let mut child = Command::new(path)
        .arg(label)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Give an unlaunchable helper a moment to fail so we can fall back.
    std::thread::sleep(std::time::Duration::from_millis(40));
    if let Ok(Some(_)) = child.try_wait() {
        return None;
    }
    let _ = child.stdin.as_mut().map(|s| s.flush());
    Some(Ghost { child })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn helper_starts_and_ends_on_drop() {
        if HELPER.is_empty() || std::env::var_os("SSH_CONNECTION").is_some() {
            return; // built without swiftc, or no display to show it on
        }
        let ghost = start("ghost-test.md").expect("overlay helper available on macOS");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let pid = ghost.child.id();
        drop(ghost);
        // Reaped: the pid is gone (or at least not our live child).
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "helper still running after drop");
    }
}
