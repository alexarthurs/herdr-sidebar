//! Pure decision/layout helpers used by the native launcher and exposed through
//! stdin→stdout diagnostic modes. Keeping the parsing here makes the behavior
//! unit-testable and validates ids/paths before they reach an API request.
//!
//! - `--launch-decision`: `herdr pane list` JSON → `OPEN` | `FOCUS <pane_id>` |
//!   `CLOSE <pane_id>`, scoped to the focused pane's tab (toggle behavior).
//! - `--focused-pane`: `herdr pane list` JSON → `<pane_id>\t<cwd>` of the focused
//!   pane (cwd stripped of the `\\?\` verbatim prefix herdr reports on Windows).
//! - `--open-plan`: `herdr pane layout` JSON → `<edge_pane_id>\t<ratio>\t<swap>`,
//!   the split target, original-pane share, and whether the configured dock
//!   side needs a swap.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

/// The pane label the launcher assigns (`pane rename`) and later looks for.
pub const PANE_LABEL: &str = "Explorer";

/// Source id for `pane.report_metadata`; its token marks a pane as the
/// Explorer independently of the (cosmetic, clearable) label.
pub const METADATA_SOURCE: &str = "herdr-sidebar-explorer";

/// Present only while a TUI is starting. Windows launchers stamp it after a
/// raw split; directly spawned Unix TUIs stamp it at process startup. The
/// first full identity report clears it, so a pane carrying this token cannot
/// hold unsaved in-memory state yet.
pub const STARTING_TOKEN: &str = "herdr-sidebar-starting";

const MIN_SIDEBAR_SHARE: f64 = 0.15;
const MAX_SIDEBAR_SHARE: f64 = 0.5;

#[derive(Deserialize)]
struct PaneListMsg {
    result: PaneListResult,
}

#[derive(Deserialize)]
struct PaneListResult {
    #[serde(default)]
    panes: Vec<Pane>,
}

#[derive(Deserialize)]
struct Pane {
    pane_id: Option<String>,
    label: Option<String>,
    cwd: Option<String>,
    /// Live cwd of the foreground process. Unlike `cwd`, this changes after
    /// a shell `cd` or when an agent switches projects.
    foreground_cwd: Option<String>,
    #[serde(default)]
    focused: bool,
    tab_id: Option<String>,
    workspace_id: Option<String>,
    /// Metadata tokens reported via `pane.report_metadata`; shape of the
    /// values is host-defined, only key presence matters here.
    #[serde(default)]
    tokens: serde_json::Map<String, serde_json::Value>,
}

/// A non-sidebar pane in the same tab whose live cwd can be followed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SiblingCwd {
    pane_id: String,
    cwd: String,
    focused: bool,
}

/// Process-local state shared by the Explorer and Source Control views.
///
/// `pane.list` order is not an API guarantee, so candidates are sorted by id.
/// A manual folder choice suppresses focus-only changes; it is released only
/// when an already-observed sibling actually changes its live cwd.
#[derive(Default, Debug)]
pub struct CwdFollower {
    seen: BTreeMap<String, String>,
    selected: Option<String>,
    initialized: bool,
    manual_override: bool,
}

impl CwdFollower {
    /// Record a successful user-selected folder. The next samples still
    /// refresh the baseline, but do not re-root until a known pane moves.
    pub fn mark_manual_folder(&mut self) {
        self.manual_override = true;
    }

    /// Start following afresh after the persisted setting is enabled.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Return the next live cwd to follow, if this sample changes the chosen
    /// root according to the deterministic/manual-precedence rules above.
    pub fn next_cwd(&mut self, pane_list_json: &str, my_pane_id: &str) -> Option<String> {
        let siblings = sibling_cwds(pane_list_json, my_pane_id);
        let next_seen = siblings
            .iter()
            .map(|s| (s.pane_id.clone(), s.cwd.clone()))
            .collect::<BTreeMap<_, _>>();
        let changed = siblings
            .iter()
            .filter(|s| self.seen.get(&s.pane_id).is_some_and(|old| old != &s.cwd))
            .collect::<Vec<_>>();
        let was_initialized = self.initialized;
        self.initialized = true;

        if self.manual_override {
            self.seen = next_seen;
            if !was_initialized || changed.is_empty() {
                return None;
            }
            let picked = pick_sibling(&changed, self.selected.as_deref())?;
            self.selected = Some(picked.pane_id.clone());
            self.manual_override = false;
            return Some(picked.cwd.clone());
        }

        let prior_selected = self.selected.clone();
        let prior_cwd = prior_selected.as_ref().and_then(|id| self.seen.get(id)).cloned();
        let picked = pick_sibling(
            &siblings.iter().collect::<Vec<_>>(),
            self.selected.as_deref(),
        );
        self.seen = next_seen;
        let picked = picked?;
        self.selected = Some(picked.pane_id.clone());
        (prior_selected.as_deref() != Some(picked.pane_id.as_str())
            || prior_cwd.as_deref() != Some(picked.cwd.as_str()))
        .then(|| picked.cwd.clone())
    }
}

fn pick_sibling<'a>(
    siblings: &[&'a SiblingCwd],
    selected: Option<&str>,
) -> Option<&'a SiblingCwd> {
    siblings
        .iter()
        .copied()
        .find(|s| s.focused)
        .or_else(|| siblings.iter().copied().find(|s| Some(s.pane_id.as_str()) == selected))
        .or_else(|| siblings.first().copied())
}

impl Pane {
    /// An Explorer is recognized by its metadata token (reported by the TUI at
    /// startup — survives the label being cleared while collapsed) or by the
    /// "Explorer" label (retained for restored panes whose metadata does not
    /// survive a Herdr server restart).
    fn is_explorer(&self) -> bool {
        self.tokens.contains_key(METADATA_SOURCE)
            || self.label.as_deref() == Some(PANE_LABEL)
            || self.label.as_deref() == Some(SIDEBAR_LABEL)
    }

    /// One of OUR labels with NO heartbeat token is a corpse. The main way
    /// this happens: herdr resumes a restarted server's panes with their
    /// labels and scrollback, but the process inside is a fresh shell and
    /// metadata tokens do not survive. Native launches hold the shared lock
    /// until identity is stamped, so queued hooks never mistake a fresh pane
    /// for one of these restored corpses.
    fn our_label_without_token(&self) -> bool {
        matches!(self.label.as_deref(), Some("Sidebar" | "Explorer"))
            && !self.tokens.contains_key(METADATA_SOURCE)
    }
}

/// The unified pane's label (mirrors state::SIDEBAR_LABEL; kept here so the
/// launch module stays dependency-free for its tests).
const SIDEBAR_LABEL: &str = "Sidebar";

#[derive(Deserialize)]
struct LayoutMsg {
    result: LayoutResult,
}

#[derive(Deserialize)]
struct LayoutResult {
    layout: Layout,
}

#[derive(Deserialize)]
struct Layout {
    #[serde(default)]
    panes: Vec<LayoutPane>,
    #[serde(default)]
    splits: Vec<LayoutSplit>,
    area: Option<Rect>,
    tab_id: Option<String>,
}

#[derive(Deserialize)]
struct LayoutSplit {
    direction: Option<String>,
    ratio: Option<f64>,
    rect: Option<Rect>,
}

#[derive(Deserialize)]
struct LayoutPane {
    pane_id: Option<String>,
    rect: Option<Rect>,
}

