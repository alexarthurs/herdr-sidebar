//! Sidebar ensure/toggle, driven entirely over the socket API (see `ipc`) so a
//! focus-event hook never spawns a console process. Unix actions/hooks call
//! this through the main binary; Windows uses the GUI-subsystem sidecar. The
//! decision/plan parsing is the unit-tested `launch` module, fed the socket
//! responses (same JSON the CLI prints).

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::{ipc, launch, state::View};

const REPLACE_ATTEMPTS_PER_WINDOW: u32 = 3;
const REPLACE_WINDOW_SECS: u64 = 60;

/// Why the native launcher was invoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A focus/create hook quietly ensures the Explorer exists.
    Ensure,
    /// An explicit user action toggles the requested view.
    Toggle(View),
    /// An explicit host keybinding opens or focuses a specific activity.
    Activate(Target),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Explorer,
    Search,
    SourceControl,
    QuickOpen,
}

impl Target {
    pub fn from_env_value(value: &str) -> Option<Self> {
        match value {
            "explorer" => Some(Self::Explorer),
            "search" => Some(Self::Search),
            "source-control" => Some(Self::SourceControl),
            "quick-open" => Some(Self::QuickOpen),
            _ => None,
        }
    }

    pub fn env_value(self) -> &'static str {
        match self {
            Self::Explorer => "explorer",
            Self::Search => "search",
            Self::SourceControl => "source-control",
            Self::QuickOpen => "quick-open",
        }
    }

    pub fn pane_view(self, merged: bool) -> View {
        match self {
            Self::SourceControl if !merged => View::SourceControl,
            _ => View::Explorer,
        }
    }

    pub fn initial_view(self) -> View {
        match self {
            Self::SourceControl => View::SourceControl,
            _ => View::Explorer,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Explorer => "f9",
            Self::Search => "f10",
            Self::SourceControl => "f11",
            Self::QuickOpen => "f12",
        }
    }
}

/// Serialize concurrent runs (pane/tab events arrive in bursts; unguarded,
/// one switch opened four panes). The OS releases this lock if a launcher
/// crashes, so no retry timer or stale-lock cleanup is needed.
pub struct LaunchLock {
    _file: File,
    _legacy: Option<File>,
}

impl LaunchLock {
    /// Acquire the shared launcher lock. Discrete user actions should wait;
    /// redundant focus hooks should use a non-blocking attempt and yield.
    pub fn acquire(wait: bool) -> Option<Self> {
        let dir = crate::rundir::dir("launch");
        crate::rundir::ensure_private(&dir).ok()?;
        let path = dir.join("launcher.lock");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        let acquired = if wait {
            file.lock().is_ok()
        } else {
            file.try_lock().is_ok()
        };
        if !acquired {
            return None;
        }
        // During one upgrade window, also take the pre-runtime-dir lock so
        // already-running old hooks and new hooks cannot launch concurrently.
        let legacy = if let Some(legacy_path) = crate::state::state_path()
            .map(|path| path.with_file_name("launcher.lock"))
            .filter(|legacy| *legacy != path)
        {
            match File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(legacy_path)
            {
                Ok(file) => {
                    let acquired = if wait {
                        file.lock().is_ok()
                    } else {
                        file.try_lock().is_ok()
                    };
                    if !acquired {
                        return None;
                    }
                    Some(file)
                }
                // Compatibility must not make the new private lock unusable
                // forever because a stale old path cannot be opened.
                Err(_) => None,
            }
        } else {
            None
        };
        Some(Self {
            _file: file,
            _legacy: legacy,
        })
    }
}

use crate::snooze;

/// One tab's sidebars, as the refresh found them.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RefreshTab {
    tab: String,
    /// (pane id, shows Source Control only)
    sidebars: Vec<(String, bool)>,
}

/// What the refresh will do: tabs it can restart, and tabs whose ONLY panes
/// are sidebars (closing them would close the tab, so they keep the old
/// build until the tab is next used and reported as such).
#[derive(Debug, Default, PartialEq, Eq)]
struct RefreshPlan {
    tabs: Vec<RefreshTab>,
    sidebar_only: Vec<(String, usize)>,
}

/// What a refresh did. `kept` sidebars did not close (a draft could not be
/// saved, or the TUI is not responding) and still run the old build; nothing
/// is ever killed to get past them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RefreshReport {
    pub refreshed: usize,
    pub kept: usize,
    pub errors: Vec<String>,
}

impl RefreshReport {
    pub fn is_complete(&self) -> bool {
        self.kept == 0 && self.errors.is_empty()
    }
}

