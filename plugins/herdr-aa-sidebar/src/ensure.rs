//! Sidebar ensure/toggle, driven entirely over the socket API (see `ipc`) so a
//! focus-event hook never spawns a console process. Mirrors the unix shell
//! scripts' flow; the decision/plan parsing is the unit-tested `launch` module,
//! fed the socket responses (same JSON the CLI prints).

use std::path::PathBuf;

use crate::{ipc, launch};

/// Serialize concurrent runs (focus events arrive in bursts — tab.focused +
/// workspace.focused per switch; unguarded, one switch opened four panes).
/// Losing the race skips this run; the next event re-fires it.
struct Lock(PathBuf);

impl Lock {
    fn acquire() -> Option<Self> {
        let dir = std::env::temp_dir().join("herdr-aa-sidebar-ensure.lock");
        if std::fs::create_dir(&dir).is_ok() {
            return Some(Self(dir));
        }
        // Break locks older than 30s (a crashed run), otherwise yield.
        let stale = std::fs::metadata(&dir)
            .and_then(|m| m.created().or_else(|_| m.modified()))
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 30);
        if !stale {
            return None;
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).ok().map(|_| Self(dir))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// Per-tab "the user closed the sidebar here" markers: the toggle CLOSE writes
/// one, the quiet ensure honors it — otherwise the very next focus event would
/// reopen what the user just closed. Toggle OPEN clears it. Markers for tabs
/// that no longer exist are swept each run (tab ids can be recycled).
mod snooze {
    use std::path::PathBuf;

    pub fn dir() -> PathBuf {
        std::env::temp_dir().join("herdr-aa-sidebar-snooze")
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
}

/// Quiet mode (hooks): make sure the focused tab has an Explorer, never moving
/// focus, and respecting a tab the user toggled closed. Toggle mode (the
/// action): open-or-focus-or-close, like VS Code's explorer shortcut.
pub fn run(toggle: bool) -> std::io::Result<()> {
    let Some(_lock) = Lock::acquire() else {
        return Ok(());
    };
    let panes = ipc::call_text("pane.list", serde_json::json!({}))?;
    let tab = launch::focused_tab(&panes);
    let snooze_dir = snooze::dir();
    snooze::sweep(&snooze_dir, &launch::live_tabs(&panes));
    let now = crate::state::unix_now();
    match launch::launch_decision(&panes, now).split_once(' ') {
        Some(("FOCUS", id)) => {
            if toggle {
                focus(id)?;
            }
        }
        Some(("CLOSE", id)) => {
            if toggle {
                ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }))?;
                snooze::set(&snooze_dir, &tab);
            }
        }
        Some(("REPLACE", id)) => {
            // A dead pane (stale heartbeat): close it and dock a fresh one,
            // quiet or toggle alike — a corpse should never block the dock.
            ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }))?;
            open(&panes, toggle)?;
        }
        _ => {
            if toggle {
                snooze::clear(&snooze_dir, &tab);
                open(&panes, true)?;
            } else if !snooze::is_set(&snooze_dir, &tab) {
                open(&panes, false)?;
            }
        }
    }
    Ok(())
}

fn focus(pane_id: &str) -> std::io::Result<()> {
    // The API has focus-by-id (`pane.focus`), unlike the CLI's zoom-cycle hack.
    ipc::call_text("pane.focus", serde_json::json!({ "pane_id": pane_id }))?;
    Ok(())
}

