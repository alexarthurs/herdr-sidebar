//! Auto-hide in narrow tabs: the counterpart of herdr's own
//! `ui.mobile_width_threshold` for this plugin's pane. When the whole tab
//! area is at or below the configured column count, a docked sidebar hides
//! itself; the quiet ensure hook docks it again once the tab is wide enough.
//!
//! Plugins cannot hook herdr's `layout.updated` event, and a hidden sidebar
//! has no pane left to receive resizes. So hiding also starts a small
//! detached watcher (unix) that polls the tab width and runs the quiet ensure
//! hook once the tab is wide again, while that tab is the focused one.
//! Background tabs, and Windows, reopen at the next focus event instead.
//!
//! The marker is per tab and separate from [`crate::snooze`]: a sidebar the
//! user hid (« / b) stays hidden however wide the tab gets, while one hidden
//! for width comes back on its own. Markers for tabs that no longer exist are
//! swept with the snooze markers.

use std::path::PathBuf;
use std::time::Duration;

/// How often a watcher re-reads the width of its auto-hidden tab.
const WATCH_EVERY: Duration = Duration::from_secs(1);
/// Consecutive socket failures after which a watcher assumes herdr is gone.
const WATCH_MAX_FAILURES: u32 = 10;

pub fn dir() -> PathBuf {
    crate::rundir::dir("autohide")
}

/// A tab `width` columns wide is too narrow for a sidebar. `threshold` 0
/// means the setting is off. The width is the whole tab area, which opening
/// or closing the sidebar does not change, so the check cannot flap on its
/// own.
pub fn should_hide(threshold: u16, width: i64) -> bool {
    threshold > 0 && width <= i64::from(threshold)
}

/// Per-pane state that keeps auto-hide from closing a sidebar the user just
/// opened on purpose in a narrow tab (the explicit toggle ignores the width).
/// Such a pane starts disarmed and arms once the tab has been wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Latch {
    armed: bool,
}

impl Latch {
    /// `width` is the tab area when the pane starts; `None` when the layout
    /// could not be read, which leaves the pane armed.
    pub fn new(threshold: u16, width: Option<i64>) -> Self {
        Self {
            armed: !width.is_some_and(|width| should_hide(threshold, width)),
        }
    }

    /// Record a new tab area width; true when the sidebar should hide now.
    pub fn observe(&mut self, threshold: u16, width: i64) -> bool {
        if !should_hide(threshold, width) {
            self.armed = true;
        }
        self.armed && should_hide(threshold, width)
    }
}

/// Mark this pane's tab as auto-hidden and close the pane. The marker is
/// written before the close for the same reason as the snooze marker: a focus
/// event landing in between must see it. The caller saves anything a direct
/// pane close would lose first.
pub fn hide_pane(pane_id: &str) -> std::io::Result<()> {
    let panes = crate::ipc::call_text("pane.list", serde_json::json!({}))?;
    let tab = crate::launch::tab_of(&panes, pane_id);
    if tab.is_empty() {
        return Err(std::io::Error::other("could not resolve this tab"));
    }
    let dir = dir();
    crate::snooze::set(&dir, &tab)?;
    // Best effort: without a watcher the tab still reopens on the next focus.
    #[cfg(unix)]
    let _ = spawn_watcher(&tab);
    if let Err(e) = crate::ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id })) {
        let _ = crate::snooze::clear(&dir, &tab);
        return Err(e);
    }
    Ok(())
}

/// Start `--autohide-watch <tab>` in a session of its own: herdr hangs up
/// the whole session of the pane it closes, and the sidebar pane is about to
/// close.
#[cfg(unix)]
fn spawn_watcher(tab: &str) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--autohide-watch", tab])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().map(|_| ())
}

/// The watcher loop behind `--autohide-watch <tab>`. It exits once the tab's
/// auto-hide marker is gone (reopened by it, a focus hook or the toggle), the
/// user snoozed the tab, the tab closed, or herdr stopped answering.
pub fn watch(tab: &str) -> std::io::Result<()> {
    if tab.is_empty() {
        return Err(std::io::Error::other("--autohide-watch needs a tab id"));
    }
    let dir = dir();
    let snooze_dir = crate::snooze::dir();
    let mut failures = 0;
    loop {
        std::thread::sleep(WATCH_EVERY);
        if !crate::snooze::is_set(&dir, tab) || crate::snooze::is_set(&snooze_dir, tab) {
            return Ok(());
        }
        let Ok(panes) = crate::ipc::call_text("pane.list", serde_json::json!({})) else {
            failures += 1;
            if failures >= WATCH_MAX_FAILURES {
                return Ok(());
            }
            continue;
        };
        failures = 0;
        if !crate::launch::live_tabs(&panes).contains(tab) {
            return Ok(());
        }
        // Docking into a background tab could move focus; the tab.focused
        // hook covers it when the user switches there.
        if crate::launch::focused_tab(&panes) != tab {
            continue;
        }
        let fp = crate::launch::focused_pane_in(&panes, tab);
        let Some((pane_id, _)) = fp.split_once('\t') else {
            continue;
        };
        let Some(width) =
            crate::ipc::call_text("pane.layout", serde_json::json!({ "pane_id": pane_id }))
                .ok()
                .and_then(|layout| crate::launch::layout_width(&layout))
        else {
            continue;
        };
        let threshold = crate::state::load_state().auto_hide_below;
        if !should_hide(threshold, width) {
            // The quiet hook re-checks everything (snooze, auto-open, width)
            // and clears the marker when it docks; if it skipped, for
            // example on a busy launch lock, the next tick retries.
            let _ = std::process::Command::new(std::env::current_exe()?)
                .arg("--ensure")
                .env("HERDR_TAB_ID", tab)
                .env_remove("HERDR_PLUGIN_EVENT_JSON")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_never_hides() {
        assert!(!should_hide(0, 1));
    }

    #[test]
    fn hides_at_or_below_the_threshold() {
        assert!(should_hide(80, 80));
        assert!(should_hide(80, 40));
        assert!(!should_hide(80, 81));
    }

    #[test]
    fn latch_hides_a_pane_that_started_wide() {
        let mut latch = Latch::new(80, Some(120));
        assert!(!latch.observe(80, 100));
        assert!(latch.observe(80, 80));
    }

    #[test]
    fn latch_keeps_a_pane_opened_in_a_narrow_tab_until_the_tab_widens() {
        let mut latch = Latch::new(80, Some(70));
        assert!(!latch.observe(80, 60));
        assert!(!latch.observe(80, 80));
        assert!(!latch.observe(80, 81));
        assert!(latch.observe(80, 80));
    }

    #[test]
    fn latch_without_a_layout_starts_armed() {
        let mut latch = Latch::new(80, None);
        assert!(latch.observe(80, 70));
    }

    #[test]
    fn turning_the_setting_on_applies_at_the_next_resize() {
        let mut latch = Latch::new(0, Some(70));
        assert!(!latch.observe(0, 60));
        assert!(latch.observe(80, 60));
    }
}