/// The tabs to refresh: every tab holding a sidebar plus at least one other
/// pane (closing a tab's only pane would close the tab). Preview, editor,
/// inline and takeover viewers are never closed — they may hold unsaved
/// buffers — and simply get a fresh sidebar docked beside them.
fn refresh_plan(panes_json: &str) -> Result<RefreshPlan, String> {
    let value: serde_json::Value =
        serde_json::from_str(launch::strip_bom(panes_json)).map_err(|error| error.to_string())?;
    let panes = value["result"]["panes"]
        .as_array()
        .ok_or("missing pane list")?;
    let mut tabs = std::collections::BTreeMap::<String, Vec<(String, bool)>>::new();
    for pane in panes {
        let tokens = &pane["tokens"];
        if tokens[crate::viewer::METADATA_SOURCE].is_string() {
            continue;
        }
        let explorer = tokens[launch::METADATA_SOURCE].is_string();
        let git = tokens[launch::SC_METADATA_SOURCE].is_string();
        if (explorer || git)
            && let (Some(id), Some(tab)) = (pane["pane_id"].as_str(), pane["tab_id"].as_str())
        {
            tabs.entry(tab.into())
                .or_default()
                .push((id.into(), !explorer));
        }
    }
    let mut plan = RefreshPlan::default();
    for (tab, sidebars) in tabs {
        let has_other = panes.iter().any(|pane| {
            pane["tab_id"] == tab.as_str()
                && !sidebars
                    .iter()
                    .any(|(id, _)| pane["pane_id"] == id.as_str())
        });
        if has_other {
            plan.tabs.push(RefreshTab { tab, sidebars });
        } else {
            plan.sidebar_only.push((tab, sidebars.len()));
        }
    }
    Ok(plan)
}

/// Everything `refresh_with` needs from herdr, injectable for tests.
trait RefreshHost {
    fn list(&mut self) -> Result<String, String>;
    /// Ask a sidebar to save its drafts and close itself (Ctrl+Q). Never a
    /// hard close: even a pane that looked "starting" in an older snapshot may
    /// be running by now, and one that never reacts is simply kept.
    fn request_close(&mut self, pane: &str) -> Result<(), String>;
    fn open(&mut self, panes_json: &str, tab: &str, view: View) -> Result<(), String>;
    fn wait(&mut self);
}

/// 50 x 100ms: a generous bound for every sidebar to save and close.
const REFRESH_CLOSE_WAIT_STEPS: usize = 50;

fn refresh_with(host: &mut impl RefreshHost) -> Result<RefreshReport, String> {
    let plan = refresh_plan(&host.list()?)?;
    let mut report = RefreshReport::default();
    for (tab, count) in &plan.sidebar_only {
        report.kept += count;
        report.errors.push(format!(
            "{tab}: the sidebar is the tab's only pane; add another pane and retry"
        ));
    }
    // Ask every sidebar first, so they all save and close in parallel.
    let mut asked = Vec::new();
    for tab in &plan.tabs {
        for (id, git) in &tab.sidebars {
            match host.request_close(id) {
                Ok(()) => asked.push((tab.tab.clone(), id.clone(), *git)),
                Err(error) => {
                    report.kept += 1;
                    report.errors.push(format!("{}: {error}", tab.tab));
                }
            }
        }
    }
    // A bounded wait for them to close themselves. This is a one-off
    // maintenance action, not a focus hook, so a short wait is acceptable; a
    // sidebar that never closes is left running, never killed.
    let mut snapshot = host.list()?;
    for _ in 0..REFRESH_CLOSE_WAIT_STEPS {
        if asked.iter().all(|(_, id, _)| !pane_present(&snapshot, id)) {
            break;
        }
        host.wait();
        snapshot = host.list()?;
    }
    let old: Vec<&str> = asked.iter().map(|(_, id, _)| id.as_str()).collect();
    for tab in &plan.tabs {
        let mut views = std::collections::BTreeSet::new();
        for (tab_id, id, git) in asked.iter().filter(|(tab_id, ..)| *tab_id == tab.tab) {
            if pane_present(&snapshot, id) {
                report.kept += 1;
                report
                    .errors
                    .push(format!("{tab_id}: a sidebar kept running (unsaved draft?)"));
            } else {
                views.insert(*git);
            }
        }
        // Reopen per tab, independently: one failure never strands the rest.
        for git in views {
            let view = if git {
                View::SourceControl
            } else {
                View::Explorer
            };
            let opened = host
                .list()
                .and_then(|fresh| host.open(&fresh, &tab.tab, view));
            // `open` can return Ok without docking anything (no pane left in
            // scope, a split that came back empty): count only a sidebar that
            // is really there now.
            let verified = opened
                .and_then(|()| host.list())
                .map(|after| fresh_sidebar_in(&after, &tab.tab, view, &old));
            match verified {
                Ok(true) => report.refreshed += 1,
                Ok(false) => report
                    .errors
                    .push(format!("{}: no sidebar appeared after reopening", tab.tab)),
                Err(error) => report
                    .errors
                    .push(format!("{}: reopen failed: {error}", tab.tab)),
            }
        }
    }
    Ok(report)
}

/// A sidebar for `view` in `tab` that is not one of the panes just closed.
fn fresh_sidebar_in(panes_json: &str, tab: &str, view: View, old: &[&str]) -> bool {
    let token = match view {
        View::SourceControl => launch::SC_METADATA_SOURCE,
        View::Explorer => launch::METADATA_SOURCE,
    };
    serde_json::from_str::<serde_json::Value>(launch::strip_bom(panes_json))
        .ok()
        .and_then(|value| {
            value["result"]["panes"].as_array().map(|panes| {
                panes.iter().any(|pane| {
                    pane["tab_id"] == tab
                        && pane["tokens"][token].is_string()
                        && pane["pane_id"]
                            .as_str()
                            .is_some_and(|id| !old.contains(&id))
                })
            })
        })
        .unwrap_or(false)
}

