//! The focus hooks publish one small snapshot per Herdr session. Sidebar
//! processes can then share the host's focus decision without polling it.
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ipc, launch, rundir};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Focus {
    workspace_id: String,
    tab_id: String,
    pane_id: String,
}

impl Focus {
    fn from_pane(pane: &launch::Pane) -> Option<Self> {
        Self::new(
            pane.workspace_id.as_deref()?,
            pane.tab_id.as_deref()?,
            pane.pane_id.as_deref()?,
        )
    }

    fn new(workspace: &str, tab: &str, pane: &str) -> Option<Self> {
        (launch::is_flag_safe(workspace)
            && launch::is_flag_safe(tab)
            && launch::is_flag_safe(pane)
            && tab
                .split_once(':')
                .is_some_and(|(ws, id)| ws == workspace && !id.is_empty())
            && pane
                .split_once(':')
                .is_some_and(|(ws, id)| ws == workspace && !id.is_empty()))
        .then(|| Self {
            workspace_id: workspace.into(),
            tab_id: tab.into(),
            pane_id: pane.into(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Marker {
    generation: u64,
    focus: Focus,
}

fn parse_marker(json: &str) -> Option<Marker> {
    let marker: Marker = serde_json::from_str(json).ok()?;
    let focus = &marker.focus;
    Focus::new(&focus.workspace_id, &focus.tab_id, &focus.pane_id)?;
    (marker.generation > 0).then_some(marker)
}

fn session_dir() -> Option<PathBuf> {
    let socket = ipc::socket_path()?;
    Some(rundir::dir("focus").join(crate::viewer::document_token(&socket.to_string_lossy())))
}

fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent() {
        rundir::ensure_private(parent)?;
    }
    rundir::ensure_private(dir)
}

fn read_local(path: &Path) -> Option<String> {
    if !rundir::is_private(path.parent()?) {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn read_marker(path: &Path) -> Option<Marker> {
    parse_marker(&read_local(path)?)
}

fn write_local(path: &Path, text: &str) -> std::io::Result<()> {
    let staging = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = fs::write(&staging, text).and_then(|()| fs::rename(&staging, path));
    if result.is_err() {
        let _ = fs::remove_file(staging);
    }
    result
}

fn pane_reply(response: &str) -> Option<launch::Pane> {
    #[derive(Deserialize)]
    struct Reply {
        result: ResultPane,
    }
    #[derive(Deserialize)]
    struct ResultPane {
        pane: launch::Pane,
    }
    let reply: Reply = serde_json::from_str(launch::strip_bom(response)).ok()?;
    Some(reply.result.pane)
}

pub(crate) fn current_pane() -> std::io::Result<Option<launch::Pane>> {
    let reply = ipc::call_text("pane.current", json!({}))?;
    Ok(pane_reply(&reply))
}

fn current_focus() -> std::io::Result<Option<Focus>> {
    Ok(current_pane()?.as_ref().and_then(Focus::from_pane))
}

/// Herdr starts hooks on separate threads and provides no event sequence, so
/// the right scope for docking is not necessarily the latest session focus.
/// Serialize the single-pane read and publish independently of LaunchLock.
pub(crate) fn publish_focus() -> std::io::Result<()> {
    let Some(dir) = session_dir() else {
        return Ok(());
    };
    prepare_dir(&dir)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("publish.lock"))?;
    lock.lock()?;
    let path = dir.join("current.json");
    let Some(focus) = current_focus()? else {
        // A missing marker lets older hosts keep following at the idle cadence.
        let _ = fs::remove_file(path);
        return Ok(());
    };
    let generation =
        read_marker(&path).map_or(1, |marker| marker.generation.wrapping_add(1).max(1));
    write_local(
        &path,
        &serde_json::to_string(&Marker { generation, focus })?,
    )
}

fn move_path(dir: &Path, terminal_id: &str) -> PathBuf {
    dir.join(format!(
        "move-{}.txt",
        crate::viewer::document_token(terminal_id)
    ))
}

fn moved_sidebar(event: &Value) -> Option<&str> {
    let pane = &event["data"]["pane"];
    let terminal = pane["terminal_id"].as_str()?;
    (pane["tokens"][launch::METADATA_SOURCE].is_string()
        || pane["tokens"][launch::SC_METADATA_SOURCE].is_string()
        || pane["tokens"][launch::STARTING_TOKEN].is_string())
    .then_some(terminal)
}

pub(crate) fn publish_move(event_json: &str) -> std::io::Result<()> {
    let event = serde_json::from_str::<Value>(launch::strip_bom(event_json))?;
    let Some(id) = moved_sidebar(&event) else {
        return Ok(());
    };
    let Some(dir) = session_dir() else {
        return Ok(());
    };
    prepare_dir(&dir)?;
    // An invalidation is enough. The pane resolves its own current tab, so
    // two moves dispatched out of order cannot restore old tab membership.
    write_local(
        &move_path(&dir, id),
        &format!("{:?}-{}", SystemTime::now(), std::process::id()),
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Visibility {
    Visible,
    Hidden,
    #[default]
    Unknown,
}

fn visibility(tab: &str, marker: Option<&Marker>) -> Visibility {
    match marker {
        Some(marker) if !tab.is_empty() => {
            if marker.focus.tab_id == tab {
                Visibility::Visible
            } else {
                Visibility::Hidden
            }
        }
        _ => Visibility::Unknown,
    }
}

impl Visibility {
    fn cadence(self, follow: bool, needs_focus: bool) -> Option<Duration> {
        match self {
            Self::Visible if follow => Some(launch::FOLLOW_CWD_EVERY),
            Self::Unknown if follow || needs_focus => Some(launch::FOLLOW_CWD_IDLE),
            _ => None,
        }
    }
}

fn stale_at_startup(marker: Option<Marker>, snapshot: Option<&Focus>) -> Option<Marker> {
    marker.filter(|marker| Some(&marker.focus) != snapshot)
}

/// The two views share one focus reader and one single-flight pane sample,
/// including across an in-process view switch or a cwd-triggered rebuild.
#[derive(Default)]
pub struct SidebarActivity {
    original_pane_id: String,
    pane_id: String,
    tab: String,
    terminal_id: Option<String>,
    marker_path: Option<PathBuf>,
    move_path: Option<PathBuf>,
    last_move: Option<String>,
    startup_marker: Option<Marker>,
    last_read: Option<Instant>,
    visibility: Visibility,
    pane_focused: bool,
    fallback_focused: bool,
    following: bool,
    poller: launch::FollowPoller,
    follower: launch::CwdFollower,
}

impl SidebarActivity {
    pub fn from_env() -> Self {
        let Ok(original_pane_id) = std::env::var("HERDR_PANE_ID") else {
            return Self::default();
        };
        if original_pane_id.is_empty() {
            return Self::default();
        }
        let dir = session_dir();
        let marker_path = dir.as_ref().map(|dir| dir.join("current.json"));
        // A persisted marker can outlive the server or an interval with the
        // plugin disabled. Validate it once against the host. A publication
        // that lands during startup wins on the first local read.
        let before = marker_path.as_deref().and_then(read_marker);
        let focused = before.as_ref().and_then(|_| current_focus().ok().flatten());
        let own = get_pane(&original_pane_id);
        let mut activity = Self {
            startup_marker: stale_at_startup(before, focused.as_ref()),
            pane_id: original_pane_id.clone(),
            original_pane_id,
            marker_path,
            ..Self::default()
        };
        if let Some(own) = &own {
            activity.update_membership(own);
            activity.fallback_focused = own.focused;
        }
        activity
    }

    pub fn pane_is_focused(&self) -> bool {
        self.pane_id.is_empty() || self.pane_focused
    }

    pub fn tab_is_hidden(&self) -> bool {
        self.visibility == Visibility::Hidden
    }

    pub fn workspace_id(&self) -> Option<&str> {
        self.tab.split_once(':').map(|(workspace, _)| workspace)
    }

    pub fn mark_manual_folder(&mut self) {
        self.follower.mark_manual_folder();
    }

    pub fn reset_follower(&mut self) {
        self.follower.reset();
        self.poller.reset();
    }

    pub fn poll(&mut self, follow: bool, needs_focus: bool) -> Option<String> {
        if self.pane_id.is_empty() {
            return None;
        }
        let now = Instant::now();
        if self
            .last_read
            .is_none_or(|at| at.elapsed() >= launch::FOLLOW_CWD_EVERY)
        {
            self.last_read = Some(now);
            self.read_focus();
        }
        if follow != self.following {
            self.following = follow;
            if follow {
                self.poller.reset();
            } else {
                self.poller.break_confirmation();
            }
        }
        let cadence = self.visibility.cadence(follow, needs_focus);
        let mut target = None;
        if let Some(sample) = self.poller.take() {
            if let Some(panes) = sample.and_then(|json| launch::parse_panes(&json)) {
                if let Some(own) = panes.iter().find(|pane| {
                    pane.pane_id.as_deref() == Some(&self.pane_id)
                        || self.terminal_id.is_some() && pane.terminal_id == self.terminal_id
                }) {
                    self.update_membership(own);
                }
                self.fallback_focused = panes
                    .iter()
                    .any(|pane| pane.focused && pane.pane_id.as_deref() == Some(&self.pane_id));
                if follow && cadence.is_some() {
                    target = self
                        .poller
                        .accept(&panes, &self.pane_id, &mut self.follower);
                }
            } else {
                self.fallback_focused = false;
                self.poller.break_confirmation();
            }
        }
        if self.visibility == Visibility::Unknown {
            self.pane_focused = self.fallback_focused;
        }
        if let Some(every) = cadence {
            self.poller
                .maybe_start(now, every, || ipc::call_text("pane.list", json!({})).ok());
        }
        target
    }

    fn read_focus(&mut self) {
        let moved = self.move_path.as_deref().and_then(read_local);
        if moved.is_some() && moved != self.last_move {
            self.last_move = moved;
            // The original pane id remains an API alias across workspaces.
            // The reply contains the canonical id used by focus and list.
            if let Some(pane) = get_pane(&self.original_pane_id) {
                self.update_membership(&pane);
            } else {
                self.tab.clear();
            }
            self.poller.reset();
        }
        let marker = self.marker_path.as_deref().and_then(read_marker);
        self.apply_marker(marker.as_ref());
    }

    fn apply_marker(&mut self, marker: Option<&Marker>) {
        let marker = marker.filter(|marker| Some(*marker) != self.startup_marker.as_ref());
        if marker.is_some() {
            self.startup_marker = None;
        }
        let next = visibility(&self.tab, marker);
        if next != self.visibility {
            self.visibility = next;
            self.poller.reset();
        }
        if let Some(marker) = marker {
            self.pane_focused = next == Visibility::Visible && marker.focus.pane_id == self.pane_id;
        } else {
            self.pane_focused = self.fallback_focused;
        }
    }

    fn update_membership(&mut self, own: &launch::Pane) {
        if let (Some(id), Some(tab)) = (&own.pane_id, &own.tab_id) {
            self.pane_id.clone_from(id);
            self.tab.clone_from(tab);
            if self.terminal_id.is_none()
                && let Some(terminal) = &own.terminal_id
            {
                self.terminal_id = Some(terminal.clone());
                self.move_path = self
                    .marker_path
                    .as_deref()
                    .and_then(Path::parent)
                    .map(|dir| move_path(dir, terminal));
            }
        }
    }
}

fn get_pane(pane_id: &str) -> Option<launch::Pane> {
    let reply = ipc::call_text("pane.get", json!({"pane_id": pane_id})).ok()?;
    pane_reply(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(generation: u64, tab: &str, pane: &str) -> Marker {
        Marker {
            generation,
            focus: Focus::new("w1", tab, pane).unwrap(),
        }
    }

    #[test]
    fn focus_marker_validates_its_scope() {
        let good = marker(1, "w1:t2", "w1:p3");
        assert_eq!(
            parse_marker(&serde_json::to_string(&good).unwrap()),
            Some(good)
        );
        for bad in [
            "",
            "truncated{",
            r#"{"generation":1,"focus":{"workspace_id":"w2","tab_id":"w1:t2","pane_id":"w1:p3"}}"#,
            r#"{"generation":1,"focus":{"workspace_id":"w1","tab_id":"w1:","pane_id":"w1:p3"}}"#,
            r#"{"generation":1,"focus":{"workspace_id":"w1","tab_id":"w1:t2","pane_id":""}}"#,
            r#"{"generation":0,"focus":{"workspace_id":"w1","tab_id":"w1:t2","pane_id":"w1:p3"}}"#,
        ] {
            assert_eq!(parse_marker(bad), None);
        }
    }

    #[test]
    fn stale_startup_marker_falls_back_until_a_new_publication() {
        let old = marker(2, "w1:t1", "w1:p1");
        let current = marker(3, "w1:t2", "w1:p2");
        assert_eq!(
            stale_at_startup(Some(current.clone()), Some(&current.focus)),
            None
        );
        let mut own = SidebarActivity {
            pane_id: "w1:p2".into(),
            tab: "w1:t2".into(),
            startup_marker: stale_at_startup(Some(old.clone()), Some(&current.focus)),
            ..Default::default()
        };
        own.apply_marker(Some(&old));
        assert_eq!(own.visibility, Visibility::Unknown);
        own.apply_marker(Some(&current));
        assert_eq!(own.visibility, Visibility::Visible);
        assert!(own.pane_is_focused());
        own.apply_marker(None);
        assert_eq!(own.visibility, Visibility::Unknown);
    }

    #[test]
    fn only_visible_sidebars_follow_fast_and_only_the_focused_pane_refreshes_git() {
        let mark = marker(1, "w1:t2", "w1:p3");
        let mut own = SidebarActivity {
            pane_id: "w1:p2".into(),
            tab: "w1:t2".into(),
            ..Default::default()
        };
        own.apply_marker(Some(&mark));
        assert_eq!(
            own.visibility.cadence(true, true),
            Some(launch::FOLLOW_CWD_EVERY)
        );
        assert_eq!(own.visibility.cadence(false, true), None);
        assert!(!own.pane_is_focused(), "a sibling has the keyboard focus");
        own.apply_marker(Some(&marker(2, "w1:t2", "w1:p2")));
        assert!(own.pane_is_focused());
        own.apply_marker(Some(&marker(3, "w1:t1", "w1:p1")));
        assert_eq!(own.visibility.cadence(true, true), None);
        assert!(!own.pane_is_focused());
        assert_eq!(
            Visibility::Unknown.cadence(true, false),
            Some(launch::FOLLOW_CWD_IDLE)
        );
        assert_eq!(
            Visibility::Unknown.cadence(false, true),
            Some(launch::FOLLOW_CWD_IDLE)
        );
        assert_eq!(Visibility::Unknown.cadence(false, false), None);
    }

    #[test]
    fn moves_are_keyed_by_terminal_across_public_pane_id_changes() {
        let event = json!({"data": {
            "previous_pane_id": "w1:p1",
            "pane": {
                "pane_id": "w2:p6", "terminal_id": "term-A",
                "tokens": { launch::SC_METADATA_SOURCE: "123" }
            }
        }});
        assert_eq!(moved_sidebar(&event), Some("term-A"));
        assert_eq!(
            moved_sidebar(&json!({"data":{"pane":{"terminal_id":"term-A"}}})),
            None
        );

        let panes = launch::parse_panes(
            r#"{"result":{"panes":[{
            "pane_id":"w2:p6","tab_id":"w2:t2","terminal_id":"term-A"
        }]}}"#,
        )
        .unwrap();
        let mut own = SidebarActivity {
            original_pane_id: "w1:p1".into(),
            pane_id: "w1:p1".into(),
            tab: "w1:t1".into(),
            ..Default::default()
        };
        own.update_membership(&panes[0]);
        assert_eq!(own.pane_id, "w2:p6");
        assert_eq!(own.workspace_id(), Some("w2"));
        assert_eq!(own.original_pane_id, "w1:p1");
    }
}
