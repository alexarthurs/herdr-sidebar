//! Sidebar ensure/toggle, driven entirely over the socket API (see `ipc`) so a
//! focus-event hook never spawns a console process. Unix actions/hooks call
//! this through the main binary; Windows uses the GUI-subsystem sidecar. The
//! decision/plan parsing is the unit-tested `launch` module, fed the socket
//! responses (same JSON the CLI prints).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{autohide, ipc, launch, state::View};

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
    /// A pane exited or closed (`pane.exited` / `pane.closed` hooks): close
    /// every tab left holding only sidebars, the way herdr closes a tab
    /// whose last pane exits (issue #108).
    Reap,
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
        send_ctrl_q(pane)
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

/// Ask a running sidebar TUI to save its drafts and close its own pane.
fn send_ctrl_q(pane: &str) -> Result<(), String> {
    let response = ipc::call_text(
        "pane.send_input",
        serde_json::json!({ "pane_id": pane, "text": "", "keys": ["ctrl+q"] }),
    )
    .map_err(|error| error.to_string())?;
    ipc_ok(&response)
}

/// Everything the lonely-tab reap needs from herdr, injectable for tests.
trait ReapHost {
    fn list(&mut self) -> Result<String, String>;
    /// Tabs a takeover journal still owes a restore (`takeover::journaled_tabs`).
    fn journaled_tabs(&mut self) -> std::collections::BTreeSet<String>;
    /// Ctrl+Q: the TUI saves drafts and closes its own pane, or stays open
    /// with its error notice when a draft cannot be saved.
    fn request_close(&mut self, pane: &str) -> Result<(), String>;
    /// `pane.close`, only for panes that cannot hold TUI state.
    fn close(&mut self, pane: &str) -> Result<(), String>;
}

#[derive(Debug, PartialEq, Eq)]
enum ReapScope {
    Tab(String),
    Workspace(String),
}

impl ReapScope {
    fn from_event(event: &str, context_tab: &str) -> Option<Self> {
        let scope = launch::event_scope(event);
        match launch::event_kind(event).as_str() {
            "pane_exited" => {
                let tab = launch::event_scope_with_tab_context(event, "", context_tab);
                tab.contains(':').then_some(Self::Tab(tab))
            }
            "pane_closed" if !scope.is_empty() && !scope.contains(':') => {
                Some(Self::Workspace(scope))
            }
            _ => None,
        }
    }

    fn contains(&self, tab: &str) -> bool {
        match self {
            Self::Tab(id) => tab == id,
            Self::Workspace(id) => tab.split_once(':').is_some_and(|(ws, _)| ws == id),
        }
    }
}

/// Ask sidebars in the removal scope to close. Closing the last pane closes
/// the tab; nothing waits for a running sidebar to acknowledge Ctrl+Q.
fn reap_with(host: &mut impl ReapHost, scope: &ReapScope) -> Result<usize, String> {
    let lonely: Vec<_> = launch::lonely_sidebar_tabs(&host.list()?)
        .into_iter()
        .filter(|tab| scope.contains(&tab.tab))
        .collect();
    if lonely.is_empty() {
        return Ok(0);
    }
    let journaled = host.journaled_tabs();
    let mut asked = 0;
    for tab in lonely.iter().filter(|tab| !journaled.contains(&tab.tab)) {
        for (pane, _) in &tab.panes {
            // A shell could have arrived, or a starting pane could have
            // become a live TUI since the candidate snapshot.
            let current = launch::lonely_sidebar_tabs(&host.list()?);
            let Some((_, how)) = current
                .iter()
                .find(|candidate| candidate.tab == tab.tab)
                .and_then(|candidate| candidate.panes.iter().find(|(id, _)| id == pane))
            else {
                continue;
            };
            let result = match how {
                launch::ReapClose::Graceful => host.request_close(pane),
                launch::ReapClose::Direct => host.close(pane),
            };
            if result.is_ok() {
                asked += 1;
            }
        }
    }
    Ok(asked)
}

struct LiveReap;

impl ReapHost for LiveReap {
    fn list(&mut self) -> Result<String, String> {
        ipc::call_text("pane.list", serde_json::json!({})).map_err(|error| error.to_string())
    }

    fn journaled_tabs(&mut self) -> std::collections::BTreeSet<String> {
        crate::takeover::journaled_tabs()
    }

    fn request_close(&mut self, pane: &str) -> Result<(), String> {
        send_ctrl_q(pane)
    }