/// Whether `pane_id` is in a `pane.list` snapshot. An unreadable snapshot
/// counts as present: never reopen over a pane that may still be there.
fn pane_present(panes_json: &str, pane_id: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(launch::strip_bom(panes_json))
        .ok()
        .and_then(|value| {
            value["result"]["panes"].as_array().map(|panes| {
                panes
                    .iter()
                    .any(|pane| pane["pane_id"].as_str() == Some(pane_id))
            })
        })
        .unwrap_or(true)
}

fn ipc_ok(response: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(launch::strip_bom(response)).map_err(|error| error.to_string())?;
    match value.get("error") {
        Some(error) => Err(error.to_string()),
        None => Ok(()),
    }
}

struct LiveRefresh;

impl RefreshHost for LiveRefresh {
    fn list(&mut self) -> Result<String, String> {
        ipc::call_text("pane.list", serde_json::json!({})).map_err(|error| error.to_string())
    }

    fn request_close(&mut self, pane: &str) -> Result<(), String> {
        let response = ipc::call_text(
            "pane.send_input",
            serde_json::json!({ "pane_id": pane, "text": "", "keys": ["ctrl+q"] }),
        )
        .map_err(|error| error.to_string())?;
        ipc_ok(&response)
    }

    fn open(&mut self, panes_json: &str, tab: &str, view: View) -> Result<(), String> {
        open(panes_json, false, tab, view, None, true).map_err(|error| error.to_string())
    }

    fn wait(&mut self) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Restart every docked sidebar on the installed build. Sidebars close
/// themselves gracefully (Ctrl+Q saves drafts first); preview, editor, inline
/// and takeover viewers are never touched.
pub fn refresh_all() -> std::io::Result<RefreshReport> {
    let _lock = LaunchLock::acquire(true)
        .ok_or_else(|| std::io::Error::other("could not lock sidebar refresh"))?;
    refresh_with(&mut LiveRefresh).map_err(std::io::Error::other)
}

/// Quiet mode (hooks): make sure the focused tab has an Explorer, never moving
/// focus, and respecting a tab the user toggled closed. Toggle mode (the
/// action): open-or-focus-or-close, like VS Code's explorer shortcut.
pub fn run(mode: Mode) -> std::io::Result<()> {
    let toggle = matches!(mode, Mode::Toggle(_));
    let activation = match mode {
        Mode::Activate(target) => Some(target),
        _ => None,
    };
    let explicit = toggle || activation.is_some();
    let state = crate::state::load_state();
    let view = match mode {
        Mode::Ensure => View::Explorer,
        Mode::Toggle(view) => view,
        Mode::Activate(target) => target.pane_view(state.merged),
    };
    // Auto-open off (⚙ Settings): hooks leave closed tabs alone; the user's
    // explicit toggle still works.
    if !explicit && !state.auto_open {
        return Ok(());
    }
    let event_json = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
    let creation_event = !explicit
        && matches!(
            launch::event_kind(&event_json).as_str(),
            "tab_created" | "workspace_created"
        );
    let wait_for_lock = must_wait_for_lock(explicit, &event_json);
    let Some(_lock) = LaunchLock::acquire(wait_for_lock) else {
        return Ok(());
    };
    let mut panes = ipc::call_text("pane.list", serde_json::json!({}))?;
    // The tab THIS event is about. During a workspace switch the globally
    // focused pane is still the space you came from, which docked sidebars
    // into the wrong project. A toggle is a deliberate act on the focused
    // tab, so it stays unscoped.
    let scope = if explicit {
        String::new()
    } else {
        let context_tab = std::env::var("HERDR_TAB_ID").unwrap_or_default();
        launch::event_scope_with_tab_context(&event_json, &panes, &context_tab)
    };
    let tab = snooze_tab_for_scope(&panes, &scope);
    if !tab.is_empty() {
        // Budgeted, and never touches a viewer that is still present.
        crate::takeover::recover_locked(&tab, &panes);
        panes = ipc::call_text("pane.list", serde_json::json!({}))?;
    }
    let snooze_dir = snooze::dir();
    let live_tabs = launch::live_tabs(&panes);
    snooze::migrate_legacy(&snooze_dir, &live_tabs);
    snooze::sweep(&snooze_dir, &live_tabs);
    let now = crate::state::unix_now();
    let decision_view = activation.map_or(view, |target| match target {
        Target::SourceControl => View::SourceControl,
        _ => View::Explorer,
    });
    let decision = match decision_view {
        View::Explorer => launch::launch_decision_in(&panes, now, &scope),
        View::SourceControl => launch::launch_decision_git(&panes, now),
    };
    let decision = if toggle && state.strict_toggle {
        launch::focus_as_close(&decision)
    } else {
        decision
    };
    let replace_dir = crate::rundir::dir("launch");
    sweep_replace_backoff(&replace_dir, now);
    let tracks_snooze = view == View::Explorer;
    match decision.split_once(' ') {
        Some(("FOCUS", id)) => {
            if toggle {
                focus(id)?;
            } else if let Some(target) = activation {
                activate_existing(id, target)?;
            }
        }
        Some(("CLOSE", id)) => {
            if toggle {
                // Set the marker BEFORE closing: if the quiet ensure hook's
                // very next focus event lands between the close and the
                // marker write, it re-docks a sidebar the user just asked
                // to close. An explicit toggle surfaces a marker failure
                // rather than closing into a state the hook won't respect.
                if tracks_snooze {
                    if tab.is_empty() {
                        return Err(std::io::Error::other(
                            "hide failed: could not resolve the sidebar tab",
                        ));
                    }
                    snooze::set(&snooze_dir, &tab)?;
                }
                if let Err(e) = request_close(&panes, id) {
                    // The close never happened; don't leave a stale marker
                    // snoozing a tab that still has its sidebar open.
                    if tracks_snooze {
                        let _ = snooze::clear(&snooze_dir, &tab);
                    }
                    return Err(e);
                }
            } else if let Some(target) = activation {
                activate_existing(id, target)?;
            }
        }
        Some(("REPLACE", id)) => {
            let replace_scope = {
                let tab = launch::tab_of(&panes, id);
                if tab.is_empty() { scope.clone() } else { tab }
            };
            if explicit {
                clear_replace_backoff(&replace_dir, &scope, decision_view);
                let tab = launch::tab_of(&panes, id);
                let workspace = launch::workspace_of(&panes, id);
                clear_replace_backoff(&replace_dir, &tab, decision_view);
                clear_replace_backoff(&replace_dir, &workspace, decision_view);
            } else {
                match replacement_allowed(&replace_dir, &replace_scope, decision_view, now) {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!(
                            "herdr-sidebar: suppressed repeated replacement in scope {scope:?}; \
                             use the sidebar action to retry"
                        );
                        return Ok(());
                    }
                    Err(error) => {
                        eprintln!(
                            "herdr-sidebar: replacement suppressed because retry state failed: \
                             {error}"
                        );
                        return Ok(());
                    }
                }
            }
            // A dead pane (stale heartbeat): close it and dock a fresh one,
            // quiet or toggle alike — a corpse should never block the dock.
            ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }))?;
            // Closing a focused corpse changes focus and invalidates its pane
            // id. Re-plan from a fresh snapshot rather than splitting a pane
            // that no longer exists.
            panes = ipc::call_text("pane.list", serde_json::json!({}))?;
            if let Some(target) = activation {
                prepare_activation(target);
                open(&panes, true, &scope, view, Some(target), creation_event)?;
            } else {
                open(
                    &panes,
                    toggle && state.focus_on_open,
                    &scope,
                    view,
                    None,
                    creation_event,
                )?;
            }
        }
        _ => {
            if toggle {
                if tracks_snooze {
                    // Opening is about to happen regardless of whether the
                    // marker clears; a stale marker here just means the next
                    // quiet hook wrongly leaves it closed, not a resource we
                    // need to fail loudly over.
                    let _ = snooze::clear(&snooze_dir, &tab);
                }
                // "Focus on open: off" (⚙ Settings) docks in the background:
                // open()'s quiet path already hands focus back after the swap.
                open(
                    &panes,
                    state.focus_on_open,
                    &scope,
                    view,
                    None,
                    creation_event,
                )?;
            } else if let Some(target) = activation {
                if tracks_snooze {
                    // Same best-effort reasoning as the toggle-open branch above.
                    let _ = snooze::clear(&snooze_dir, &tab);
                }
                prepare_activation(target);
                open(&panes, true, &scope, view, Some(target), creation_event)?;
            } else if !snooze::is_set(&snooze_dir, &tab) {
                open(&panes, false, &scope, view, None, creation_event)?;
            }
        }
    }
    Ok(())
}