#[derive(Deserialize)]
struct Rect {
    x: i64,
    y: i64,
    width: i64,
    #[serde(default)]
    height: i64,
}

/// Windows PowerShell 5.1 prepends a UTF-8 BOM when piping into a native
/// process's stdin (verified live); serde_json rejects a BOM before `{`.
pub(crate) fn strip_bom(input: &str) -> &str {
    input.trim_start_matches('\u{feff}')
}

/// A live TUI re-stamps its identity token with the unix time every few
/// seconds; a token older than this is a DEAD pane (its process was killed
/// out from under it) and should be replaced, not focused. process_info
/// can't tell the difference on Windows — the TUI child never shows in the
/// pane's foreground group (verified live).
pub const HEARTBEAT_STALE_SECS: u64 = 20;

/// True when `key` is present but its heartbeat timestamp is missing,
/// unparsable, or older than [`HEARTBEAT_STALE_SECS`]. Absent key = false
/// (a fresh pane the launcher labeled but whose TUI hasn't reported yet).
fn token_stale(
    tokens: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    now: u64,
) -> bool {
    let Some(value) = tokens.get(key) else { return false };
    let ts = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()));
    match ts {
        Some(ts) => now.saturating_sub(ts) > HEARTBEAT_STALE_SECS,
        None => true, // pre-heartbeat token format: treat as dead once seen
    }
}

/// `OPEN`, `FOCUS <id>`, `CLOSE <id>`, or `REPLACE <id>` (dead pane: close
/// it, then open fresh) from a `pane list` JSON. Unparseable input, no
/// focused pane, or an unsafe id all degrade to `OPEN` — the safe default is
/// a fresh explorer, never acting on a pane in an unknown tab.
pub fn launch_decision(pane_list_json: &str, now: u64) -> String {
    launch_decision_in(pane_list_json, now, "")
}

/// [`launch_decision`] confined to `scope` (a tab or workspace id), so the
/// decision reasons about the tab the hook is actually docking into. An
/// empty scope keeps the global behavior.
///
/// This MUST use the same scope as [`focused_pane_in`]: deciding against the
/// focused tab while docking into another one answers OPEN for a tab that
/// already has a sidebar, and twins it.
pub fn launch_decision_in(pane_list_json: &str, now: u64, scope: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return "OPEN".to_string();
    };
    let panes = &msg.result.panes;
    let in_scope = |p: &&Pane| {
        scope.is_empty()
            || p.tab_id.as_deref() == Some(scope)
            || p.workspace_id.as_deref() == Some(scope)
    };
    let Some(focused) = panes
        .iter()
        .find(|p| in_scope(p) && p.focused)
        .or_else(|| panes.iter().find(in_scope))
    else {
        return "OPEN".to_string();
    };
    let explorer = panes
        .iter()
        .find(|p| p.is_explorer() && p.tab_id.as_deref() == focused.tab_id.as_deref());
    let Some(pane) = explorer else {
        return "OPEN".to_string();
    };
    let Some(id) = pane.pane_id.as_deref().filter(|id| is_flag_safe(id)) else {
        return "OPEN".to_string();
    };
    if token_stale(&pane.tokens, METADATA_SOURCE, now) || pane.our_label_without_token() {
        return format!("REPLACE {id}");
    }
    if Some(id) == focused.pane_id.as_deref() {
        format!("CLOSE {id}")
    } else {
        format!("FOCUS {id}")
    }
}

/// Strict-toggle mapping (⚙ Settings): a `FOCUS` decision becomes `CLOSE`,
/// so one toggle press opens and the next press closes, wherever focus is.
/// Every other decision passes through untouched; kept pure so both the CLI
/// mode and the Windows ensure sidecar share one tested mapping.
pub fn focus_as_close(decision: &str) -> String {
    match decision.split_once(' ') {
        Some(("FOCUS", id)) => format!("CLOSE {id}"),
        _ => decision.to_string(),
    }
}

/// Source-control identity for the separated Source Control pane.
pub const SC_PANE_LABEL: &str = "Source Control";
pub const SC_METADATA_SOURCE: &str = "herdr-sidebar-git";
const PREVIEW_METADATA_SOURCE: &str = "herdr-sidebar-preview";

/// Like [`launch_decision`], but for the separated Source Control pane (the
/// unified Sidebar pane carries BOTH tokens, so it satisfies this too).
pub fn launch_decision_git(pane_list_json: &str, now: u64) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return "OPEN".to_string();
    };
    let panes = &msg.result.panes;
    let Some(focused) = panes.iter().find(|p| p.focused) else {
        return "OPEN".to_string();
    };
    let panel = panes.iter().find(|p| {
        (p.tokens.contains_key(SC_METADATA_SOURCE) || p.label.as_deref() == Some(SC_PANE_LABEL))
            && p.tab_id.as_deref() == focused.tab_id.as_deref()
    });
    let Some(pane) = panel else {
        return "OPEN".to_string();
    };
    let Some(id) = pane.pane_id.as_deref().filter(|id| is_flag_safe(id)) else {
        return "OPEN".to_string();
    };
    if token_stale(&pane.tokens, SC_METADATA_SOURCE, now)
        || (pane.label.as_deref() == Some(SC_PANE_LABEL)
            && !pane.tokens.contains_key(SC_METADATA_SOURCE))
    {
        return format!("REPLACE {id}");
    }
    if Some(id) == focused.pane_id.as_deref() {
        format!("CLOSE {id}")
    } else {
        format!("FOCUS {id}")
    }
}

/// Whether `pane_id` carries any of our identity tokens.
pub fn pane_has_token(pane_list_json: &str, pane_id: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return false;
    };
    msg.result
        .panes
        .iter()
        .filter(|p| p.pane_id.as_deref() == Some(pane_id))
        .any(|p| {
            p.tokens.contains_key(METADATA_SOURCE) || p.tokens.contains_key(SC_METADATA_SOURCE)
        })
}

/// Whether `pane_id` has started but not yet reached the TUI event loop. The
/// first full identity report clears this marker.
pub fn pane_is_starting(pane_list_json: &str, pane_id: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return false;
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))
        .is_some_and(|p| p.tokens.contains_key(STARTING_TOKEN))
}

/// `<pane_id>\t<cwd>` of the focused pane, or empty on any failure. The cwd
/// keeps its spaces (hence the tab separator) but loses any `\\?\` verbatim
/// prefix.
pub fn focused_pane(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|pane| pane.focused)
        .map(pane_spawn_target)
        .unwrap_or_default()
}

fn pane_spawn_target(pane: &Pane) -> String {
    let Some(id) = pane.pane_id.as_deref().filter(|id| is_flag_safe(id)) else {
        return String::new();
    };
    let cwd = pane
        .foreground_cwd
        .as_deref()
        .or(pane.cwd.as_deref())
        .map(strip_verbatim)
        .unwrap_or_default();
    format!("{id}\t{cwd}")
}

pub(crate) fn is_preview_label(label: &str) -> bool {
    label == "Preview"
        || label.starts_with("Preview · ")
        || label.starts_with("Editor · ")
        || label.ends_with(" · preview")
        || label.ends_with(" · editor")
}