fn open(panes_json: &str, focus_new: bool) -> std::io::Result<()> {
    let fp = launch::focused_pane(panes_json);
    let Some((fid, fcwd)) = fp.split_once('\t') else {
        return Ok(());
    };

    let layout = ipc::call_text("pane.layout", serde_json::json!({ "pane_id": fid }))?;
    let plan = launch::open_plan(&layout);
    let (target, ratio) = plan
        .split_once('\t')
        .map(|(t, r)| (t.to_string(), r.parse::<f64>().unwrap_or(0.25)))
        .unwrap_or_else(|| (fid.to_string(), 0.25));

    let mut split = serde_json::json!({
        "target_pane_id": target,
        "direction": "right",
        "ratio": ratio,
        "focus": false,
    });
    if !fcwd.is_empty() {
        split["cwd"] = serde_json::Value::String(fcwd.to_string());
    }
    let response = ipc::call_text("pane.split", split)?;
    let Some(new_pane) = launch::split_pane_id(&response) else {
        return Ok(());
    };

    ipc::call_text(
        "pane.swap",
        serde_json::json!({ "source_pane_id": new_pane, "target_pane_id": target }),
    )?;
    if let Some(command) = explorer_command() {
        ipc::call_text(
            "pane.send_input",
            serde_json::json!({ "pane_id": new_pane, "text": command, "keys": ["Enter"] }),
        )?;
    }
    ipc::call_text(
        "pane.rename",
        serde_json::json!({ "pane_id": new_pane, "label": launch::PANE_LABEL }),
    )?;
    full_height_repair(&new_pane);

    if focus_new {
        focus(&new_pane)?;
    } else {
        // Quiet mode must never move focus, but the split/swap can (focus
        // follows the SLOT, not the pane) — unconditionally restore the pane
        // that was focused when we started.
        focus(fid)?;
    }
    Ok(())
}

/// Grow the freshly-opened explorer into a full-height left column. When the
/// tab's left area was already split vertically, the explorer only gets the
/// top slot; each repair step re-parents the pane below it as a down-split of
/// the pane beside it. herdr no-ops same-tab moves, so each step bounces the
/// pane through a temporary tab (herdr auto-closes it once emptied).
/// Best-effort: any miss just leaves the layout as it was.
fn full_height_repair(pane_id: &str) {
    for _ in 0..4 {
        let Ok(layout) =
            ipc::call_text("pane.layout", serde_json::json!({ "pane_id": pane_id }))
        else {
            return;
        };
        let Some(step) = launch::repair_step(&layout, pane_id) else {
            return;
        };
        let bounced = ipc::call_text(
            "pane.move",
            serde_json::json!({
                "pane_id": step.below,
                "destination": { "type": "new_tab" },
                "focus": false,
            }),
        );
        if bounced.is_err() {
            return;
        }
        let _ = ipc::call_text(
            "pane.move",
            serde_json::json!({
                "pane_id": step.below,
                "destination": {
                    "type": "tab",
                    "tab_id": step.tab,
                    "target_pane_id": step.right,
                    "split": "down",
                },
                "focus": false,
            }),
        );
    }
}

/// The shell command that starts the Explorer TUI in the new pane: the sibling
/// binary next to this sidecar, quoted for the pane's shell.
fn explorer_command() -> Option<String> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    #[cfg(windows)]
    {
        let exe = dir.join("herdr-aa-sidebar.exe");
        Some(format!("& \"{}\"", exe.display()))
    }
    #[cfg(not(windows))]
    {
        let exe = dir.join("herdr-aa-sidebar");
        Some(format!("exec \"{}\"", exe.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snooze_set_clear_and_sweep() {
        let dir = std::env::temp_dir().join(format!("aa-ft-snooze-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        snooze::set(&dir, "w1:t1");
        snooze::set(&dir, "w1:t2");
        assert!(snooze::is_set(&dir, "w1:t1"));
        assert!(!snooze::is_set(&dir, "w1:t9"));
        assert!(!snooze::is_set(&dir, ""), "empty tab id never snoozes");

        snooze::clear(&dir, "w1:t1");
        assert!(!snooze::is_set(&dir, "w1:t1"));

        // Sweep drops markers for tabs that no longer exist.
        let live = std::collections::BTreeSet::from(["w1:t3".to_string()]);
        snooze::sweep(&dir, &live);
        assert!(!snooze::is_set(&dir, "w1:t2"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