pub fn request_close(panes_json: &str, pane_id: &str) -> std::io::Result<()> {
    if launch::pane_is_starting(panes_json, pane_id) {
        // No TUI event loop or in-memory draft exists yet.
        ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id }))?;
    } else {
        // Ctrl+Q is handled before overlays/focus modes. The TUI persists any
        // Source Control draft and closes its own pane; this launcher does not
        // poll for an acknowledgement.
        ipc::call_text(
            "pane.send_input",
            serde_json::json!({ "pane_id": pane_id, "text": "", "keys": ["ctrl+q"] }),
        )?;
    }
    Ok(())
}

fn replace_backoff_path(dir: &Path, scope: &str, view: View) -> PathBuf {
    let hash = view
        .token()
        .bytes()
        .chain(std::iter::once(0))
        .chain(scope.bytes())
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    dir.join(format!("replace-{hash:016x}.txt"))
}

fn replacement_allowed(dir: &Path, scope: &str, view: View, now: u64) -> std::io::Result<bool> {
    crate::rundir::ensure_private(dir)?;
    let path = replace_backoff_path(dir, scope, view);
    let (mut attempts, last) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| {
            let mut fields = text.split_whitespace();
            Some((
                fields.next()?.parse::<u32>().ok()?,
                fields.next()?.parse::<u64>().ok()?,
            ))
        })
        .unwrap_or((0, 0));
    if now.saturating_sub(last) >= REPLACE_WINDOW_SECS {
        attempts = 0;
    }
    if attempts >= REPLACE_ATTEMPTS_PER_WINDOW {
        return Ok(false);
    }
    std::fs::write(path, format!("{} {now}\n", attempts + 1))?;
    Ok(true)
}