/// Eligible panes in our tab, sorted by pane id so the fallback is stable.
/// `foreground_cwd` is deliberately required: falling back to `cwd` would
/// resurrect a stale spawn directory when the host has no live cwd signal.
fn sibling_cwds(pane_list_json: &str, my_pane_id: &str) -> Vec<SiblingCwd> {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return Vec::new();
    };
    let panes = &msg.result.panes;
    let Some(me) = panes.iter().find(|p| p.pane_id.as_deref() == Some(my_pane_id)) else {
        return Vec::new();
    };
    let mut siblings = panes
        .iter()
        .filter(|p| {
            p.tab_id.as_deref() == me.tab_id.as_deref()
                && p.pane_id.as_deref() != Some(my_pane_id)
                && !p.is_explorer()
                && !p.tokens.contains_key(SC_METADATA_SOURCE)
                && !p.tokens.contains_key(PREVIEW_METADATA_SOURCE)
                && p.label.as_deref() != Some(SC_PANE_LABEL)
                && !p.label.as_deref().is_some_and(is_preview_label)
        })
        .filter_map(|p| {
            let pane_id = p.pane_id.as_deref().filter(|id| is_flag_safe(id))?;
            let cwd = p.foreground_cwd.as_deref().map(strip_verbatim)?;
            (!cwd.is_empty()).then(|| SiblingCwd {
                pane_id: pane_id.to_string(),
                cwd: cwd.to_string(),
                focused: p.focused,
            })
        })
        .collect::<Vec<_>>();
    siblings.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
    siblings
}

/// `<pane_id>\t<ratio>\t<swap>` for the configured edge. Left docking splits
/// the leftmost pane and swaps; right docking splits the rightmost pane with
/// an inverted original-pane ratio and needs no swap. Empty on any failure.
pub fn open_plan(layout_json: &str, dock_right: bool, target_cols: u16) -> String {
    let Ok(msg) = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)) else {
        return String::new();
    };
    let mut best: Option<(&str, &Rect)> = None;
    for pane in &msg.result.layout.panes {
        let (Some(id), Some(rect)) = (pane.pane_id.as_deref(), pane.rect.as_ref()) else {
            continue;
        };
        if !is_flag_safe(id) || rect.width <= 0 {
            continue;
        }
        // The configured edge wins; among a stacked edge column, topmost wins.
        let better = match best {
            None => true,
            Some((_, b)) if dock_right => {
                (rect.x + rect.width, -rect.y) > (b.x + b.width, -b.y)
            }
            Some((_, b)) => (rect.x, rect.y) < (b.x, b.y),
        };
        if better {
            best = Some((id, rect));
        }
    }
    let Some((id, rect)) = best else {
        return String::new();
    };
    let sidebar_share =
        (f64::from(target_cols) / rect.width as f64).clamp(MIN_SIDEBAR_SHARE, MAX_SIDEBAR_SHARE);
    let ratio = if dock_right { 1.0 - sidebar_share } else { sidebar_share };
    format!("{id}\t{ratio:.6}\t{}", !dock_right)
}

/// Width of the whole tab area that owns a pane split. A pane-only divider
/// resize leaves this unchanged; terminal chrome/sidebar changes do not.
pub fn layout_width(layout_json: &str) -> Option<i64> {
    let msg = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)).ok()?;
    msg.result.layout.area.map(|area| area.width)
}

/// The tab (preferred) or workspace the event concerns, from
/// `HERDR_PLUGIN_EVENT_JSON`; "" when the payload names neither.
///
/// The ensure hook used to root a new sidebar in the GLOBALLY focused pane's
/// cwd, which during a workspace switch is still the space you came from —
/// observed live as tremor's sidebar rooted in bedrock. The payload knows
/// which tab is being docked; this is that answer.
pub fn event_scope(event_json: &str) -> String {
    event_scope_in(event_json, "")
}

/// [`event_scope`] with a pane snapshot available to resolve `pane.focused`.
/// That event carries a pane id but no tab id, and treating its workspace id
/// as a scope lets the globally focused pane from any sibling tab win.
pub fn event_scope_in(event_json: &str, pane_list_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(strip_bom(event_json)) else {
        return String::new();
    };
    let data = value.get("data").unwrap_or(&value);
    let pick = |key: &str| -> Option<String> {
        data.get(key)
            .and_then(|v| v.as_str())
            .or_else(|| {
                // workspace_created carries a nested WorkspaceInfo.
                data.get(key.trim_end_matches("_id"))
                    .and_then(|w| w.get(key))
                    .and_then(|v| v.as_str())
            })
            .map(str::to_string)
    };
    let pane_id = pick("pane_id");
    let tab_from_pane = pane_id.as_deref().and_then(|pane_id| {
        let msg = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)).ok()?;
        msg.result
            .panes
            .iter()
            .find(|pane| pane.pane_id.as_deref() == Some(pane_id))?
            .tab_id
            .clone()
    });
    pick("tab_id")
        .or(tab_from_pane)
        .or_else(|| pick("workspace_id"))
        .filter(|s| is_flag_safe(s))
        .unwrap_or_default()
}

/// The pane whose cwd a sidebar docked into `scope` should be rooted from:
/// the focused pane WITHIN that scope, else any pane in it (a brand-new space
/// may not have a focused pane yet). An empty scope keeps the old global
/// behavior. Returns `<pane_id>\t<cwd>`, or "" when the scope has no panes.
pub fn focused_pane_in(pane_list_json: &str, scope: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    let panes = &msg.result.panes;
    let workspace_tab = (!scope.is_empty() && !scope.contains(':'))
        .then(|| {
            panes
                .iter()
                .filter(|pane| pane.workspace_id.as_deref() == Some(scope))
                .filter_map(|pane| pane.tab_id.as_deref())
                .collect::<BTreeSet<_>>()
        })
        .filter(|tabs| tabs.len() == 1)
        .and_then(|tabs| tabs.into_iter().next());
    let in_scope = |p: &&Pane| {
        scope.is_empty()
            || p.tab_id.as_deref() == Some(scope)
            || workspace_tab.is_some_and(|tab_id| p.tab_id.as_deref() == Some(tab_id))
    };
    let Some(chosen) = panes
        .iter()
        .find(|p| in_scope(p) && p.focused)
        .or_else(|| panes.iter().find(in_scope))
    else {
        return String::new();
    };
    pane_spawn_target(chosen)
}

/// Which event invoked the ensure hook, from `HERDR_PLUGIN_EVENT_JSON`.
///
/// All launcher hooks run the same native implementation, so the payload is the only way to
/// treat space creation differently from an ordinary focus. The envelope
/// `EventEnvelope` currently serializes the discriminator as lower_snake in
/// `event`, while manifest hook names use dotted form. Both are accepted, and
/// the usual nested wrappers remain supported for older payload shapes.
///
/// The result is interpolated into a shell command, so it is restricted to a
/// plain `lower_snake` identifier; anything else yields "".
pub fn event_kind(event_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(strip_bom(event_json)) else {
        return String::new();
    };
    let kind = ["type", "data", "event"]
        .iter()
        .find_map(|key| match value.get(key) {
            Some(serde_json::Value::String(s)) => Some(s.as_str()),
            Some(inner) => inner.get("type").and_then(|v| v.as_str()),
            None => None,
        })
        .unwrap_or_default();
    let kind = kind.replace('.', "_");
    let safe = !kind.is_empty()
        && kind.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        && kind.len() <= 64;
    if safe { kind } else { String::new() }
}

