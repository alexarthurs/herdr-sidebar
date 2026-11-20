//! The focus hooks publish one small snapshot per Herdr session. Sidebar
//! processes share that decision, with bounded checks for missed hooks.
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ipc, launch, rundir};

const FOCUS_CHECK_EVERY: Duration = Duration::from_secs(10);
const MEMBERSHIP_EVERY: Duration = Duration::from_secs(60);
const MEMBERSHIP_RETRY_EVERY: Duration = Duration::from_secs(1);

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

fn retry_once<T, E>(mut attempt: impl FnMut() -> Result<T, E>) -> Result<T, E> {
    attempt().or_else(|_| attempt())
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
    if let Some(pane) = pane_reply(&reply) {
        return Ok(Some(pane));
    }
    let value: Value = serde_json::from_str(launch::strip_bom(&reply))?;
    if value["error"]["code"] == "pane_not_found" {
        Ok(None)
    } else {
        Err(std::io::Error::other("invalid pane.current response"))
    }
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
    let lock = publish_lock(&dir)?;
    lock.lock()?;
    write_focus(&dir, current_focus)
}

fn publish_lock(dir: &Path) -> std::io::Result<File> {
    prepare_dir(dir)?;
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("publish.lock"))
}

fn recheck_focus(
    dir: &Path,
    clock: impl FnOnce() -> SystemTime,
    fetch: impl FnOnce() -> std::io::Result<Option<Focus>>,
) -> std::io::Result<()> {
    let lock = publish_lock(dir)?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let checked = dir.join("last-check.txt");
    let now = clock()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let last = read_local(&checked).and_then(|text| text.parse::<u128>().ok());
    if last.is_some_and(|last| now >= last && now - last < FOCUS_CHECK_EVERY.as_millis()) {
        return Ok(());
    }
    // Reserve the session's interval before IPC, even if the host times out.
    retry_once(|| write_local(&checked, &now.to_string()))?;
    write_focus(dir, fetch)
}

enum MembershipCheck {
    Skipped,
    Fetched(Option<launch::Pane>),
}

fn recheck_membership(dir: &Path, fetch: impl FnOnce() -> Option<launch::Pane>) -> MembershipCheck {
    let Ok(lock) = publish_lock(dir) else {
        return MembershipCheck::Skipped;
    };
    if lock.try_lock().is_err() {
        return MembershipCheck::Skipped;
    }
    // Check for a busy publisher, then release the lock before pane.get.
    // Membership does not publish focus and must not hold up a real hook.
    drop(lock);
    MembershipCheck::Fetched(fetch())
}

fn write_focus(
    dir: &Path,
    fetch: impl FnOnce() -> std::io::Result<Option<Focus>>,
) -> std::io::Result<()> {
    let path = dir.join("current.json");
    let previous = read_marker(&path);
    let Some(focus) = fetch()? else {
        // A missing marker lets older hosts keep following at the idle cadence.
        return retry_once(|| match fs::remove_file(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        });
    };
    // Publish even when focus agrees: another sidebar may still hold a
    // pane.list result older than this successful session-wide recheck.
    let marker = next_marker(previous.as_ref(), focus);
    let text = serde_json::to_string(&marker)?;
    retry_once(|| write_local(&path, &text))
}