fn clear_replace_backoff(dir: &Path, scope: &str, view: View) {
    if crate::rundir::is_private(dir) {
        let _ = std::fs::remove_file(replace_backoff_path(dir, scope, view));
    }
}

fn sweep_replace_backoff(dir: &Path, now: u64) {
    if !crate::rundir::is_private(dir) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_marker = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("replace-") && name.ends_with(".txt"));
        if !is_marker {
            continue;
        }
        let last = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.split_whitespace().nth(1)?.parse::<u64>().ok());
        if last.is_none_or(|last| now.saturating_sub(last) >= REPLACE_WINDOW_SECS * 2) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn snooze_tab_for_scope(panes_json: &str, scope: &str) -> String {
    if scope.contains(':') {
        scope.to_string()
    } else if scope.is_empty() {
        launch::focused_tab(panes_json)
    } else {
        String::new()
    }
}

fn focused_pane_id(panes_json: &str) -> String {
    launch::focused_pane_in(panes_json, "")
        .split_once('\t')
        .map(|(pane_id, _)| pane_id.to_string())
        .unwrap_or_default()
}

fn creation_focus_repair(
    panes_before: &str,
    panes_after: &str,
    new_sidebar: &str,
    displaced_by_swap: Option<&str>,
) -> Option<String> {
    let after = focused_pane_id(panes_after);
    if after != new_sidebar {
        return None;
    }
    if let Some(displaced) = displaced_by_swap.filter(|pane| !pane.is_empty()) {
        return Some(displaced.to_string());
    }
    let before = focused_pane_id(panes_before);
    (!before.is_empty() && before != new_sidebar).then_some(before)
}

fn focus(pane_id: &str) -> std::io::Result<()> {
    // The API has focus-by-id (`pane.focus`), unlike the CLI's zoom-cycle hack.
    ipc::call_text("pane.focus", serde_json::json!({ "pane_id": pane_id }))?;
    Ok(())
}

fn activate_existing(pane_id: &str, target: Target) -> std::io::Result<()> {
    prepare_activation(target);
    focus(pane_id)?;
    ipc::call_text(
        "pane.send_input",
        serde_json::json!({ "pane_id": pane_id, "text": "", "keys": [target.key()] }),
    )?;
    Ok(())
}

fn prepare_activation(target: Target) {
    crate::state::update_state(|state| match target {
        Target::Explorer | Target::QuickOpen => {
            state.active = View::Explorer;
            state.search_active = false;
        }
        Target::Search => {
            state.active = View::Explorer;
            state.search_active = true;
        }
        Target::SourceControl => {
            state.active = View::SourceControl;
            state.search_active = false;
        }
    });
}

fn open(
    panes_json: &str,
    focus_new: bool,
    scope: &str,
    view: View,
    initial: Option<Target>,
    creation_event: bool,
) -> std::io::Result<()> {
    // Root the new sidebar from a pane in the scope we are docking into —
    // the decision above answered for that scope, and the two must agree or
    // we dock into one tab with another tab's cwd.
    let fp = launch::focused_pane_in(panes_json, scope);
    let Some((fid, fcwd)) = fp.split_once('\t') else {
        return Ok(());
    };
    let state = crate::state::load_state();
    let dock_right = state.dock_right;
    let layout = ipc::call_text("pane.layout", serde_json::json!({ "pane_id": fid }))?;
    let plan = launch::open_plan(&layout, dock_right, state.sidebar_width);
    let mut fields = plan.split('\t');
    let target = fields
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(fid)
        .to_string();
    let ratio = fields
        .next()
        .and_then(|r| r.parse::<f64>().ok())
        .unwrap_or(if dock_right { 0.75 } else { 0.25 });
    let needs_swap = fields
        .next()
        .and_then(|s| s.parse::<bool>().ok())
        .unwrap_or(!dock_right);

    #[cfg(unix)]
    let new_pane = ipc::open_plugin_pane(
        &target,
        view,
        std::path::Path::new(fcwd),
        view == View::Explorer && state.merged,
        initial.map(Target::env_value),
    )?;
    #[cfg(windows)]
    let new_pane = {
        let mut split = serde_json::json!({
            "target_pane_id": target,
            "direction": "right",
            "ratio": ratio,
            "focus": false,
        });
        if !fcwd.is_empty() {
            split["cwd"] = serde_json::Value::String(fcwd.to_string());
        }
        let mut env = crate::state::spawn_env();
        if let Some(initial) = initial
            && let Some(env) = env.as_object_mut()
        {
            env.insert(
                crate::state::INITIAL_ACTIVITY_ENV.to_string(),
                serde_json::Value::String(initial.env_value().to_string()),
            );
        }
        split["env"] = env;
        let response = ipc::call_text("pane.split", split)?;
        let Some(new_pane) = launch::split_pane_id(&response) else {
            return Ok(());
        };
        if let Err(error) =
            ipc::report_starting_identity(&new_pane, view, view == View::Explorer && state.merged)
        {
            let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": new_pane }));
            return Err(error);
        }
        new_pane
    };

    if needs_swap {
        ipc::call_text(
            "pane.swap",
            serde_json::json!({ "source_pane_id": new_pane, "target_pane_id": target }),
        )?;
    }
    #[cfg(unix)]
    resize_direct_spawn(&new_pane, dock_right, ratio);
    #[cfg(windows)]
    {
        let command = if view == View::Explorer {
            crate::state::EXECUTABLE_NAME.to_string()
        } else {
            format!(
                "{} --view {}",
                crate::state::EXECUTABLE_NAME,
                view.view_flag()
            )
        };
        ipc::call_text(
            "pane.send_input",
            serde_json::json!({
                "pane_id": new_pane,
                "text": command,
                "keys": ["Enter"]
            }),
        )?;
        ipc::call_text(
            "pane.rename",
            serde_json::json!({ "pane_id": new_pane, "label": view.label() }),
        )?;
    }
    full_height_repair(&new_pane, dock_right);

    if focus_new {
        focus(&new_pane)?;
    } else if creation_event {
        // Background creation must not steal focus, but an intended focus may
        // land while this hook is docking (notably a preview tab's explicit
        // focus transition). Re-read and repair only when the layout operation
        // itself left focus on the new sidebar pane; otherwise do nothing.
        if let Ok(after) = ipc::call_text("pane.list", serde_json::json!({}))
            && let Some(previous) = creation_focus_repair(
                panes_json,
                &after,
                &new_pane,
                needs_swap.then_some(target.as_str()),
            )
        {
            focus(&previous)?;
        }
    } else {
        // Focus events are scoped to the tab/workspace the client is moving
        // into, even while the global pane snapshot still points at the one it
        // left. Restore that scoped pane, not the stale global one.
        focus(fid)?;
    }
    Ok(())
}

