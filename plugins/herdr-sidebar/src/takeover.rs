//! Temporary preview takeover (`Preview opens in: replace`, issue #64).
//!
//! The working panes beside the sidebar are parked in a background tab while
//! one inline viewer fills the space, then replayed back into their exact
//! split tree. The journal (`Plan`) lives in the private runtime directory,
//! keyed by session + tab, and every mutation happens under the shared
//! launcher lock.
//!
//! Safety rules, each covered by a hermetic test below:
//! - A viewer pane that is still PRESENT is never moved or closed by
//!   recovery, whatever its heartbeat says: a stale stamp can be a suspended
//!   laptop or a stalled loop holding an unsaved editor buffer. Only the
//!   closing viewer itself (or a viewer that is gone) lets a restore run.
//! - Restore is best-effort and always finishes: panes that disappeared are
//!   pruned from the saved tree, a subtree whose move fails stays parked in a
//!   relabelled tab, and the journal is deleted. A lost pane can therefore
//!   never trap the tab's previews behind a journal that can no longer apply.
//! - Hook-driven retries are budgeted, so a failing host cannot turn focus
//!   events into a restore loop.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ensure::LaunchLock, ipc, rundir, snooze};

const PARKING_LABEL: &str = "Working panes (temporary preview)";
/// A parking tab that still holds panes after its journal is gone. The label
/// tells the user the panes are theirs to move; the tab is also un-snoozed so
/// a sidebar docks there like in any ordinary tab.
const UNRESTORED_LABEL: &str = "Working panes (not restored)";
const HOOK_ATTEMPTS_PER_WINDOW: u32 = 3;
const HOOK_WINDOW_SECS: u64 = 60;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
enum Node {
    Pane(String),
    Split {
        direction: String,
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    fn contains(&self, pane: &str) -> bool {
        match self {
            Self::Pane(id) => id == pane,
            Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    fn representative(&self) -> &str {
        match self {
            Self::Pane(id) => id,
            Self::Split { first, .. } => first.representative(),
        }
    }

    fn panes(&self, out: &mut Vec<String>) {
        match self {
            Self::Pane(id) => out.push(id.clone()),
            Self::Split { first, second, .. } => {
                first.panes(out);
                second.panes(out);
            }
        }
    }

    fn pane_count(&self) -> usize {
        match self {
            Self::Pane(_) => 1,
            Self::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// The tree with every pane `keep` rejects removed; a split that loses
    /// one side collapses into the survivor, exactly as herdr collapses a
    /// split when one of its panes closes.
    fn prune(&self, keep: &dyn Fn(&str) -> bool) -> Option<Node> {
        match self {
            Self::Pane(id) => keep(id).then(|| self.clone()),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => match (first.prune(keep), second.prune(keep)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    direction: direction.clone(),
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(survivor), None) | (None, Some(survivor)) => Some(survivor),
                (None, None) => None,
            },
        }
    }

    fn replace(&mut self, old: &str, new: &str) {
        match self {
            Self::Pane(id) if id == old => *id = new.into(),
            Self::Split { first, second, .. } => {
                first.replace(old, new);
                second.replace(old, new);
            }
            Self::Pane(_) => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
struct Rect {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

#[derive(Deserialize)]
struct LayoutPane {
    pane_id: String,
    rect: Rect,
}

#[derive(Deserialize)]
struct LayoutSplit {
    direction: String,
    ratio: f64,
    rect: Rect,
}

#[derive(Deserialize)]
struct Layout {
    area: Rect,
    panes: Vec<LayoutPane>,
    splits: Vec<LayoutSplit>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Plan {
    tab: String,
    owner: String,
    parking_tab: String,
    parking_pane: String,
    viewer: Option<String>,
    tree: Node,
    terminals: BTreeMap<String, String>,
    /// The herdr session this journal belongs to. Journals of OTHER sessions
    /// share the directory and must never be swept by this one.
    #[serde(default)]
    session: String,
    #[serde(default)]
    hook_attempts: u32,
    #[serde(default)]
    last_hook_attempt: u64,
}

/// Everything the takeover needs from the outside world, so the restore
/// logic runs against a simulated host in tests.
trait Env {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
    fn load(&mut self, tab: &str) -> Result<Option<Plan>, String>;
    fn store(&mut self, plan: &Plan) -> Result<(), String>;
    fn discard(&mut self, tab: &str) -> Result<(), String>;
    fn plans(&mut self) -> Vec<Plan>;
    fn set_snoozed(&mut self, tab: &str, snoozed: bool) -> Result<(), String>;
    fn now(&self) -> u64;
    fn session(&self) -> String;
}

struct RealEnv;

impl Env for RealEnv {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let response = ipc::call_text(method, params).map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_str(crate::launch::strip_bom(&response))
            .map_err(|error| error.to_string())?;
        value
            .get("result")
            .cloned()
            .ok_or_else(|| value.get("error").unwrap_or(&value).to_string())
    }

    fn load(&mut self, tab: &str) -> Result<Option<Plan>, String> {
        match std::fs::read(path(tab)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| error.to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn store(&mut self, plan: &Plan) -> Result<(), String> {
        let dir = rundir::dir("takeover");
        rundir::ensure_private(&dir).map_err(|error| error.to_string())?;
        let text = serde_json::to_string(plan).map_err(|error| error.to_string())?;
        crate::viewer::write_scratch_file_in(&path(&plan.tab), &text, &dir)
            .map_err(|error| error.to_string())
    }

    fn discard(&mut self, tab: &str) -> Result<(), String> {
        match std::fs::remove_file(path(tab)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn plans(&mut self) -> Vec<Plan> {
        let dir = rundir::dir("takeover");
        if !rundir::is_private(&dir) {
            return Vec::new();
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| std::fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect()
    }

    fn set_snoozed(&mut self, tab: &str, snoozed: bool) -> Result<(), String> {
        let dir = snooze::dir();
        if snoozed {
            snooze::set(&dir, tab)
        } else {
            snooze::clear(&dir, tab)
        }
        .map_err(|error| error.to_string())
    }

    fn now(&self) -> u64 {
        crate::state::unix_now()
    }

    fn session(&self) -> String {
        session_key()
    }
}

fn session_key() -> String {
    ipc::socket_path()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn path(tab: &str) -> PathBuf {
    let key = crate::viewer::document_token(&format!("{}:{tab}", session_key()));
    rundir::dir("takeover").join(format!("{key}.json"))
}

/// One pane of a `pane.list` snapshot, reduced to what restore decides on.
#[derive(Clone, Debug)]
struct Live {
    id: String,
    tab: String,
    workspace: String,
    terminal: Option<String>,
    sidebar: bool,
}

fn live_panes(list: &Value) -> Result<Vec<Live>, String> {
    let panes = list["panes"].as_array().ok_or("missing pane list")?;
    Ok(panes
        .iter()
        .filter_map(|pane| {
            let tokens = &pane["tokens"];
            Some(Live {
                id: pane["pane_id"].as_str()?.to_string(),
                tab: pane["tab_id"].as_str()?.to_string(),
                workspace: pane["workspace_id"].as_str().unwrap_or_default().into(),
                terminal: pane["terminal_id"].as_str().map(str::to_string),
                sidebar: tokens[crate::launch::METADATA_SOURCE].is_string()
                    || tokens[crate::launch::SC_METADATA_SOURCE].is_string(),
            })
        })
        .collect())
}

fn tree(layout: &Layout, region: Rect) -> Result<Node, String> {
    if let Some(pane) = layout.panes.iter().find(|pane| pane.rect == region) {
        return Ok(Node::Pane(pane.pane_id.clone()));
    }
    let split = layout
        .splits
        .iter()
        .find(|split| split.rect == region)
        .ok_or("cannot capture the current split layout")?;
    if !split.ratio.is_finite() || !(0.1..=0.9).contains(&split.ratio) {
        return Err("invalid layout split ratio".into());
    }
    let mut first = region;
    let mut second = region;
    match split.direction.as_str() {
        "right" => {
            let boundary = layout
                .panes
                .iter()
                .filter(|pane| {
                    pane.rect.y == region.y
                        && pane.rect.x > region.x
                        && pane.rect.x < region.x + region.width
                })
                .map(|pane| pane.rect.x)
                .min_by_key(|boundary| {
                    (*boundary - region.x - (region.width as f64 * split.ratio) as i64).abs()
                })
                .ok_or("missing vertical split boundary")?;
            first.width = boundary - region.x;
            second.x = boundary;
            second.width = region.width - first.width;
        }
        "down" => {
            let boundary = layout
                .panes
                .iter()
                .filter(|pane| {
                    pane.rect.x == region.x
                        && pane.rect.y > region.y
                        && pane.rect.y < region.y + region.height
                })
                .map(|pane| pane.rect.y)
                .min_by_key(|boundary| {
                    (*boundary - region.y - (region.height as f64 * split.ratio) as i64).abs()
                })
                .ok_or("missing horizontal split boundary")?;
            first.height = boundary - region.y;
            second.y = boundary;
            second.height = region.height - first.height;
        }
        _ => return Err("unsupported split direction".into()),
    }
    Ok(Node::Split {
        direction: split.direction.clone(),
        ratio: split.ratio,
        first: Box::new(tree(layout, first)?),
        second: Box::new(tree(layout, second)?),
    })
}

fn move_pane(
    env: &mut impl Env,
    pane: &str,
    tab: &str,
    target: &str,
    direction: &str,
    ratio: f64,
) -> Result<(), String> {
    env.call(
        "pane.move",
        json!({"pane_id": pane, "focus": false,
        "destination": {"type": "tab", "tab_id": tab,
            "target_pane_id": target, "split": direction, "ratio": ratio}}),
    )?;
    Ok(())
}

/// Move `pane` into `parking_tab`, splitting that tab's largest pane.
fn park_pane(env: &mut impl Env, pane: &str, parking_tab: &str) -> Result<(), String> {
    let list = env.call("pane.list", json!({}))?;
    let anchor = live_panes(&list)?
        .into_iter()
        .find(|live| live.tab == parking_tab)
        .ok_or("the temporary tab is gone")?;
    let layout: Layout = serde_json::from_value(
        env.call("pane.layout", json!({"pane_id": anchor.id}))?["layout"].clone(),
    )
    .map_err(|error| error.to_string())?;
    let target = layout
        .panes
        .iter()
        .filter(|target| target.rect.width >= 16 || target.rect.height >= 8)
        .max_by_key(|target| target.rect.width * target.rect.height)
        .ok_or("not enough room to park the working panes")?;
    let direction = if target.rect.width >= 16 && target.rect.width >= target.rect.height * 2 {
        "right"
    } else if target.rect.height >= 8 {
        "down"
    } else {
        "right"
    };
    move_pane(env, pane, parking_tab, &target.pane_id, direction, 0.5)
}

/// Rebuild `node` around `anchor` (already in `tab`) by moving each sibling
/// subtree's representative next to it. Best-effort: a subtree whose move
/// fails is skipped and stays parked. Returns how many panes were left behind.
fn replay_with(
    node: &Node,
    anchor: &str,
    tab: &str,
    request: &mut impl FnMut(&str, Value) -> Result<Value, String>,
) -> usize {
    let Node::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    else {
        return 0;
    };
    let anchor_first = first.contains(anchor);
    let (own, other) = if anchor_first {
        (first, second)
    } else {
        (second, first)
    };
    let incoming = other.representative();
    let moved = request(
        "pane.move",
        json!({"pane_id": incoming, "focus": false,
        "destination": {"type": "tab", "tab_id": tab, "target_pane_id": anchor,
            "split": direction, "ratio": ratio}}),
    );
    if moved.is_err() {
        return other.pane_count() + replay_with(own, anchor, tab, request);
    }
    if !anchor_first {
        // A failed swap only mirrors this split; every pane is still shown.
        let _ = request(
            "pane.swap",
            json!({"source_pane_id": incoming, "target_pane_id": anchor}),
        );
    }
    replay_with(own, anchor, tab, request) + replay_with(other, incoming, tab, request)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// The journal's own viewer is closing and may be moved out of the way.
    ViewerClosing,
    /// Anyone else. A present viewer blocks the restore.
    Recover,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The viewer still exists; nothing was touched.
    ViewerPresent,
    /// The journal applied; `unrestored` panes stayed in the relabelled tab.
    Restored { unrestored: usize },
    /// The previewed tab itself is gone; parked panes stay where they are.
    Abandoned,
}

fn abandon(env: &mut impl Env, plan: &Plan, panes: &[Live]) -> Result<(), String> {
    let parking_holds_work = panes
        .iter()
        .any(|pane| pane.tab == plan.parking_tab && pane.id != plan.parking_pane);
    if parking_holds_work {
        let _ = env.call(
            "tab.rename",
            json!({"tab_id": plan.parking_tab, "label": UNRESTORED_LABEL}),
        );
        let _ = env.set_snoozed(&plan.parking_tab, false);
        if panes.iter().any(|pane| pane.id == plan.parking_pane) {
            let _ = env.call("pane.close", json!({"pane_id": plan.parking_pane}));
        }
    } else if panes.iter().any(|pane| pane.id == plan.parking_pane) {
        let _ = env.call("pane.close", json!({"pane_id": plan.parking_pane}));
    }
    env.discard(&plan.tab)
}

fn restore_with(env: &mut impl Env, plan: &Plan, trigger: Trigger) -> Result<Outcome, String> {
    let list = env.call("pane.list", json!({}))?;
    let panes = live_panes(&list)?;
    let find = |id: &str| panes.iter().find(|pane| pane.id == id);
    let viewer = plan.viewer.as_deref().and_then(find);
    if trigger == Trigger::Recover && viewer.is_some() {
        return Ok(Outcome::ViewerPresent);
    }
    if !panes.iter().any(|pane| pane.tab == plan.tab) {
        abandon(env, plan, &panes)?;
        return Ok(Outcome::Abandoned);
    }

    let mut plan = plan.clone();
    let recorded = |plan: &Plan, pane: &Live| {
        plan.terminals.get(&pane.id).map(String::as_str) == pane.terminal.as_deref()
    };
    let owner_present = find(&plan.owner).is_some_and(|pane| pane.tab == plan.tab);
    if !owner_present
        && let Some(replacement) = panes.iter().find(|pane| {
            pane.tab == plan.tab && pane.sidebar && !plan.terminals.contains_key(&pane.id)
        })
    {
        // The ensure hook replaced a crashed sidebar: restore around it.
        let old = plan.owner.clone();
        plan.tree.replace(&old, &replacement.id);
        plan.terminals.remove(&old);
        if let Some(terminal) = &replacement.terminal {
            plan.terminals
                .insert(replacement.id.clone(), terminal.clone());
        }
        plan.owner = replacement.id.clone();
    }

    // Panes the user closed, or moved to a third tab, are left out of the
    // restore rather than blocking it.
    let alive: BTreeSet<String> = panes
        .iter()
        .filter(|pane| {
            (pane.tab == plan.tab || pane.tab == plan.parking_tab)
                && plan.tree.contains(&pane.id)
                && recorded(&plan, pane)
        })
        .map(|pane| pane.id.clone())
        .collect();
    let parking_exists = panes.iter().any(|pane| pane.tab == plan.parking_tab);
    let tree = plan.tree.prune(&|id| alive.contains(id));
    let Some(tree) = tree.filter(|_| parking_exists) else {
        // Nothing parked can be restored. Anything unrecognised still in the
        // parking tab is the user's: relabel and un-snooze it, never leave it
        // under the "temporary preview" name.
        abandon(env, &plan, &panes)?;
        return Ok(Outcome::Restored { unrestored: 0 });
    };

    let in_tab = |id: &str| find(id).is_some_and(|pane| pane.tab == plan.tab);
    let anchor = if alive.contains(&plan.owner) && in_tab(&plan.owner) {
        plan.owner.clone()
    } else {
        tree.representative().to_string()
    };
    if !in_tab(&anchor) {
        // No sidebar to rebuild around (the user hid it): bring the first
        // working pane in beside whatever the tab still shows. This is the
        // only step that may fail before anything moved.
        let target = viewer
            .filter(|viewer| viewer.tab == plan.tab)
            .or_else(|| panes.iter().find(|pane| pane.tab == plan.tab))
            .map(|pane| pane.id.clone())
            .ok_or("the previewed tab has no panes")?;
        move_pane(env, &anchor, &plan.tab, &target, "right", 0.5)?;
    }

    // From here every step is best-effort: the journal is deleted at the end
    // whatever happens, so a restore can never leave the tab trapped.
    let mut members = Vec::new();
    tree.panes(&mut members);
    for id in &members {
        if *id != anchor && in_tab(id) {
            let _ = park_pane(env, id, &plan.parking_tab);
        }
    }
    if trigger == Trigger::ViewerClosing
        && let Some(viewer) = viewer.filter(|viewer| viewer.tab == plan.tab && viewer.id != anchor)
    {
        let _ = park_pane(env, &viewer.id, &plan.parking_tab);
    }
    // Panes in the parking tab the journal cannot account for (identity
    // changed, opened there by the user) are left in place — and must not
    // disappear behind the snoozed "temporary preview" label.
    let foreign = panes
        .iter()
        .filter(|pane| {
            pane.tab == plan.parking_tab
                && pane.id != plan.parking_pane
                && plan.viewer.as_deref() != Some(pane.id.as_str())
                && !alive.contains(&pane.id)
        })
        .count();
    let unrestored = foreign
        + replay_with(&tree, &anchor, &plan.tab, &mut |method, params| {
            env.call(method, params)
        });
    let _ = env.call("pane.focus", json!({"pane_id": anchor}));
    env.discard(&plan.tab)?;
    if unrestored > 0 {
        let _ = env.call(
            "tab.rename",
            json!({"tab_id": plan.parking_tab, "label": UNRESTORED_LABEL}),
        );
        let _ = env.set_snoozed(&plan.parking_tab, false);
    }
    let _ = env.call("pane.close", json!({"pane_id": plan.parking_pane}));
    Ok(Outcome::Restored { unrestored })
}

/// The takeover's own viewer is closing: bring the working panes back. A call
/// from any other viewer (an ordinary inline preview) is a plain recovery,
/// which leaves a still-present takeover viewer alone.
pub fn restore(tab: &str, closing_viewer: &str) -> Result<bool, String> {
    if !path(tab).exists() {
        return Ok(false);
    }
    let _lock = LaunchLock::acquire(true).ok_or("could not lock the sidebar layout")?;
    let mut env = RealEnv;
    let Some(plan) = env.load(tab)? else {
        return Ok(false);
    };
    let trigger = if plan.viewer.as_deref() == Some(closing_viewer) {
        Trigger::ViewerClosing
    } else {
        Trigger::Recover
    };
    Ok(!matches!(
        restore_with(&mut env, &plan, trigger)?,
        Outcome::ViewerPresent
    ))
}

/// Give up on a journal the user cannot restore (second close after a
/// failed restore): the parked panes stay in their relabelled tab.
pub fn abandon_tab(tab: &str) -> Result<(), String> {
    if !path(tab).exists() {
        return Ok(());
    }
    let _lock = LaunchLock::acquire(true).ok_or("could not lock the sidebar layout")?;
    let mut env = RealEnv;
    let Some(plan) = env.load(tab)? else {
        return Ok(());
    };
    let list = env.call("pane.list", json!({}))?;
    let panes = live_panes(&list)?;
    abandon(&mut env, &plan, &panes)
}

/// Explicit recovery (a file click, the sidebar's Esc): restore only when
/// the viewer is gone.
pub fn recover(tab: &str) -> Result<(), String> {
    if !path(tab).exists() {
        return Ok(());
    }
    let _lock = LaunchLock::acquire(true).ok_or("could not lock the sidebar layout")?;
    let mut env = RealEnv;
    if let Some(plan) = env.load(tab)? {
        restore_with(&mut env, &plan, Trigger::Recover)?;
    }
    Ok(())
}

/// Hook-driven recovery; the caller already holds the launcher lock.
pub(crate) fn recover_locked(tab: &str, panes_json: &str) {
    let mut env = RealEnv;
    sweep_with(&mut env, panes_json);
    let _ = recover_from_hook(&mut env, tab);
}

fn recover_from_hook(env: &mut impl Env, tab: &str) -> Result<(), String> {
    let Some(mut plan) = env.load(tab)? else {
        return Ok(());
    };
    let now = env.now();
    if now.saturating_sub(plan.last_hook_attempt) >= HOOK_WINDOW_SECS {
        plan.hook_attempts = 0;
    }
    if plan.hook_attempts >= HOOK_ATTEMPTS_PER_WINDOW {
        return Ok(());
    }
    // The common case — the viewer is simply still open — is not an attempt,
    // or ordinary focus traffic would exhaust the budget before the user
    // ever closes it.
    let listed = env.call("pane.list", json!({}));
    if let Ok(list) = &listed
        && let Some(viewer) = plan.viewer.as_deref()
        && live_panes(list).is_ok_and(|panes| panes.iter().any(|pane| pane.id == viewer))
    {
        return Ok(());
    }
    // Count the attempt BEFORE it runs: a crash mid-restore still spends it.
    plan.hook_attempts += 1;
    plan.last_hook_attempt = now;
    env.store(&plan)?;
    listed?;
    restore_with(env, &plan, Trigger::Recover).map(drop)
}

/// Journals of this session whose previewed tab no longer exists (tab closed,
/// or ids changed across a server restart) can never apply: abandon them.
fn sweep_with(env: &mut impl Env, panes_json: &str) {
    let Ok(value) = serde_json::from_str::<Value>(crate::launch::strip_bom(panes_json)) else {
        return;
    };
    let Ok(panes) = live_panes(&value["result"]) else {
        return;
    };
    // An empty snapshot is far more likely a transient host failure than a
    // session with no tabs at all; never abandon on it.
    if panes.is_empty() {
        return;
    }
    let session = env.session();
    for plan in env.plans() {
        if plan.session == session && !panes.iter().any(|pane| pane.tab == plan.tab) {
            let _ = abandon(env, &plan, &panes);
        }
    }
}

pub fn open(
    owner: &str,
    cwd: &Path,
    doc: &str,
    payload: &str,
    tab: &str,
    dock_right: bool,
    width: u16,
) -> Result<crate::viewer::PreviewTarget, String> {
    let _lock = LaunchLock::acquire(true).ok_or("could not lock the sidebar layout")?;
    open_with(&mut RealEnv, owner, cwd, tab, |_| {
        crate::viewer::spawn_inline_pane(owner, cwd, doc, payload, tab, dock_right, width)
    })
}

fn open_with<E: Env>(
    env: &mut E,
    owner: &str,
    cwd: &Path,
    tab: &str,
    spawn: impl FnOnce(&mut E) -> Result<crate::viewer::PreviewTarget, String>,
) -> Result<crate::viewer::PreviewTarget, String> {
    if let Some(plan) = env.load(tab)?
        && restore_with(env, &plan, Trigger::Recover)? == Outcome::ViewerPresent
    {
        return Err(
            "the previous temporary preview is still open; close it (q) to bring your \
             working panes back"
                .into(),
        );
    }
    let layout: Layout = serde_json::from_value(
        env.call("pane.layout", json!({"pane_id": owner}))?["layout"].clone(),
    )
    .map_err(|error| error.to_string())?;
    let original = tree(&layout, layout.area)?;
    if !original.contains(owner) {
        return Err("sidebar is missing from the current layout".into());
    }
    let list = env.call("pane.list", json!({}))?;
    let panes = live_panes(&list)?;
    let mut ids = Vec::new();
    original.panes(&mut ids);
    let mut terminals = BTreeMap::new();
    for id in &ids {
        let terminal = panes
            .iter()
            .find(|pane| pane.id == *id)
            .and_then(|pane| pane.terminal.clone())
            .ok_or("missing working pane identity")?;
        terminals.insert(id.clone(), terminal);
    }
    let workspace = panes
        .iter()
        .find(|pane| pane.id == owner)
        .map(|pane| pane.workspace.clone())
        .filter(|workspace| !workspace.is_empty())
        .ok_or("missing workspace identity")?;
    let created = env.call(
        "tab.create",
        json!({"workspace_id": workspace, "focus": false,
        "label": PARKING_LABEL, "cwd": cwd.display().to_string()}),
    )?;
    let parking_tab = created["tab"]["tab_id"]
        .as_str()
        .ok_or("missing parking tab")?
        .to_string();
    let parking_pane = created["root_pane"]["pane_id"]
        .as_str()
        .ok_or("missing parking pane")?
        .to_string();
    let mut plan = Plan {
        tab: tab.into(),
        owner: owner.into(),
        parking_tab,
        parking_pane,
        viewer: None,
        tree: original,
        terminals,
        session: env.session(),
        hook_attempts: 0,
        last_hook_attempt: 0,
    };
    if let Err(error) = env
        .set_snoozed(&plan.parking_tab, true)
        .and_then(|_| env.store(&plan))
    {
        let _ = env.call("pane.close", json!({"pane_id": plan.parking_pane}));
        return Err(error);
    }
    let mut result = Ok(());
    for pane in &ids {
        if pane != owner
            && let Err(error) = park_pane(env, pane, &plan.parking_tab)
        {
            result = Err(error);
            break;
        }
    }
    let result = result.and_then(|_| {
        let target = spawn(env)?;
        plan.viewer = Some(target.pane_id.clone());
        env.store(&plan)?;
        Ok(target)
    });
    if result.is_err() {
        // A viewer that did start is ours to close: it has not shown the user
        // anything they could have edited yet.
        let _ = restore_with(env, &plan, Trigger::ViewerClosing);
        if let Some(viewer) = &plan.viewer {
            let _ = env.call("pane.close", json!({"pane_id": viewer}));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 40,
    };

    fn sample() -> Layout {
        serde_json::from_value(json!({
            "area": {"x":0,"y":0,"width":120,"height":40},
            "panes": [
                {"pane_id":"sidebar","rect":{"x":0,"y":0,"width":30,"height":40}},
                {"pane_id":"top","rect":{"x":30,"y":0,"width":90,"height":24}},
                {"pane_id":"bottom","rect":{"x":30,"y":24,"width":90,"height":16}}
            ],
            "splits": [
                {"direction":"right","ratio":0.25,"rect":{"x":0,"y":0,"width":120,"height":40}},
                {"direction":"down","ratio":0.6,"rect":{"x":30,"y":0,"width":90,"height":40}}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn restore_replays_nested_directions_and_exact_ratios() {
        let layout = sample();
        let node = tree(&layout, layout.area).unwrap();
        let mut requests = Vec::new();
        let left = replay_with(&node, "sidebar", "w1:t1", &mut |method, params| {
            requests.push((method.to_string(), params));
            Ok(json!({}))
        });
        assert_eq!(left, 0);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].1["destination"]["ratio"], 0.25);
        assert_eq!(requests[0].1["pane_id"], "top");
        assert_eq!(requests[1].1["destination"]["split"], "down");
        assert_eq!(requests[1].1["destination"]["ratio"], 0.6);
        assert_eq!(requests[1].1["destination"]["target_pane_id"], "top");
    }

    #[test]
    fn right_docked_anchor_swaps_without_inverting_saved_ratio() {
        let node = Node::Split {
            direction: "right".into(),
            ratio: 0.75,
            first: Box::new(Node::Pane("work".into())),
            second: Box::new(Node::Pane("sidebar".into())),
        };
        let mut requests = Vec::new();
        replay_with(&node, "sidebar", "w1:t1", &mut |method, params| {
            requests.push((method.to_string(), params));
            Ok(json!({}))
        });
        assert_eq!(requests[0].1["destination"]["ratio"], 0.75);
        assert_eq!(requests[1].0, "pane.swap");
        assert_eq!(requests[1].1["source_pane_id"], "work");
    }

    #[test]
    fn a_failed_move_skips_only_that_subtree() {
        let layout = sample();
        let node = tree(&layout, layout.area).unwrap();
        let mut calls = 0;
        let left = replay_with(&node, "sidebar", "tab", &mut |_, _| {
            calls += 1;
            Err("move failed".into())
        });
        assert_eq!(calls, 1, "the skipped subtree is not attempted");
        assert_eq!(left, 2);
    }

    #[test]
    fn pruning_collapses_splits_like_herdr_does() {
        let layout = sample();
        let node = tree(&layout, layout.area).unwrap();
        let pruned = node.prune(&|id| id != "bottom").unwrap();
        assert_eq!(
            pruned,
            Node::Split {
                direction: "right".into(),
                ratio: 0.25,
                first: Box::new(Node::Pane("sidebar".into())),
                second: Box::new(Node::Pane("top".into())),
            }
        );
        assert!(node.prune(&|_| false).is_none());
    }

    // ---- A simulated herdr host -------------------------------------------

    struct Tab {
        workspace: String,
        root: Option<Node>,
        label: String,
    }

    #[derive(Default)]
    struct Fake {
        tabs: BTreeMap<String, Tab>,
        terminals: BTreeMap<String, String>,
        tokens: BTreeMap<String, Value>,
        plans: BTreeMap<String, Plan>,
        snoozed: BTreeSet<String>,
        focused: Option<String>,
        fail_moves_of: BTreeSet<String>,
        fail_list: bool,
        next: u32,
        now: u64,
        mutations: Vec<String>,
        list_calls: usize,
    }

    fn remove_leaf(node: Node, id: &str) -> Option<Node> {
        match node {
            Node::Pane(pane) if pane == id => None,
            Node::Pane(_) => Some(node),
            Node::Split {
                direction,
                ratio,
                first,
                second,
            } => match (remove_leaf(*first, id), remove_leaf(*second, id)) {
                (Some(first), Some(second)) => Some(Node::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(survivor), None) | (None, Some(survivor)) => Some(survivor),
                (None, None) => None,
            },
        }
    }

    fn split_leaf(node: &mut Node, target: &str, new: &str, direction: &str, ratio: f64) {
        match node {
            Node::Pane(id) if id == target => {
                *node = Node::Split {
                    direction: direction.into(),
                    ratio,
                    first: Box::new(Node::Pane(target.into())),
                    second: Box::new(Node::Pane(new.into())),
                };
            }
            Node::Split { first, second, .. } => {
                split_leaf(first, target, new, direction, ratio);
                split_leaf(second, target, new, direction, ratio);
            }
            Node::Pane(_) => {}
        }
    }

    fn swap_leaves(node: &mut Node, a: &str, b: &str) {
        match node {
            Node::Pane(id) if id == a => *id = b.into(),
            Node::Pane(id) if id == b => *id = a.into(),
            Node::Split { first, second, .. } => {
                swap_leaves(first, a, b);
                swap_leaves(second, a, b);
            }
            Node::Pane(_) => {}
        }
    }

    fn rects(node: &Node, rect: Rect, panes: &mut Vec<Value>, splits: &mut Vec<Value>) {
        match node {
            Node::Pane(id) => panes.push(json!({"pane_id": id, "rect": rect})),
            Node::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                splits.push(json!({"direction": direction, "ratio": ratio, "rect": rect}));
                let (mut a, mut b) = (rect, rect);
                if direction == "right" {
                    a.width = (rect.width as f64 * ratio).round() as i64;
                    b.x = rect.x + a.width;
                    b.width = rect.width - a.width;
                } else {
                    a.height = (rect.height as f64 * ratio).round() as i64;
                    b.y = rect.y + a.height;
                    b.height = rect.height - a.height;
                }
                rects(first, a, panes, splits);
                rects(second, b, panes, splits);
            }
        }
    }

    impl Fake {
        fn tab_of(&self, pane: &str) -> Option<String> {
            self.tabs
                .iter()
                .find(|(_, tab)| tab.root.as_ref().is_some_and(|root| root.contains(pane)))
                .map(|(id, _)| id.clone())
        }

        fn fresh(&mut self, prefix: &str) -> String {
            self.next += 1;
            format!("{prefix}{}", self.next)
        }

        fn add_tab(&mut self, id: &str, root: Node) {
            let mut panes = Vec::new();
            root.panes(&mut panes);
            for pane in panes {
                self.terminals
                    .entry(pane.clone())
                    .or_insert_with(|| format!("term-{pane}"));
            }
            self.tabs.insert(
                id.into(),
                Tab {
                    workspace: "w1".into(),
                    root: Some(root),
                    label: id.into(),
                },
            );
        }

        fn detach(&mut self, pane: &str) {
            if let Some(tab) = self.tab_of(pane) {
                let entry = self.tabs.get_mut(&tab).unwrap();
                entry.root = entry.root.take().and_then(|root| remove_leaf(root, pane));
                if entry.root.is_none() {
                    self.tabs.remove(&tab);
                }
            }
        }

        fn close(&mut self, pane: &str) {
            self.detach(pane);
            self.terminals.remove(pane);
            self.tokens.remove(pane);
        }

        fn root(&self, tab: &str) -> Option<&Node> {
            self.tabs.get(tab).and_then(|tab| tab.root.as_ref())
        }
    }

    impl Env for Fake {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            if method != "pane.list" && method != "pane.layout" {
                self.mutations.push(format!("{method} {params}"));
            }
            match method {
                "pane.list" => {
                    self.list_calls += 1;
                    if self.fail_list {
                        return Err("host unavailable".into());
                    }
                    let mut panes = Vec::new();
                    for (tab_id, tab) in &self.tabs {
                        let mut ids = Vec::new();
                        if let Some(root) = &tab.root {
                            root.panes(&mut ids);
                        }
                        for id in ids {
                            panes.push(json!({
                                "pane_id": id, "tab_id": tab_id,
                                "workspace_id": tab.workspace,
                                "terminal_id": self.terminals.get(&id),
                                "tokens": self.tokens.get(&id).cloned().unwrap_or(json!({})),
                                "focused": self.focused.as_deref() == Some(id.as_str()),
                            }));
                        }
                    }
                    Ok(json!({"panes": panes}))
                }
                "pane.layout" => {
                    let pane = params["pane_id"].as_str().unwrap();
                    let tab = self.tab_of(pane).ok_or("pane_not_found")?;
                    let (mut panes, mut splits) = (Vec::new(), Vec::new());
                    rects(self.root(&tab).unwrap(), AREA, &mut panes, &mut splits);
                    Ok(json!({"layout": {"area": AREA, "panes": panes, "splits": splits}}))
                }
                "pane.move" => {
                    let pane = params["pane_id"].as_str().unwrap().to_string();
                    if self.fail_moves_of.contains(&pane) {
                        return Err("pane_move_failed".into());
                    }
                    let dest = &params["destination"];
                    let tab = dest["tab_id"].as_str().unwrap().to_string();
                    let target = dest["target_pane_id"].as_str().unwrap().to_string();
                    if self.tab_of(&target).as_deref() != Some(tab.as_str()) {
                        return Err("target_not_in_tab".into());
                    }
                    if self.tab_of(&pane).as_deref() == Some(tab.as_str()) {
                        return Ok(json!({})); // herdr: same-tab move is a no-op
                    }
                    self.detach(&pane);
                    let direction = dest["split"].as_str().unwrap().to_string();
                    let ratio = dest["ratio"].as_f64().unwrap();
                    let root = self.tabs.get_mut(&tab).unwrap().root.as_mut().unwrap();
                    split_leaf(root, &target, &pane, &direction, ratio);
                    Ok(json!({}))
                }
                "pane.swap" => {
                    let a = params["source_pane_id"].as_str().unwrap();
                    let b = params["target_pane_id"].as_str().unwrap();
                    for tab in self.tabs.values_mut() {
                        if let Some(root) = &mut tab.root {
                            swap_leaves(root, a, b);
                        }
                    }
                    Ok(json!({}))
                }
                "pane.split" => {
                    let target = params["target_pane_id"].as_str().unwrap().to_string();
                    let tab = self.tab_of(&target).ok_or("pane_not_found")?;
                    let new = self.fresh("p");
                    self.terminals.insert(new.clone(), format!("term-{new}"));
                    let ratio = params["ratio"].as_f64().unwrap_or(0.5);
                    let root = self.tabs.get_mut(&tab).unwrap().root.as_mut().unwrap();
                    split_leaf(root, &target, &new, "right", ratio);
                    Ok(json!({"pane": {"pane_id": new}}))
                }
                "pane.close" => {
                    let pane = params["pane_id"].as_str().unwrap().to_string();
                    self.tab_of(&pane).ok_or("pane_not_found")?;
                    self.close(&pane);
                    Ok(json!({}))
                }
                "pane.focus" => {
                    self.focused = params["pane_id"].as_str().map(str::to_string);
                    Ok(json!({}))
                }
                "tab.rename" => {
                    let tab = params["tab_id"].as_str().unwrap();
                    let label = params["label"].as_str().unwrap().to_string();
                    self.tabs.get_mut(tab).ok_or("tab_not_found")?.label = label;
                    Ok(json!({}))
                }
                "tab.create" => {
                    let tab = self.fresh("w1:t");
                    let pane = self.fresh("p");
                    self.add_tab(&tab, Node::Pane(pane.clone()));
                    self.tabs.get_mut(&tab).unwrap().label =
                        params["label"].as_str().unwrap_or_default().into();
                    Ok(json!({"tab": {"tab_id": tab}, "root_pane": {"pane_id": pane}}))
                }
                other => Err(format!("unexpected method {other}")),
            }
        }

        fn load(&mut self, tab: &str) -> Result<Option<Plan>, String> {
            Ok(self.plans.get(tab).cloned())
        }

        fn store(&mut self, plan: &Plan) -> Result<(), String> {
            self.plans.insert(plan.tab.clone(), plan.clone());
            Ok(())
        }

        fn discard(&mut self, tab: &str) -> Result<(), String> {
            self.plans.remove(tab);
            Ok(())
        }

        fn plans(&mut self) -> Vec<Plan> {
            self.plans.values().cloned().collect()
        }

        fn set_snoozed(&mut self, tab: &str, snoozed: bool) -> Result<(), String> {
            if snoozed {
                self.snoozed.insert(tab.into());
            } else {
                self.snoozed.remove(tab);
            }
            Ok(())
        }

        fn now(&self) -> u64 {
            self.now
        }

        fn session(&self) -> String {
            "session-a".into()
        }
    }

    fn pane(id: &str) -> Box<Node> {
        Box::new(Node::Pane(id.into()))
    }

    /// `sidebar | (top / bottom)`, or its mirror when docked right.
    fn working_layout(dock_right: bool) -> Node {
        let work = Box::new(Node::Split {
            direction: "down".into(),
            ratio: 0.6,
            first: pane("top"),
            second: pane("bottom"),
        });
        if dock_right {
            Node::Split {
                direction: "right".into(),
                ratio: 0.75,
                first: work,
                second: pane("sidebar"),
            }
        } else {
            Node::Split {
                direction: "right".into(),
                ratio: 0.25,
                first: pane("sidebar"),
                second: work,
            }
        }
    }

    fn fake_with_takeover(dock_right: bool) -> (Fake, Node) {
        let mut fake = Fake::default();
        let original = working_layout(dock_right);
        fake.add_tab("w1:main", original.clone());
        fake.tokens
            .insert("sidebar".into(), json!({"herdr-sidebar-explorer": "1"}));
        let target = open_with(&mut fake, "sidebar", Path::new("/work"), "w1:main", |env| {
            let response = env.call(
                "pane.split",
                json!({"target_pane_id": "sidebar", "direction": "right", "ratio": 0.3}),
            )?;
            let viewer = response["pane"]["pane_id"].as_str().unwrap().to_string();
            Ok(crate::viewer::PreviewTarget {
                pane_id: viewer,
                tab_id: "w1:main".into(),
                origin_tab_id: "w1:main".into(),
                inline: true,
            })
        })
        .unwrap();
        fake.tokens.insert(
            target.pane_id.clone(),
            json!({"herdr-sidebar-preview": "1", "hs-preview-inline": "1"}),
        );
        fake.mutations.clear();
        (fake, original)
    }

    fn plan(fake: &Fake) -> Plan {
        fake.plans.get("w1:main").cloned().expect("journal present")
    }

    #[test]
    fn takeover_parks_everything_but_the_sidebar_and_the_viewer() {
        let (fake, _) = fake_with_takeover(false);
        let plan = plan(&fake);
        let mut shown = Vec::new();
        fake.root("w1:main").unwrap().panes(&mut shown);
        assert_eq!(
            shown,
            vec!["sidebar".to_string(), plan.viewer.clone().unwrap()]
        );
        assert!(fake.snoozed.contains(&plan.parking_tab));
        assert_eq!(fake.tabs[&plan.parking_tab].label, PARKING_LABEL);
    }

    #[test]
    fn closing_viewer_restores_the_exact_tree_for_both_docks() {
        for dock_right in [false, true] {
            let (mut fake, original) = fake_with_takeover(dock_right);
            let plan = plan(&fake);
            let outcome = restore_with(&mut fake, &plan, Trigger::ViewerClosing).unwrap();
            assert_eq!(outcome, Outcome::Restored { unrestored: 0 });
            assert_eq!(
                fake.root("w1:main"),
                Some(&original),
                "dock_right={dock_right}"
            );
            assert!(fake.plans.is_empty());
            // Only the (self-closing) viewer is left in the temporary tab.
            let mut parked = Vec::new();
            fake.root(&plan.parking_tab).unwrap().panes(&mut parked);
            assert_eq!(parked, vec![plan.viewer.clone().unwrap()]);
            assert_eq!(fake.focused.as_deref(), Some("sidebar"));
        }
    }

    /// H1: a present viewer — however stale its heartbeat — is never moved or
    /// closed by recovery, from a hook or from a file click.
    #[test]
    fn recovery_never_touches_a_present_viewer_with_a_stale_heartbeat() {
        let (mut fake, _) = fake_with_takeover(false);
        let plan = plan(&fake);
        let viewer = plan.viewer.clone().unwrap();
        fake.tokens.insert(
            viewer.clone(),
            json!({"herdr-sidebar-preview": "0", "hs-preview-inline": "1"}),
        );
        fake.now = 10_000;
        assert_eq!(
            restore_with(&mut fake, &plan, Trigger::Recover).unwrap(),
            Outcome::ViewerPresent
        );
        recover_from_hook(&mut fake, "w1:main").unwrap();
        let err = open_with(&mut fake, "sidebar", Path::new("/work"), "w1:main", |_| {
            panic!("must not spawn over a present viewer")
        })
        .unwrap_err();
        assert!(err.contains("still open"), "{err}");
        assert!(
            fake.mutations.iter().all(|m| !m.starts_with("pane.move")
                && !m.starts_with("pane.close")
                && !m.starts_with("pane.swap")),
            "{:?}",
            fake.mutations
        );
        assert!(fake.tab_of(&viewer).is_some());
        assert!(fake.plans.contains_key("w1:main"));
    }

    /// Healthy focus traffic must not spend the retry budget: after a burst
    /// of hooks while the viewer is open, the first hook after it is closed
    /// from herdr restores immediately.
    #[test]
    fn healthy_focus_bursts_do_not_spend_the_recovery_budget() {
        let (mut fake, original) = fake_with_takeover(false);
        let viewer = plan(&fake).viewer.unwrap();
        fake.now = 5_000;
        for _ in 0..(HOOK_ATTEMPTS_PER_WINDOW * 4) {
            recover_from_hook(&mut fake, "w1:main").unwrap();
        }
        assert_eq!(plan(&fake).hook_attempts, 0);
        fake.close(&viewer);
        recover_from_hook(&mut fake, "w1:main").unwrap();
        assert_eq!(fake.root("w1:main"), Some(&original));
        assert!(fake.plans.is_empty());
    }

    /// H1 end to end: the SAME host snapshot that recovery sees is what a
    /// file click's stale-preview cleanup sees. Neither may close a present
    /// takeover viewer whose heartbeat went stale (suspend, stall).
    #[test]
    fn a_click_never_kills_a_stale_takeover_viewer() {
        let (mut fake, _) = fake_with_takeover(false);
        let plan = plan(&fake);
        let viewer = plan.viewer.clone().unwrap();
        fake.tokens.insert(
            viewer.clone(),
            json!({"herdr-sidebar-preview": "100", "hs-preview-inline": "1",
                   "hs-preview-path": "0123456789abcdef"}),
        );
        let list = json!({ "result": fake.call("pane.list", json!({})).unwrap() }).to_string();
        let previews = crate::viewer::previews_in(&list);
        let stale = previews.iter().find(|p| p.pane_id == viewer).unwrap();
        assert!(stale.stale && !stale.resumed);
        assert!(
            crate::viewer::stale_cleanup(&previews, &list).is_empty(),
            "click-time cleanup must not close a present viewer"
        );
        // The click is answered without spawning a second viewer or waiting.
        assert_eq!(
            crate::viewer::unresponsive_inline_in(&previews, true, "w1:main")
                .map(|p| p.pane_id.as_str()),
            Some(viewer.as_str())
        );
        assert!(
            !previews
                .iter()
                .any(|p| crate::viewer::routable(p, true, "w1:main"))
        );
        assert_eq!(
            restore_with(&mut fake, &plan, Trigger::Recover).unwrap(),
            Outcome::ViewerPresent
        );
        assert!(fake.tab_of(&viewer).is_some());
        assert!(fake.plans.contains_key("w1:main"));
    }

    #[test]
    fn a_closed_viewer_is_recovered_by_the_next_hook() {
        let (mut fake, original) = fake_with_takeover(false);
        let viewer = plan(&fake).viewer.unwrap();
        fake.close(&viewer);
        recover_from_hook(&mut fake, "w1:main").unwrap();
        assert_eq!(fake.root("w1:main"), Some(&original));
        assert!(fake.plans.is_empty());
    }

    /// H2: a parked pane that exited is pruned instead of blocking restore.
    #[test]
    fn an_exited_parked_pane_is_pruned_not_trapping_the_journal() {
        let (mut fake, original) = fake_with_takeover(false);
        let viewer = plan(&fake).viewer.unwrap();
        fake.close("bottom");
        fake.close(&viewer);
        recover_from_hook(&mut fake, "w1:main").unwrap();
        assert_eq!(
            fake.root("w1:main"),
            original.prune(&|id| id != "bottom").as_ref()
        );
        assert!(fake.plans.is_empty(), "the tab's previews are not trapped");
    }

    #[test]
    fn a_pane_moved_to_another_tab_is_left_where_the_user_put_it() {
        let (mut fake, _) = fake_with_takeover(false);
        let viewer = plan(&fake).viewer.unwrap();
        fake.add_tab("w1:other", Node::Pane("elsewhere".into()));
        fake.detach("top");
        let root = fake
            .tabs
            .get_mut("w1:other")
            .unwrap()
            .root
            .as_mut()
            .unwrap();
        split_leaf(root, "elsewhere", "top", "right", 0.5);
        fake.close(&viewer);
        recover_from_hook(&mut fake, "w1:main").unwrap();
        assert_eq!(fake.tab_of("top").as_deref(), Some("w1:other"));
        assert_eq!(fake.tab_of("bottom").as_deref(), Some("w1:main"));
        assert!(fake.plans.is_empty());
    }

    #[test]
    fn unrecognisable_parked_panes_are_relabelled_not_hidden() {
        let (mut fake, _) = fake_with_takeover(false);
        let plan = plan(&fake);
        // Both parked panes lost their recorded identity (e.g. respawned).
        for id in ["top", "bottom"] {
            fake.terminals.insert(id.into(), format!("respawned-{id}"));
        }
        fake.close(plan.viewer.as_deref().unwrap());
        recover_from_hook(&mut fake, "w1:main").unwrap();
        assert!(fake.plans.is_empty());
        assert_eq!(fake.tabs[&plan.parking_tab].label, UNRESTORED_LABEL);
        assert!(!fake.snoozed.contains(&plan.parking_tab));
        for id in ["top", "bottom"] {
            assert_eq!(fake.tab_of(id).as_deref(), Some(plan.parking_tab.as_str()));
        }
    }

    #[test]
    fn a_closed_parking_tab_discards_the_journal_without_moves() {
        let (mut fake, _) = fake_with_takeover(false);
        let plan = plan(&fake);
        for id in ["top", "bottom", plan.parking_pane.as_str()] {
            fake.close(id);
        }
        fake.close(plan.viewer.as_deref().unwrap());
        recover_from_hook(&mut fake, "w1:main").unwrap();
        assert!(fake.plans.is_empty());
        assert!(fake.mutations.iter().all(|m| !m.starts_with("pane.move")));
        assert_eq!(fake.root("w1:main"), Some(&Node::Pane("sidebar".into())));
    }

    /// Hiding the sidebar mid-takeover leaves only the viewer; its close
    /// still brings the working panes back around the first of them.
    #[test]
    fn a_hidden_sidebar_does_not_block_the_restore() {
        let (mut fake, original) = fake_with_takeover(false);
        let plan = plan(&fake);
        fake.close("sidebar");
        let outcome = restore_with(&mut fake, &plan, Trigger::ViewerClosing).unwrap();
        assert_eq!(outcome, Outcome::Restored { unrestored: 0 });
        assert_eq!(
            fake.root("w1:main"),
            original.prune(&|id| id != "sidebar").as_ref()
        );
        assert!(fake.plans.is_empty());
    }

    #[test]
    fn a_replacement_sidebar_takes_the_saved_owner_slot() {
        let (mut fake, original) = fake_with_takeover(false);
        let plan = plan(&fake);
        let viewer = plan.viewer.clone().unwrap();
        // The ensure hook replaced a crashed sidebar beside the viewer.
        fake.close("sidebar");
        let root = fake.tabs.get_mut("w1:main").unwrap().root.as_mut().unwrap();
        split_leaf(root, &viewer, "sidebar2", "right", 0.5);
        fake.terminals
            .insert("sidebar2".into(), "term-sidebar2".into());
        fake.tokens
            .insert("sidebar2".into(), json!({"herdr-sidebar-git": "1"}));
        restore_with(&mut fake, &plan, Trigger::ViewerClosing).unwrap();
        let mut expected = original;
        expected.replace("sidebar", "sidebar2");
        assert_eq!(fake.root("w1:main"), Some(&expected));
    }

    #[test]
    fn a_pane_added_during_the_preview_is_kept() {
        let (mut fake, _) = fake_with_takeover(false);
        let plan = plan(&fake);
        let root = fake.tabs.get_mut("w1:main").unwrap().root.as_mut().unwrap();
        split_leaf(root, "sidebar", "extra", "down", 0.5);
        fake.terminals.insert("extra".into(), "term-extra".into());
        restore_with(&mut fake, &plan, Trigger::ViewerClosing).unwrap();
        assert_eq!(fake.tab_of("extra").as_deref(), Some("w1:main"));
        for id in ["top", "bottom"] {
            assert_eq!(fake.tab_of(id).as_deref(), Some("w1:main"));
        }
        assert!(fake.plans.is_empty());
    }

    /// M2/M3: a move the host rejects leaves that subtree parked in a
    /// relabelled, un-snoozed tab; the journal is still deleted, so nothing
    /// retries in a loop and the viewer can close.
    #[test]
    fn a_rejected_move_finishes_with_a_labelled_leftover_tab() {
        let (mut fake, _) = fake_with_takeover(false);
        let plan = plan(&fake);
        fake.fail_moves_of.insert("top".into());
        let outcome = restore_with(&mut fake, &plan, Trigger::ViewerClosing).unwrap();
        assert_eq!(outcome, Outcome::Restored { unrestored: 2 });
        assert!(fake.plans.is_empty());
        assert_eq!(fake.tabs[&plan.parking_tab].label, UNRESTORED_LABEL);
        assert!(!fake.snoozed.contains(&plan.parking_tab));
        for id in ["top", "bottom"] {
            assert_eq!(fake.tab_of(id).as_deref(), Some(plan.parking_tab.as_str()));
        }
    }

    #[test]
    fn a_closed_preview_tab_abandons_but_keeps_the_parked_work() {
        let (mut fake, _) = fake_with_takeover(false);
        let plan = plan(&fake);
        fake.close("sidebar");
        fake.close(plan.viewer.as_deref().unwrap());
        assert!(!fake.tabs.contains_key("w1:main"));
        assert_eq!(
            restore_with(&mut fake, &plan, Trigger::Recover).unwrap(),
            Outcome::Abandoned
        );
        assert!(fake.plans.is_empty());
        assert_eq!(fake.tabs[&plan.parking_tab].label, UNRESTORED_LABEL);
        assert!(!fake.snoozed.contains(&plan.parking_tab));
        assert!(fake.tab_of("top").is_some() && fake.tab_of("bottom").is_some());
    }

    #[test]
    fn hook_recovery_is_budgeted_per_window() {
        let (mut fake, _) = fake_with_takeover(false);
        let viewer = plan(&fake).viewer.unwrap();
        fake.close(&viewer);
        fake.fail_list = true;
        fake.now = 1_000;
        for _ in 0..HOOK_ATTEMPTS_PER_WINDOW {
            assert!(recover_from_hook(&mut fake, "w1:main").is_err());
        }
        let calls = fake.list_calls;
        recover_from_hook(&mut fake, "w1:main").unwrap();
        assert_eq!(fake.list_calls, calls, "budget exhausted: no host calls");
        fake.fail_list = false;
        fake.now += HOOK_WINDOW_SECS;
        recover_from_hook(&mut fake, "w1:main").unwrap();
        assert!(fake.plans.is_empty());
    }

    #[test]
    fn sweep_abandons_only_this_sessions_dead_journals() {
        let (mut fake, _) = fake_with_takeover(false);
        let mut foreign = plan(&fake);
        foreign.tab = "w9:gone".into();
        foreign.session = "session-b".into();
        fake.plans.insert(foreign.tab.clone(), foreign);
        let mut dead = plan(&fake);
        dead.tab = "w8:gone".into();
        dead.parking_tab = "w8:parking-gone".into();
        dead.parking_pane = "w8:p-gone".into();
        fake.plans.insert(dead.tab.clone(), dead);
        let list = fake.call("pane.list", json!({})).unwrap();
        sweep_with(&mut fake, &json!({ "result": list }).to_string());
        assert!(fake.plans.contains_key("w1:main"), "live journal kept");
        assert!(
            fake.plans.contains_key("w9:gone"),
            "other session untouched"
        );
        assert!(!fake.plans.contains_key("w8:gone"));
        // An empty or unreadable snapshot never abandons anything.
        sweep_with(&mut fake, r#"{"result":{"panes":[]}}"#);
        sweep_with(&mut fake, "not json");
        assert!(fake.plans.contains_key("w9:gone"));
    }

    #[test]
    fn a_failed_spawn_rolls_the_takeover_back() {
        let mut fake = Fake::default();
        let original = working_layout(false);
        fake.add_tab("w1:main", original.clone());
        let err = open_with(&mut fake, "sidebar", Path::new("/work"), "w1:main", |_| {
            Err("viewer failed to start".into())
        })
        .unwrap_err();
        assert!(err.contains("viewer failed"));
        assert_eq!(fake.root("w1:main"), Some(&original));
        assert!(fake.plans.is_empty());
    }

    #[test]
    fn journals_written_before_the_budget_fields_still_load() {
        let old = r#"{"tab":"w1:t1","owner":"s","parking_tab":"w1:t2","parking_pane":"p",
            "viewer":null,"tree":{"Pane":"s"},"terminals":{"s":"t"}}"#;
        let plan: Plan = serde_json::from_str(old).unwrap();
        assert_eq!(plan.hook_attempts, 0);
        assert!(plan.session.is_empty());
    }

    #[test]
    #[ignore = "requires an attached Herdr session and a freshly built sidebar on PATH"]
    fn live_takeover_roundtrip() {
        live_takeover_case(false, false);
        live_takeover_case(true, false);
        live_takeover_case(false, true);
    }

    /// Visible pane text over the socket (`pane.read`), never a spawned CLI:
    /// a console child could flash a Windows Terminal window.
    fn visible_text(env: &mut RealEnv, pane: &str) -> String {
        env.call("pane.read", json!({"pane_id": pane, "source": "visible"}))
            .ok()
            .and_then(|result| result["read"]["text"].as_str().map(str::to_string))
            .unwrap_or_default()
    }

    fn wait_for(env: &mut RealEnv, pane: &str, needle: &str) -> bool {
        (0..100).any(|_| {
            let found = visible_text(env, pane).contains(needle);
            if !found {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            found
        })
    }

    /// M1 (Windows running-TUI transport): the shell-pane round trip above
    /// cannot show whether a RUNNING crossterm TUI still receives input after
    /// being parked in another tab and moved back. This parks a real preview
    /// viewer (read-only, never edited) and asserts it still reacts to a key.
    /// It creates, and closes, only its own disposable tab. Third-party TUIs
    /// (Claude Code, vim, …) are NOT covered and still need manual checks.
    #[test]
    #[ignore = "M1: needs an attached Herdr session (HERDR_ENV=1) and a freshly built herdr-sidebar on PATH; uses only its own disposable tab"]
    fn live_parked_running_tui_still_takes_input_after_restore() {
        assert_eq!(std::env::var("HERDR_ENV").as_deref(), Ok("1"));
        let mut env = RealEnv;
        let focused = env.call("pane.list", json!({})).unwrap()["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane["focused"] == true)
            .and_then(|pane| pane["pane_id"].as_str())
            .unwrap()
            .to_string();
        let workspace = std::env::var("HERDR_WORKSPACE_ID").unwrap();
        let scratch = std::env::temp_dir().join(format!("herdr-m1-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        let doc = scratch.join("lines.txt");
        let text: String = (1..=400).map(|n| format!("m1 line {n}\n")).collect();
        std::fs::write(&doc, text).unwrap();
        let other = scratch.join("other.txt");
        std::fs::write(&other, "takeover document\n").unwrap();

        let lock = LaunchLock::acquire(true).unwrap();
        let created = env
            .call(
                "tab.create",
                json!({"workspace_id": workspace, "focus": false, "label": "Sidebar M1 test"}),
            )
            .unwrap();
        let tab = created["tab"]["tab_id"].as_str().unwrap().to_string();
        let owner = created["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        snooze::set(&snooze::dir(), &tab).unwrap();
        drop(lock);

        let result = (|| -> Result<(), String> {
            let key = crate::viewer::doc_key_for_file(&doc);
            let tui = crate::viewer::spawn_inline_pane(
                &owner,
                &scratch,
                &key,
                &format!("file\t{}", doc.display()),
                &tab,
                false,
                30,
            )?
            .pane_id;
            if !wait_for(&mut env, &tui, "m1 line 1") {
                return Err("the running viewer never rendered its document".into());
            }
            if visible_text(&mut env, &tui).contains("m1 line 400") {
                return Err("test document must not fit on one screen".into());
            }
            let takeover = open(
                &owner,
                &scratch,
                &crate::viewer::doc_key_for_file(&other),
                &format!("file\t{}", other.display()),
                &tab,
                false,
                30,
            )?;
            let parking = env.load(&tab)?.ok_or("no journal")?.parking_tab;
            let parked = env.call("pane.list", json!({}))?["panes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|pane| pane["pane_id"] == tui && pane["tab_id"] == parking);
            if !parked {
                return Err("the running viewer was not parked".into());
            }
            // `q` must reach the takeover VIEWER, not the shell it starts in:
            // wait until it has acknowledged and rendered its document, or a
            // slow start would read as a false M1 failure.
            let expected = crate::viewer::document_token(&crate::viewer::doc_key_for_file(&other));
            let acknowledged = (0..100).any(|_| {
                let ready = env.call("pane.list", json!({})).is_ok_and(|list| {
                    list["panes"].as_array().is_some_and(|panes| {
                        panes.iter().any(|pane| {
                            pane["pane_id"] == takeover.pane_id
                                && pane["tokens"]["hs-preview-path"] == expected
                        })
                    })
                });
                if !ready {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                ready
            });
            if !acknowledged || !wait_for(&mut env, &takeover.pane_id, "takeover document") {
                return Err("the takeover viewer never rendered its document".into());
            }
            env.call(
                "pane.send_input",
                json!({"pane_id": takeover.pane_id, "keys": ["q"]}),
            )?;
            if !(0..100).any(|_| {
                let gone = !path(&tab).exists();
                if !gone {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                gone
            }) {
                return Err("takeover did not restore".into());
            }
            env.call("pane.send_input", json!({"pane_id": tui, "keys": ["G"]}))?;
            if !wait_for(&mut env, &tui, "m1 line 400") {
                return Err(
                    "the restored running viewer no longer reacts to input (M1 reproduced)".into(),
                );
            }
            Ok(())
        })();
        if let Ok(Some(plan)) = env.load(&tab) {
            let _ = restore(&tab, plan.viewer.as_deref().unwrap_or_default());
            let _ = env.call("tab.close", json!({"tab_id": plan.parking_tab}));
        }
        let _ = env.call("pane.focus", json!({"pane_id": focused}));
        let _ = env.call("tab.close", json!({"tab_id": tab}));
        let _ = std::fs::remove_dir_all(&scratch);
        assert!(result.is_ok(), "{result:?}");
    }

    fn live_takeover_case(dock_right: bool, force_close: bool) {
        assert_eq!(std::env::var("HERDR_ENV").as_deref(), Ok("1"));
        let mut env = RealEnv;
        let list = env.call("pane.list", json!({})).unwrap();
        let focused = list["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane["focused"] == true)
            .unwrap()["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        let workspace = std::env::var("HERDR_WORKSPACE_ID").unwrap();
        let lock = LaunchLock::acquire(true).unwrap();
        let created = env
            .call(
                "tab.create",
                json!({"workspace_id":workspace,"focus":false,"label":"Sidebar takeover test"}),
            )
            .unwrap();
        let tab = created["tab"]["tab_id"].as_str().unwrap().to_string();
        let owner = created["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        snooze::set(&snooze::dir(), &tab).unwrap();
        drop(lock);
        let result = (|| -> Result<(), String> {
            let split = env.call(
                "pane.split",
                json!({"target_pane_id":owner,"direction":"right","ratio":0.25,"focus":false}),
            )?;
            let work = split["pane"]["pane_id"]
                .as_str()
                .ok_or("missing test work pane")?
                .to_string();
            if dock_right {
                env.call(
                    "pane.swap",
                    json!({"source_pane_id": owner,"target_pane_id": work}),
                )?;
                env.call(
                    "layout.set_split_ratio",
                    json!({"pane_id": owner,"path":[],"ratio":0.75}),
                )?;
            }
            env.call(
                "pane.split",
                json!({"target_pane_id":work,"direction":"down","ratio":0.6,"focus":false}),
            )?;
            let before = env.call("pane.layout", json!({"pane_id":owner}))?["layout"].clone();
            let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
            let file = cwd.join("Cargo.toml");
            let request = format!("file\t{}", file.display());
            let target = open(
                &owner,
                &cwd,
                "takeover-test",
                &request,
                &tab,
                dock_right,
                30,
            )?;
            let during = env.call("pane.layout", json!({"pane_id":owner}))?;
            if during["layout"]["panes"].as_array().unwrap().len() != 2 {
                return Err("takeover left extra panes visible".into());
            }
            let parking = env.load(&tab)?.unwrap().parking_tab;
            let expected = crate::viewer::document_token(&crate::viewer::doc_key_for_file(&file));
            let mut ready = false;
            for _ in 0..100 {
                let list = env.call("pane.list", json!({}))?;
                ready = list["panes"].as_array().unwrap().iter().any(|pane| {
                    pane["pane_id"] == target.pane_id
                        && pane["tokens"]["hs-preview-path"] == expected
                });
                if ready {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if !ready {
                return Err("viewer did not acknowledge the test file".into());
            }
            if force_close {
                env.call("pane.close", json!({"pane_id":target.pane_id}))?;
                recover(&tab)?;
            } else {
                env.call(
                    "pane.send_input",
                    json!({"pane_id":target.pane_id,"keys":["q"]}),
                )?;
            }
            for _ in 0..100 {
                if !path(&tab).exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let after = env.call("pane.layout", json!({"pane_id":owner}))?["layout"].clone();
            let geometry = |value: &Value| {
                let mut panes: Vec<_> = value["panes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|pane| (pane["pane_id"].to_string(), pane["rect"].to_string()))
                    .collect();
                panes.sort();
                panes
            };
            if geometry(&before) != geometry(&after) {
                return Err("layout was not restored exactly".into());
            }
            if path(&tab).exists() {
                return Err("restore journal survived close".into());
            }
            let list = env.call("pane.list", json!({}))?;
            if list["panes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|pane| pane["tab_id"] == parking)
            {
                return Err("parking tab survived close".into());
            }
            Ok(())
        })();
        if let Ok(Some(plan)) = env.load(&tab) {
            let _ = restore(&tab, plan.viewer.as_deref().unwrap_or_default());
            let _ = env.call("tab.close", json!({"tab_id":plan.parking_tab}));
        }
        let _ = env.call("pane.focus", json!({"pane_id":focused}));
        let _ = env.call("tab.close", json!({"tab_id":tab}));
        assert!(result.is_ok(), "{result:?}");
    }
}