/// A workspace's label from a `workspace list` JSON, empty when unknown.
///
/// The label is how a remembered tree root is keyed: workspace IDs identify a
/// space *instance* and get reassigned when a space is closed and recreated,
/// so they cannot carry a choice across that boundary.
pub fn workspace_label(workspace_list_json: &str, workspace_id: &str) -> String {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        #[serde(default)]
        workspaces: Vec<Ws>,
    }
    #[derive(Deserialize)]
    struct Ws {
        workspace_id: Option<String>,
        label: Option<String>,
    }
    if workspace_id.is_empty() {
        return String::new();
    }
    serde_json::from_str::<Msg>(strip_bom(workspace_list_json))
        .ok()
        .and_then(|msg| {
            msg.result
                .workspaces
                .into_iter()
                .find(|w| w.workspace_id.as_deref() == Some(workspace_id))
                .and_then(|w| w.label)
        })
        .unwrap_or_default()
}

/// The focused pane's tab id from a `pane list` JSON (flag-safe, else empty).
pub fn focused_tab(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.focused)
        .and_then(|p| p.tab_id.clone())
        .filter(|t| is_flag_safe(t))
        .unwrap_or_default()
}

/// The tab containing `pane_id` ("" when absent) — the hide path snoozes it.
pub fn tab_of(pane_list_json: &str, pane_id: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))
        .and_then(|p| p.tab_id.clone())
        .unwrap_or_default()
}

/// The workspace a pane belongs to, empty when the pane is unknown. Preview
/// routing is scoped by it so one project's ephemeral tab is never reused
/// from another.
pub fn workspace_of(pane_list_json: &str, pane_id: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    msg.result
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))
        .and_then(|p| p.workspace_id.clone())
        .unwrap_or_default()
}

/// All tab ids present in a `pane list` JSON — the live-tab set the snooze
/// cleanup checks markers against.
pub fn live_tabs(pane_list_json: &str) -> std::collections::BTreeSet<String> {
    serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json))
        .map(|msg| {
            msg.result
                .panes
                .into_iter()
                .filter_map(|p| p.tab_id)
                .collect()
        })
        .unwrap_or_default()
}

/// The created pane's id from a `pane.split` response
/// (`{"result":{"pane":{"pane_id":..}}}`), validated flag-safe.
pub fn split_pane_id(response_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        pane: Option<Info>,
    }
    #[derive(Deserialize)]
    struct Info {
        pane_id: Option<String>,
    }
    serde_json::from_str::<Msg>(strip_bom(response_json))
        .ok()?
        .result
        .pane?
        .pane_id
        .filter(|id| is_flag_safe(id))
}

/// The created pane id from a `plugin.pane.open` response. Direct plugin-pane
/// spawning starts the manifest argv without showing an intermediary shell.
pub fn plugin_pane_id(response_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        plugin_pane: PluginPane,
    }
    #[derive(Deserialize)]
    struct PluginPane {
        pane: Info,
    }
    #[derive(Deserialize)]
    struct Info {
        pane_id: Option<String>,
    }
    serde_json::from_str::<Msg>(strip_bom(response_json))
        .ok()?
        .result
        .plugin_pane
        .pane
        .pane_id
        .filter(|id| is_flag_safe(id))
}

/// One step of the full-height repair: `below` (a pane under the sidebar,
/// truncating its column) should be re-parented as a down-split of `beside`
/// (the pane toward the tab interior) in `tab`.
pub struct RepairStep {
    pub below: String,
    pub beside: String,
    pub tab: String,
}

/// When `pane_id` is not a full-height column of its tab, find the pane
/// directly below it and the pane beside it toward the tab interior — moving
/// the former under the latter (via a bounce through a temp tab; herdr no-ops
/// same-tab moves) grows the sidebar to full height. `None` when already full
/// height or the layout doesn't match. Called in a loop: each step removes one
/// pane from under the sidebar.
pub fn repair_step(
    layout_json: &str,
    pane_id: &str,
    dock_right: bool,
) -> Option<RepairStep> {
    let msg = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)).ok()?;
    let layout = &msg.result.layout;
    let area = layout.area.as_ref()?;
    let tab = layout.tab_id.as_deref().filter(|t| is_flag_safe(t))?;
    let me = layout
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    if me.height >= area.height {
        return None;
    }
    let find = |pred: &dyn Fn(&Rect) -> bool| -> Option<String> {
        layout
            .panes
            .iter()
            .filter(|p| p.pane_id.as_deref() != Some(pane_id))
            .filter_map(|p| Some((p.pane_id.as_deref()?, p.rect.as_ref()?)))
            .find(|(id, rect)| is_flag_safe(id) && pred(rect))
            .map(|(id, _)| id.to_string())
    };
    let below = find(&|r: &Rect| {
        r.y == me.y + me.height && r.x <= me.x && me.x < r.x + r.width
    })?;
    let beside = if dock_right {
        find(&|r: &Rect| r.x + r.width == me.x && r.y == me.y)?
    } else {
        find(&|r: &Rect| r.x == me.x + me.width && r.y == me.y)?
    };
    Some(RepairStep { below, beside, tab: tab.to_string() })
}

/// One `herdr pane resize` invocation: which way to move our interior edge and by
/// how much. `amount` is a RATIO delta — herdr adds it to the nearest split's
/// ratio (`layout.rs::resize_focused`: `current_ratio ± delta`), it is NOT
/// columns.
pub struct ResizeStep {
    pub direction: &'static str,
    pub amount: f64,
}

/// Compute the resize step that brings `pane_id` from `term_cols_now` to
/// `term_cols_target` terminal columns, from a `pane layout` JSON.
///
/// The sidebar's interior edge is the divider of some horizontal split: right
/// edge for a left dock, left edge for a right dock. Pick the innermost matching
/// split and convert the column delta into that split's ratio space. `None`
/// when the pane/split can't be found or the pane is already at the target.
pub fn resize_plan(
    layout_json: &str,
    pane_id: &str,
    term_cols_now: u16,
    term_cols_target: u16,
    dock_right: bool,
) -> Option<ResizeStep> {
    resize_plan_inner(
        layout_json,
        pane_id,
        term_cols_now,
        term_cols_target,
        dock_right,
        false,
    )
}

/// Re-assert a preferred column width after the tab's available width changes.
/// The target is exact in the normal range, but yields to the same 15%-50%
/// share bounds used when the sidebar first spawns.
pub fn preferred_resize_plan(
    layout_json: &str,
    pane_id: &str,
    term_cols_now: u16,
    preferred_cols: u16,
    dock_right: bool,
) -> Option<ResizeStep> {
    resize_plan_inner(
        layout_json,
        pane_id,
        term_cols_now,
        preferred_cols,
        dock_right,
        true,
    )
}