/// `plugin.pane.open` starts a direct process but currently exposes no split
/// ratio. It creates a 50/50 split; after any left-dock swap, move the TUI's
/// interior edge to the ratio already computed by `open_plan`.
#[cfg(unix)]
fn resize_direct_spawn(pane_id: &str, dock_right: bool, ratio: f64) {
    let amount = (ratio - 0.5).abs();
    if amount < 0.005 {
        return;
    }
    let direction = if dock_right { "right" } else { "left" };
    let _ = ipc::call_text(
        "pane.resize",
        serde_json::json!({
            "pane_id": pane_id,
            "direction": direction,
            "amount": amount,
        }),
    );
}

/// Grow the freshly-opened explorer into a full-height edge column. When the
/// tab's chosen edge was already split vertically, the explorer only gets the
/// top slot; each repair step re-parents the pane below it as a down-split of
/// the pane beside it. herdr no-ops same-tab moves, so each step bounces the
/// pane through a temporary tab (herdr auto-closes it once emptied).
/// Best-effort: any miss just leaves the layout as it was.
fn full_height_repair(pane_id: &str, dock_right: bool) {
    for _ in 0..4 {
        let Ok(layout) = ipc::call_text("pane.layout", serde_json::json!({ "pane_id": pane_id }))
        else {
            return;
        };
        let Some(step) = launch::repair_step(&layout, pane_id, dock_right) else {
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
                    "target_pane_id": step.beside,
                    "split": "down",
                },
                "focus": false,
            }),
        );
    }
}