fn next_marker(previous: Option<&Marker>, focus: Focus) -> Marker {
    let generation = previous.map_or(1, |marker| marker.generation.wrapping_add(1).max(1));
    Marker { generation, focus }
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
    let path = move_path(&dir, id);
    let stamp = format!("{:?}-{}", SystemTime::now(), std::process::id());
    retry_once(|| write_local(&path, &stamp))
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

enum ActivitySample {
    Panes {
        json: String,
        marker: Option<Marker>,
    },
    Focus,
    Membership(MembershipCheck),
}

fn sample_due(last: Option<Instant>, now: Instant, every: Duration) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= every)
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
    move_pending: bool,
    last_membership_attempt: Option<Instant>,
    last_membership: Option<Instant>,
    last_focus_check: Option<Instant>,
    startup_marker: Option<Marker>,
    last_read: Option<Instant>,
    visibility: Visibility,
    pane_focused: bool,
    last_marker: Option<Marker>,
    visible_list_focus: Option<bool>,
    fallback_focused: bool,
    following: bool,
    poller: launch::FollowPoller<ActivitySample>,
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
        // A marker can outlive the host. The shared worker validates it
        // under the session limit. A fresh publication wins on first read.
        let startup_marker = marker_path.as_deref().and_then(read_marker);
        let own = get_pane(&original_pane_id);
        let mut activity = Self {
            startup_marker,
            pane_id: original_pane_id.clone(),
            original_pane_id,
            marker_path,
            last_membership: Some(Instant::now()),
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
        let mut target = None;
        if let Some(sample) = self.poller.take() {
            match sample {
                Some(ActivitySample::Focus) => self.read_focus(),
                Some(ActivitySample::Membership(check)) => {
                    self.apply_membership_check(now, check);
                    self.read_focus();
                }
                Some(ActivitySample::Panes { json, marker }) => {
                    if let Some(panes) = launch::parse_panes(&json) {
                        if let Some(own) = panes.iter().find(|pane| {
                            pane.pane_id.as_deref() == Some(&self.pane_id)
                                || self.terminal_id.is_some()
                                    && pane.terminal_id == self.terminal_id
                        }) {
                            self.update_membership(own);
                            self.last_membership = Some(now);
                            self.read_focus();
                            self.observe_list_focus(marker.as_ref(), own.focused);
                        }
                        if follow && self.visibility.cadence(follow, needs_focus).is_some() {
                            target = self
                                .poller
                                .accept(&panes, &self.pane_id, &mut self.follower);
                        }
                    }
                }
                None => {
                    self.fallback_focused = false;
                    self.poller.break_confirmation();
                }
            }
        }
        if self.visibility == Visibility::Unknown {
            self.pane_focused = self.fallback_focused;
        }
        self.request_sample(now, follow, needs_focus);
        target
    }

    fn membership_due(&self, now: Instant) -> bool {
        sample_due(self.last_membership_attempt, now, MEMBERSHIP_RETRY_EVERY)
            && (self.move_pending || sample_due(self.last_membership, now, MEMBERSHIP_EVERY))
    }

    fn observe_move(&mut self, moved: Option<String>) {
        if moved.is_some() && moved != self.last_move {
            self.last_move = moved;
            self.move_pending = true;
            self.poller.reset();
        }
    }

    fn apply_membership_check(&mut self, now: Instant, check: MembershipCheck) {
        if let MembershipCheck::Fetched(own) = check {
            self.last_membership = Some(now);
            if let Some(own) = own {
                self.fallback_focused = own.focused;
                self.update_membership(&own);
                self.move_pending = false;
            }
        }
    }

    fn focus_check_due(&self, now: Instant, follow: bool) -> bool {
        (self.startup_marker.is_some()
            || self.visibility == Visibility::Hidden
            || self.visibility == Visibility::Visible && !follow)
            && sample_due(self.last_focus_check, now, FOCUS_CHECK_EVERY)
    }

    fn request_sample(&mut self, now: Instant, follow: bool, needs_focus: bool) {
        let membership_due = self.membership_due(now);
        let focus_due = self.focus_check_due(now, follow);
        if membership_due || focus_due {
            let Some(dir) = self.marker_path.as_deref().and_then(Path::parent) else {
                return;
            };
            let dir = dir.to_path_buf();
            if membership_due {
                let id = self.original_pane_id.clone();
                if self.poller.maybe_start(now, Duration::ZERO, move || {
                    let check = recheck_membership(&dir, || get_pane(&id));
                    Some(ActivitySample::Membership(check))
                }) {
                    self.last_membership_attempt = Some(now);
                }
            } else if self.poller.maybe_start(now, Duration::ZERO, move || {
                let _ = recheck_focus(&dir, SystemTime::now, current_focus);
                Some(ActivitySample::Focus)
            }) {
                self.last_focus_check = Some(now);
            }
        } else if let Some(every) = self.visibility.cadence(follow, needs_focus) {
            let marker = self.last_marker.clone();
            self.poller.maybe_start(now, every, move || {
                let json = ipc::call_text("pane.list", json!({})).ok()?;
                Some(ActivitySample::Panes { json, marker })
            });
        }
    }

    fn read_focus(&mut self) {
        let moved = self.move_path.as_deref().and_then(read_local);
        self.observe_move(moved);
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
        if marker != self.last_marker.as_ref() {
            self.last_marker = marker.cloned();
            self.visible_list_focus = None;
        }
        self.pane_focused = match (next, marker) {
            (Visibility::Visible, Some(marker)) => self
                .visible_list_focus
                .unwrap_or(marker.focus.pane_id == self.pane_id),
            (Visibility::Unknown, _) => self.fallback_focused,
            _ => false,
        };
    }

    fn observe_list_focus(&mut self, marker: Option<&Marker>, focused: bool) {
        self.fallback_focused = focused;
        // A hook can publish while a list is in flight. Its marker wins
        // until a sample started against that publication comes back.
        if self.visibility == Visibility::Visible && marker == self.last_marker.as_ref() {
            self.visible_list_focus = Some(focused);
            self.pane_focused = focused;
        }
    }

    fn update_membership(&mut self, own: &launch::Pane) {
        if let (Some(id), Some(tab)) = (&own.pane_id, &own.tab_id) {
            if self.pane_id != *id || self.tab != *tab {
                self.visible_list_focus = None;
                self.poller.reset();
            }
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
        let mut own = SidebarActivity {
            pane_id: "w1:p2".into(),
            tab: "w1:t2".into(),
            startup_marker: Some(old.clone()),
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
    fn visible_sidebar_uses_list_focus_until_a_new_marker_and_checks_without_a_list() {
        let shell = marker(1, "w1:t2", "w1:p1");
        let newer = marker(2, "w1:t2", "w1:p1");
        let mut own = SidebarActivity {
            pane_id: "w1:p2".into(),
            tab: "w1:t2".into(),
            ..Default::default()
        };
        own.apply_marker(Some(&shell));
        assert!(!own.pane_is_focused());
        own.observe_list_focus(Some(&shell), true);
        own.apply_marker(Some(&shell));
        assert!(
            own.pane_is_focused(),
            "rereading the old marker preserves the sample"
        );
        own.observe_list_focus(Some(&shell), false);
        assert!(!own.pane_is_focused());
        own.apply_marker(Some(&newer));
        own.observe_list_focus(Some(&shell), true);
        assert!(
            !own.pane_is_focused(),
            "a newer hook wins over an in-flight list"
        );

        let now = Instant::now();
        own.last_focus_check = Some(now);
        assert!(!own.focus_check_due(now, false));
        assert!(own.focus_check_due(now + FOCUS_CHECK_EVERY, false));
        assert!(!own.focus_check_due(now + FOCUS_CHECK_EVERY, true));
    }

    #[test]
    fn hidden_revalidation_repairs_a_disagreeing_marker_and_retries_once() {
        let now = Instant::now();
        assert!(!sample_due(
            Some(now),
            now + Duration::from_secs(9),
            FOCUS_CHECK_EVERY
        ));
        assert!(sample_due(
            Some(now),
            now + FOCUS_CHECK_EVERY,
            FOCUS_CHECK_EVERY
        ));
        let old = marker(1, "w1:t1", "w1:p1");
        let mut own = SidebarActivity {
            tab: "w1:t2".into(),
            ..Default::default()
        };
        own.apply_marker(Some(&old));
        assert!(own.tab_is_hidden());

        let current = Focus::new("w1", "w1:t2", "w1:p2").unwrap();
        let fixed = next_marker(Some(&old), current.clone());
        own.apply_marker(Some(&fixed));
        assert!(!own.tab_is_hidden());
        assert_eq!(
            fixed,
            Marker {
                generation: 2,
                focus: current
            }
        );

        let mut attempts = 0;
        assert_eq!(
            retry_once(|| {
                attempts += 1;
                Err::<(), _>("busy")
            }),
            Err("busy")
        );
        assert_eq!(attempts, 2);
    }

    #[test]
    fn opportunistic_checks_share_the_session_interval_and_skip_a_busy_lock() {
        let start = SystemTime::now();
        let unique = start
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("sidebar-focus-{}-{unique}", std::process::id()));
        let dir = root.join("session");
        let hook = publish_lock(&dir).unwrap();
        hook.lock().unwrap();
        recheck_focus(&dir, || start, || panic!("focus hook holds the lock")).unwrap();
        assert!(matches!(
            recheck_membership(&dir, || panic!("focus hook holds the lock")),
            MembershipCheck::Skipped
        ));
        drop(hook);

        let fetch = || Ok(Some(Focus::new("w1", "w1:t2", "w1:p2").unwrap()));
        recheck_focus(&dir, || start, fetch).unwrap();
        let current = read_marker(&dir.join("current.json")).unwrap();
        recheck_focus(
            &dir,
            || start + FOCUS_CHECK_EVERY - Duration::from_millis(1),
            || panic!("another sidebar already checked"),
        )
        .unwrap();
        let mut sidebar = SidebarActivity {
            pane_id: current.focus.pane_id.clone(),
            tab: current.focus.tab_id.clone(),
            ..Default::default()
        };
        sidebar.apply_marker(Some(&current));
        sidebar.observe_list_focus(Some(&current), false);
        assert!(!sidebar.pane_is_focused());

        recheck_focus(&dir, || start + FOCUS_CHECK_EVERY, fetch).unwrap();
        let confirmed = read_marker(&dir.join("current.json")).unwrap();
        assert_eq!(confirmed.focus, current.focus);
        assert_eq!(confirmed.generation, current.generation + 1);
        sidebar.apply_marker(Some(&confirmed));
        assert!(
            sidebar.pane_is_focused(),
            "a successful agreeing recheck beats the old list"
        );
        assert_eq!(
            read_marker(&dir.join("current.json")).unwrap().generation,
            2
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn membership_ipc_leaves_the_publication_lock_available() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sidebar-membership-{}-{unique}",
            std::process::id()
        ));
        let dir = root.join("session");
        let result = recheck_membership(&dir, || {
            let hook = publish_lock(&dir).unwrap();
            assert!(hook.try_lock().is_ok(), "a hook can lock during pane.get");
            None
        });
        assert!(matches!(result, MembershipCheck::Fetched(None)));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_idle_membership_check_skipped_for_a_busy_lock_retries_soon() {
        let now = Instant::now();
        let mut own = SidebarActivity {
            last_membership: Some(now - MEMBERSHIP_EVERY),
            ..Default::default()
        };
        assert!(own.membership_due(now));
        own.last_membership_attempt = Some(now);
        own.apply_membership_check(now, MembershipCheck::Skipped);
        assert!(!own.membership_due(now));
        assert!(own.membership_due(now + MEMBERSHIP_RETRY_EVERY));
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
        // Recover membership even if no invalidation arrived.
        let now = Instant::now();
        own.last_membership = Some(now);
        assert!(!own.membership_due(now));
        assert!(own.membership_due(now + MEMBERSHIP_EVERY));
        own.observe_move(Some("move".into()));
        assert!(
            own.membership_due(now),
            "a new move bypasses the idle interval"
        );

        own.last_membership_attempt = Some(now);
        own.apply_membership_check(now, MembershipCheck::Skipped);
        assert!(!own.membership_due(now), "busy retries are bounded");
        let retry = now + MEMBERSHIP_RETRY_EVERY;
        assert!(
            own.membership_due(retry),
            "a busy lock does not consume the idle interval"
        );

        own.last_membership_attempt = Some(retry);
        own.apply_membership_check(retry, MembershipCheck::Fetched(panes.into_iter().next()));
        own.observe_move(Some("move".into()));
        assert!(!own.membership_due(retry + MEMBERSHIP_RETRY_EVERY));
        assert!(own.membership_due(retry + MEMBERSHIP_EVERY));
        assert_eq!(own.pane_id, "w2:p6");
        assert_eq!(own.workspace_id(), Some("w2"));
        assert_eq!(own.original_pane_id, "w1:p1");
    }
}