    fn close(&mut self, pane: &str) -> Result<(), String> {
        let response = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane }))
            .map_err(|error| error.to_string())?;
        ipc_ok(&response)
    }
}

/// `--reap` (the `pane.exited` / `pane.closed` hooks). A discrete event, so
/// it WAITS for the launcher lock instead of yielding like a focus hook: a
/// skipped reap would leave the tab until some unrelated pane next closes.
/// Holding the lock also means it never sees a tab our own launch, refresh
/// or takeover is in the middle of rearranging.
fn reap() -> std::io::Result<()> {
    let event = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
    let context_tab = std::env::var("HERDR_TAB_ID").unwrap_or_default();
    let Some(scope) = ReapScope::from_event(&event, &context_tab) else {
        return Ok(());
    };
    if !crate::state::load_state().close_lonely_tab {
        return Ok(());
    }
    let Some(_lock) = LaunchLock::acquire(true) else {
        return Ok(());
    };
    reap_with(&mut LiveReap, &scope)
        .map(drop)
        .map_err(std::io::Error::other)
}

/// Quiet mode (hooks): make sure the focused tab has an Explorer, never moving
/// focus, and respecting a tab the user toggled closed. Toggle mode (the
/// action): open-or-focus-or-close, like VS Code's explorer shortcut.
pub fn run(mode: Mode) -> std::io::Result<()> {
    if mode == Mode::Reap {
        return reap();
    }
    let toggle = matches!(mode, Mode::Toggle(_));
    let activation = match mode {
        Mode::Activate(target) => Some(target),
        _ => None,
    };
    let explicit = toggle || activation.is_some();
    let event_json = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
    let event_kind = launch::event_kind(&event_json);
    if mode == Mode::Ensure {
        match event_kind.as_str() {
            "pane_moved" => return crate::focus::publish_move(&event_json),
            "pane_focused" | "tab_focused" | "workspace_focused" => {
                // Focus is useful even when auto-open is off or the launch
                // lock is busy. A publication failure must not stop docking.
                let _ = crate::focus::publish_focus();
            }
            _ => {}
        }
    }
    let state = crate::state::load_state();
    let view = match mode {
        Mode::Ensure | Mode::Reap => View::Explorer,
        Mode::Toggle(view) => view,
        Mode::Activate(target) => target.pane_view(state.merged),
    };
    // Auto-open off (⚙ Settings): hooks leave closed tabs alone; the user's
    // explicit toggle still works.
    if !explicit && !state.auto_open {
        return Ok(());
    }
    let creation_event =
        !explicit && matches!(event_kind.as_str(), "tab_created" | "workspace_created");
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
    // Budgeted, and never touches a viewer that is still present. The
    // no-op path leaves the snapshot valid; recovery may change layout.
    if !tab.is_empty() && crate::takeover::recover_locked(&tab, &panes) {
        panes = ipc::call_text("pane.list", serde_json::json!({}))?;
    }
    let snooze_dir = snooze::dir();
    if let Some(live_tabs) = launch::live_tabs(&panes) {
        snooze::migrate_legacy(&snooze_dir, &live_tabs);
        snooze::sweep(&snooze_dir, &live_tabs);
    }
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
            } else if !snooze::is_set(&snooze_dir, &tab)
                && auto_hide_allows_open(&panes, &tab, state.auto_hide_below)
            {
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

pub struct PendingUnifiedClose {
    width: u16,
    tab: String,
    siblings: Vec<String>,
    started: Instant,
    last_check: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UnifiedClose {
    Waiting,
    Closed(u16),
    TimedOut,
}

impl PendingUnifiedClose {
    pub fn request(pane_id: &str, other: View, width: u16) -> std::io::Result<Option<Self>> {
        let list = ipc::call_text("pane.list", serde_json::json!({}))?;
        ipc_ok(&list).map_err(std::io::Error::other)?;
        let siblings = crate::ui::sibling_panes_of(&list, pane_id, other);
        if siblings.is_empty() {
            return Ok(None);
        }
        let mut failure = None;
        for id in &siblings {
            if let Err(error) = request_close(&list, id) {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(Some(Self {
            width,
            tab: launch::tab_of(&list, pane_id),
            siblings,
            started: Instant::now(),
            last_check: None,
        }))
    }

    pub fn poll(&mut self) -> UnifiedClose {
        self.check(Instant::now(), |pane, tab| {
            ipc::call_text("pane.get", serde_json::json!({"pane_id": pane}))
                .ok()
                .is_some_and(|reply| sibling_gone(&reply, tab))
        })
    }

    fn check(&mut self, now: Instant, mut gone: impl FnMut(&str, &str) -> bool) -> UnifiedClose {
        if self
            .last_check
            .is_none_or(|at| now.duration_since(at) >= Duration::from_millis(120))
        {
            self.last_check = Some(now);
            self.siblings.retain(|pane| !gone(pane, &self.tab));
            if self.siblings.is_empty() {
                return UnifiedClose::Closed(self.width);
            }
        }
        if now.duration_since(self.started) >= Duration::from_secs(2) {
            return UnifiedClose::TimedOut;
        }
        UnifiedClose::Waiting
    }
}

fn sibling_gone(reply: &str, tab: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(launch::strip_bom(reply))
        .ok()
        .is_some_and(|value| {
            value["error"]["code"] == "pane_not_found"
                || value["result"]["pane"]["tab_id"]
                    .as_str()
                    .is_some_and(|current| current != tab)
        })
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

/// Auto-hide (⚙ Settings): the quiet hook leaves a narrow tab without a
/// sidebar and docks one again once the tab is wider than the setting. The
/// width is read through any pane of the already-resolved `tab`: a workspace
/// scope with several tabs has no single focused pane to ask. A layout that
/// cannot be read does not block the dock.
fn auto_hide_allows_open(panes_json: &str, tab: &str, threshold: u16) -> bool {
    if threshold == 0 {
        return true;
    }
    let pane = launch::pane_in_tab(panes_json, tab);
    if pane.is_empty() {
        return true;
    }
    ipc::call_text("pane.layout", serde_json::json!({ "pane_id": pane }))
        .ok()
        .and_then(|layout| launch::layout_width(&layout))
        .is_none_or(|width| !autohide::should_hide(threshold, width))
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
    before: &str,
    current_pane: Option<&str>,
    new_sidebar: &str,
    displaced_by_swap: Option<&str>,
) -> Option<String> {
    if current_pane != Some(new_sidebar) {
        return None;
    }
    let previous = if before.is_empty() {
        displaced_by_swap.unwrap_or_default().to_string()
    } else {
        before.to_string()
    };
    (!previous.is_empty() && previous != new_sidebar).then_some(previous)
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

    // A creation hook can be queued while the user changes tabs. Capture the
    // focused pane immediately before swapping, not only at hook entry.
    let mut focus_before_layout = None;
    if needs_swap {
        focus_before_layout = ipc::call_text("pane.list", serde_json::json!({})).ok();
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
        let before = focus_before_layout.as_deref().unwrap_or(panes_json);
        let previous = focused_pane_id(before);
        let current = crate::focus::current_pane().ok().flatten();
        if let Some(previous) = creation_focus_repair(
            &previous,
            current.as_ref().and_then(|pane| pane.pane_id.as_deref()),
            &new_pane,
            needs_swap.then_some(target.as_str()),
        ) {
            // Herdr has no conditional focus request. Navigation between
            // pane.current and pane.focus can still win the check and lose focus.
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

    #[test]
    fn unified_close_checks_only_requested_siblings_and_times_out() {
        let start = Instant::now();
        let mut pending = PendingUnifiedClose {
            width: 32,
            tab: "w1:t1".into(),
            siblings: vec!["w1:p1".into(), "w1:p2".into()],
            started: start,
            last_check: None,
        };
        assert_eq!(
            pending.check(start, |pane, tab| tab == "w1:t1" && pane == "w1:p1"),
            UnifiedClose::Waiting
        );
        assert_eq!(
            pending.check(start + Duration::from_millis(50), |_, _| panic!(
                "throttled"
            )),
            UnifiedClose::Waiting
        );
        assert_eq!(
            pending.check(start + Duration::from_secs(2), |pane, _| {
                assert_eq!(pane, "w1:p2");
                false
            }),
            UnifiedClose::TimedOut
        );
        assert_eq!(
            pending.check(start + Duration::from_secs(3), |_, _| true),
            UnifiedClose::Closed(32)
        );
    }

    #[test]
    fn unified_close_recognizes_missing_or_moved_panes_but_not_api_errors() {
        assert!(sibling_gone(
            r#"{"error":{"code":"pane_not_found"}}"#,
            "w1:t1"
        ));
        assert!(sibling_gone(
            r#"{"result":{"pane":{"tab_id":"w1:t2"}}}"#,
            "w1:t1"
        ));
        assert!(!sibling_gone(
            r#"{"result":{"pane":{"tab_id":"w1:t1"}}}"#,
            "w1:t1"
        ));
        assert!(!sibling_gone(
            r#"{"error":{"code":"unavailable"}}"#,
            "w1:t1"
        ));
        assert!(!sibling_gone("truncated{", "w1:t1"));
    }

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

    /// A simulated herdr for `reap_with`. A Ctrl+Q'd sidebar closes itself
    /// unless it is in `keeps` (a draft it cannot save); `pane.close` always
    /// works. Either removes the pane, as its own close would.
    #[derive(Default)]
    struct FakeReap {
        panes: Vec<serde_json::Value>,
        journaled: std::collections::BTreeSet<String>,
        keeps: Vec<String>,
        asked: Vec<String>,
        closed: Vec<String>,
    }

    impl FakeReap {
        fn pane(&mut self, id: &str, tab: &str, label: &str, tokens: serde_json::Value) {
            self.panes.push(
                serde_json::json!({"pane_id": id, "tab_id": tab, "label": label, "tokens": tokens}),
            );
        }
        fn has(&self, id: &str) -> bool {
            self.panes.iter().any(|pane| pane["pane_id"] == id)
        }
        fn remove(&mut self, id: &str) -> Result<(), String> {
            if !self.has(id) {
                return Err("pane_not_found".into());
            }
            self.panes.retain(|pane| pane["pane_id"] != id);
            Ok(())
        }
    }

    impl ReapHost for FakeReap {
        fn list(&mut self) -> Result<String, String> {
            Ok(serde_json::json!({"result": {"panes": self.panes}}).to_string())
        }
        fn journaled_tabs(&mut self) -> std::collections::BTreeSet<String> {
            self.journaled.clone()
        }
        fn request_close(&mut self, pane: &str) -> Result<(), String> {
            self.asked.push(pane.into());
            if self.keeps.iter().any(|id| id == pane) {
                return Ok(());
            }
            self.remove(pane)
        }
        fn close(&mut self, pane: &str) -> Result<(), String> {
            self.closed.push(pane.into());
            self.remove(pane)
        }
    }

    const REAP_NOW: u64 = 10_000;

    fn live_sidebar() -> serde_json::Value {
        let now = REAP_NOW.to_string();
        serde_json::json!({ launch::METADATA_SOURCE: now, launch::SC_METADATA_SOURCE: now })
    }

    /// Issue #108: the last shell exits and the sidebar is alone. It is asked
    /// to close itself (drafts saved), which closes the tab; tabs with any
    /// other pane — a shell, a viewer that may hold unsaved edits — and
    /// tabs a takeover journal still needs are left alone.
    #[test]
    fn reap_closes_only_sidebar_only_tabs_and_gracefully() {
        let mut host = FakeReap::default();
        host.pane("alone", "w1:t1", "Sidebar", live_sidebar());
        host.pane("busy", "w1:t2", "Sidebar", live_sidebar());
        host.pane("shell", "w1:t2", "", serde_json::json!({}));
        host.pane("previewing", "w1:t3", "", live_sidebar());
        host.pane(
            "viewer",
            "w1:t3",
            "a.rs · preview",
            serde_json::json!({ crate::viewer::METADATA_SOURCE: REAP_NOW.to_string() }),
        );
        host.pane("parked", "w1:t4", "Sidebar", live_sidebar());
        host.journaled.insert("w1:t4".into());
        let mut young = serde_json::json!({ launch::SC_METADATA_SOURCE: REAP_NOW.to_string() });
        young[launch::STARTING_TOKEN] = serde_json::json!("1");
        host.pane("young", "w1:t5", "Source Control", young);
        host.pane("corpse", "w1:t6", "Explorer", serde_json::json!({}));

        assert_eq!(reap_with(&mut host, &ReapScope::Workspace("w1".into())), Ok(3));
        assert_eq!(host.asked, vec!["alone"], "a live TUI only gets Ctrl+Q");
        assert_eq!(host.closed, vec!["young", "corpse"], "no TUI state yet");
        for kept in ["busy", "shell", "previewing", "viewer", "parked"] {
            assert!(host.has(kept), "{kept} must survive");
        }

        // Each self-close emits pane.closed, which re-runs the reap: with
        // nothing lonely left it does nothing, so it can never loop.
        host.asked.clear();
        host.closed.clear();
        assert_eq!(reap_with(&mut host, &ReapScope::Workspace("w1".into())), Ok(0));
        assert!(host.asked.is_empty() && host.closed.is_empty());
    }

    /// A separated Explorer + Source Control pair is lonely together; each
    /// gets Ctrl+Q. One whose draft cannot be saved stays open (it shows
    /// its own error), and a later event just asks it again — never a kill.
    #[test]
    fn reap_asks_every_sidebar_of_a_separated_pair_and_never_kills_a_draft() {
        let mut host = FakeReap::default();
        let now = "1";
        host.pane(
            "explorer",
            "w1:t1",
            "Explorer",
            serde_json::json!({ launch::METADATA_SOURCE: now }),
        );
        host.pane(
            "git",
            "w1:t1",
            "Source Control",
            serde_json::json!({ launch::SC_METADATA_SOURCE: now }),
        );
        host.keeps.push("git".into());
        assert_eq!(reap_with(&mut host, &ReapScope::Workspace("w1".into())), Ok(2));
        assert_eq!(host.asked, vec!["explorer", "git"]);
        assert!(host.closed.is_empty());
        assert!(host.has("git") && !host.has("explorer"));
        assert_eq!(reap_with(&mut host, &ReapScope::Workspace("w1".into())), Ok(1));
        assert!(host.has("git"), "an unsaved draft is never hard-closed");
        assert!(host.closed.is_empty());
    }

    #[test]
    fn reap_uses_exit_tab_and_close_workspace_without_global_fallback() {
        let exit = r#"{"event":"pane.exited","data":{"pane_id":"w1:p9","workspace_id":"w1"}}"#;
        let close = r#"{"event":"pane.closed","data":{"pane_id":"w1:p9","workspace_id":"w1"}}"#;
        let mut host = FakeReap::default();
        host.pane("exit", "w1:t1", "Explorer", live_sidebar());
        host.pane("other-tab", "w1:t2", "Explorer", live_sidebar());
        host.pane("other-workspace", "w2:t1", "Explorer", live_sidebar());

        assert_eq!(ReapScope::from_event(exit, "w2:t1"), None);
        assert_eq!(ReapScope::from_event("", "w1:t1"), None);
        let scope = ReapScope::from_event(exit, "w1:t1").unwrap();
        assert_eq!(reap_with(&mut host, &scope), Ok(1));
        assert!(host.has("other-tab") && host.has("other-workspace"));
        let scope = ReapScope::from_event(close, "w2:t1").unwrap();
        assert_eq!(reap_with(&mut host, &scope), Ok(1));
        assert!(host.has("other-workspace"));
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

        let before = focused_pane_id(before);
        let sidebar_stole_focus = Some("w2:p2");
        assert_eq!(
            creation_focus_repair(&before, sidebar_stole_focus, "w2:p2", None).as_deref(),
            Some("w1:p1")
        );

        let intended_preview_focus = Some("w2:p1");
        assert_eq!(
            creation_focus_repair(&before, intended_preview_focus, "w2:p2", Some("w2:p1")),
            None,
            "a concurrent intended focus must never be snapped back"
        );

        assert_eq!(
            creation_focus_repair(&before, sidebar_stole_focus, "w2:p2", Some("w2:p1")).as_deref(),
            Some("w1:p1"),
            "a background worktree's sidebar must not switch away from the original workspace"
        );

        let same_tab_before = r#"{"result":{"panes":[
            {"pane_id":"w2:p1","tab_id":"w2:t1","workspace_id":"w2","focused":true},
            {"pane_id":"w2:p3","tab_id":"w2:t1","workspace_id":"w2","focused":false}
        ]}}"#;
        assert_eq!(
            creation_focus_repair(&focused_pane_id(same_tab_before), sidebar_stole_focus, "w2:p2", Some("w2:p1"))
                .as_deref(),
            Some("w2:p1"),
            "a dock in the focused tab restores its original pane"
        );
        let no_prior_focus = r#"{"result":{"panes":[
            {"pane_id":"w2:p1","tab_id":"w2:t1","workspace_id":"w2","focused":false}
        ]}}"#;
        assert_eq!(
            creation_focus_repair(&focused_pane_id(no_prior_focus), sidebar_stole_focus, "w2:p2", Some("w2:p1"))
                .as_deref(),
            Some("w2:p1"),
            "without a recorded prior focus, restore the displaced pane"
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
