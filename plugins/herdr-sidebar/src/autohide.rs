//! Auto-hide in narrow tabs: the counterpart of herdr's own
//! `ui.mobile_width_threshold` for this plugin's pane. When the whole tab
//! area is at or below the configured column count, a docked sidebar closes
//! itself; the quiet ensure hook declines to dock one into a tab that narrow
//! and docks it again at the first focus event once the tab is wider.
//!
//! There is deliberately no marker and no watcher process. Plugins cannot
//! hook herdr's `layout.updated`, so coming back "on its own" would need a
//! detached poller per hidden tab; the width check in the ensure hook is
//! stateless, works on every platform, and a user snooze (« / b) still wins
//! because the hook checks it first.

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

/// Whether `pane_id` shares its tab with any other pane. Closing the last
/// pane closes the whole tab, which auto-hide must never do (a sidebar left
/// alone in a preview tab, for example).
pub fn has_company(pane_list_json: &str, pane_id: &str) -> bool {
    let tab = crate::launch::tab_of(pane_list_json, pane_id);
    !tab.is_empty() && crate::launch::tab_pane_count(pane_list_json, &tab) > 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sidebar_alone_in_its_tab_never_auto_hides() {
        let list = |panes: &str| format!(r#"{{"result":{{"panes":[{panes}]}}}}"#);
        let alone =
            list(r#"{"pane_id":"w1:p3","tab_id":"w1:t2"},{"pane_id":"w1:p1","tab_id":"w1:t1"}"#);
        assert!(!has_company(&alone, "w1:p3"));
        let shared =
            list(r#"{"pane_id":"w1:p3","tab_id":"w1:t1"},{"pane_id":"w1:p1","tab_id":"w1:t1"}"#);
        assert!(has_company(&shared, "w1:p3"));
        assert!(!has_company("not json", "w1:p3"));
        assert!(
            !has_company(&shared, "w1:p9"),
            "an unknown pane counts as alone"
        );
    }

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