fn resize_plan_inner(
    layout_json: &str,
    pane_id: &str,
    term_cols_now: u16,
    term_cols_target: u16,
    dock_right: bool,
    clamp_share: bool,
) -> Option<ResizeStep> {
    let msg = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)).ok()?;
    let layout = &msg.result.layout;
    let pane_rect = layout
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    let divider_x = if dock_right { pane_rect.x } else { pane_rect.x + pane_rect.width };
    let split = layout
        .splits
        .iter()
        .filter(|s| s.direction.as_deref() == Some("right"))
        .filter_map(|s| Some((s.rect.as_ref()?, s.ratio?)))
        .filter(|(rect, ratio)| {
            let split_divider = rect.x + (f64::from(rect.width as i32) * ratio).round() as i64;
            rect.x <= pane_rect.x && (split_divider - divider_x).abs() <= 2 && rect.width > 0
        })
        .min_by_key(|(rect, _)| rect.width)?;

    // The pane rect can be a couple of columns wider than the terminal inside
    // it (pane chrome); express the target in rect space. Preferred-width
    // reassertion keeps the content usable at extreme tab widths.
    let chrome = pane_rect.width - i64::from(term_cols_now);
    let requested = i64::from(term_cols_target) + chrome.max(0);
    let target_rect_w = if clamp_share {
        let min = (split.0.width as f64 * MIN_SIDEBAR_SHARE).ceil() as i64;
        let max = (split.0.width as f64 * MAX_SIDEBAR_SHARE).floor() as i64;
        requested.clamp(min, max.max(min))
    } else {
        requested
    };

    let delta = (target_rect_w - pane_rect.width) as f64 / split.0.width as f64;
    if delta.abs() < 0.005 {
        return None;
    }
    Some(ResizeStep {
        direction: match (dock_right, delta > 0.0) {
            (false, true) | (true, false) => "right",
            (false, false) | (true, true) => "left",
        },
        amount: delta.abs(),
    })
}

/// True when the id can be passed as a positional argument to the herdr CLI
/// without any risk of being parsed as a flag.
fn is_flag_safe(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'))
}