fn must_wait_for_lock(explicit: bool, event_json: &str) -> bool {
    explicit || launch::event_kind(event_json) == "tab_created"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simulated herdr for `refresh_with`: panes are (id, tab, tokens).
    /// Sidebars in `closes` save and close themselves a few waits after
    /// Ctrl+Q; everything else ignores it (unsaved draft, hung TUI, shell).
    #[derive(Default)]
    struct FakeRefresh {
        panes: Vec<(String, String, serde_json::Value)>,
        closes: std::collections::BTreeMap<String, usize>,
        asked: Vec<String>,
        refuse_input: Vec<String>,
        open_noop: Vec<String>,
        open_fails: Vec<String>,
        next: usize,
    }

    impl FakeRefresh {
        fn pane(&mut self, id: &str, tab: &str, tokens: serde_json::Value) {
            self.panes.push((id.into(), tab.into(), tokens));
        }
        fn has(&self, id: &str) -> bool {
            self.panes.iter().any(|(pane, ..)| pane == id)
        }
    }

    impl RefreshHost for FakeRefresh {
        fn list(&mut self) -> Result<String, String> {
            let panes: Vec<_> = self
                .panes
                .iter()
                .map(|(id, tab, tokens)| serde_json::json!({"pane_id": id, "tab_id": tab, "tokens": tokens}))
                .collect();
            Ok(serde_json::json!({"result": {"panes": panes}}).to_string())
        }

        fn request_close(&mut self, pane: &str) -> Result<(), String> {
            if self.refuse_input.iter().any(|id| id == pane) {
                return Err("input rejected".into());
            }
            self.asked.push(pane.into());
            Ok(())
        }

        fn open(&mut self, _: &str, tab: &str, view: View) -> Result<(), String> {
            if self.open_fails.iter().any(|t| t == tab) {
                return Err("split failed".into());
            }
            if self.open_noop.iter().any(|t| t == tab) {
                return Ok(()); // like `open` when nothing is in scope
            }
            self.next += 1;
            let token = match view {
                View::Explorer => launch::METADATA_SOURCE,
                View::SourceControl => launch::SC_METADATA_SOURCE,
            };
            let id = format!("new{}", self.next);
            self.pane(&id, tab, serde_json::json!({ token: "1" }));
            Ok(())
        }

        fn wait(&mut self) {
            let asked = self.asked.clone();
            for id in asked {
                if let Some(left) = self.closes.get_mut(&id) {
                    if *left == 0 {
                        self.panes.retain(|(pane, ..)| *pane != id);
                    } else {
                        *left -= 1;
                    }
                }
            }
        }
    }

    fn sidebar() -> serde_json::Value {
        serde_json::json!({ launch::METADATA_SOURCE: "1", launch::SC_METADATA_SOURCE: "1" })
    }

    #[test]
    fn refresh_closes_sidebars_gracefully_and_never_touches_viewers() {
        let mut host = FakeRefresh::default();
        host.pane("s1", "w1:t1", sidebar());
        host.pane("work", "w1:t1", serde_json::json!({}));
        // An inline / takeover viewer with possibly unsaved edits.
        host.pane("s2", "w1:t2", sidebar());
        host.pane(
            "viewer",
            "w1:t2",
            serde_json::json!({ crate::viewer::METADATA_SOURCE: "1", "hs-preview-inline": "1" }),
        );
        host.closes.insert("s1".into(), 2);
        host.closes.insert("s2".into(), 0);
        let report = refresh_with(&mut host).unwrap();
        assert_eq!(host.asked, vec!["s1", "s2"], "every sidebar got Ctrl+Q");
        assert!(host.has("viewer") && host.has("work"));
        assert_eq!(report.refreshed, 2);
        assert!(report.is_complete(), "{report:?}");
    }

    /// A sidebar that does not close (its draft could not be saved, or it is
    /// hung) keeps running: it is reported, not killed, and not duplicated.
    #[test]
    fn a_sidebar_that_will_not_close_is_kept_and_reported() {
        let mut host = FakeRefresh::default();
        host.pane("busy", "w1:t1", sidebar());
        host.pane("work", "w1:t1", serde_json::json!({}));
        let report = refresh_with(&mut host).unwrap();
        assert!(host.has("busy"));
        assert_eq!(report.refreshed, 0);
        assert_eq!(report.kept, 1);
        assert!(!report.is_complete());
        assert_eq!(host.next, 0, "no second sidebar docked beside it");
    }

    /// Even a pane whose snapshot said "starting" only gets Ctrl+Q: by the
    /// time the request lands it may be a running TUI with state.
    #[test]
    fn starting_sidebars_are_asked_not_killed() {
        let mut host = FakeRefresh::default();
        let mut tokens = sidebar();
        tokens[launch::STARTING_TOKEN] = serde_json::json!("1");
        host.pane("young", "w1:t1", tokens);
        host.pane("work", "w1:t1", serde_json::json!({}));
        let report = refresh_with(&mut host).unwrap();
        assert_eq!(host.asked, vec!["young"]);
        assert!(host.has("young"), "never hard-closed");
        assert_eq!(report.kept, 1);
    }

    #[test]
    fn sidebar_only_tabs_are_reported_not_counted_as_refreshed() {
        let mut host = FakeRefresh::default();
        host.pane("alone", "w1:t9", sidebar());
        let report = refresh_with(&mut host).unwrap();
        assert!(host.asked.is_empty(), "closing it would close the tab");
        assert_eq!(report.kept, 1);
        assert!(!report.is_complete());
        assert!(report.errors[0].contains("only pane"), "{report:?}");
    }

    /// `open` can succeed without docking anything; only a sidebar that is
    /// really present counts, and one tab's failure never stops the others.
    #[test]
    fn reopen_is_verified_per_tab_and_failures_stay_local() {
        let mut host = FakeRefresh::default();
        for tab in ["w1:a", "w1:b", "w1:c"] {
            host.pane(&format!("s-{tab}"), tab, sidebar());
            host.pane(&format!("w-{tab}"), tab, serde_json::json!({}));
            host.closes.insert(format!("s-{tab}"), 0);
        }
        host.open_noop.push("w1:a".into());
        host.open_fails.push("w1:b".into());
        let report = refresh_with(&mut host).unwrap();
        assert_eq!(report.refreshed, 1, "only w1:c really got a sidebar");
        assert_eq!(report.errors.len(), 2, "{report:?}");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("no sidebar appeared"))
        );
        assert!(report.errors.iter().any(|e| e.contains("reopen failed")));
        assert!(!report.is_complete());
    }

    #[test]
    fn separated_panes_reopen_both_views() {
        let mut host = FakeRefresh::default();
        host.pane(
            "explorer",
            "w1:t1",
            serde_json::json!({ launch::METADATA_SOURCE: "1" }),
        );
        host.pane(
            "git",
            "w1:t1",
            serde_json::json!({ launch::SC_METADATA_SOURCE: "1" }),
        );
        host.pane("work", "w1:t1", serde_json::json!({}));
        host.closes.insert("explorer".into(), 0);
        host.closes.insert("git".into(), 0);
        let report = refresh_with(&mut host).unwrap();
        assert_eq!(report.refreshed, 2);
        assert!(report.is_complete());
    }

    #[test]
    fn snooze_set_clear_and_sweep() {
        let dir = std::env::temp_dir().join(format!("aa-ft-snooze-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        snooze::set(&dir, "w1:t1").unwrap();
        snooze::set(&dir, "w1:t2").unwrap();
        assert!(snooze::is_set(&dir, "w1:t1"));
        assert!(!snooze::is_set(&dir, "w1:t9"));
        assert!(!snooze::is_set(&dir, ""), "empty tab id never snoozes");

        snooze::clear(&dir, "w1:t1").unwrap();
        assert!(!snooze::is_set(&dir, "w1:t1"));

        // Sweep drops markers for tabs that no longer exist.
        let live = std::collections::BTreeSet::from(["w1:t3".to_string()]);
        snooze::sweep(&dir, &live);
        assert!(!snooze::is_set(&dir, "w1:t2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_events_do_not_borrow_the_globally_focused_tabs_snooze() {
        let panes = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","focused":true},
            {"pane_id":"w2:p1","tab_id":"w2:t1"}
        ]}}"#;
        assert_eq!(snooze_tab_for_scope(panes, ""), "w1:t1");
        assert_eq!(snooze_tab_for_scope(panes, "w2:t1"), "w2:t1");
        assert_eq!(snooze_tab_for_scope(panes, "w2"), "");
    }

    #[test]
    fn creation_focus_repair_only_undoes_focus_stolen_by_the_new_sidebar() {
        let before = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","focused":true,"cwd":"/one"},
            {"pane_id":"w2:p1","tab_id":"w2:t1","workspace_id":"w2","focused":false,"cwd":"/two"}
        ]}}"#;
        let scoped = launch::focused_pane_in(before, "w2:t1");
        let (scoped_pane, _) = scoped.split_once('\t').unwrap();
        assert_eq!(scoped_pane, "w2:p1", "the dock still roots in the new tab");

        let sidebar_stole_focus = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","focused":false},
            {"pane_id":"w2:p1","tab_id":"w2:t1","workspace_id":"w2","focused":false},
            {"pane_id":"w2:p2","tab_id":"w2:t1","workspace_id":"w2","focused":true}
        ]}}"#;
        assert_eq!(
            creation_focus_repair(before, sidebar_stole_focus, "w2:p2", None).as_deref(),
            Some("w1:p1")
        );

        let intended_preview_focus = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","focused":false},
            {"pane_id":"w2:p1","tab_id":"w2:t1","workspace_id":"w2","focused":true},
            {"pane_id":"w2:p2","tab_id":"w2:t1","workspace_id":"w2","focused":false}
        ]}}"#;
        assert_eq!(
            creation_focus_repair(before, intended_preview_focus, "w2:p2", Some("w2:p1")),
            None,
            "a concurrent intended focus must never be snapped back"
        );

        assert_eq!(
            creation_focus_repair(before, sidebar_stole_focus, "w2:p2", Some("w2:p1")).as_deref(),
            Some("w2:p1"),
            "after a swap, restore the displaced pane rather than stale global focus"
        );
    }

    #[test]
    fn repeated_replacements_stop_until_the_retry_window_expires() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-replace-backoff-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let now = 10_000;
        for _ in 0..REPLACE_ATTEMPTS_PER_WINDOW {
            assert!(replacement_allowed(&dir, "w2:t1", View::Explorer, now).unwrap());
        }
        assert!(!replacement_allowed(&dir, "w2:t1", View::Explorer, now).unwrap());
        assert!(
            replacement_allowed(&dir, "w2:t1", View::Explorer, now + REPLACE_WINDOW_SECS).unwrap()
        );
        clear_replace_backoff(&dir, "w2:t1", View::Explorer);
        assert!(replacement_allowed(&dir, "w2:t1", View::Explorer, now).unwrap());
        sweep_replace_backoff(&dir, now + REPLACE_WINDOW_SECS * 2);
        assert!(!replace_backoff_path(&dir, "w2:t1", View::Explorer).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discrete_tab_creation_and_manual_toggles_wait_for_the_lock() {
        assert!(must_wait_for_lock(false, r#"{"event":"tab_created"}"#));
        assert!(must_wait_for_lock(false, r#"{"event":"tab.created"}"#));
        assert!(must_wait_for_lock(true, ""));
        assert!(!must_wait_for_lock(false, r#"{"event":"tab_focused"}"#));
    }

    #[test]
    fn direct_activities_use_the_unified_pane_and_stable_keys() {
        for (target, value, key) in [
            (Target::Explorer, "explorer", "f9"),
            (Target::Search, "search", "f10"),
            (Target::SourceControl, "source-control", "f11"),
            (Target::QuickOpen, "quick-open", "f12"),
        ] {
            assert_eq!(Target::from_env_value(value), Some(target));
            assert_eq!(target.env_value(), value);
            assert_eq!(target.key(), key);
            assert_eq!(target.pane_view(true), View::Explorer);
        }
        assert_eq!(Target::SourceControl.pane_view(false), View::SourceControl);
        assert_eq!(Target::SourceControl.initial_view(), View::SourceControl);
        assert_eq!(Target::from_env_value("unknown"), None);
    }
}
