//! Per-tab "the user closed/hid the sidebar here" markers: hide (« or b) and
//! the toggle CLOSE write one, the quiet ensure hook honors it — otherwise the
//! very next focus event would reopen what the user just closed. Toggle OPEN
//! clears it. Markers for tabs that no longer exist are swept each ensure run
//! (tab ids can be recycled).

use std::path::PathBuf;

pub fn dir() -> PathBuf {
    std::env::temp_dir().join("herdr-sidebar-snooze")
}

fn marker(dir: &std::path::Path, tab: &str) -> PathBuf {
    dir.join(tab.replace(':', "_"))
}

pub fn set(dir: &std::path::Path, tab: &str) {
    if !tab.is_empty() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(marker(dir, tab), b"");
    }
}

pub fn clear(dir: &std::path::Path, tab: &str) {
    if !tab.is_empty() {
        let _ = std::fs::remove_file(marker(dir, tab));
    }
}

pub fn is_set(dir: &std::path::Path, tab: &str) -> bool {
    !tab.is_empty() && marker(dir, tab).exists()
}

pub fn sweep(dir: &std::path::Path, live_tabs: &std::collections::BTreeSet<String>) {
    let live: std::collections::BTreeSet<String> =
        live_tabs.iter().map(|t| t.replace(':', "_")).collect();
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if !live.contains(&entry.file_name().to_string_lossy().into_owned()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