fn strip_verbatim(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strict toggle rewrites only the FOCUS decision; OPEN, CLOSE, and
    /// REPLACE (and garbage) must reach the launchers untouched.
    #[test]
    fn strict_toggle_maps_focus_to_close_and_nothing_else() {
        assert_eq!(focus_as_close("FOCUS w4:p2"), "CLOSE w4:p2");
        assert_eq!(focus_as_close("OPEN"), "OPEN");
        assert_eq!(focus_as_close("CLOSE w4:p2"), "CLOSE w4:p2");
        assert_eq!(focus_as_close("REPLACE w4:p2"), "REPLACE w4:p2");
        assert_eq!(focus_as_close(""), "");
    }

    /// The decision and the dock must reason about the SAME tab. Scoping only
    /// the spawn cwd would let the decision see a focused tab with no sidebar,
    /// answer OPEN, and dock a second sidebar into a scoped tab that already
    /// had one.
    #[test]
    fn the_decision_follows_the_same_scope_as_the_dock() {
        let json = pane_list(
            r#"{"pane_id":"w4:pM","tab_id":"w4:tY","workspace_id":"w4","focused":true,"cwd":"/repo/faultline"},
               {"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","cwd":"/repo/tremor"},
               {"pane_id":"wH:p2","tab_id":"wH:t1","workspace_id":"wH","label":"Explorer",
                "tokens":{"herdr-sidebar-explorer":"9999999999"}}"#,
        );
        // Unscoped: the focused tab has no explorer, so a fresh one is right.
        assert_eq!(launch_decision_in(&json, 9999999999, ""), "OPEN");
        // Scoped to wH, whose tab ALREADY has one — docking again would twin it.
        assert_eq!(launch_decision_in(&json, 9999999999, "wH"), "FOCUS wH:p2");
    }

    /// The sidebar is rooted from the cwd it is spawned with, so picking the
    /// globally focused pane roots a new tab's sidebar in whatever project
    /// the user was last looking at — observed live as tremor's sidebar
    /// rooted in bedrock. The event payload names the scope; honor it.
    #[test]
    fn spawn_cwd_comes_from_the_events_own_scope() {
        let json = pane_list(
            r#"{"pane_id":"w4:pM","tab_id":"w4:tY","workspace_id":"w4","focused":true,"foreground_cwd":"/repo/faultline"},
               {"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","foreground_cwd":"/repo/tremor"},
               {"pane_id":"wH:p9","tab_id":"wH:t2","workspace_id":"wH","foreground_cwd":"/repo/tremor/sub"}"#,
        );
        // A multi-tab workspace is ambiguous and must not borrow one tab.
        assert_eq!(focused_pane_in(&json, "wH"), "");
        // A tab scope is more specific still.
        assert_eq!(focused_pane_in(&json, "wH:t2"), "wH:p9\t/repo/tremor/sub");
        // No scope keeps the old global behavior.
        assert_eq!(focused_pane_in(&json, ""), "w4:pM\t/repo/faultline");
        // A scope whose panes are gone yields nothing rather than guessing.
        assert_eq!(focused_pane_in(&json, "wQ"), "");
    }

    /// Within a scope the focused pane still wins; a brand-new space may have
    /// no focused pane yet, and then any pane in it is the right root.
    #[test]
    fn a_scope_prefers_its_focused_pane_but_settles_for_any() {
        let json = pane_list(
            r#"{"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","foreground_cwd":"/repo/a"},
               {"pane_id":"wH:p2","tab_id":"wH:t1","workspace_id":"wH","focused":true,"foreground_cwd":"/repo/b"}"#,
        );
        assert_eq!(focused_pane_in(&json, "wH"), "wH:p2\t/repo/b");

        let unfocused = pane_list(
            r#"{"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","foreground_cwd":"/repo/a"}"#,
        );
        assert_eq!(focused_pane_in(&unfocused, "wH"), "wH:p1\t/repo/a");
    }

    #[test]
    fn spawn_scope_falls_back_to_cwd_when_live_cwd_is_unavailable() {
        let json = pane_list(
            r#"{"pane_id":"wH:p1","tab_id":"wH:t1","workspace_id":"wH","focused":true,
                "cwd":"C:\\repo\\tremor"}"#,
        );
        assert_eq!(focused_pane_in(&json, "wH:t1"), "wH:p1\tC:\\repo\\tremor");
    }

    #[test]
    fn event_scope_prefers_the_tab_then_the_workspace() {
        let tab = r#"{"event":"tab_focused","data":{"type":"tab_focused","tab_id":"w4:tY","workspace_id":"w4"}}"#;
        assert_eq!(event_scope(tab), "w4:tY");
        let ws = r#"{"event":"workspace_created","data":{"type":"workspace_created","workspace_id":"wH"}}"#;
        assert_eq!(event_scope(ws), "wH");
        // Nested workspace object, as workspace_created carries WorkspaceInfo.
        let nested = r#"{"event":"workspace_created","data":{"type":"workspace_created","workspace":{"workspace_id":"wH"}}}"#;
        assert_eq!(event_scope(nested), "wH");
        assert_eq!(event_scope("garbage"), "");
        // Shell-unsafe ids are dropped, same as the event kind.
        assert_eq!(event_scope(r#"{"data":{"workspace_id":"a b; rm -rf /"}}"#), "");
    }

    #[test]
    fn pane_focus_scope_resolves_to_its_own_tab() {
        let event = r#"{"data":{"type":"pane_focused","pane_id":"w4:p9","workspace_id":"w4"}}"#;
        let panes = pane_list(
            r#"{"pane_id":"w4:p1","tab_id":"w4:t1","workspace_id":"w4"},
               {"pane_id":"w4:p9","tab_id":"w4:t2","workspace_id":"w4"}"#,
        );
        assert_eq!(event_scope_in(event, &panes), "w4:t2");
        assert_eq!(event_scope(event), "w4");
    }

    /// The launcher handles several event kinds through one implementation, so the only
    /// way to treat space creation specially is the payload. The envelope
    /// shape isn't documented, so the discriminator is looked for at the top
    /// level and under the usual wrappers.
    #[test]
    fn event_kind_is_found_whatever_the_envelope() {
        assert_eq!(event_kind(r#"{"type":"workspace_created"}"#), "workspace_created");
        assert_eq!(
            event_kind(r#"{"data":{"type":"workspace_created"}}"#),
            "workspace_created"
        );
        assert_eq!(
            event_kind(r#"{"event":{"type":"tab_focused"}}"#),
            "tab_focused"
        );
        assert_eq!(event_kind(r#"{"event":"tab.created"}"#), "tab_created");
    }

    /// The kind is interpolated into a shell command, so anything that is not
    /// a plain lower_snake identifier is dropped rather than passed along.
    #[test]
    fn event_kind_refuses_anything_shell_unsafe() {
        for hostile in [
            r#"{"type":"a; rm -rf /"}"#,
            r#"{"type":"$(whoami)"}"#,
            r#"{"type":"a b"}"#,
            r#"{"type":42}"#,
            r#"{"type":""}"#,
            "garbage",
        ] {
            assert_eq!(event_kind(hostile), "", "{hostile}");
        }
    }

    #[test]
    fn workspace_labels_resolve_by_id_and_degrade_quietly() {
        let json = r#"{"result":{"workspaces":[
            {"workspace_id":"wG","label":"tremor"},
            {"workspace_id":"w4","label":"faultline"},
            {"workspace_id":"wZ"}
        ]}}"#;
        assert_eq!(workspace_label(json, "wG"), "tremor");
        assert_eq!(workspace_label(json, "w4"), "faultline");
        // A space with no label, an id that is gone, and a missing id all
        // yield "" so the caller falls back to the pane's cwd.
        assert_eq!(workspace_label(json, "wZ"), "");
        assert_eq!(workspace_label(json, "wQ"), "");
        assert_eq!(workspace_label(json, ""), "");
        assert_eq!(workspace_label("garbage", "wG"), "");
    }

    fn pane_list(panes: &str) -> String {
        format!(r#"{{"id":"cli:pane:list","result":{{"panes":[{panes}]}}}}"#)
    }

    #[test]
    fn cwd_follower_uses_only_live_foreground_cwd() {
        let mut follower = CwdFollower::default();
        let live = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar","tokens":{"herdr-sidebar-explorer":"1"}},
               {"pane_id":"w1:p1","tab_id":"w1:t1","cwd":"/stale/spawn","foreground_cwd":"/live/project"}"#,
        );
        assert_eq!(follower.next_cwd(&live, "w1:p3").as_deref(), Some("/live/project"));

        follower.reset();
        let stale_only = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","cwd":"/stale/spawn"}"#,
        );
        assert_eq!(follower.next_cwd(&stale_only, "w1:p3"), None);
    }

    #[test]
    fn cwd_follower_has_deterministic_multi_pane_precedence() {
        let mut follower = CwdFollower::default();
        let unordered = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p9","tab_id":"w1:t1","foreground_cwd":"/nine"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"}"#,
        );
        // No focused sibling: lexical pane id is the stable fallback.
        assert_eq!(follower.next_cwd(&unordered, "w1:p3").as_deref(), Some("/one"));
        assert_eq!(follower.next_cwd(&unordered, "w1:p3"), None);

        let focused = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p9","tab_id":"w1:t1","foreground_cwd":"/nine","focused":true},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"}"#,
        );
        assert_eq!(follower.next_cwd(&focused, "w1:p3").as_deref(), Some("/nine"));
    }

    #[test]
    fn manual_folder_wins_until_an_observed_pane_moves() {
        let mut follower = CwdFollower::default();
        let initial = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"},
               {"pane_id":"w1:p2","tab_id":"w1:t1","foreground_cwd":"/two"}"#,
        );
        assert_eq!(follower.next_cwd(&initial, "w1:p3").as_deref(), Some("/one"));
        follower.mark_manual_folder();

        // Focus alone, pane-list reordering, and a newly-created pane do not
        // override an explicit folder choice.
        let focus_only = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p4","tab_id":"w1:t1","foreground_cwd":"/new"},
               {"pane_id":"w1:p2","tab_id":"w1:t1","foreground_cwd":"/two","focused":true},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"}"#,
        );
        assert_eq!(follower.next_cwd(&focus_only, "w1:p3"), None);

        let moved = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","foreground_cwd":"/one"},
               {"pane_id":"w1:p2","tab_id":"w1:t1","foreground_cwd":"/two/next","focused":true}"#,
        );
        assert_eq!(
            follower.next_cwd(&moved, "w1:p3").as_deref(),
            Some("/two/next")
        );
    }

    #[test]
    fn cwd_follower_ignores_plugin_panes_and_other_tabs() {
        let json = pane_list(
            r#"{"pane_id":"w1:p3","tab_id":"w1:t1","label":"Sidebar"},
               {"pane_id":"w1:p1","tab_id":"w1:t1","label":"Source Control","foreground_cwd":"/sc"},
               {"pane_id":"w1:p2","tab_id":"w1:t1","label":"Preview · routes.rs","foreground_cwd":"/preview"},
               {"pane_id":"w1:p5","tab_id":"w1:t1","label":"routes.rs · editor","foreground_cwd":"/editor"},
               {"pane_id":"w1:p4","tab_id":"w1:t2","foreground_cwd":"/other"}"#,
        );
        assert_eq!(CwdFollower::default().next_cwd(&json, "w1:p3"), None);
    }

    const FOCUSED: &str = r#"{"pane_id":"w1:p1","focused":true,"tab_id":"w1:t1","cwd":"C:\\work\\my repo"}"#;

    #[test]
    fn decision_opens_when_no_explorer_in_tab() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p9","label":"Explorer","tab_id":"w1:t2"}}"#
        ));
        assert_eq!(launch_decision(&json, 100), "OPEN", "other-tab Explorer is ignored");
    }

    #[test]
    fn decision_focuses_unfocused_explorer_in_same_tab() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","label":"Explorer","tab_id":"w1:t1","tokens":{{"herdr-sidebar-explorer":95}}}}"#
        ));
        assert_eq!(launch_decision(&json, 100), "FOCUS w1:p2");
    }

    #[test]
    fn decision_closes_when_explorer_is_focused() {
        let json = pane_list(
            r#"{"pane_id":"w1:p2","label":"Explorer","tab_id":"w1:t1","focused":true,"tokens":{"herdr-sidebar-explorer":95}}"#,
        );
        assert_eq!(launch_decision(&json, 100), "CLOSE w1:p2");
    }

    #[test]
    fn decision_recognizes_explorer_by_metadata_token_without_label() {
        // A collapsed explorer has its label cleared but keeps its token;
        // the token value is a fresh heartbeat timestamp.
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-explorer":95}}}}"#
        ));
        assert_eq!(launch_decision(&json, 100), "FOCUS w1:p2");
    }

    #[test]
    fn pane_token_probe_distinguishes_starting_and_live_sidebars() {
        let starting = pane_list(
            r#"{"pane_id":"w1:p1","tab_id":"w1:t1","label":"Explorer","focused":true,"tokens":{"herdr-sidebar-explorer":"100","herdr-sidebar-starting":"1"}}"#,
        );
        let live = pane_list(
            r#"{"pane_id":"w1:p1","tab_id":"w1:t1","label":"Explorer","tokens":{"herdr-sidebar-explorer":"100"}}"#,
        );
        // Synchronous pre-stamping makes the pane live to re-entrant hooks,
        // while retaining enough state for an immediate second toggle to
        // close it directly before its event loop starts.
        assert_eq!(launch_decision(&starting, 100), "CLOSE w1:p1");
        assert!(pane_has_token(&starting, "w1:p1"));
        assert!(pane_has_token(&live, "w1:p1"));
        assert!(!pane_has_token(&live, "w1:p2"));
        assert!(pane_is_starting(&starting, "w1:p1"));
        assert!(!pane_is_starting(&live, "w1:p1"));
        assert!(!pane_is_starting(&starting, "w1:p2"));

        let source_control = pane_list(
            r#"{"pane_id":"w1:p1","tab_id":"w1:t1","label":"Source Control","focused":true,"tokens":{"herdr-sidebar-git":"100","herdr-sidebar-starting":"1"}}"#,
        );
        assert_eq!(launch_decision_git(&source_control, 100), "CLOSE w1:p1");
        assert!(pane_is_starting(&source_control, "w1:p1"));
    }

    #[test]
    fn decision_replaces_dead_panes() {
        // Stale heartbeat (or a pre-heartbeat token shape) = a dead TUI whose
        // pane must be closed and re-docked, never focused.
        let stale = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-explorer":40}}}}"#
        ));
        assert_eq!(launch_decision(&stale, 100), "REPLACE w1:p2");
        let legacy = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-explorer":{{"value":"explorer"}}}}}}"#
        ));
        assert_eq!(launch_decision(&legacy, 100), "REPLACE w1:p2");
        // Label without a token = a RESUMED corpse (labels survive server
        // restarts, tokens and the process do not) — for every our-label.
        for label in ["Explorer", "Sidebar"] {
            let corpse = pane_list(&format!(
                r#"{FOCUSED},{{"pane_id":"w1:p2","label":"{label}","tab_id":"w1:t1"}}"#
            ));
            assert_eq!(launch_decision(&corpse, 100), "REPLACE w1:p2", "{label} corpse");
        }
        let sc_corpse = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p3","label":"Source Control","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision_git(&sc_corpse, 100), "REPLACE w1:p3");
        // Same rules for the separated Source Control decision.
        let sc_stale = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p3","tab_id":"w1:t1","tokens":{{"herdr-sidebar-git":40}}}}"#
        ));
        assert_eq!(launch_decision_git(&sc_stale, 100), "REPLACE w1:p3");
        let sc_live = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p3","tab_id":"w1:t1","tokens":{{"herdr-sidebar-git":95}}}}"#
        ));
        assert_eq!(launch_decision_git(&sc_live, 100), "FOCUS w1:p3");
    }

    #[test]
    fn decision_degrades_to_open_on_garbage_or_unsafe_ids() {
        assert_eq!(launch_decision("not json", 100), "OPEN");
        assert_eq!(launch_decision(&pane_list(r#"{"pane_id":"w1:p1"}"#), 100), "OPEN");
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"--evil","label":"Explorer","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&json, 100), "OPEN");
    }

    #[test]
    fn utf8_bom_from_powershell_pipe_is_stripped() {
        let json = format!("\u{feff}{}", pane_list(FOCUSED));
        assert_eq!(launch_decision(&json, 100), "OPEN");
        assert!(focused_pane(&json).starts_with("w1:p1\t"));
        let layout_json = format!(
            "\u{feff}{}",
            layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":90,"height":50}}"#)
        );
        assert_eq!(open_plan(&layout_json, false, 32), "w1:p1\t0.355556\ttrue");
    }

    #[test]
    fn focused_pane_reports_id_and_stripped_cwd() {
        let json = pane_list(&format!(
            r#"{{"pane_id":"w1:p3","focused":true,"tab_id":"w1:t1","cwd":"\\\\?\\C:\\work\\my repo"}},{FOCUSED}"#
        ));
        assert_eq!(focused_pane(&json), "w1:p3\tC:\\work\\my repo");
        assert_eq!(focused_pane("not json"), "");
        assert_eq!(focused_pane(&pane_list(r#"{"pane_id":"w1:p1"}"#)), "");
    }

    fn layout(panes: &str) -> String {
        format!(r#"{{"id":"cli:pane:layout","result":{{"layout":{{"panes":[{panes}]}}}}}}"#)
    }

    #[test]
    fn open_plan_picks_leftmost_topmost_pane() {
        let json = layout(
            r#"{"pane_id":"w1:p2","rect":{"x":119,"y":1,"width":90,"height":70}},
               {"pane_id":"w1:p3","rect":{"x":29,"y":36,"width":90,"height":35}},
               {"pane_id":"w1:p1","rect":{"x":29,"y":1,"width":90,"height":35}}"#,
        );
        let plan = open_plan(&json, false, 32);
        let (id, ratio) = plan.split_once('\t').unwrap();
        assert_eq!(id, "w1:p1");
        assert_eq!(ratio, "0.355556\ttrue"); // 32 / 90, then swap left
    }

    #[test]
    fn open_plan_picks_rightmost_topmost_pane_and_inverts_ratio() {
        let json = layout(
            r#"{"pane_id":"w1:p2","rect":{"x":119,"y":36,"width":90,"height":35}},
               {"pane_id":"w1:p3","rect":{"x":29,"y":1,"width":90,"height":70}},
               {"pane_id":"w1:p1","rect":{"x":119,"y":1,"width":90,"height":35}}"#,
        );
        assert_eq!(open_plan(&json, true, 32), "w1:p1\t0.644444\tfalse");
    }

    #[test]
    fn open_plan_clamps_ratio() {
        let wide = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":400,"height":50}}"#);
        assert_eq!(open_plan(&wide, false, 32), "w1:p1\t0.150000\ttrue");
        assert_eq!(open_plan(&wide, true, 32), "w1:p1\t0.850000\tfalse");
        let narrow = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":40,"height":50}}"#);
        assert_eq!(open_plan(&narrow, false, 32), "w1:p1\t0.500000\ttrue");
        assert_eq!(open_plan(&narrow, true, 32), "w1:p1\t0.500000\tfalse");
        assert_eq!(open_plan(&narrow, false, 24), "w1:p1\t0.500000\ttrue");
    }

    #[test]
    fn layout_width_distinguishes_tab_area_from_pane_width() {
        let json = r#"{"result":{"layout":{"area":{"x":0,"y":0,"width":174,"height":50},"panes":[{"pane_id":"e","rect":{"x":0,"y":0,"width":42,"height":50}}]}}}"#;
        assert_eq!(layout_width(json), Some(174));
        assert_eq!(layout_width("not json"), None);
    }

    #[test]
    fn focused_tab_and_live_tabs() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p9","tab_id":"w1:t2"}}"#
        ));
        assert_eq!(focused_tab(&json), "w1:t1");
        assert_eq!(
            live_tabs(&json).into_iter().collect::<Vec<_>>(),
            vec!["w1:t1".to_string(), "w1:t2".to_string()]
        );
        assert_eq!(focused_tab("not json"), "");
        assert!(live_tabs("not json").is_empty());
    }

    #[test]
    fn split_pane_id_extracts_and_validates() {
        let json = r#"{"id":"x","result":{"pane":{"pane_id":"w3:p5","cwd":"C:x"},"type":"pane_info"}}"#;
        assert_eq!(split_pane_id(json), Some("w3:p5".to_string()));
        let evil = r#"{"id":"x","result":{"pane":{"pane_id":"--evil"},"type":"pane_info"}}"#;
        assert_eq!(split_pane_id(evil), None);
        assert_eq!(split_pane_id("not json"), None);
        assert_eq!(split_pane_id(r#"{"id":"x","result":{"type":"ok"}}"#), None);
    }

    #[test]
    fn plugin_pane_id_extracts_and_validates() {
        let json = r#"{"result":{"plugin_pane":{"plugin_id":"herdr-sidebar","entrypoint":"sidebar","pane":{"pane_id":"w3:p5"}}}}"#;
        assert_eq!(plugin_pane_id(json), Some("w3:p5".to_string()));
        let evil = r#"{"result":{"plugin_pane":{"pane":{"pane_id":"--evil"}}}}"#;
        assert_eq!(plugin_pane_id(evil), None);
        assert_eq!(plugin_pane_id("not json"), None);
    }

    fn layout_with_splits(panes: &str, splits: &str) -> String {
        format!(
            r#"{{"id":"cli:pane:layout","result":{{"layout":{{"panes":[{panes}],"splits":[{splits}]}}}}}}"#
        )
    }

    fn layout_with_area(panes: &str) -> String {
        format!(
            r#"{{"id":"x","result":{{"layout":{{"area":{{"x":0,"y":0,"width":180,"height":50}},"tab_id":"w1:t1","panes":[{panes}]}}}}}}"#
        )
    }

    #[test]
    fn repair_step_finds_below_and_right_panes() {
        // Explorer truncated to the top-left; p7 spans below it, p1 beside it.
        let json = layout_with_area(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":32,"height":25}},
               {"pane_id":"p1","rect":{"x":32,"y":0,"width":58,"height":25}},
               {"pane_id":"p7","rect":{"x":0,"y":25,"width":90,"height":25}},
               {"pane_id":"p4","rect":{"x":90,"y":0,"width":90,"height":50}}"#,
        );
        let step = repair_step(&json, "e", false).unwrap();
        assert_eq!((step.below.as_str(), step.beside.as_str(), step.tab.as_str()), ("p7", "p1", "w1:t1"));
    }

    #[test]
    fn repair_step_mirrors_for_right_dock() {
        let json = layout_with_area(
            r#"{"pane_id":"p4","rect":{"x":0,"y":0,"width":90,"height":50}},
               {"pane_id":"p1","rect":{"x":90,"y":0,"width":58,"height":25}},
               {"pane_id":"e","rect":{"x":148,"y":0,"width":32,"height":25}},
               {"pane_id":"p7","rect":{"x":90,"y":25,"width":90,"height":25}}"#,
        );
        let step = repair_step(&json, "e", true).unwrap();
        assert_eq!((step.below.as_str(), step.beside.as_str(), step.tab.as_str()), ("p7", "p1", "w1:t1"));
    }

    #[test]
    fn repair_step_none_when_full_height_or_unmatched() {
        let full = layout_with_area(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":32,"height":50}},
               {"pane_id":"p1","rect":{"x":32,"y":0,"width":148,"height":50}}"#,
        );
        assert!(repair_step(&full, "e", false).is_none());
        assert!(repair_step(&full, "missing", false).is_none());
        assert!(repair_step("not json", "e", false).is_none());
    }

    #[test]
    fn resize_plan_converts_columns_to_split_ratio_delta() {
        // Explorer: 32 rect cols (30 terminal cols) at the left of a 160-col split.
        let json = layout_with_splits(
            r#"{"pane_id":"w1:p2","rect":{"x":0,"y":0,"width":32,"height":50}},
               {"pane_id":"w1:p1","rect":{"x":32,"y":0,"width":128,"height":50}}"#,
            r#"{"direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        // Collapse 30 → 4 terminal cols: rect 32 → 6, delta -26/160.
        let step = resize_plan(&json, "w1:p2", 30, 4, false).unwrap();
        assert_eq!(step.direction, "left");
        assert!((step.amount - 26.0 / 160.0).abs() < 1e-9);
        // Expand 30 → 40: rect 32 → 42, delta +10/160.
        let step = resize_plan(&json, "w1:p2", 30, 40, false).unwrap();
        assert_eq!(step.direction, "right");
        assert!((step.amount - 10.0 / 160.0).abs() < 1e-9);
    }

    #[test]
    fn resize_plan_mirrors_directions_for_right_dock() {
        let json = layout_with_splits(
            r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":128,"height":50}},
               {"pane_id":"w1:p2","rect":{"x":128,"y":0,"width":32,"height":50}}"#,
            r#"{"direction":"right","ratio":0.8,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        let shrink = resize_plan(&json, "w1:p2", 30, 4, true).unwrap();
        assert_eq!(shrink.direction, "right");
        assert!((shrink.amount - 26.0 / 160.0).abs() < 1e-9);
        let grow = resize_plan(&json, "w1:p2", 30, 40, true).unwrap();
        assert_eq!(grow.direction, "left");
        assert!((grow.amount - 10.0 / 160.0).abs() < 1e-9);
    }

    #[test]
    fn preferred_resize_plan_pins_columns_but_yields_at_extremes() {
        let normal = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":39,"height":50}}"#,
            r#"{"direction":"right","ratio":0.195,"rect":{"x":0,"y":0,"width":200,"height":50}}"#,
        );
        let step = preferred_resize_plan(&normal, "e", 37, 42, false).unwrap();
        assert_eq!(step.direction, "right");
        assert!((step.amount - 5.0 / 200.0).abs() < 1e-9);

        let narrow = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":20,"height":50}}"#,
            r#"{"direction":"right","ratio":0.333333,"rect":{"x":0,"y":0,"width":60,"height":50}}"#,
        );
        let step = preferred_resize_plan(&narrow, "e", 18, 42, false).unwrap();
        assert_eq!(step.direction, "right");
        assert!((step.amount - 10.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn resize_plan_picks_innermost_matching_split() {
        // Nested: root split (divider elsewhere) plus the inner split whose
        // divider is at the explorer's right edge.
        let json = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":20,"height":50}}"#,
            r#"{"direction":"right","ratio":0.5,"rect":{"x":0,"y":0,"width":200,"height":50}},
               {"direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":100,"height":50}}"#,
        );
        let step = resize_plan(&json, "e", 18, 40, false).unwrap();
        // delta computed against the inner 100-col split: +22/100.
        assert!((step.amount - 22.0 / 100.0).abs() < 1e-9);
    }

    #[test]
    fn resize_plan_returns_none_when_unresizable_or_at_target() {
        assert!(resize_plan("not json", "e", 30, 4, false).is_none());
        // No split with a divider at the pane's edge (e.g. the only pane).
        let solo = layout_with_splits(r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":100,"height":50}}"#, "");
        assert!(resize_plan(&solo, "e", 98, 30, false).is_none());
        // Already at the target.
        let json = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":32,"height":50}}"#,
            r#"{"direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert!(resize_plan(&json, "e", 30, 30, false).is_none());
    }

    #[test]
    fn open_plan_is_empty_on_failure() {
        assert_eq!(open_plan("not json", false, 32), "");
        assert_eq!(open_plan(&layout(""), false, 32), "");
        let unsafe_id = layout(r#"{"pane_id":"--x","rect":{"x":0,"y":0,"width":90,"height":50}}"#);
        assert_eq!(open_plan(&unsafe_id, true, 32), "");
    }
}
