//! TUI state and rendering: a VS Code Explorer-style tree with disclosure arrows,
//! nested indentation, per-file-type icons, and a VS Code-like hide/show command
//! (`b`) when the user wants the columns back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};
use regex::{Regex, RegexBuilder};
use unicode_width::UnicodeWidthChar;

use herdr_sidebar::actions::{self, MenuAction, MenuEntry};
use herdr_sidebar::branch_ui::{BranchPicker, FooterZones, PickerAction, draw_git_footer};
use herdr_sidebar::git::{Git, Status};
use herdr_sidebar::gitdeco::{Decorations, RepoStatus};
use herdr_sidebar::icons::{IconTheme, icon};
use herdr_sidebar::ipc;
use herdr_sidebar::state::{self as sidebar, View};
use herdr_sidebar::tree::{Row, Tree};
use herdr_sidebar::ui::{
    TitleAction, activity_button_style, activity_icons, chrome_button_style, draw_activity_caps,
    draw_scrollbar, gear_icon, hits, hits_activity_button, hits_collapse_button, hover_style,
    icon_style as ui_icon_style, input_tail, keep_visible_scroll, palette, selection_style,
    set_color_theme, sibling_panes_of, status_color, title_action_icon, title_action_spans,
    title_actions_visible, title_actions_width, truncate_to, wrap_footer_message, wrap_hints,
};

use herdr_sidebar::state::Exit;

const MY_VIEW: View = View::Explorer;

/// How often the git status decorations are re-read while the view is idle
/// (issue #19's "live update"). Two cheap `git status` calls per repo; the
/// explorer's own poll is 500ms, so this throttles them down to a quarter of
/// that.
const DECO_REFRESH: std::time::Duration = std::time::Duration::from_secs(2);
const TREE_SYNC_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

struct RepoDecorationRefresh {
    root: PathBuf,
    status: Status,
    ignored: Option<Vec<String>>,
    ignored_attempted: bool,
    ignored_degraded: bool,
}

struct DecorationRefresh {
    repos: Vec<RepoDecorationRefresh>,
}

fn ignored_scan_due(backoff_until: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    backoff_until.is_none_or(|until| now >= until)
}

/// Handle for resizing our own pane through the herdr socket API.
struct PaneCtl {
    pane_id: String,
}

impl PaneCtl {
    fn from_env() -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|id| !id.is_empty())?;
        Some(Self { pane_id })
    }

    /// Report identity tokens: always our own (so the ensure logic recognizes
    /// this pane even while the cosmetic label is cleared); in merged mode
    /// also the other view's — one Sidebar pane satisfies both plugins'
    /// launchers — otherwise clear the other view's token.
    fn report_tokens(&self, my: View, merged: bool) {
        herdr_sidebar::ipc::report_identity(&self.pane_id, my, merged);
    }

    /// Set or clear the pane label.
    fn set_label(&self, label: Option<&str>) {
        let mut params = serde_json::json!({ "pane_id": self.pane_id });
        if let Some(label) = label {
            params["label"] = serde_json::Value::String(label.to_string());
        }
        let _ = herdr_sidebar::ipc::call_text("pane.rename", params);
    }

    /// Resize our pane to `target` terminal columns over the socket API.
    /// `pane.resize`'s amount is a split-RATIO delta, so the exact amount comes
    /// from the live layout via [`herdr_sidebar::launch::resize_plan`].
    fn resize_to(&self, current: u16, target: u16, dock_right: bool) {
        let Ok(layout) = herdr_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        ) else {
            return;
        };
        let Some(step) =
            herdr_sidebar::launch::resize_plan(&layout, &self.pane_id, current, target, dock_right)
        else {
            return;
        };
        let _ = herdr_sidebar::ipc::call_text(
            "pane.resize",
            serde_json::json!({
                "pane_id": self.pane_id,
                "direction": step.direction,
                "amount": step.amount,
            }),
        );
    }

    fn resize_preferred(&self, current: u16, target: u16, dock_right: bool) {
        let Ok(layout) = herdr_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        ) else {
            return;
        };
        let Some(step) = herdr_sidebar::launch::preferred_resize_plan(
            &layout,
            &self.pane_id,
            current,
            target,
            dock_right,
        ) else {
            return;
        };
        let _ = herdr_sidebar::ipc::call_text(
            "pane.resize",
            serde_json::json!({
                "pane_id": self.pane_id,
                "direction": step.direction,
                "amount": step.amount,
            }),
        );
    }

    fn layout_width(&self) -> Option<i64> {
        let layout = herdr_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        )
        .ok()?;
        herdr_sidebar::launch::layout_width(&layout)
    }
}

/// Where the tree body was drawn last frame, for mouse hit-testing.
#[derive(Clone, Copy, Default)]
struct BodyGeom {
    top: u16,
    height: u16,
    /// Scroll offset of the list at draw time.
    offset: usize,
}

/// What a prompt's input will be used for on Enter.
enum PromptKind {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    /// Re-root the whole sidebar at a typed path (absolute, relative to the
    /// current root, or ~-prefixed).
    ChangeFolder,
    CustomEditor,
}

/// A modal layered over the tree: the context menu, a name prompt, or a
/// delete confirmation. While one is open it owns keyboard and mouse input.
enum Overlay {
    BranchPicker(BranchPicker),
    Menu {
        /// Click position the popup anchors to.
        x: u16,
        y: u16,
        /// Target path + is_dir; `None` targets the workspace root.
        target: Option<(PathBuf, bool)>,
        entries: Vec<MenuEntry>,
        selected: usize,
        /// Rendered rect from the last draw, for click hit-testing.
        rect: Rect,
    },
    Prompt {
        title: String,
        input: String,
        kind: PromptKind,
    },
    ConfirmDelete {
        path: PathBuf,
        is_dir: bool,
    },
    /// The ⚙ settings modal: mouse-toggleable panel settings.
    Settings {
        selected: usize,
        rect: Rect,
        scroll: usize,
    },
    QuickOpen {
        query: String,
        files: std::sync::Arc<Vec<QuickFile>>,
        matches: Vec<usize>,
        selected: usize,
        truncated: bool,
        loading: bool,
    },
    ContentSearch {
        query: String,
        replace: String,
        include: String,
        exclude: String,
        hits: std::sync::Arc<Vec<ContentHit>>,
        selected: usize,
        truncated: bool,
        loading: bool,
        searched: bool,
        details_expanded: bool,
        focus: SearchFocus,
        options: SearchOptions,
        error: Option<String>,
        dirty_since: Option<std::time::Instant>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SearchOptions {
    match_case: bool,
    whole_word: bool,
    regex: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SearchFocus {
    #[default]
    Query,
    Replace,
    Include,
    Exclude,
    Results,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QuickFile {
    path: PathBuf,
    label: String,
    label_lower: String,
}

struct QuickIndex {
    root: PathBuf,
    show_hidden: bool,
    files: std::sync::Arc<Vec<QuickFile>>,
    truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ContentHit {
    path: PathBuf,
    label: String,
    line: usize,
    context: String,
    matches: Vec<(usize, usize)>,
}

struct ContentSearchResult {
    root: PathBuf,
    show_hidden: bool,
    query: String,
    include: String,
    exclude: String,
    options: SearchOptions,
    hits: std::sync::Arc<Vec<ContentHit>>,
    truncated: bool,
    error: Option<String>,
}

/// One row of the Settings modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    Version,
    Update,
    UnifiedSidebar,
    DockRight,
    SidebarWidth,
    IconTheme,
    ColorTheme,
    PreviewPlacement,
    CustomEditorCommand,
    CustomEditorClick,
    AutoOpen,
    StrictToggle,
    FocusOnOpen,
    FollowCwd,
    HiddenFiles,
    Hotkeys,
    GitDecorations,
    GitFooter,
    Folder,
}

/// (setting, label, current value, enabled) — disabled rows render dimmed and
/// don't toggle.
type SettingRow = (Setting, &'static str, String, bool);

pub struct App {
    tree: Tree,
    rows: Vec<Row>,
    /// The user's explicit selection — `None` until they pick something
    /// (no row is highlighted by default; hover stays subtle).
    selected: Option<usize>,
    /// View scroll offset in rows, independent of the selection: the wheel
    /// moves this alone.
    scroll: usize,
    /// Bring the selection into view on the next draw (keyboard nav only).
    snap: bool,
    theme: IconTheme,
    pane_ctl: Option<PaneCtl>,
    /// Pane size from the last draw; sizing decisions and PageUp/PageDown
    /// strides are based on what was actually rendered.
    last_width: u16,
    /// Whole tab area width from the last layout snapshot. A divider drag
    /// changes only this pane; surrounding terminal chrome changes this too.
    last_layout_width: Option<i64>,
    last_height: u16,
    page: usize,
    /// Row index under the mouse cursor, for the hover highlight.
    hovered: Option<usize>,
    body: BodyGeom,
    overlay: Option<Overlay>,
    /// A search overlay parked under a modal (Settings / branch picker) opened
    /// from the Search view, restored with its query when the modal closes so
    /// the panel doesn't silently drop back to the tree.
    suspended_search: Option<Overlay>,
    /// Transient status/error line shown in the footer until the next action.
    notice: Option<String>,
    // Merged-sidebar state.
    sidebar_state: sidebar::State,
    other_exe: Option<std::path::PathBuf>,
    activity: ActivityZones,
    /// The ⚙ button's rect from the last draw (activity bar in unified mode,
    /// header row otherwise).
    gear: Rect,
    /// The hover title-bar buttons' click zones from the last draw (empty
    /// while they are hidden).
    title_zones: Vec<(Rect, TitleAction)>,
    /// When the mouse last moved/clicked/scrolled over this pane — the hover
    /// approximation that shows the title-bar buttons (see
    /// [`herdr_sidebar::ui::TITLE_ACTIONS_LINGER`]).
    last_mouse: Option<std::time::Instant>,
    /// Last known mouse position, for the button hover highlight.
    mouse_pos: Option<(u16, u16)>,
    /// Last left-click (row index, when) for double-click detection.
    last_click: Option<(usize, std::time::Instant)>,
    /// Where the most recent preview landed, with its document key — so a
    /// double click pins that exact tab instead of re-opening it.
    last_preview: Option<(String, herdr_sidebar::viewer::PreviewTarget)>,
    /// Last heartbeat stamp, throttling the token refresh.
    last_beat: std::time::Instant,
    /// A native folder picker running on a background thread; its result
    /// arrives here (None = cancelled).
    picking: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    /// Shared across full app rebuilds and unified-view switches so manual
    /// folder precedence is not lost when this view is recreated.
    cwd_follower: std::rc::Rc<std::cell::RefCell<herdr_sidebar::launch::CwdFollower>>,
    /// Every repository the tree can see — the containing one plus child
    /// repos, exactly the set the Source Control view shows.
    repos: Vec<Git>,
    /// Git status decorations for the visible rows (issue #19).
    deco: Decorations,
    /// Last decoration refresh, throttling the git polling.
    last_deco: std::time::Instant,
    /// Last shared tree-state read. Input bursts may call `tick` rapidly;
    /// synchronization should not turn every keypress into disk I/O.
    last_tree_sync: std::time::Instant,
    /// One background decoration refresh. Keeping at most one receiver avoids
    /// multiplying git processes when a slow repository overlaps the timer.
    deco_rx: Option<std::sync::mpsc::Receiver<DecorationRefresh>>,
    /// Status for the containing repository, populated by the same bounded
    /// background refresh that drives decorations.
    git_footer_status: Option<Status>,
    git_footer_zones: FooterZones,
    git_syncing: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    ignored_cache: HashMap<PathBuf, Vec<String>>,
    /// A failed/busy ignored-file scan is optional decoration data, not a
    /// reason to hammer an enormous repository again every two seconds.
    deco_backoff_until: HashMap<PathBuf, std::time::Instant>,
    quick_index: Option<QuickIndex>,
    quick_index_rx: Option<std::sync::mpsc::Receiver<QuickIndex>>,
    content_search_rx: Option<std::sync::mpsc::Receiver<ContentSearchResult>>,
    search_zones: SearchZones,
    search_result_rows: Vec<(Rect, usize)>,
    search_scroll: usize,
    search_snap: bool,
    pending_unified_width: Option<(u16, std::time::Instant)>,
}

/// How long two clicks on the same row still count as a double click.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(450);

/// Activity-bar click zones from the last draw: the bar's row and the column
/// ranges of the explorer / source-control icons.
#[derive(Clone, Copy)]
struct ActivityZones {
    row: u16,
    explorer: (u16, u16),
    search: (u16, u16),
    source_control: (u16, u16),
}

impl Default for ActivityZones {
    fn default() -> Self {
        // row = MAX: nothing hit-tests true before the first draw.
        Self {
            row: u16::MAX,
            explorer: (0, 0),
            search: (0, 0),
            source_control: (0, 0),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct SearchZones {
    query: Rect,
    replace: Rect,
    include: Rect,
    exclude: Rect,
    match_case: Rect,
    whole_word: Rect,
    regex: Rect,
    refresh: Rect,
    clear: Rect,
    details: Rect,
}

impl App {
    pub fn new(
        root: PathBuf,
        cwd_follower: std::rc::Rc<std::cell::RefCell<herdr_sidebar::launch::CwdFollower>>,
    ) -> Self {
        let mut tree = Tree::new(root);
        // Mirror the tree the user was already looking at. Idle ticks keep
        // same-root sidebars synchronized after startup too.
        let saved = sidebar::load_tree_state(&tree.root_path());
        tree.set_expanded(saved.expanded);
        let rows = tree.rows();
        let restored_selection = saved
            .selected
            .and_then(|want| rows.iter().position(|r| r.path == want));
        let theme = IconTheme::resolve(
            std::env::var("HERDR_SIDEBAR_ICONS")
                .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
                .ok()
                .as_deref(),
            sidebar::load_state().icons,
        );
        let pane_ctl = PaneCtl::from_env();
        let last_layout_width = pane_ctl.as_ref().and_then(PaneCtl::layout_width);
        // The other view ships in this same binary — always available.
        let other_exe = std::env::current_exe().ok();
        let sidebar_state = sidebar::load_state();
        set_color_theme(sidebar_state.color_theme);
        let repos = if sidebar_state.git_deco || sidebar_state.show_git_footer {
            Git::discover_all(&tree.root_path())
        } else {
            Vec::new()
        };
        let mut app = Self {
            tree,
            rows,
            selected: restored_selection,
            scroll: 0,
            snap: restored_selection.is_some(),
            theme,
            pane_ctl,
            last_width: sidebar_state.sidebar_width,
            last_layout_width,
            last_height: 24,
            page: 20,
            hovered: None,
            body: BodyGeom::default(),
            overlay: None,
            suspended_search: None,
            notice: None,
            sidebar_state,
            other_exe,
            activity: ActivityZones::default(),
            gear: Rect::default(),
            title_zones: Vec::new(),
            last_mouse: None,
            mouse_pos: None,
            last_click: None,
            last_preview: None,
            last_beat: std::time::Instant::now(),
            picking: None,
            cwd_follower,
            repos,
            deco: Decorations::empty(),
            // Overwritten when the first background refresh is queued below.
            last_deco: std::time::Instant::now(),
            last_tree_sync: std::time::Instant::now(),
            deco_rx: None,
            git_footer_status: None,
            git_footer_zones: FooterZones::default(),
            git_syncing: None,
            ignored_cache: HashMap::new(),
            deco_backoff_until: HashMap::new(),
            quick_index: None,
            quick_index_rx: None,
            content_search_rx: None,
            search_zones: SearchZones::default(),
            search_result_rows: Vec::new(),
            search_scroll: 0,
            search_snap: false,
            pending_unified_width: None,
        };
        app.apply_identity();
        app.request_decorations(true);
        app
    }

    pub fn root_path(&self) -> PathBuf {
        self.tree.root_path()
    }

    /// Idle work: keep the git decorations current so changes made outside
    /// the sidebar (an agent editing files, a commit in another pane) show up
    /// on their own. Self-throttling, so the event loop may call it freely.
    pub fn tick(&mut self) {
        self.sync_shared_settings();
        self.sync_shared_tree();
        self.collect_quick_index();
        self.collect_content_search();
        self.collect_git_sync();
        self.start_content_search_if_due();
        self.collect_decorations();
        if self.last_deco.elapsed() < DECO_REFRESH {
            return;
        }
        self.request_decorations(false);
    }

    pub fn is_syncing(&self) -> bool {
        self.git_syncing.is_some()
    }

    /// A separated Source Control pane can change this shared setting while
    /// the Explorer keeps running. Re-read just this field so the tree reacts
    /// without adopting unrelated process-local mode changes.
    fn sync_shared_settings(&mut self) {
        let shared = sidebar::load_state();
        self.sidebar_state.dock_right = shared.dock_right;
        self.sidebar_state.strict_toggle = shared.strict_toggle;
        self.sidebar_state.focus_on_open = shared.focus_on_open;
        self.sidebar_state.custom_editor_on_click = shared.custom_editor_on_click;
        let old_git_footer = self.sidebar_state.show_git_footer;
        self.sidebar_state.show_git_footer = shared.show_git_footer;
        if shared.color_theme != self.sidebar_state.color_theme {
            self.sidebar_state.color_theme = shared.color_theme;
            set_color_theme(shared.color_theme);
        }
        if shared.sidebar_width != self.sidebar_state.sidebar_width {
            self.sidebar_state.sidebar_width = shared.sidebar_width;
            if let Some(ctl) = &self.pane_ctl {
                ctl.resize_preferred(self.last_width, shared.sidebar_width, shared.dock_right);
            }
        }
        let enabled = shared.git_deco;
        if enabled == self.sidebar_state.git_deco && old_git_footer == shared.show_git_footer {
            return;
        }
        self.sidebar_state.git_deco = enabled;
        self.rediscover_repos();
        if !enabled {
            self.deco = Decorations::empty();
        }
        self.request_decorations(true);
    }

    fn sync_shared_tree(&mut self) {
        if self.last_tree_sync.elapsed() < TREE_SYNC_EVERY {
            return;
        }
        self.last_tree_sync = std::time::Instant::now();
        let shared = sidebar::load_tree_state(&self.tree.root_path());
        if apply_shared_tree_state(
            &mut self.tree,
            &mut self.rows,
            &mut self.selected,
            &mut self.scroll,
            shared,
        ) {
            self.hovered = None;
            self.snap = self.selected.is_some();
        }
    }

    pub fn on_resize(&mut self, width: u16) {
        self.last_width = width;
        let unified_close_completed = self.poll_unified_close();
        if let Some(ctl) = &self.pane_ctl {
            let layout_width = ctl.layout_width();
            let surrounding_changed = self
                .last_layout_width
                .zip(layout_width)
                .is_some_and(|(before, now)| before != now);
            self.last_layout_width = layout_width.or(self.last_layout_width);
            if !unified_close_completed && surrounding_changed {
                ctl.resize_preferred(
                    width,
                    self.sidebar_state.sidebar_width,
                    self.sidebar_state.dock_right,
                );
            }
        }
    }

    /// Queue a status read off the event-loop thread. Preview tabs each carry
    /// a sidebar, so periodic refreshes back off while this pane is not
    /// focused; explicit refresh/stage actions pass `force = true`.
    fn request_decorations(&mut self, force: bool) {
        self.last_deco = std::time::Instant::now();
        if !self.sidebar_state.git_deco && !self.sidebar_state.show_git_footer {
            self.deco = Decorations::empty();
            self.deco_rx = None;
            self.git_footer_status = None;
            self.ignored_cache.clear();
            self.deco_backoff_until.clear();
            return;
        }
        if self.deco_rx.is_some() || (!force && !self.pane_is_focused()) {
            return;
        }
        let ignored_backoffs = self.deco_backoff_until.clone();
        let scan_now = std::time::Instant::now();
        let include_ignored = self.sidebar_state.git_deco;
        let repos = self.repos.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let repos = repos
                .iter()
                .filter_map(|repo| {
                    let status = repo.status().ok()?;
                    let root = repo.root().to_path_buf();
                    let ignored_attempted = include_ignored
                        && ignored_scan_due(ignored_backoffs.get(&root).copied(), scan_now);
                    let (ignored, ignored_degraded) = if ignored_attempted {
                        match repo.ignored() {
                            Ok(ignored) => (Some(ignored), false),
                            Err(_) => (None, true),
                        }
                    } else {
                        (None, false)
                    };
                    Some(RepoDecorationRefresh {
                        root,
                        ignored,
                        ignored_attempted,
                        ignored_degraded,
                        status,
                    })
                })
                .collect();
            let _ = tx.send(DecorationRefresh { repos });
        });
        self.deco_rx = Some(rx);
    }

    fn collect_decorations(&mut self) {
        let Some(rx) = &self.deco_rx else { return };
        match rx.try_recv() {
            Ok(refresh) => {
                self.git_footer_status = refresh.repos.first().map(|repo| repo.status.clone());
                let mut statuses = Vec::with_capacity(refresh.repos.len());
                for repo in refresh.repos {
                    if let Some(ignored) = &repo.ignored {
                        self.ignored_cache
                            .insert(repo.root.clone(), ignored.clone());
                    }
                    if repo.ignored_attempted {
                        if repo.ignored_degraded {
                            self.deco_backoff_until.insert(
                                repo.root.clone(),
                                std::time::Instant::now() + std::time::Duration::from_secs(60),
                            );
                        } else {
                            self.deco_backoff_until.remove(&repo.root);
                        }
                    }
                    statuses.push(RepoStatus {
                        ignored: repo.ignored.unwrap_or_else(|| {
                            self.ignored_cache
                                .get(&repo.root)
                                .cloned()
                                .unwrap_or_default()
                        }),
                        root: repo.root,
                        status: repo.status,
                    });
                }
                self.deco = if self.sidebar_state.git_deco {
                    Decorations::build(&statuses)
                } else {
                    Decorations::empty()
                };
                self.deco_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.deco_rx = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn pane_is_focused(&self) -> bool {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.as_str()) else {
            return true;
        };
        ipc::call_text("pane.list", serde_json::json!({}))
            .ok()
            .is_some_and(|json| pane_focused_in(&json, pane_id))
    }

    /// Rediscover the repositories under the current root — after a re-root,
    /// or an explicit refresh that may have added or removed one.
    fn rediscover_repos(&mut self) {
        self.deco_rx = None;
        self.repos = if self.sidebar_state.git_deco || self.sidebar_state.show_git_footer {
            Git::discover_all(&self.tree.root_path())
        } else {
            Vec::new()
        };
    }

    /// The decoration letter for a row, if any (see [`Decorations::letter`]).
    fn row_deco(&self, row: &Row) -> Option<char> {
        self.deco.letter(&row.path, row.is_dir && !row.is_symlink)
    }

    /// Re-stamp the identity tokens so launchers know this pane is alive.
    /// Cheap (two socket round-trips); the event loop calls this every few
    /// seconds.
    pub fn heartbeat(&mut self) {
        self.poll_unified_close();
        if self.last_beat.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        self.last_beat = std::time::Instant::now();
        if let Some(ctl) = &self.pane_ctl {
            ctl.report_tokens(MY_VIEW, self.merged());
        }
        self.follow_sibling_cwd();
    }

    fn poll_unified_close(&mut self) -> bool {
        let Some((target, started)) = self.pending_unified_width else {
            return false;
        };
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.clone()) else {
            self.pending_unified_width = None;
            return false;
        };
        let Ok(json) = herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})) else {
            return false;
        };
        if sibling_panes_of(&json, &pane_id, MY_VIEW.other()).is_empty() {
            self.pending_unified_width = None;
            if let Some(ctl) = &self.pane_ctl {
                ctl.resize_to(self.last_width, target, self.sidebar_state.dock_right);
            }
            return true;
        }
        if started.elapsed() >= std::time::Duration::from_secs(2) {
            self.pending_unified_width = None;
            self.sidebar_state = sidebar::update_state(|state| {
                state.merged = false;
                state.active = MY_VIEW;
            });
            self.apply_identity();
            self.notice = Some("Source Control stayed open; unified mode cancelled".into());
        }
        false
    }

    fn follow_sibling_cwd(&mut self) {
        if !self.sidebar_state.follow_cwd || self.overlay.is_some() || self.picking.is_some() {
            return;
        }
        let Some(ctl) = &self.pane_ctl else { return };
        let Ok(panes) = herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})) else {
            return;
        };
        let target = self
            .cwd_follower
            .borrow_mut()
            .next_cwd(&panes, &ctl.pane_id);
        if let Some(target) = target
            && Path::new(&target) != self.tree.root_path()
        {
            self.change_folder_impl(&target, false);
        }
    }

    /// The merged sidebar is on and actually usable (other plugin present).
    fn merged(&self) -> bool {
        self.sidebar_state.merged && self.other_exe.is_some()
    }

    /// The label this pane should carry while expanded.
    fn pane_label(&self) -> &'static str {
        if self.merged() {
            sidebar::SIDEBAR_LABEL
        } else {
            herdr_sidebar::launch::PANE_LABEL
        }
    }

    /// Push our label + metadata tokens to herdr for the current mode.
    fn apply_identity(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        ctl.set_label(Some(self.pane_label()));
        ctl.report_tokens(MY_VIEW, self.merged());
    }

    pub fn clear_identity(&self) {
        if let Some(ctl) = &self.pane_ctl {
            herdr_sidebar::ipc::clear_identity(&ctl.pane_id);
        }
    }

    /// Open a file in the preview pane BESIDE the sidebar (the tree stays
    /// visible): the shared viewer client reuses the tab's viewer pane or
    /// spawns one next to us.
    fn open_preview(&mut self, path: &Path) {
        self.open_preview_at(path, None);
    }

    fn open_preview_at(&mut self, path: &Path, line: Option<usize>) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone()) else {
            self.notice = Some("preview needs a herdr pane".into());
            return;
        };
        let payload = line
            .map(|line| herdr_sidebar::viewer::file_request_at(path, line))
            .unwrap_or_else(|| herdr_sidebar::viewer::file_request(path));
        let doc_key = herdr_sidebar::viewer::doc_key_for_file(path);
        match herdr_sidebar::viewer::open_in_pane(
            &pane_id,
            &self.tree.root_path(),
            &doc_key,
            &payload,
        ) {
            // Remember where it landed: a double click pins THIS tab rather
            // than re-opening, which would race the viewer's first stamp.
            Ok(target) => self.last_preview = Some((doc_key, target)),
            Err(e) => self.notice = Some(e),
        }
    }

    /// Hide the sidebar: snooze this tab (so the quiet ensure hook doesn't
    /// immediately re-dock a fresh one) and close our own pane. The herdr
    /// prefix+b keybinding (→ the toggle action) brings it back.
    fn hide(&mut self) {
        self.close(true);
    }

    fn close(&mut self, snooze: bool) {
        // Closing our own pane kills this process: save a pending remembered
        // root first (the outer loop's flush never runs on this path).
        crate::flush_pending_root();
        let Some(ctl) = &self.pane_ctl else { return };
        if snooze {
            let tab = match herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})) {
                Ok(json) => herdr_sidebar::launch::tab_of(&json, &ctl.pane_id),
                Err(e) => {
                    self.notice = Some(format!("hide failed: {e}"));
                    return;
                }
            };
            if tab.is_empty() {
                self.notice = Some("hide failed: could not resolve this tab".into());
                return;
            }
            // Set the marker BEFORE closing: if it fails, closing anyway
            // would let the very next focus event re-dock a sidebar the
            // user just asked to hide.
            if let Err(e) = herdr_sidebar::snooze::set(&herdr_sidebar::snooze::dir(), &tab) {
                self.notice = Some(format!("hide failed: {e}"));
                return;
            }
        }
        let _ = herdr_sidebar::ipc::call_text(
            "pane.close",
            serde_json::json!({ "pane_id": ctl.pane_id }),
        );
    }

    // ---- Unified-sidebar operations ----

    /// Toggle the unified sidebar. On: adopt this pane as the Sidebar and
    /// close the other panel's standalone pane in this tab. Off: split the
    /// other view back out into its own pane. Deliberately silent — the
    /// layout change is its own feedback.
    fn set_unified(&mut self, on: bool) {
        if on == self.merged() || self.other_exe.is_none() {
            return;
        }
        self.sidebar_state = sidebar::update_state(|state| {
            state.merged = on;
            state.active = MY_VIEW;
        });
        self.apply_identity();
        if on {
            let width = self.last_width;
            match self.close_other_standalone_pane() {
                Ok(true) => {
                    self.pending_unified_width = Some((width, std::time::Instant::now()));
                }
                Ok(false) => {}
                Err(error) => {
                    self.sidebar_state = sidebar::update_state(|state| {
                        state.merged = false;
                        state.active = MY_VIEW;
                    });
                    self.apply_identity();
                    self.notice = Some(format!("unified mode cancelled: {error}"));
                }
            }
        } else {
            self.spawn_other_pane();
        }
    }

    /// Hand the pane to the other view (the supervisor swaps processes).
    fn switch_to(&mut self, view: View) -> Option<Exit> {
        if !self.merged() || view == MY_VIEW {
            return None;
        }
        self.sidebar_state = sidebar::update_state(|state| {
            state.active = view;
            state.search_active = false;
        });
        Some(Exit::Switch)
    }

    /// Close the other panel's standalone pane in our tab, if one is open.
    fn close_other_standalone_pane(&self) -> std::io::Result<bool> {
        let Some(ctl) = &self.pane_ctl else {
            return Ok(false);
        };
        let json = herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({}))?;
        let ids = sibling_panes_of(&json, &ctl.pane_id, MY_VIEW.other());
        if ids.is_empty() {
            return Ok(false);
        }
        let mut failure = None;
        for id in ids {
            if let Err(error) = herdr_sidebar::ensure::request_close(&json, &id) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(true), Err)
    }

    /// Open the other view in a fresh pane beside this one (detach).
    fn spawn_other_pane(&self) {
        let (Some(ctl), Some(_)) = (&self.pane_ctl, &self.other_exe) else {
            return;
        };
        let Some(_lock) = herdr_sidebar::ensure::LaunchLock::acquire(true) else {
            return;
        };
        // Grow to double width FIRST, then split 50/50 — each separated panel
        // keeps the width the unified sidebar had, instead of halving.
        ctl.resize_to(
            self.last_width,
            self.last_width.saturating_mul(2).saturating_add(1),
            self.sidebar_state.dock_right,
        );
        let other = MY_VIEW.other();
        #[cfg(unix)]
        let _ = herdr_sidebar::ipc::open_plugin_pane(
            &ctl.pane_id,
            other,
            &self.tree.root_path(),
            false,
            None,
        );
        #[cfg(windows)]
        {
            let response = herdr_sidebar::ipc::call_text(
                "pane.split",
                serde_json::json!({
                    "target_pane_id": ctl.pane_id,
                    "direction": "right",
                    "ratio": 0.5,
                    "focus": false,
                    "cwd": self.tree.root_path().display().to_string(),
                    "env": sidebar::spawn_env(),
                }),
            );
            let Some(new_pane) = response
                .ok()
                .and_then(|r| herdr_sidebar::launch::split_pane_id(&r))
            else {
                return;
            };
            if herdr_sidebar::ipc::report_starting_identity(&new_pane, other, false).is_err() {
                let _ = herdr_sidebar::ipc::call_text(
                    "pane.close",
                    serde_json::json!({ "pane_id": new_pane }),
                );
                return;
            }
            let flag = other.view_flag();
            let command = format!("{} --view {flag}", sidebar::EXECUTABLE_NAME);
            let _ = herdr_sidebar::ipc::call_text(
                "pane.send_input",
                serde_json::json!({ "pane_id": new_pane, "text": command, "keys": ["Enter"] }),
            );
            let _ = herdr_sidebar::ipc::call_text(
                "pane.rename",
                serde_json::json!({ "pane_id": new_pane, "label": other.label() }),
            );
        }
    }

    /// Handle one key press; `Some(exit)` ends the event loop.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Exit> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if key.code == KeyCode::Char('q')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.close(false);
            return None;
        }
        if (key.code == KeyCode::Char('p')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT))
            || key.code == KeyCode::F(12)
        {
            self.suspended_search = None;
            self.open_quick_open();
            return None;
        }
        if matches!(key.code, KeyCode::Char('f' | 'F'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            if let Some(Overlay::ContentSearch { focus, .. }) = self.overlay.as_mut() {
                *focus = SearchFocus::Query;
            } else if self.overlay.is_none() {
                // Ctrl+F is the "find" gesture — open ready to type.
                self.open_content_search(true);
            }
            return None;
        }
        // View switching from the keyboard, VS Code's activity-bar order:
        // 1 Explorer, 2 Search, 3 Source Control. Ctrl+1/2/3 always switch (an
        // editor's group-focus chord), so they work even mid-word in a focused
        // search field. Bare 1/2/3 ALSO switch while the Search box is NOT
        // focused (its Results list) — the state you land in when switching to
        // Search — so the keys stay a switcher until you deliberately focus the
        // box (Ctrl+F / Tab / click); a focused box captures digits as text so
        // "3" is searchable. The tree's own bare 1/2/3 are handled further down.
        let injected_view = match key.code {
            KeyCode::F(9) => Some('1'),
            KeyCode::F(10) => Some('2'),
            KeyCode::F(11) => Some('3'),
            _ => None,
        };
        if let Some(c) = injected_view.or(match key.code {
            KeyCode::Char(c @ ('1' | '2' | '3')) => Some(c),
            _ => None,
        }) {
            let ctrl = injected_view.is_some()
                || (key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT));
            let bare_switch = key.modifiers.is_empty() && self.search_text_unfocused();
            if ctrl || bare_switch {
                if ctrl {
                    self.overlay = None;
                    self.suspended_search = None;
                }
                return match c {
                    '1' => {
                        // Explorer is this app's own tree: dropping the search
                        // overlay lands on it in-process.
                        if matches!(self.overlay, Some(Overlay::ContentSearch { .. })) || ctrl {
                            self.overlay = None;
                            if self.merged() {
                                self.sidebar_state = sidebar::update_state(|state| {
                                    state.search_active = false;
                                });
                            }
                        }
                        self.switch_to(View::Explorer)
                    }
                    '2' => {
                        self.open_content_search(false);
                        None
                    }
                    _ => self.switch_to(View::SourceControl),
                };
            }
        }
        self.notice = None;
        if self.overlay.is_some() {
            if matches!(self.overlay, Some(Overlay::ContentSearch { .. }))
                && matches!(
                    key.code,
                    KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
                )
            {
                self.search_snap = true;
            }
            self.overlay_key(key);
            return None;
        }
        match key.code {
            KeyCode::Char('q') => return Some(Exit::Quit),
            // Esc never quits the sidebar — it closes the preview instead.
            KeyCode::Esc => self.close_preview(),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.page as isize)),
            KeyCode::PageDown => self.move_by(self.page as isize),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(self.rows.len().saturating_sub(1)),
            KeyCode::Right | KeyCode::Char('l') => self.expand_or_enter(),
            KeyCode::Left | KeyCode::Char('h') => self.collapse_or_parent(),
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('r') => {
                self.refresh_tree();
            }
            KeyCode::Char('.') => {
                self.tree.show_hidden = !self.tree.show_hidden;
                self.invalidate_quick_index();
                self.rebuild();
            }
            KeyCode::Char('i') => self.set_theme(self.theme.toggled()),
            KeyCode::Char('b') => self.hide(),
            KeyCode::Char('c') => self.change_folder_dialog(),
            KeyCode::Char('m') => self.open_menu_for_selection(),
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('1') => return self.switch_to(View::Explorer),
            KeyCode::Char('2') => self.open_content_search(false),
            KeyCode::Char('3') => return self.switch_to(View::SourceControl),
            _ => {}
        }
        None
    }

    /// `Some(exit)` ends the event loop, mirroring on_key.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit> {
        // Any mouse activity = "the mouse is over this pane": it shows the
        // hover title-bar buttons until the linger expires.
        self.last_mouse = Some(std::time::Instant::now());
        self.mouse_pos = Some((mouse.column, mouse.row));
        if self.overlay.is_some() && !matches!(self.overlay, Some(Overlay::ContentSearch { .. })) {
            self.overlay_mouse(mouse);
            return None;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            let zones = self.activity;
            if self.merged() {
                if hits_activity_button(zones.explorer, zones.row, mouse.column, mouse.row) {
                    self.overlay = None;
                    self.sidebar_state = sidebar::update_state(|state| state.search_active = false);
                    return None;
                }
                if hits_activity_button(zones.search, zones.row, mouse.column, mouse.row) {
                    self.open_content_search(false);
                    return None;
                }
                if hits_activity_button(zones.source_control, zones.row, mouse.column, mouse.row) {
                    return self.switch_to(View::SourceControl);
                }
            }
            let gear = self.gear;
            if hits(gear, mouse.column, mouse.row) {
                self.open_settings();
                return None;
            }
            if (self.overlay.is_none()
                || matches!(self.overlay, Some(Overlay::ContentSearch { .. })))
                && hits(self.git_footer_zones.branch, mouse.column, mouse.row)
            {
                self.open_branch_picker();
                return None;
            }
            if (self.overlay.is_none()
                || matches!(self.overlay, Some(Overlay::ContentSearch { .. })))
                && hits(self.git_footer_zones.sync, mouse.column, mouse.row)
            {
                self.sync_git_footer();
                return None;
            }
        }
        if matches!(self.overlay, Some(Overlay::ContentSearch { .. })) {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && hits_collapse_button(mouse.column, mouse.row, self.last_width, self.last_height)
            {
                self.hide();
                return None;
            }
            self.search_mouse(mouse);
            return None;
        }
        match mouse.kind {
            MouseEventKind::Moved => {
                self.hovered = self.row_at(mouse.row);
            }
            MouseEventKind::ScrollUp => self.scroll_view(-3),
            MouseEventKind::ScrollDown => self.scroll_view(3),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(&(_, action)) = self
                    .title_zones
                    .iter()
                    .find(|(rect, _)| hits(*rect, mouse.column, mouse.row))
                {
                    self.title_action(action);
                    return None;
                }
                if hits_collapse_button(mouse.column, mouse.row, self.last_width, self.last_height)
                {
                    self.hide();
                    return None;
                }
                let Some(index) = self.row_at(mouse.row) else {
                    self.clear_selection();
                    return None;
                };
                self.select(index);
                let row = &self.rows[index];
                let (is_dir, path) = (row.is_dir, row.path.clone());
                let on_chevron = is_dir && hits_chevron(mouse.column, row.depth);
                // Double click = second click on the same row inside the window.
                let now = std::time::Instant::now();
                let double = self
                    .last_click
                    .take()
                    .is_some_and(|(i, at)| i == index && now.duration_since(at) < DOUBLE_CLICK);
                self.last_click = Some((index, now));
                if is_dir {
                    // A single click anywhere on the row toggles. Suppress a
                    // name's second click so a double-click cannot immediately
                    // undo the first; explicit chevron clicks always toggle.
                    if folder_click_toggles(on_chevron, double) {
                        self.toggle();
                    }
                } else if self.sidebar_state.custom_editor_on_click
                    && actions::configured_editor().is_some()
                {
                    if !double {
                        self.open_custom_editor(&path);
                    }
                } else if double {
                    // Pin the tab the first click just opened. Re-opening
                    // here would race the viewer's first token stamp and
                    // spawn a second tab for the same file.
                    let doc_key = herdr_sidebar::viewer::doc_key_for_file(&path);
                    match self.last_preview.as_ref() {
                        Some((key, target)) if *key == doc_key => {
                            if !herdr_sidebar::viewer::pin_target(target, &doc_key) {
                                self.notice = Some(
                                    "preview switch blocked; resolve unsaved changes in its tab"
                                        .into(),
                                );
                            }
                        }
                        _ => self.open_preview(&path),
                    }
                } else {
                    // A click on a file previews it in the ephemeral tab.
                    self.open_preview(&path);
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.notice = None;
                self.open_context_menu(mouse.column, mouse.row);
            }
            _ => {}
        }
        None
    }

    /// One of the hover title-bar buttons was clicked.
    fn title_action(&mut self, action: TitleAction) {
        match action {
            TitleAction::NewFile => self.open_create_prompt(false),
            TitleAction::NewFolder => self.open_create_prompt(true),
            TitleAction::Refresh => self.refresh_tree(),
            TitleAction::CollapseAll => {
                self.tree.collapse_all();
                self.scroll = 0;
                self.rebuild();
            }
            TitleAction::ViewAsTree | TitleAction::ViewAsList => {}
        }
    }

    /// The title-bar New File / New Folder buttons: prompt for a name,
    /// creating in the VS Code target (see [`create_target_dir`]).
    fn open_create_prompt(&mut self, folder: bool) {
        let dir = create_target_dir(self.selected_row(), self.tree.root_path());
        self.overlay = Some(Overlay::Prompt {
            title: if folder { "New folder" } else { "New file" }.into(),
            input: String::new(),
            kind: if folder {
                PromptKind::NewFolder(dir)
            } else {
                PromptKind::NewFile(dir)
            },
        });
    }

    /// Open the file context menu at the click position, targeting the row
    /// under the cursor (or the workspace root on empty space).
    fn open_context_menu(&mut self, x: u16, y: u16) {
        let target = self.row_at(y).map(|index| {
            self.select(index);
            let row = &self.rows[index];
            (row.path.clone(), row.is_dir)
        });
        self.show_menu(x, y, target);
    }

    /// `m`: the same context menu, opened from the KEYBOARD on the current
    /// selection (issue #18 — mobile herdr clients and terminals that swallow
    /// right-click have no other way in). With nothing selected it targets the
    /// workspace root, exactly like a right-click on empty space.
    fn open_menu_for_selection(&mut self) {
        let target = self
            .selected_row()
            .map(|row| (row.path.clone(), row.is_dir));
        let (x, y) = self.selection_anchor();
        self.show_menu(x, y, target);
    }

    /// Where a keyboard-opened popup anchors: just under the selected row when
    /// it is on screen, else the top of the body.
    fn selection_anchor(&self) -> (u16, u16) {
        let Some(index) = self.selected else {
            return (0, self.body.top);
        };
        let visible =
            index >= self.body.offset && index < self.body.offset + usize::from(self.body.height);
        let y = if visible {
            self.body.top + (index - self.body.offset) as u16
        } else {
            self.body.top
        };
        let depth = self.rows.get(index).map(|r| r.depth).unwrap_or(0);
        let x = ((depth * 2) as u16).min(self.last_width.saturating_sub(1));
        (x, y)
    }

    /// Build and show the menu popup for a resolved target.
    fn show_menu(&mut self, x: u16, y: u16, target: Option<(PathBuf, bool)>) {
        let in_repo = target
            .as_ref()
            .is_some_and(|(path, _)| Git::owner_of(path).is_ok());
        let entries = actions::menu_entries(target.as_ref().map(|(_, is_dir)| *is_dir), in_repo);
        let selected = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Action(..)))
            .unwrap_or(0);
        self.overlay = Some(Overlay::Menu {
            x,
            y,
            target,
            entries,
            selected,
            rect: Rect::default(),
        });
    }

    fn overlay_key(&mut self, key: KeyEvent) {
        enum Cmd {
            Nothing,
            Close,
            Activate,
            ConfirmPrompt,
            OpenQuick(PathBuf),
            StartContentSearch,
            OpenContent(PathBuf, usize),
            ToggleSetting(usize),
            AdjustWidth(bool),
            DeleteConfirmed(PathBuf, bool),
            Picker(PickerAction),
        }
        let settings = self.settings_rows();
        let row_count = settings.len();
        let cmd = match self.overlay.as_mut() {
            Some(Overlay::Settings { selected, .. }) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => Cmd::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    Cmd::Nothing
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(row_count.saturating_sub(1));
                    Cmd::Nothing
                }
                KeyCode::Left | KeyCode::Char('h')
                    if settings.get(*selected).map(|row| row.0) == Some(Setting::SidebarWidth) =>
                {
                    Cmd::AdjustWidth(false)
                }
                KeyCode::Right | KeyCode::Char('l')
                    if settings.get(*selected).map(|row| row.0) == Some(Setting::SidebarWidth) =>
                {
                    Cmd::AdjustWidth(true)
                }
                KeyCode::Enter | KeyCode::Char(' ') => Cmd::ToggleSetting(*selected),
                _ => Cmd::Nothing,
            },
            Some(Overlay::BranchPicker(picker)) => Cmd::Picker(picker.key(key)),
            Some(Overlay::Menu {
                entries, selected, ..
            }) => match key.code {
                KeyCode::Esc => Cmd::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = step_menu(entries, *selected, -1);
                    Cmd::Nothing
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = step_menu(entries, *selected, 1);
                    Cmd::Nothing
                }
                KeyCode::Enter => Cmd::Activate,
                _ => Cmd::Nothing,
            },
            Some(Overlay::Prompt { input, .. }) => match key.code {
                KeyCode::Esc => Cmd::Close,
                KeyCode::Backspace => {
                    input.pop();
                    Cmd::Nothing
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    Cmd::Nothing
                }
                KeyCode::Enter => Cmd::ConfirmPrompt,
                _ => Cmd::Nothing,
            },
            Some(Overlay::QuickOpen {
                query,
                files,
                matches,
                selected,
                ..
            }) => match key.code {
                KeyCode::Esc => Cmd::Close,
                KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                    Cmd::Nothing
                }
                KeyCode::Down => {
                    *selected = (*selected + 1).min(matches.len().saturating_sub(1));
                    Cmd::Nothing
                }
                KeyCode::Backspace => {
                    query.pop();
                    *matches = quick_matches(files, query);
                    *selected = 0;
                    Cmd::Nothing
                }
                KeyCode::Enter => matches
                    .get(*selected)
                    .and_then(|index| files.get(*index))
                    .map(|file| Cmd::OpenQuick(file.path.clone()))
                    .unwrap_or(Cmd::Nothing),
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    query.push(c);
                    *matches = quick_matches(files, query);
                    *selected = 0;
                    Cmd::Nothing
                }
                _ => Cmd::Nothing,
            },
            Some(Overlay::ContentSearch {
                query,
                replace,
                include,
                exclude,
                hits,
                selected,
                truncated,
                loading,
                searched,
                details_expanded,
                focus,
                options,
                error,
                dirty_since,
            }) => {
                let mark_dirty =
                    |query: &str,
                     hits: &mut std::sync::Arc<Vec<ContentHit>>,
                     selected: &mut usize,
                     truncated: &mut bool,
                     loading: &mut bool,
                     searched: &mut bool,
                     error: &mut Option<String>,
                     dirty_since: &mut Option<std::time::Instant>| {
                        *hits = std::sync::Arc::new(Vec::new());
                        *selected = 0;
                        *truncated = false;
                        *loading = false;
                        *searched = false;
                        *error = None;
                        *dirty_since = (!query.trim().is_empty()).then(std::time::Instant::now);
                    };
                if key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    let toggled = match key.code {
                        KeyCode::Char('c' | 'C') => {
                            options.match_case = !options.match_case;
                            true
                        }
                        KeyCode::Char('w' | 'W') => {
                            options.whole_word = !options.whole_word;
                            true
                        }
                        KeyCode::Char('r' | 'R') => {
                            options.regex = !options.regex;
                            true
                        }
                        _ => false,
                    };
                    if toggled {
                        mark_dirty(
                            query,
                            hits,
                            selected,
                            truncated,
                            loading,
                            searched,
                            error,
                            dirty_since,
                        );
                    }
                    Cmd::Nothing
                } else if matches!(key.code, KeyCode::Char('j' | 'J'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT)
                {
                    *details_expanded = !*details_expanded;
                    if !*details_expanded
                        && matches!(*focus, SearchFocus::Include | SearchFocus::Exclude)
                    {
                        *focus = SearchFocus::Query;
                    }
                    Cmd::Nothing
                } else {
                    match key.code {
                        KeyCode::Esc => Cmd::Close,
                        KeyCode::Tab => {
                            *focus = match *focus {
                                SearchFocus::Query => SearchFocus::Replace,
                                SearchFocus::Replace if *details_expanded => SearchFocus::Include,
                                SearchFocus::Replace => SearchFocus::Results,
                                SearchFocus::Include => SearchFocus::Exclude,
                                SearchFocus::Exclude => SearchFocus::Results,
                                SearchFocus::Results => SearchFocus::Query,
                            };
                            Cmd::Nothing
                        }
                        KeyCode::BackTab => {
                            *focus = match *focus {
                                SearchFocus::Query => SearchFocus::Results,
                                SearchFocus::Replace => SearchFocus::Query,
                                SearchFocus::Include => SearchFocus::Replace,
                                SearchFocus::Exclude => SearchFocus::Include,
                                SearchFocus::Results if *details_expanded => SearchFocus::Exclude,
                                SearchFocus::Results => SearchFocus::Replace,
                            };
                            Cmd::Nothing
                        }
                        KeyCode::Up if *focus == SearchFocus::Results => {
                            *selected = selected.saturating_sub(1);
                            Cmd::Nothing
                        }
                        KeyCode::Down if *focus == SearchFocus::Results => {
                            *selected = (*selected + 1).min(hits.len().saturating_sub(1));
                            Cmd::Nothing
                        }
                        KeyCode::Down if *focus != SearchFocus::Replace && !hits.is_empty() => {
                            *focus = SearchFocus::Results;
                            Cmd::Nothing
                        }
                        KeyCode::Enter if *focus == SearchFocus::Results => hits
                            .get(*selected)
                            .map(|hit| Cmd::OpenContent(hit.path.clone(), hit.line))
                            .unwrap_or(Cmd::Nothing),
                        KeyCode::Enter
                            if matches!(
                                *focus,
                                SearchFocus::Query | SearchFocus::Include | SearchFocus::Exclude
                            ) && !*loading
                                && !query.trim().is_empty() =>
                        {
                            Cmd::StartContentSearch
                        }
                        KeyCode::Backspace if *focus == SearchFocus::Query => {
                            query.pop();
                            mark_dirty(
                                query,
                                hits,
                                selected,
                                truncated,
                                loading,
                                searched,
                                error,
                                dirty_since,
                            );
                            Cmd::Nothing
                        }
                        KeyCode::Backspace if *focus == SearchFocus::Replace => {
                            replace.pop();
                            Cmd::Nothing
                        }
                        KeyCode::Backspace
                            if matches!(*focus, SearchFocus::Include | SearchFocus::Exclude) =>
                        {
                            if *focus == SearchFocus::Include {
                                include.pop();
                            } else {
                                exclude.pop();
                            }
                            mark_dirty(
                                query,
                                hits,
                                selected,
                                truncated,
                                loading,
                                searched,
                                error,
                                dirty_since,
                            );
                            Cmd::Nothing
                        }
                        KeyCode::Char(c)
                            if (!key.modifiers.contains(KeyModifiers::CONTROL)
                                || key.modifiers.contains(KeyModifiers::ALT))
                                && *focus == SearchFocus::Query =>
                        {
                            query.push(c);
                            mark_dirty(
                                query,
                                hits,
                                selected,
                                truncated,
                                loading,
                                searched,
                                error,
                                dirty_since,
                            );
                            Cmd::Nothing
                        }
                        KeyCode::Char(c)
                            if (!key.modifiers.contains(KeyModifiers::CONTROL)
                                || key.modifiers.contains(KeyModifiers::ALT))
                                && *focus == SearchFocus::Replace =>
                        {
                            replace.push(c);
                            Cmd::Nothing
                        }
                        KeyCode::Char(c)
                            if (!key.modifiers.contains(KeyModifiers::CONTROL)
                                || key.modifiers.contains(KeyModifiers::ALT))
                                && matches!(
                                    *focus,
                                    SearchFocus::Include | SearchFocus::Exclude
                                ) =>
                        {
                            if *focus == SearchFocus::Include {
                                include.push(c);
                            } else {
                                exclude.push(c);
                            }
                            mark_dirty(
                                query,
                                hits,
                                selected,
                                truncated,
                                loading,
                                searched,
                                error,
                                dirty_since,
                            );
                            Cmd::Nothing
                        }
                        _ => Cmd::Nothing,
                    }
                }
            }
            Some(Overlay::ConfirmDelete { path, is_dir }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Cmd::DeleteConfirmed(path.clone(), *is_dir)
                }
                _ => Cmd::Close,
            },
            None => Cmd::Nothing,
        };
        match cmd {
            Cmd::Nothing => {}
            Cmd::Close => {
                let closing_search = matches!(self.overlay, Some(Overlay::ContentSearch { .. }));
                // A modal opened from Search resumes it; a plain search close
                // (Esc in the search box) has nothing parked and drops to tree.
                self.overlay = self.suspended_search.take();
                if closing_search && self.merged() {
                    self.sidebar_state = sidebar::update_state(|state| state.search_active = false);
                }
            }
            Cmd::Activate => self.activate_menu_entry(),
            Cmd::ConfirmPrompt => self.confirm_prompt(),
            Cmd::OpenQuick(path) => {
                self.overlay = None;
                self.open_preview(&path);
            }
            Cmd::StartContentSearch => self.start_content_search(),
            Cmd::OpenContent(path, line) => {
                self.open_preview_at(&path, Some(line));
            }
            Cmd::ToggleSetting(index) => self.toggle_setting(index),
            Cmd::AdjustWidth(wider) => self.adjust_sidebar_width(wider),
            Cmd::DeleteConfirmed(path, is_dir) => {
                self.overlay = None;
                match actions::delete(&path, is_dir) {
                    Ok(()) => self.refresh_tree(),
                    Err(err) => self.notice = Some(format!("delete failed: {err}")),
                }
            }
            Cmd::Picker(action) => self.handle_picker_action(action),
        }
    }

    fn overlay_mouse(&mut self, mouse: MouseEvent) {
        enum Cmd {
            Nothing,
            Close,
            Activate,
            ToggleSetting(usize),
            Reopen(u16, u16),
            Picker(PickerAction),
        }
        let row_count = self.settings_rows().len();
        let cmd = match self.overlay.as_mut() {
            Some(Overlay::Settings {
                selected,
                rect,
                scroll,
            }) => {
                // Rows start just inside the top border (the title renders ON
                // the border, not on its own line).
                let row_at = |row: u16, col: u16| -> Option<usize> {
                    let index = usize::from(row.saturating_sub(rect.y + 1)) + *scroll;
                    (col > rect.x
                        && col < rect.x + rect.width.saturating_sub(1)
                        && row > rect.y
                        && row < rect.y + rect.height.saturating_sub(1)
                        && index < row_count)
                        .then_some(index)
                };
                match mouse.kind {
                    MouseEventKind::Moved => {
                        if let Some(i) = row_at(mouse.row, mouse.column) {
                            *selected = i;
                        }
                        Cmd::Nothing
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        match row_at(mouse.row, mouse.column) {
                            Some(i) => {
                                *selected = i;
                                Cmd::ToggleSetting(i)
                            }
                            None if mouse.column >= rect.x
                                && mouse.column < rect.x + rect.width
                                && mouse.row >= rect.y
                                && mouse.row < rect.y + rect.height =>
                            {
                                Cmd::Nothing
                            }
                            None => Cmd::Close,
                        }
                    }
                    _ => Cmd::Nothing,
                }
            }
            Some(Overlay::Menu {
                entries,
                selected,
                rect,
                ..
            }) => {
                let inner = rect.inner(ratatui::layout::Margin::new(1, 1));
                let item_at = |row: u16, col: u16| -> Option<usize> {
                    (col >= inner.x
                        && col < inner.x + inner.width
                        && row >= inner.y
                        && row < inner.y + inner.height)
                        .then(|| usize::from(row - inner.y))
                        .filter(|i| {
                            *i < entries.len() && matches!(entries[*i], MenuEntry::Action(..))
                        })
                };
                match mouse.kind {
                    MouseEventKind::Moved => {
                        if let Some(i) = item_at(mouse.row, mouse.column) {
                            *selected = i;
                        }
                        Cmd::Nothing
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(i) = item_at(mouse.row, mouse.column) {
                            *selected = i;
                            Cmd::Activate
                        } else {
                            Cmd::Close
                        }
                    }
                    MouseEventKind::Down(MouseButton::Right) => {
                        Cmd::Reopen(mouse.column, mouse.row)
                    }
                    _ => Cmd::Nothing,
                }
            }
            Some(Overlay::BranchPicker(picker)) => Cmd::Picker(picker.mouse(mouse)),
            // Prompts/confirms are keyboard-driven; clicks do nothing.
            _ => Cmd::Nothing,
        };
        match cmd {
            Cmd::Nothing => {}
            Cmd::Close => self.overlay = None,
            Cmd::Activate => self.activate_menu_entry(),
            Cmd::ToggleSetting(index) => self.toggle_setting(index),
            Cmd::Reopen(x, y) => {
                self.overlay = None;
                self.open_context_menu(x, y);
            }
            Cmd::Picker(action) => self.handle_picker_action(action),
        }
    }

    /// Park a live Search overlay so a modal can open over it and be restored
    /// on close, rather than clobbering it (which dropped the user back to the
    /// tree). No-op when the current overlay isn't Search.
    fn suspend_search_for_modal(&mut self) {
        if matches!(self.overlay, Some(Overlay::ContentSearch { .. })) {
            self.suspended_search = self.overlay.take();
        }
    }

    fn open_branch_picker(&mut self) {
        let Some(git) = self.repos.first().cloned() else {
            return;
        };
        match BranchPicker::open(git) {
            Ok(picker) => {
                self.suspend_search_for_modal();
                self.overlay = Some(Overlay::BranchPicker(picker));
            }
            Err(error) => self.notice = Some(error),
        }
    }

    fn handle_picker_action(&mut self, action: PickerAction) {
        match action {
            PickerAction::None => {}
            // Resume a Search overlay parked under the picker (else → tree).
            PickerAction::Close => self.overlay = self.suspended_search.take(),
            PickerAction::Checkout(branch) => {
                let Some(Overlay::BranchPicker(picker)) = self.overlay.take() else {
                    return;
                };
                match picker.git.checkout_branch(&branch) {
                    Ok(()) => {
                        self.notice = Some(format!("switched to {}", branch.name));
                        self.refresh_tree();
                    }
                    Err(error) => self.notice = Some(error),
                }
                self.overlay = self.suspended_search.take();
            }
            PickerAction::Create(name) => {
                let Some(Overlay::BranchPicker(picker)) = self.overlay.take() else {
                    return;
                };
                match picker.git.create_branch(&name) {
                    Ok(()) => {
                        self.notice = Some(format!("created {name}"));
                        self.refresh_tree();
                    }
                    Err(error) => self.notice = Some(error),
                }
                self.overlay = self.suspended_search.take();
            }
        }
    }

    fn sync_git_footer(&mut self) {
        if self.git_syncing.is_some() {
            return;
        }
        let Some(status) = &self.git_footer_status else {
            return;
        };
        if !status.has_upstream {
            self.notice = Some("no upstream to sync with".into());
            return;
        }
        let Some(git) = self.repos.first().cloned() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(git.sync());
        });
        self.git_syncing = Some(rx);
    }

    fn collect_git_sync(&mut self) {
        let Some(rx) = &self.git_syncing else { return };
        match rx.try_recv() {
            Ok(Ok(summary)) => {
                self.git_syncing = None;
                self.notice = Some(summary);
                self.request_decorations(true);
            }
            Ok(Err(error)) => {
                self.git_syncing = None;
                self.notice = Some(error);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.git_syncing = None;
                self.notice = Some("sync failed".into());
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    // ---- Settings modal ----

    fn open_settings(&mut self) {
        herdr_sidebar::updates::check();
        self.suspend_search_for_modal();
        self.overlay = Some(Overlay::Settings {
            selected: 0,
            rect: Rect::default(),
            scroll: 0,
        });
    }

    fn collect_quick_index(&mut self) {
        let result = self
            .quick_index_rx
            .as_ref()
            .map(std::sync::mpsc::Receiver::try_recv);
        match result {
            Some(Ok(index)) => {
                self.quick_index_rx = None;
                if index.root != self.tree.root_path() || index.show_hidden != self.tree.show_hidden
                {
                    if let Some(Overlay::QuickOpen { loading, .. }) = self.overlay.as_mut() {
                        *loading = false;
                    }
                    return;
                }
                if let Some(Overlay::QuickOpen {
                    query,
                    files,
                    matches,
                    selected,
                    truncated,
                    loading,
                }) = self.overlay.as_mut()
                {
                    *files = std::sync::Arc::clone(&index.files);
                    *matches = quick_matches(files, query);
                    *selected = 0;
                    *truncated = index.truncated;
                    *loading = false;
                }
                self.quick_index = Some(index);
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                self.quick_index_rx = None;
                if let Some(Overlay::QuickOpen { loading, .. }) = self.overlay.as_mut() {
                    *loading = false;
                }
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
        }
    }

    fn invalidate_quick_index(&mut self) {
        self.quick_index = None;
        self.quick_index_rx = None;
    }

    pub fn open_quick_open(&mut self) {
        let root = self.tree.root_path();
        let show_hidden = self.tree.show_hidden;
        let cached = self
            .quick_index
            .as_ref()
            .filter(|index| index.root == root && index.show_hidden == show_hidden)
            .map(|index| (std::sync::Arc::clone(&index.files), index.truncated));
        let (files, truncated, loading) = if let Some((files, truncated)) = cached {
            (files, truncated, false)
        } else {
            if self.quick_index_rx.is_none() {
                let (tx, rx) = std::sync::mpsc::channel();
                let worker_root = root.clone();
                std::thread::spawn(move || {
                    let (files, truncated) =
                        collect_quick_files(&worker_root, show_hidden, QUICK_OPEN_FILE_LIMIT);
                    let _ = tx.send(QuickIndex {
                        root: worker_root,
                        show_hidden,
                        files: std::sync::Arc::new(files),
                        truncated,
                    });
                });
                self.quick_index_rx = Some(rx);
            }
            (std::sync::Arc::new(Vec::new()), false, true)
        };
        let matches = quick_matches(&files, "");
        self.overlay = Some(Overlay::QuickOpen {
            query: String::new(),
            files,
            matches,
            selected: 0,
            truncated,
            loading,
        });
    }

    /// True when the search overlay is open and its focused field holds no
    /// text (Results focus counts as empty — nothing is being typed there), so
    /// True when the search overlay is open with NO text field focused (the
    /// Results list), so a bare 1/2/3 is a view switch. Switching into Search
    /// lands here; a focused input (Ctrl+F, Tab, or a click) captures digits as
    /// text so you can search for "3".
    fn search_text_unfocused(&self) -> bool {
        matches!(
            self.overlay,
            Some(Overlay::ContentSearch {
                focus: SearchFocus::Results,
                ..
            })
        )
    }

    /// Open the Search view. `focus_query` puts the caret in the search box
    /// ready to type (the Ctrl+F "find" gesture); switching in via 2 / the
    /// activity bar passes `false`, so the box is not focused and bare 1/2/3
    /// keep switching views until the user deliberately focuses it.
    pub fn open_content_search(&mut self, focus_query: bool) {
        if matches!(self.overlay, Some(Overlay::ContentSearch { .. })) {
            return;
        }
        self.overlay = Some(Overlay::ContentSearch {
            query: String::new(),
            replace: String::new(),
            include: String::new(),
            exclude: String::new(),
            hits: std::sync::Arc::new(Vec::new()),
            selected: 0,
            truncated: false,
            loading: false,
            searched: false,
            details_expanded: false,
            focus: if focus_query {
                SearchFocus::Query
            } else {
                SearchFocus::Results
            },
            options: SearchOptions::default(),
            error: None,
            dirty_since: None,
        });
        self.search_scroll = 0;
        self.search_snap = false;
        if self.merged() {
            self.sidebar_state = sidebar::update_state(|state| {
                state.active = View::Explorer;
                state.search_active = true;
            });
        }
    }

    fn start_content_search_if_due(&mut self) {
        let due = matches!(
            self.overlay,
            Some(Overlay::ContentSearch {
                dirty_since: Some(started),
                ..
            }) if started.elapsed() >= std::time::Duration::from_millis(300)
        );
        if due && self.content_search_rx.is_none() {
            self.start_content_search();
        }
    }

    fn search_mouse(&mut self, mouse: MouseEvent) {
        enum Cmd {
            Nothing,
            Search,
            Open(PathBuf, usize),
            Scroll(isize),
        }
        let zones = self.search_zones;
        let clicked_result = self
            .search_result_rows
            .iter()
            .find(|(area, _)| hits(*area, mouse.column, mouse.row))
            .map(|(_, index)| *index);
        let Some(Overlay::ContentSearch {
            query,
            replace,
            include,
            exclude,
            hits: search_hits,
            selected,
            truncated,
            loading,
            searched,
            details_expanded,
            focus,
            options,
            error,
            dirty_since,
            ..
        }) = self.overlay.as_mut()
        else {
            return;
        };
        let mut dirty = false;
        let cmd = match mouse.kind {
            MouseEventKind::ScrollUp => Cmd::Scroll(-3),
            MouseEventKind::ScrollDown => Cmd::Scroll(3),
            MouseEventKind::Down(MouseButton::Left)
                if hits(zones.refresh, mouse.column, mouse.row) =>
            {
                Cmd::Search
            }
            MouseEventKind::Down(MouseButton::Left)
                if hits(zones.clear, mouse.column, mouse.row) =>
            {
                query.clear();
                replace.clear();
                include.clear();
                exclude.clear();
                *search_hits = std::sync::Arc::new(Vec::new());
                *selected = 0;
                *truncated = false;
                *loading = false;
                *searched = false;
                *error = None;
                *dirty_since = None;
                *focus = SearchFocus::Query;
                Cmd::Nothing
            }
            MouseEventKind::Down(MouseButton::Left)
                if hits(zones.details, mouse.column, mouse.row) =>
            {
                *details_expanded = !*details_expanded;
                if !*details_expanded
                    && matches!(*focus, SearchFocus::Include | SearchFocus::Exclude)
                {
                    *focus = SearchFocus::Query;
                }
                Cmd::Nothing
            }
            MouseEventKind::Down(MouseButton::Left)
                if hits(zones.match_case, mouse.column, mouse.row) =>
            {
                options.match_case = !options.match_case;
                dirty = true;
                Cmd::Nothing
            }
            MouseEventKind::Down(MouseButton::Left)
                if hits(zones.whole_word, mouse.column, mouse.row) =>
            {
                options.whole_word = !options.whole_word;
                dirty = true;
                Cmd::Nothing
            }
            MouseEventKind::Down(MouseButton::Left)
                if hits(zones.regex, mouse.column, mouse.row) =>
            {
                options.regex = !options.regex;
                dirty = true;
                Cmd::Nothing
            }
            MouseEventKind::Down(MouseButton::Left)
                if hits(zones.query, mouse.column, mouse.row) =>
            {
                *focus = SearchFocus::Query;
                Cmd::Nothing
            }
            MouseEventKind::Down(MouseButton::Left)
                if hits(zones.replace, mouse.column, mouse.row) =>
            {
                *focus = SearchFocus::Replace;
                Cmd::Nothing
            }
            MouseEventKind::Down(MouseButton::Left)
                if *details_expanded && hits(zones.include, mouse.column, mouse.row) =>
            {
                *focus = SearchFocus::Include;
                Cmd::Nothing
            }
            MouseEventKind::Down(MouseButton::Left)
                if *details_expanded && hits(zones.exclude, mouse.column, mouse.row) =>
            {
                *focus = SearchFocus::Exclude;
                Cmd::Nothing
            }
            MouseEventKind::Down(MouseButton::Left) => clicked_result
                .and_then(|index| {
                    *selected = index;
                    *focus = SearchFocus::Results;
                    search_hits
                        .get(index)
                        .map(|hit| Cmd::Open(hit.path.clone(), hit.line))
                })
                .unwrap_or(Cmd::Nothing),
            _ => Cmd::Nothing,
        };
        if dirty {
            *search_hits = std::sync::Arc::new(Vec::new());
            *selected = 0;
            *truncated = false;
            *loading = false;
            *searched = false;
            *error = None;
            *dirty_since = (!query.trim().is_empty()).then(std::time::Instant::now);
        }
        match cmd {
            Cmd::Nothing => {}
            Cmd::Search => self.start_content_search(),
            Cmd::Open(path, line) => self.open_preview_at(&path, Some(line)),
            Cmd::Scroll(delta) => {
                self.search_snap = false;
                self.search_scroll = if delta < 0 {
                    self.search_scroll.saturating_sub(delta.unsigned_abs())
                } else {
                    self.search_scroll.saturating_add(delta as usize)
                };
            }
        }
    }

    fn start_content_search(&mut self) {
        let Some(Overlay::ContentSearch {
            query,
            include,
            exclude,
            hits,
            selected,
            truncated,
            loading,
            searched,
            options,
            error,
            dirty_since,
            ..
        }) = self.overlay.as_mut()
        else {
            return;
        };
        let query = query.clone();
        let include = include.clone();
        let exclude = exclude.clone();
        if query.is_empty() || self.content_search_rx.is_some() {
            return;
        }
        *hits = std::sync::Arc::new(Vec::new());
        *selected = 0;
        *truncated = false;
        *loading = true;
        *searched = true;
        *error = None;
        *dirty_since = None;
        self.search_scroll = 0;
        self.search_snap = true;
        let options = *options;
        let root = self.tree.root_path();
        let show_hidden = self.tree.show_hidden;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = collect_content_hits(
                &root,
                show_hidden,
                &query,
                &include,
                &exclude,
                options,
                CONTENT_SEARCH_MATCH_LIMIT,
            );
            let (hits, truncated, error) = match result {
                Ok((hits, truncated)) => (hits, truncated, None),
                Err(error) => (Vec::new(), false, Some(error)),
            };
            let _ = tx.send(ContentSearchResult {
                root,
                show_hidden,
                query,
                include,
                exclude,
                options,
                hits: std::sync::Arc::new(hits),
                truncated,
                error,
            });
        });
        self.content_search_rx = Some(rx);
    }

    fn collect_content_search(&mut self) {
        let result = self
            .content_search_rx
            .as_ref()
            .map(std::sync::mpsc::Receiver::try_recv);
        match result {
            Some(Ok(result)) => {
                self.content_search_rx = None;
                if result.root != self.tree.root_path()
                    || result.show_hidden != self.tree.show_hidden
                {
                    return;
                }
                if let Some(Overlay::ContentSearch {
                    query,
                    include,
                    exclude,
                    hits,
                    selected,
                    truncated,
                    loading,
                    searched,
                    options,
                    error,
                    dirty_since,
                    ..
                }) = self.overlay.as_mut()
                    && *query == result.query
                    && *include == result.include
                    && *exclude == result.exclude
                    && *options == result.options
                {
                    *hits = result.hits;
                    *selected = 0;
                    *truncated = result.truncated;
                    *loading = false;
                    *searched = true;
                    *error = result.error;
                    *dirty_since = None;
                }
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                self.content_search_rx = None;
                if let Some(Overlay::ContentSearch { loading, .. }) = self.overlay.as_mut() {
                    *loading = false;
                }
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
        }
    }

    fn open_custom_editor(&mut self, path: &Path) {
        let Some(pane_id) = self
            .pane_ctl
            .as_ref()
            .map(|control| control.pane_id.clone())
        else {
            self.notice = Some("custom editor needs a herdr pane".into());
            return;
        };
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy();
        self.notice = Some(
            match actions::open_in_editor_tab(&pane_id, &self.tree.root_path(), path) {
                Ok(()) => format!("opened in custom editor: {name}"),
                Err(error) => format!("custom editor failed: {error}"),
            },
        );
    }

    /// The modal's rows for the current state.
    fn settings_rows(&self) -> Vec<SettingRow> {
        let update = herdr_sidebar::updates::row();
        vec![
            (
                Setting::Version,
                "Sidebar version",
                herdr_sidebar::updates::VERSION.into(),
                false,
            ),
            (Setting::Update, update.0, update.1, update.2),
            (
                Setting::UnifiedSidebar,
                "Unified sidebar",
                if self.merged() { "on" } else { "off" }.to_string(),
                self.other_exe.is_some(),
            ),
            (
                Setting::DockRight,
                "Dock on the right",
                if self.sidebar_state.dock_right {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::SidebarWidth,
                "Sidebar width",
                format!("{} cols", self.sidebar_state.sidebar_width),
                true,
            ),
            (
                Setting::IconTheme,
                "Icon theme",
                match self.theme {
                    IconTheme::Material => "material",
                    IconTheme::Emoji => "emoji",
                }
                .to_string(),
                true,
            ),
            (
                Setting::ColorTheme,
                "Color theme",
                self.sidebar_state.color_theme.label().to_string(),
                true,
            ),
            (
                Setting::PreviewPlacement,
                "Preview opens in",
                self.sidebar_state.preview_placement.label().to_string(),
                true,
            ),
            (
                Setting::CustomEditorCommand,
                "Custom editor...",
                actions::configured_editor()
                    .map(|command| truncate_to(command, 24))
                    .unwrap_or_else(|| "not set".into()),
                true,
            ),
            (
                Setting::CustomEditorClick,
                "Use editor on click",
                if self.sidebar_state.custom_editor_on_click {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                actions::configured_editor().is_some(),
            ),
            (
                Setting::HiddenFiles,
                "Hidden files",
                if self.tree.show_hidden {
                    "shown"
                } else {
                    "hidden"
                }
                .to_string(),
                true,
            ),
            (
                Setting::Hotkeys,
                "Footer hotkeys",
                if self.show_hotkeys() {
                    "shown"
                } else {
                    "hidden"
                }
                .to_string(),
                true,
            ),
            (
                Setting::AutoOpen,
                "Auto-open sidebar",
                if self.sidebar_state.auto_open {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::StrictToggle,
                "Strict toggle",
                if self.sidebar_state.strict_toggle {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::FocusOnOpen,
                "Focus on open",
                if self.sidebar_state.focus_on_open {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::FollowCwd,
                "Follow pane folder",
                sidebar::follow_cwd_setting_value(self.sidebar_state.follow_cwd),
                true,
            ),
            (
                Setting::GitDecorations,
                "Git decorations",
                if self.sidebar_state.git_deco {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::GitFooter,
                "Git footer",
                if self.sidebar_state.show_git_footer {
                    "shown"
                } else {
                    "hidden"
                }
                .to_string(),
                true,
            ),
            (
                Setting::Folder,
                "Change folder…",
                self.tree.root_name(),
                true,
            ),
        ]
    }

    fn toggle_setting(&mut self, index: usize) {
        let rows = self.settings_rows();
        let Some(row) = rows.get(index) else { return };
        let (setting, enabled) = (row.0, row.3);
        if !enabled {
            return;
        }
        match setting {
            Setting::Version => {}
            Setting::Update => {
                self.notice = Some(
                    herdr_sidebar::updates::activate()
                        .map(|_| "Update requested".into())
                        .unwrap_or_else(|error| error),
                );
            }
            Setting::UnifiedSidebar => {
                // The pane layout changes underneath the modal; close it.
                self.overlay = None;
                let on = !self.merged();
                self.set_unified(on);
            }
            Setting::DockRight => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.dock_right = !state.dock_right);
            }
            Setting::SidebarWidth => self.adjust_sidebar_width(true),
            Setting::IconTheme => self.set_theme(self.theme.toggled()),
            Setting::ColorTheme => {
                self.sidebar_state = sidebar::update_state(|state| {
                    state.color_theme = state.color_theme.next();
                });
                set_color_theme(self.sidebar_state.color_theme);
            }
            Setting::PreviewPlacement => {
                self.sidebar_state = sidebar::update_state(|state| {
                    state.preview_placement = state.preview_placement.other();
                });
            }
            Setting::CustomEditorCommand => {
                self.overlay = Some(Overlay::Prompt {
                    title: "Custom editor".into(),
                    input: actions::configured_editor().unwrap_or_default(),
                    kind: PromptKind::CustomEditor,
                });
            }
            Setting::CustomEditorClick => {
                self.sidebar_state = sidebar::update_state(|state| {
                    state.custom_editor_on_click = !state.custom_editor_on_click;
                });
            }
            Setting::HiddenFiles => {
                self.tree.show_hidden = !self.tree.show_hidden;
                self.invalidate_quick_index();
                self.rebuild();
            }
            Setting::Hotkeys => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.show_hotkeys = !state.show_hotkeys);
            }
            Setting::AutoOpen => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.auto_open = !state.auto_open);
            }
            Setting::StrictToggle => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.strict_toggle = !state.strict_toggle);
            }
            Setting::FocusOnOpen => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.focus_on_open = !state.focus_on_open);
            }
            Setting::FollowCwd => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.follow_cwd = !state.follow_cwd);
                self.cwd_follower.borrow_mut().reset();
            }
            Setting::GitDecorations => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.git_deco = !state.git_deco);
                self.rediscover_repos();
                self.request_decorations(true);
            }
            Setting::GitFooter => {
                self.sidebar_state = sidebar::update_state(|state| {
                    state.show_git_footer = !state.show_git_footer;
                });
                self.rediscover_repos();
                self.request_decorations(true);
            }
            Setting::Folder => {
                self.overlay = None;
                self.change_folder_dialog();
            }
        }
    }

    fn adjust_sidebar_width(&mut self, wider: bool) {
        self.sidebar_state = sidebar::update_state(|state| {
            state.sidebar_width = sidebar::step_sidebar_width(state.sidebar_width, wider);
        });
        if let Some(ctl) = &self.pane_ctl {
            ctl.resize_preferred(
                self.last_width,
                self.sidebar_state.sidebar_width,
                self.sidebar_state.dock_right,
            );
        }
    }

    /// Render the centered Settings popup and remember its rect for clicks.
    fn draw_settings(&mut self, frame: &mut Frame) {
        let rows = self.settings_rows();
        let area = frame.area();
        let desired_width = rows
            .iter()
            .map(|(_, label, value, _)| label.chars().count() + value.chars().count() + 5)
            .max()
            .unwrap_or(30)
            .max(30) as u16;
        let width = desired_width.min(area.width);
        // The hotkey reference lives here now; the footer chips are opt-in.
        let hint_lines = wrap_hints(&self.hints(), width.saturating_sub(2), 0);
        let Some(Overlay::Settings {
            selected,
            rect,
            scroll,
        }) = self.overlay.as_mut()
        else {
            return;
        };
        let height = (rows.len() as u16 + 5 + hint_lines.len() as u16).min(area.height);
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 3,
            width,
            height,
        );
        *rect = popup;
        let inner_height = usize::from(height.saturating_sub(2));
        let content_height = rows.len() + 3 + hint_lines.len();
        *scroll = keep_visible_scroll(*selected, inner_height, content_height);

        let inner_w = usize::from(width.saturating_sub(2));
        let mut lines: Vec<Line> = Vec::new();
        for (i, (_, label, value, enabled)) in rows.iter().enumerate() {
            let pad = inner_w.saturating_sub(label.chars().count() + value.chars().count() + 2);
            let text = format!(" {label}{}{value} ", " ".repeat(pad.max(1)));
            let style = if !enabled {
                Style::default().dim()
            } else if i == *selected {
                selection_style(true)
            } else {
                Style::default()
            };
            lines.push(Line::styled(text, style));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " Hotkeys",
            Style::default().bold(),
        )));
        lines.extend(hint_lines);
        lines.push(Line::from(" click/⏎ · ←/→ width · esc".dim()));

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).scroll((*scroll as u16, 0)).block(
                ratatui::widgets::Block::bordered()
                    .title(" Settings ")
                    .border_style(Style::default().dim()),
            ),
            popup,
        );
    }

    fn activate_menu_entry(&mut self) {
        let Some(Overlay::Menu {
            target,
            entries,
            selected,
            ..
        }) = self.overlay.take()
        else {
            return;
        };
        let MenuEntry::Action(action, _) = entries[selected] else {
            return;
        };
        // Creation targets: the folder itself, a file's parent, or the root.
        let create_dir = match &target {
            Some((path, true)) => path.clone(),
            Some((path, false)) => path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.tree.root_path()),
            None => self.tree.root_path(),
        };
        match action {
            MenuAction::NewFile => {
                self.overlay = Some(Overlay::Prompt {
                    title: "New file".into(),
                    input: String::new(),
                    kind: PromptKind::NewFile(create_dir),
                });
            }
            MenuAction::NewFolder => {
                self.overlay = Some(Overlay::Prompt {
                    title: "New folder".into(),
                    input: String::new(),
                    kind: PromptKind::NewFolder(create_dir),
                });
            }
            MenuAction::CopyPath | MenuAction::CopyRelativePath => {
                let Some((path, _)) = &target else { return };
                let text = if action == MenuAction::CopyPath {
                    path.display().to_string()
                } else {
                    path.strip_prefix(self.tree.root_path())
                        .unwrap_or(path)
                        .display()
                        .to_string()
                };
                self.notice = Some(match actions::copy_to_clipboard(&text) {
                    Ok(actions::ClipboardWrite::Native) => format!("copied: {text}"),
                    Ok(actions::ClipboardWrite::Osc52Unacknowledged) => {
                        format!("sent to terminal clipboard: {text}")
                    }
                    Err(err) => format!("copy failed: {err}"),
                });
            }
            MenuAction::Rename => {
                let Some((path, _)) = target else { return };
                let current = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.overlay = Some(Overlay::Prompt {
                    title: "Rename".into(),
                    input: current,
                    kind: PromptKind::Rename(path),
                });
            }
            MenuAction::Delete => {
                let Some((path, is_dir)) = target else { return };
                self.overlay = Some(Overlay::ConfirmDelete { path, is_dir });
            }
            MenuAction::OpenExternal => {
                let Some((path, _)) = target else { return };
                let name = path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy();
                self.notice = Some(match actions::open_external(&path) {
                    Ok(()) => format!("opened: {name}"),
                    Err(err) => format!("open failed: {err}"),
                });
            }
            MenuAction::Stage => {
                let Some((path, _)) = target else { return };
                self.stage(&path);
            }
            MenuAction::Reveal => {
                let (path, directory) = target.unwrap_or_else(|| (self.tree.root_path(), true));
                actions::reveal(&path, directory);
            }
            MenuAction::ChangeFolder => self.change_folder_dialog(),
            MenuAction::ChangeFolderTyped => self.change_folder_prompt(),
        }
    }

    /// `c` / the context menu: the NATIVE folder picker, on a background
    /// thread so the pane's liveness heartbeat keeps beating while the
    /// dialog is open (a frozen TUI would read as a corpse after 20s).
    #[cfg(any(windows, target_os = "macos"))]
    fn change_folder_dialog(&mut self) {
        if self.picking.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let start = self.tree.root_path();
        std::thread::spawn(move || {
            let _ = tx.send(actions::pick_folder(&start));
        });
        self.picking = Some(rx);
        self.notice = Some("folder picker open… (check your other windows)".into());
    }

    /// No native dialogs here — fall back to the typed prompt.
    #[cfg(not(any(windows, target_os = "macos")))]
    fn change_folder_dialog(&mut self) {
        self.change_folder_prompt();
    }

    /// Collect a finished folder pick, if any (called from the poll loop).
    pub fn poll_picker(&mut self) {
        let Some(rx) = &self.picking else { return };
        match rx.try_recv() {
            Ok(Some(path)) => {
                self.picking = None;
                self.change_folder(&path.display().to_string());
            }
            Ok(None) => {
                self.picking = None;
                self.notice = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(_) => self.picking = None,
        }
    }

    /// `c` / the context menu: prompt for a new root folder, prefilled with
    /// the current one so relative tweaks are quick.
    fn change_folder_prompt(&mut self) {
        self.overlay = Some(Overlay::Prompt {
            title: "Folder".into(),
            input: self.tree.root_path().display().to_string(),
            kind: PromptKind::ChangeFolder,
        });
    }

    /// Re-root everything at `target` (also the PROCESS cwd, so the Source
    /// Control view follows on the next view switch).
    fn change_folder(&mut self, raw: &str) {
        self.change_folder_impl(raw, true);
    }

    fn change_folder_impl(&mut self, raw: &str, manual: bool) {
        let raw = raw.trim();
        if raw.is_empty() {
            self.notice = Some("empty path".into());
            return;
        }
        let expanded = match raw.strip_prefix('~') {
            Some(rest) => {
                let home = std::env::var("USERPROFILE")
                    .or_else(|_| std::env::var("HOME"))
                    .unwrap_or_default();
                format!("{home}{rest}")
            }
            None => raw.to_string(),
        };
        let target = PathBuf::from(&expanded);
        let target = if target.is_relative() {
            self.tree.root_path().join(target)
        } else {
            target
        };
        if !target.is_dir() || std::env::set_current_dir(&target).is_err() {
            self.notice = Some(format!("not a folder: {raw}"));
            return;
        }
        let root = std::env::current_dir().unwrap_or(target);
        if manual && self.sidebar_state.follow_cwd {
            self.cwd_follower.borrow_mut().mark_manual_folder();
        }
        let cwd_follower = std::rc::Rc::clone(&self.cwd_follower);
        *self = App::new(root, cwd_follower);
        // Only confirm an explicit folder change; an automatic cwd-follow
        // re-root must not pop an unprompted "folder: …" notice.
        if manual {
            self.notice = Some(format!("folder: {}", self.tree.root_name()));
        }
    }

    fn confirm_prompt(&mut self) {
        let Some(Overlay::Prompt { input, kind, .. }) = self.overlay.take() else {
            return;
        };
        // Folder changes take a full PATH — they skip the name validation.
        if matches!(kind, PromptKind::ChangeFolder) {
            self.change_folder(&input);
            return;
        }
        if matches!(kind, PromptKind::CustomEditor) {
            if sidebar::save_editor_command(&input) {
                self.notice = Some("custom editor saved; Enter still previews files".into());
            } else {
                self.notice = Some("custom editor command must be one non-empty line".into());
            }
            return;
        }
        let Some(name) = actions::validate_name(&input) else {
            self.notice = Some("invalid name".into());
            return;
        };
        let result = match &kind {
            PromptKind::NewFile(dir) => actions::create_file(dir, name),
            PromptKind::NewFolder(dir) => actions::create_folder(dir, name),
            PromptKind::Rename(path) => actions::rename(path, name),
            PromptKind::ChangeFolder => unreachable!("handled above"),
            PromptKind::CustomEditor => unreachable!("handled above"),
        };
        match result {
            Ok(created) => {
                if let PromptKind::NewFile(dir) | PromptKind::NewFolder(dir) = &kind {
                    self.tree.expand(dir);
                }
                self.refresh_tree();
                if let Some(index) = self.rows.iter().position(|r| r.path == created) {
                    self.select(index);
                }
            }
            Err(err) => self.notice = Some(format!("failed: {err}")),
        }
    }

    /// "Stage Changes" (issue #20): `git add` everything under `path` that
    /// belongs to the repository that OWNS it. The owner is the nearest
    /// enclosing repo, so staging inside a nested checkout stages there — and
    /// staging a parent directory never reaches across the boundary into it
    /// (see [`Git::stage_under`]). Decorations refresh either way, so a failed
    /// stage cannot leave the tree showing something stale; the Source Control
    /// view picks the new index up on its own poll.
    fn stage(&mut self, path: &Path) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let result = Git::owner_of(path)
            .and_then(|repo| repo.stage_under(path).map(|done| (done, repo.name())));
        self.notice = Some(match result {
            // A stage that skipped everything needs saying WHY, or the
            // boundary rule reads as a silent no-op.
            Ok((done, _)) if done.count == 0 && done.skipped_nested > 0 => format!(
                "{name}: nothing staged — {} path(s) belong to a nested repo",
                done.skipped_nested
            ),
            Ok((done, _)) if done.count == 0 => format!("nothing to stage in {name}"),
            Ok((done, repo)) if done.count == 1 => format!("staged {name} in {repo}"),
            Ok((done, repo)) => {
                format!("staged {} paths under {name} in {repo}", done.count)
            }
            Err(err) => format!("stage failed: {err}"),
        });
        self.request_decorations(true);
    }

    fn refresh_tree(&mut self) {
        self.tree.refresh();
        self.invalidate_quick_index();
        self.rediscover_repos();
        self.request_decorations(true);
        self.rebuild();
    }

    /// The visible row index at a pane-local mouse row, if it lands on one.
    fn row_at(&self, mouse_row: u16) -> Option<usize> {
        row_index_at(self.body, self.rows.len(), mouse_row)
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected?)
    }

    fn select(&mut self, index: usize) {
        if !self.rows.is_empty() {
            self.selected = Some(index.min(self.rows.len() - 1));
            self.snap = true;
            self.persist_tree();
        }
    }

    fn clear_selection(&mut self) {
        if self.selected.take().is_some() {
            self.hovered = None;
            self.last_click = None;
            self.persist_tree();
        }
    }

    /// Publish this root's tree shape and selection. Other same-root sidebars
    /// adopt it during their next idle tick.
    fn persist_tree(&self) {
        sidebar::save_tree_state(
            &self.tree.root_path(),
            &sidebar::TreeState {
                expanded: self.tree.expanded_paths(),
                selected: self.selected_row().map(|r| r.path.clone()),
            },
        );
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        // First keyboard step on a selection-less list picks the first row.
        let Some(current) = self.selected else {
            self.select(0);
            return;
        };
        let next = (current as isize + delta).clamp(0, self.rows.len().saturating_sub(1) as isize);
        self.select(next as usize);
    }

    /// Wheel: move the VIEW only — the selection stays where it is.
    fn scroll_view(&mut self, delta: isize) {
        let max = self.rows.len().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    /// Right/l: expand a collapsed directory, step into an expanded one.
    fn expand_or_enter(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if row.expanded {
            // First child, if any, sits directly below at depth + 1.
            let index = self.selected.unwrap_or(0);
            if self
                .rows
                .get(index + 1)
                .is_some_and(|next| next.depth == row.depth + 1)
            {
                self.select(index + 1);
            }
        } else {
            let path = row.path.clone();
            self.tree.expand(&path);
            self.rebuild();
        }
    }

    /// Left/h: collapse an expanded directory, otherwise jump to the parent row.
    fn collapse_or_parent(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.is_dir && row.expanded {
            let path = row.path.clone();
            self.tree.collapse(&path);
            self.rebuild();
            return;
        }
        let index = self.selected.unwrap_or(0);
        let depth = row.depth;
        if depth == 0 {
            return;
        }
        if let Some(parent) = self.rows[..index]
            .iter()
            .rposition(|r| r.depth == depth - 1)
        {
            self.select(parent);
        }
    }

    fn toggle(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let path = row.path.clone();
        if !row.is_dir {
            // Enter on a file opens the zoomed preview, like clicking it.
            self.open_preview(&path);
            return;
        }
        self.tree.toggle(&path);
        self.rebuild();
    }

    /// Recompute visible rows, keeping the selection on the same path when it
    /// still exists (else the nearest valid index).
    fn rebuild(&mut self) {
        self.hovered = None;
        rebuild_tree_rows(
            &mut self.tree,
            &mut self.rows,
            &mut self.selected,
            &mut self.scroll,
        );
        self.persist_tree();
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.last_width = frame.area().width;
        self.last_height = frame.area().height;
        // No own border/title: herdr already frames the pane and titles it with
        // the pane label ("Explorer"/"Sidebar") — a second border read as a
        // double frame.
        let search_active = matches!(self.overlay, Some(Overlay::ContentSearch { .. }));
        let footer_height = if search_active {
            1
        } else {
            self.footer_height(frame.area().width)
        };
        // A breathing row above and below the icons keeps the activity bar
        // from crowding the pane border.
        let activity_height = if self.merged() { 3 } else { 0 };
        let [activity, header, body, footer] = Layout::vertical([
            Constraint::Length(activity_height),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(footer_height),
        ])
        .areas(frame.area());
        self.page = body.height.saturating_sub(1).max(1) as usize;

        if self.merged() {
            self.draw_activity_bar(frame, activity);
        }
        if search_active {
            let search_area = Rect::new(
                header.x,
                header.y,
                header.width,
                header.height.saturating_add(body.height),
            );
            self.draw_content_search(frame, search_area);
            let [hint, button] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(3)]).areas(footer);
            self.git_footer_zones = FooterZones::default();
            if self.sidebar_state.show_git_footer
                && let Some(status) = &self.git_footer_status
            {
                self.git_footer_zones = draw_git_footer(
                    frame,
                    hint,
                    self.theme,
                    status,
                    self.git_syncing.is_some(),
                    self.mouse_pos,
                );
            }
            frame.render_widget(
                Paragraph::new("«".bold().fg(palette().header_accent)).alignment(Alignment::Center),
                button,
            );
            self.body = BodyGeom::default();
            return;
        }
        self.draw_header(frame, header);

        if self.rows.is_empty() {
            frame.render_widget(Paragraph::new("  (empty)".dim().italic()), body);
        } else {
            let h = (body.height as usize).max(1);
            self.scroll = self.scroll.min(self.rows.len().saturating_sub(h));
            if self.snap {
                if let Some(sel) = self.selected {
                    if sel < self.scroll {
                        self.scroll = sel;
                    } else if sel >= self.scroll + h {
                        self.scroll = sel + 1 - h;
                    }
                }
                self.snap = false;
            }
            let theme = self.theme;
            let hovered = self.hovered;
            let selected = self.selected;
            let items: Vec<ListItem> = self
                .rows
                .iter()
                .enumerate()
                .skip(self.scroll)
                .take(h)
                .map(|(i, r)| {
                    row_item(
                        r,
                        theme,
                        hovered == Some(i),
                        selected == Some(i),
                        self.row_deco(r),
                        body.width,
                    )
                })
                .collect();
            frame.render_widget(List::new(items), body);
            draw_scrollbar(frame, body, self.rows.len(), h, self.scroll);
        }
        self.body = BodyGeom {
            top: body.y,
            height: body.height,
            offset: self.scroll,
        };

        // Collapse button at the bottom-right of the LAST footer line,
        // mirroring herdr's own sidebar. hits_collapse_button targets the
        // pane's bottom row, which is exactly that line.
        let last_line = Rect::new(
            footer.x,
            footer.y + footer.height.saturating_sub(1),
            footer.width,
            1,
        );
        let [_, footer_button] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(3)]).areas(last_line);
        frame.render_widget(
            Paragraph::new("«".bold().fg(palette().header_accent)).alignment(Alignment::Center),
            footer_button,
        );
        let footer_lines: Vec<Line> = if let Some((msg, color)) = self.footer_message() {
            wrap_footer_message(&msg, footer.width, 4)
                .into_iter()
                .map(|l| l.fg(color).into())
                .collect()
        } else {
            match &self.overlay {
                Some(Overlay::Prompt { title, input, .. }) => {
                    // One line, always: drop the hint when narrow, and show
                    // the TAIL of a long input so the cursor stays visible.
                    let head = format!(" {title}: ");
                    let hint = "  (⏎ ok · esc cancel)";
                    let fixed = Span::raw(head.as_str()).width() + 1 + 4;
                    let width = usize::from(footer.width);
                    let hint_fits =
                        fixed + Span::raw(hint).width() + Span::raw(input.as_str()).width()
                            <= width;
                    let avail = width
                        .saturating_sub(fixed)
                        .saturating_sub(if hint_fits {
                            Span::raw(hint).width()
                        } else {
                            0
                        })
                        .max(4);
                    let mut spans = vec![
                        Span::styled(head, Style::default().bold()),
                        Span::raw(input_tail(input, avail)),
                        Span::styled("█", Style::default().dim()),
                    ];
                    if hint_fits {
                        spans.push(Span::styled(hint, Style::default().dim()));
                    }
                    vec![Line::from(spans)]
                }
                _ if self.show_hotkeys() => wrap_hints(&self.hints(), frame.area().width, 3),
                _ => Vec::new(),
            }
        };
        let git_footer = self.sidebar_state.show_git_footer && self.git_footer_status.is_some();
        let menu_hint = git_footer && footer_lines.is_empty();
        let footer_empty = footer_lines.is_empty();
        let content_height = footer.height.saturating_sub(u16::from(git_footer));
        let footer_content = Rect::new(footer.x, footer.y, footer.width, content_height);
        frame.render_widget(Paragraph::new(footer_lines), footer_content);
        if menu_hint {
            frame.render_widget(
                Paragraph::new("m / ctrl+rclick for menus")
                    .style(Style::default().fg(Color::DarkGray))
                    .alignment(Alignment::Right),
                footer_content,
            );
        }
        self.git_footer_zones = FooterZones::default();
        if git_footer {
            let status_area = Rect::new(
                last_line.x,
                last_line.y,
                last_line.width.saturating_sub(3),
                1,
            );
            if let Some(status) = &self.git_footer_status {
                self.git_footer_zones = draw_git_footer(
                    frame,
                    status_area,
                    self.theme,
                    status,
                    self.git_syncing.is_some(),
                    self.mouse_pos,
                );
            }
        } else if footer_empty {
            let hint_area = Rect::new(
                last_line.x,
                last_line.y,
                last_line.width.saturating_sub(3),
                1,
            );
            frame.render_widget(
                Paragraph::new(" m / ctrl+rclick: menu".dim().italic()),
                hint_area,
            );
        }

        match self.overlay {
            Some(Overlay::BranchPicker(_)) => {
                if let Some(Overlay::BranchPicker(picker)) = self.overlay.as_mut() {
                    picker.draw(frame);
                }
            }
            Some(Overlay::Menu { .. }) => self.draw_menu(frame),
            Some(Overlay::Settings { .. }) => self.draw_settings(frame),
            Some(Overlay::QuickOpen { .. }) => self.draw_quick_open(frame),
            _ => {}
        }
    }

    /// The workspace-name header (the root folder's name, uppercase like VS
    /// Code); standalone mode puts the ⚙ at its right edge (unified mode's ⚙
    /// lives in the activity bar instead), and the hover title-action buttons
    /// sit just left of it.
    fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
        let gear = (!self.merged()).then(|| {
            Span::styled(
                format!("{} ", gear_icon(self.theme)),
                Style::default().dim(),
            )
        });
        let gear_w = gear.as_ref().map(Span::width).unwrap_or(0) as u16;
        self.title_zones.clear();
        let (action_spans, actions_w) = if title_actions_visible(self.last_mouse) {
            let actions = [
                TitleAction::NewFile,
                TitleAction::NewFolder,
                TitleAction::Refresh,
                TitleAction::CollapseAll,
            ];
            let w = title_actions_width(self.theme, &actions);
            let ax = area.x + area.width.saturating_sub(gear_w + w);
            let (spans, zones) =
                title_action_spans(self.theme, &actions, ax, area.y, self.mouse_pos);
            self.title_zones = zones;
            (spans, w)
        } else {
            (Vec::new(), 0)
        };
        // The name yields to the buttons and gear in narrow panes.
        let avail = usize::from(area.width.saturating_sub(gear_w + actions_w));
        let root_label = truncate_to(format!(" {}", self.tree.root_name().to_uppercase()), avail);
        let name = Span::styled(
            root_label,
            Style::default().bold().fg(palette().header_accent),
        );
        let pad = usize::from(area.width)
            .saturating_sub(name.width() + usize::from(actions_w) + usize::from(gear_w));
        let mut spans = vec![name, Span::raw(" ".repeat(pad))];
        spans.extend(action_spans);
        if let Some(gear) = gear {
            let gx = area.x + area.width.saturating_sub(gear_w);
            self.gear = Rect::new(gx, area.y, gear_w, 1);
            spans.push(gear);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Switch icon themes and REMEMBER it — an auto-detected theme that
    /// guessed wrong (font installed but not selected, or vice versa) must
    /// stay corrected across restarts.
    fn set_theme(&mut self, theme: IconTheme) {
        self.theme = theme;
        self.sidebar_state = sidebar::update_state(|state| state.icons = Some(theme));
    }

    /// The persisted "show hotkeys in the footer" setting.
    fn show_hotkeys(&self) -> bool {
        self.sidebar_state.show_hotkeys
    }

    /// Esc: close the preview pane in this tab, if one is open.
    fn close_preview(&mut self) {
        if let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone())
            && let Some(notice) = herdr_sidebar::viewer::close_in_tab(&pane_id)
        {
            self.notice = Some(notice);
        }
    }

    /// The hotkey hints for the current mode.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let mut hints = vec![
            ("↑↓", "move"),
            ("←→", "fold"),
            ("⏎", "toggle"),
            ("r", "refresh"),
            (".", "dotfiles"),
            ("c", "folder"),
            ("ctrl+p", "files"),
            ("ctrl+f", "text"),
            ("m", "menu"),
            ("s", "settings"),
            ("b", "hide"),
            ("q", "quit"),
        ];
        if self.merged() {
            hints.extend([("1", "files"), ("2", "search"), ("3", "git")]);
        }
        hints
    }

    /// Rows the footer needs at `width`: notices and confirms WRAP in narrow
    /// panes (a one-line assumption used to clip "Delete '…' permanently?
    /// (y/N)" mid-question); the name prompt stays one line (its input
    /// shrinks instead); hints wrap as before.
    fn footer_height(&self, width: u16) -> u16 {
        let git_footer = self.sidebar_state.show_git_footer && self.git_footer_status.is_some();
        let content = if let Some((msg, _)) = self.footer_message() {
            wrap_footer_message(&msg, width, 4).len() as u16
        } else if matches!(self.overlay, Some(Overlay::Prompt { .. })) {
            1
        } else if self.show_hotkeys() {
            wrap_hints(&self.hints(), width, 3).len() as u16
        } else if git_footer {
            2
        } else {
            0
        };
        (content + u16::from(git_footer)).max(1)
    }

    /// The uniform-style footer message, if one is active: a notice, or the
    /// delete confirm. Shared by footer_height and draw so they agree.
    fn footer_message(&self) -> Option<(String, Color)> {
        if let Some(notice) = &self.notice {
            return Some((notice.clone(), palette().modified));
        }
        if let Some(Overlay::ConfirmDelete { path, .. }) = &self.overlay {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Some((
                format!("Delete '{name}' permanently? (y/N)"),
                palette().deleted,
            ));
        }
        None
    }

    /// The VS Code activity bar: view-switcher icons plus a detach button.
    /// The area is three rows tall; the outer rows stay in the pane
    /// background, and only the ACTIVE icon's highlight chip extends into
    /// them by a half block — a tall button with built-in breathing room,
    /// no strip container.
    fn draw_activity_bar(&mut self, frame: &mut Frame, area: Rect) {
        let outer_top = area.y;
        let outer_bottom = area.y + 2;
        let area = Rect::new(area.x, area.y + 1, area.width, 1);
        let (exp_icon, search_icon, git_icon) = activity_icons(self.theme);
        let search_active = matches!(self.overlay, Some(Overlay::ContentSearch { .. }));
        // Both FA glyphs (folder, code-fork) render two cells wide in the
        // non-Mono Nerd Font; reserve the second cell in each chip so the
        // highlights are equal-sized with centered icons.
        let slack = if self.theme == IconTheme::Material {
            " "
        } else {
            ""
        };
        let mut spans = [
            Span::raw(" "),
            Span::raw(format!(" {exp_icon}{slack} ")),
            Span::raw(" "),
            Span::raw(format!(" {search_icon}{slack} ")),
            Span::raw(" "),
            Span::raw(format!(" {git_icon}{slack} ")),
        ];
        // Hit zones from the actual span widths (emoji vs nerd-glyph widths differ).
        let mut x = area.x;
        let mut bounds = Vec::new();
        for span in &spans {
            let w = span.width() as u16;
            bounds.push((x, x + w));
            x += w;
        }
        self.activity = ActivityZones {
            row: area.y,
            explorer: bounds[1],
            search: bounds[3],
            source_control: bounds[5],
        };
        let hovered = |bounds| {
            self.mouse_pos
                .is_some_and(|(x, y)| hits_activity_button(bounds, area.y, x, y))
        };
        let explorer_hovered = hovered(bounds[1]);
        let search_hovered = hovered(bounds[3]);
        let git_hovered = hovered(bounds[5]);
        spans[1].style = activity_button_style(!search_active, explorer_hovered);
        spans[3].style = activity_button_style(search_active, search_hovered);
        spans[5].style = activity_button_style(false, git_hovered);
        let (chip_start, chip_end) = if search_active { bounds[3] } else { bounds[1] };
        draw_activity_caps(
            frame,
            (chip_start, chip_end),
            outer_top,
            outer_bottom,
            palette().selection_bg,
        );
        for (active, is_hovered, button_bounds) in [
            (!search_active, explorer_hovered, bounds[1]),
            (search_active, search_hovered, bounds[3]),
            (false, git_hovered, bounds[5]),
        ] {
            if !active && is_hovered {
                draw_activity_caps(
                    frame,
                    button_bounds,
                    outer_top,
                    outer_bottom,
                    palette().activity_hover_bg,
                );
            }
        }
        let gear_text = format!(" {} ", gear_icon(self.theme));
        let gear_w = Span::raw(gear_text.as_str()).width() as u16;
        let gear_x = area.x + area.width.saturating_sub(gear_w);
        self.gear = Rect::new(gear_x, outer_top, gear_w, 3);
        let gear_hovered = self.mouse_pos.is_some_and(|(x, y)| hits(self.gear, x, y));
        let gear = Span::styled(gear_text, activity_button_style(false, gear_hovered));
        if gear_hovered {
            draw_activity_caps(
                frame,
                (gear_x, gear_x + gear_w),
                outer_top,
                outer_bottom,
                palette().activity_hover_bg,
            );
        }

        let pad = usize::from(area.width)
            .saturating_sub(spans.iter().map(Span::width).sum::<usize>() + usize::from(gear_w));
        let mut line = spans.to_vec();
        line.push(Span::raw(" ".repeat(pad)));
        line.push(gear);
        frame.render_widget(Paragraph::new(Line::from(line)), area);
    }

    /// Render the context-menu popup near its anchor, clamped inside the pane,
    /// and remember its rect for mouse hit-testing.
    fn draw_menu(&mut self, frame: &mut Frame) {
        let Some(Overlay::Menu {
            x,
            y,
            entries,
            selected,
            rect,
            ..
        }) = self.overlay.as_mut()
        else {
            return;
        };
        let area = frame.area();
        let label_width = entries
            .iter()
            .map(|e| match e {
                MenuEntry::Action(_, label) => label.chars().count(),
                MenuEntry::Separator => 0,
            })
            .max()
            .unwrap_or(0) as u16;
        let width = (label_width + 4).min(area.width);
        let height = (entries.len() as u16 + 2).min(area.height);
        let px = (*x).min(area.width.saturating_sub(width));
        let py = (*y + 1).min(area.height.saturating_sub(height));
        let popup = Rect::new(px, py, width, height);
        *rect = popup;

        let items: Vec<ListItem> = entries
            .iter()
            .enumerate()
            .map(|(i, entry)| match entry {
                MenuEntry::Separator => {
                    ListItem::new(Line::from("─".repeat(usize::from(width - 2)).dim()))
                }
                MenuEntry::Action(_, label) => {
                    let line = Line::raw(format!(" {label}"));
                    if i == *selected {
                        ListItem::new(line).style(selection_style(true))
                    } else {
                        ListItem::new(line)
                    }
                }
            })
            .collect();
        frame.render_widget(Clear, popup);
        frame.render_widget(
            List::new(items)
                .block(ratatui::widgets::Block::bordered().border_style(Style::default().dim())),
            popup,
        );
    }

    fn draw_quick_open(&mut self, frame: &mut Frame) {
        let Some(Overlay::QuickOpen {
            query,
            files,
            matches,
            selected,
            truncated,
            loading,
        }) = self.overlay.as_ref()
        else {
            return;
        };
        let area = frame.area();
        let width = area.width.clamp(1, 82);
        let visible = usize::from(area.height.saturating_sub(5))
            .min(matches.len())
            .max(1);
        let height = (visible as u16 + 5).min(area.height).max(1);
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 4,
            width,
            height,
        );
        let inner = popup.inner(ratatui::layout::Margin::new(1, 1));
        let query_area = Rect::new(inner.x, inner.y, inner.width, inner.height.min(1));
        let list_area = Rect::new(
            inner.x,
            inner.y.saturating_add(2),
            inner.width,
            inner.height.saturating_sub(3),
        );
        let start = selected.saturating_sub(usize::from(list_area.height).saturating_sub(1));
        let items = matches
            .iter()
            .enumerate()
            .skip(start)
            .take(usize::from(list_area.height))
            .filter_map(|(match_index, file_index)| {
                let file = files.get(*file_index)?;
                let label =
                    truncate_path_tail(&file.label, usize::from(list_area.width).saturating_sub(1));
                let line = Line::raw(format!(" {label}"));
                Some(if match_index == *selected {
                    ListItem::new(line).style(selection_style(true))
                } else {
                    ListItem::new(line)
                })
            })
            .collect::<Vec<_>>();
        let status = if *loading {
            "indexing files…".to_string()
        } else if matches.is_empty() {
            "no matching files".to_string()
        } else if *truncated {
            format!(
                "{} matches · first {QUICK_OPEN_FILE_LIMIT} files indexed",
                matches.len()
            )
        } else {
            format!("{} matches", matches.len())
        };

        frame.render_widget(Clear, popup);
        frame.render_widget(
            ratatui::widgets::Block::bordered()
                .title(" Quick Open ")
                .border_style(Style::default().dim()),
            popup,
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" > ", Style::default().bold()),
                Span::raw(query),
                Span::styled("█", Style::default().dim()),
            ])),
            query_area,
        );
        frame.render_widget(List::new(items), list_area);
        if inner.height > 1 {
            frame.render_widget(
                Paragraph::new(status.dim()).alignment(Alignment::Right),
                Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
            );
        }
    }

    fn draw_content_search(&mut self, frame: &mut Frame, area: Rect) {
        let Some(Overlay::ContentSearch {
            query,
            replace,
            include,
            exclude,
            hits,
            selected,
            truncated,
            loading,
            searched,
            details_expanded,
            focus,
            options,
            error,
            ..
        }) = self.overlay.as_ref()
        else {
            return;
        };
        let query = query.clone();
        let replace = replace.clone();
        let include = include.clone();
        let exclude = exclude.clone();
        let hits = std::sync::Arc::clone(hits);
        let selected = *selected;
        let truncated = *truncated;
        let loading = *loading;
        let searched = *searched;
        let details_expanded = *details_expanded;
        let focus = *focus;
        let options = *options;
        let error = error.clone();
        let has_error = error.is_some();

        self.search_zones = SearchZones::default();
        self.search_result_rows.clear();
        if area.height == 0 || area.width == 0 {
            return;
        }

        let title = Rect::new(area.x, area.y, area.width, 1);
        let toolbar_width = 9.min(area.width);
        let toolbar_x = area.x + area.width.saturating_sub(toolbar_width);
        let (refresh_icon, clear_icon, details_icon) = search_toolbar_icons(self.theme);
        frame.render_widget(Paragraph::new(" Search").bold(), title);
        self.search_zones.refresh = Rect::new(toolbar_x, title.y, 3.min(toolbar_width), 1);
        self.search_zones.clear = Rect::new(toolbar_x.saturating_add(3), title.y, 3, 1);
        self.search_zones.details = Rect::new(toolbar_x.saturating_add(6), title.y, 3, 1);
        let hovered = |rect: Rect| {
            self.mouse_pos.is_some_and(|(x, y)| {
                x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
            })
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {refresh_icon} "),
                    chrome_button_style(hovered(self.search_zones.refresh)),
                ),
                Span::styled(
                    format!(" {clear_icon} "),
                    chrome_button_style(hovered(self.search_zones.clear)),
                ),
                Span::styled(
                    format!(" {details_icon} "),
                    chrome_button_style(hovered(self.search_zones.details)),
                ),
            ]))
            .alignment(Alignment::Right),
            Rect::new(toolbar_x, title.y, toolbar_width, 1),
        );

        let query_y = area.y.saturating_add(2);
        let query_box = Rect::new(
            area.x.saturating_add(2),
            query_y,
            area.width.saturating_sub(3),
            3,
        );
        let option_width = query_box.width.min(9);
        let option_x = query_box.x
            + query_box
                .width
                .saturating_sub(option_width.saturating_add(1));
        let query_border = if focus == SearchFocus::Query {
            Style::default().fg(palette().accent_focus)
        } else {
            Style::default().dim()
        };
        frame.render_widget(Block::bordered().border_style(query_border), query_box);
        let query_inner = Rect::new(
            query_box.x.saturating_add(1),
            query_box.y.saturating_add(1),
            query_box
                .width
                .saturating_sub(option_width.saturating_add(2)),
            1,
        );
        self.search_zones.query = query_inner;
        frame.render_widget(
            Paragraph::new(search_input_line(
                &query,
                "Search",
                focus == SearchFocus::Query,
                query_inner.width,
            )),
            query_inner,
        );

        let option_style = |active: bool| {
            if active {
                selection_style(true)
            } else {
                Style::default().dim()
            }
        };
        let options_area = Rect::new(option_x, query_box.y.saturating_add(1), option_width, 1);
        let option_spans = [
            Span::styled("Aa ", option_style(options.match_case)),
            Span::styled("ab ", option_style(options.whole_word)),
            Span::styled(".* ", option_style(options.regex)),
        ];
        let mut option_cursor = options_area.x;
        let mut option_bounds = Vec::new();
        for span in &option_spans {
            let width = span.width() as u16;
            option_bounds.push(Rect::new(option_cursor, options_area.y, width, 1));
            option_cursor = option_cursor.saturating_add(width);
        }
        self.search_zones.match_case = option_bounds[0];
        self.search_zones.whole_word = option_bounds[1];
        self.search_zones.regex = option_bounds[2];
        frame.render_widget(
            Paragraph::new(Line::from(option_spans.to_vec())),
            options_area,
        );

        let replace_y = query_box.y.saturating_add(query_box.height);
        let replace_box = Rect::new(
            area.x.saturating_add(2),
            replace_y,
            area.width.saturating_sub(3),
            3,
        );
        let replace_border = if focus == SearchFocus::Replace {
            Style::default().fg(palette().accent_focus)
        } else {
            Style::default().dim()
        };
        frame.render_widget(Block::bordered().border_style(replace_border), replace_box);
        let replace_inner = Rect::new(
            replace_box.x.saturating_add(1),
            replace_box.y.saturating_add(1),
            replace_box.width.saturating_sub(2),
            1,
        );
        self.search_zones.replace = replace_inner;
        frame.render_widget(
            Paragraph::new(search_input_line(
                &replace,
                "Replace",
                focus == SearchFocus::Replace,
                replace_inner.width,
            )),
            replace_inner,
        );
        let mut results_y = replace_box
            .y
            .saturating_add(replace_box.height)
            .saturating_add(1);

        if details_expanded {
            let include_label = Rect::new(
                area.x.saturating_add(2),
                results_y,
                area.width.saturating_sub(3),
                1,
            );
            frame.render_widget(Paragraph::new("files to include").dim(), include_label);
            let include_box = Rect::new(
                area.x.saturating_add(2),
                results_y.saturating_add(1),
                area.width.saturating_sub(3),
                3,
            );
            frame.render_widget(
                Block::bordered().border_style(if focus == SearchFocus::Include {
                    Style::default().fg(palette().accent_focus)
                } else {
                    Style::default().dim()
                }),
                include_box,
            );
            let include_inner = Rect::new(
                include_box.x.saturating_add(1),
                include_box.y.saturating_add(1),
                include_box.width.saturating_sub(2),
                1,
            );
            self.search_zones.include = include_inner;
            frame.render_widget(
                Paragraph::new(search_input_line(
                    &include,
                    "e.g. *.ts, src/**/include",
                    focus == SearchFocus::Include,
                    include_inner.width,
                )),
                include_inner,
            );

            let exclude_label_y = include_box.y.saturating_add(include_box.height);
            let exclude_label = Rect::new(
                area.x.saturating_add(2),
                exclude_label_y,
                area.width.saturating_sub(3),
                1,
            );
            frame.render_widget(Paragraph::new("files to exclude").dim(), exclude_label);
            let exclude_box = Rect::new(
                area.x.saturating_add(2),
                exclude_label_y.saturating_add(1),
                area.width.saturating_sub(3),
                3,
            );
            frame.render_widget(
                Block::bordered().border_style(if focus == SearchFocus::Exclude {
                    Style::default().fg(palette().accent_focus)
                } else {
                    Style::default().dim()
                }),
                exclude_box,
            );
            let exclude_inner = Rect::new(
                exclude_box.x.saturating_add(1),
                exclude_box.y.saturating_add(1),
                exclude_box.width.saturating_sub(2),
                1,
            );
            self.search_zones.exclude = exclude_inner;
            frame.render_widget(
                Paragraph::new(search_input_line(
                    &exclude,
                    "e.g. node_modules, **/*.min.js",
                    focus == SearchFocus::Exclude,
                    exclude_inner.width,
                )),
                exclude_inner,
            );
            results_y = exclude_box
                .y
                .saturating_add(exclude_box.height)
                .saturating_add(1);
        }

        let status = if let Some(error) = error {
            Some(error)
        } else if loading {
            Some("Searching…".to_string())
        } else if !searched {
            None
        } else if hits.is_empty() {
            Some("No results found".to_string())
        } else if truncated {
            Some(format!("{} results · result limit reached", hits.len()))
        } else {
            Some(format!("{} results", hits.len()))
        };
        if results_y >= area.y.saturating_add(area.height) {
            return;
        }
        let list_y = if let Some(status) = status {
            let status_area = Rect::new(
                area.x.saturating_add(1),
                results_y,
                area.width.saturating_sub(2),
                1,
            );
            frame.render_widget(
                Paragraph::new(status).style(if has_error {
                    Style::default().fg(palette().deleted)
                } else {
                    Style::default().dim()
                }),
                status_area,
            );
            results_y.saturating_add(1)
        } else {
            results_y
        };
        let list_area = Rect::new(
            area.x,
            list_y,
            area.width,
            area.y.saturating_add(area.height).saturating_sub(list_y),
        );
        let mut rows: Vec<(Option<usize>, Line<'static>)> = Vec::new();
        let mut hit_index = 0;
        while hit_index < hits.len() {
            let label = &hits[hit_index].label;
            let mut end = hit_index + 1;
            while end < hits.len() && hits[end].label == *label {
                end += 1;
            }
            let count = end - hit_index;
            let count_text = format!(" {count}");
            let label_width =
                usize::from(list_area.width).saturating_sub(2 + Span::raw(&count_text).width());
            rows.push((
                None,
                Line::from(vec![
                    Span::styled("⌄ ", Style::default().dim()),
                    Span::styled(
                        truncate_path_tail(label, label_width),
                        Style::default().bold(),
                    ),
                    Span::styled(count_text, Style::default().dim()),
                ]),
            ));
            for index in hit_index..end {
                let hit = &hits[index];
                let prefix = format!("   {}: ", hit.line);
                let context_width =
                    usize::from(list_area.width).saturating_sub(Span::raw(&prefix).width());
                let mut spans = vec![Span::styled(
                    prefix,
                    Style::default().fg(palette().accent_focus),
                )];
                spans.extend(highlighted_search_context(
                    &hit.context,
                    &hit.matches,
                    context_width,
                    Style::default().fg(palette().header_accent).bold(),
                ));
                rows.push((Some(index), Line::from(spans)));
            }
            hit_index = end;
        }
        let selected_row = rows
            .iter()
            .position(|(index, _)| *index == Some(selected))
            .unwrap_or(0);
        let viewport = usize::from(list_area.height);
        let max_start = rows.len().saturating_sub(viewport);
        let mut start = if self.search_snap {
            selected_row.saturating_sub(viewport / 2).min(max_start)
        } else {
            self.search_scroll.min(max_start)
        };
        if self.search_snap {
            while start > 0 && rows[start].0.is_some() {
                start -= 1;
            }
        }
        self.search_scroll = start;
        self.search_snap = false;
        for (row_offset, (index, line)) in rows.into_iter().skip(start).take(viewport).enumerate() {
            let row_area = Rect::new(
                list_area.x,
                list_area.y + row_offset as u16,
                list_area.width,
                1,
            );
            if let Some(index) = index {
                self.search_result_rows.push((row_area, index));
                frame.render_widget(
                    Paragraph::new(line).style(if index == selected {
                        selection_style(focus == SearchFocus::Results)
                    } else {
                        Style::default()
                    }),
                    row_area,
                );
            } else {
                frame.render_widget(Paragraph::new(line), row_area);
            }
        }
    }
}

fn search_input_line(value: &str, placeholder: &str, focused: bool, width: u16) -> Line<'static> {
    let available = usize::from(width).saturating_sub(usize::from(focused));
    let mut spans = if value.is_empty() && !focused {
        vec![Span::styled(
            truncate_to(placeholder.to_string(), available),
            Style::default().dim(),
        )]
    } else if value.is_empty() {
        Vec::new()
    } else {
        vec![Span::raw(input_tail(value, available))]
    };
    if focused {
        spans.push(Span::raw("█"));
    }
    Line::from(spans)
}

fn search_toolbar_icons(theme: IconTheme) -> (&'static str, &'static str, &'static str) {
    match theme {
        IconTheme::Material => (
            title_action_icon(theme, TitleAction::Refresh),
            "\u{eabf}",
            "\u{ea7c}",
        ),
        IconTheme::Emoji => (title_action_icon(theme, TitleAction::Refresh), "×", "⋯"),
    }
}

fn highlighted_search_context(
    context: &str,
    matches: &[(usize, usize)],
    max_width: usize,
    match_style: Style,
) -> Vec<Span<'static>> {
    let context_width = Span::raw(context).width();
    let (visible_end, truncated) = if context_width <= max_width {
        (context.len(), false)
    } else if max_width < 2 {
        (0, false)
    } else {
        let mut width = 0;
        let mut end = 0;
        for (index, character) in context.char_indices() {
            let character_width = character.width().unwrap_or(0);
            if width + character_width + 1 > max_width {
                break;
            }
            width += character_width;
            end = index + character.len_utf8();
        }
        (end, true)
    };
    let mut spans = Vec::new();
    let mut cursor = 0;
    for &(start, end) in matches {
        if start >= visible_end {
            break;
        }
        let start = start.max(cursor);
        let end = end.min(visible_end);
        if start >= end {
            continue;
        }
        if start > cursor {
            spans.push(Span::raw(context[cursor..start].to_string()));
        }
        spans.push(Span::styled(context[start..end].to_string(), match_style));
        cursor = end;
    }
    if cursor < visible_end {
        spans.push(Span::raw(context[cursor..visible_end].to_string()));
    }
    if truncated {
        spans.push(Span::raw("…"));
    }
    spans
}

const QUICK_OPEN_FILE_LIMIT: usize = 20_000;
const CONTENT_SEARCH_MATCH_LIMIT: usize = 1_000;
const CONTENT_SEARCH_FILE_LIMIT: usize = 20_000;
const CONTENT_SEARCH_MAX_BYTES: u64 = 1024 * 1024;

fn collect_quick_files(root: &Path, show_hidden: bool, limit: usize) -> (Vec<QuickFile>, bool) {
    let mut files = Vec::new();
    let mut truncated = false;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(!show_hidden)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    for entry in builder.build().flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.into_path();
        let label = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let label_lower = label.to_lowercase();
        files.push(QuickFile {
            path,
            label,
            label_lower,
        });
        if files.len() >= limit {
            truncated = true;
            break;
        }
    }
    files.sort_by(|left, right| left.label_lower.cmp(&right.label_lower));
    (files, truncated)
}

fn collect_content_hits(
    root: &Path,
    show_hidden: bool,
    query: &str,
    include: &str,
    exclude: &str,
    options: SearchOptions,
    limit: usize,
) -> Result<(Vec<ContentHit>, bool), String> {
    if query.is_empty() || limit == 0 {
        return Ok((Vec::new(), false));
    }
    let pattern = if options.regex {
        let pattern = if options.whole_word {
            format!(r"\b(?:{query})\b")
        } else {
            query.to_string()
        };
        Some(
            RegexBuilder::new(&pattern)
                .case_insensitive(!options.match_case)
                .build()
                .map_err(|error| format!("Invalid regular expression: {error}"))?,
        )
    } else {
        None
    };
    let includes = build_search_globs(include, "include")?;
    let excludes = build_search_globs(exclude, "exclude")?;
    let (files, file_limit_reached) =
        collect_quick_files(root, show_hidden, CONTENT_SEARCH_FILE_LIMIT);
    let mut hits = Vec::new();
    for file in files {
        if includes
            .as_ref()
            .is_some_and(|patterns| !patterns.is_match(&file.label))
            || excludes
                .as_ref()
                .is_some_and(|patterns| patterns.is_match(&file.label))
        {
            continue;
        }
        if std::fs::metadata(&file.path)
            .map(|metadata| metadata.len() > CONTENT_SEARCH_MAX_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&file.path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            let ranges = content_line_match_ranges(line, query, options, pattern.as_ref());
            let matched = pattern
                .as_ref()
                .is_some_and(|pattern| pattern.is_match(line))
                || !ranges.is_empty();
            if !matched {
                continue;
            }
            let (context, matches) = content_hit_context(line, &ranges);
            hits.push(ContentHit {
                path: file.path.clone(),
                label: file.label.clone(),
                line: index + 1,
                context,
                matches,
            });
            if hits.len() >= limit {
                return Ok((hits, true));
            }
        }
    }
    Ok((hits, file_limit_reached))
}

fn build_search_globs(raw: &str, label: &str) -> Result<Option<GlobSet>, String> {
    let patterns = raw
        .split(',')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .collect::<Vec<_>>();
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).map_err(|error| format!("Invalid {label} pattern: {error}"))?;
        builder.add(glob);
        if !pattern.contains(['*', '?', '[', '{']) {
            let directory = pattern.trim_end_matches(['/', '\\']);
            let descendant = format!("{directory}/**");
            builder.add(
                Glob::new(&descendant)
                    .map_err(|error| format!("Invalid {label} pattern: {error}"))?,
            );
            if !directory.contains(['/', '\\']) {
                let nested_descendant = format!("**/{directory}/**");
                builder.add(
                    Glob::new(&nested_descendant)
                        .map_err(|error| format!("Invalid {label} pattern: {error}"))?,
                );
            }
        }
    }
    builder
        .build()
        .map(Some)
        .map_err(|error| format!("Invalid {label} pattern: {error}"))
}

#[cfg(test)]
fn content_line_matches(
    line: &str,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
) -> bool {
    if let Some(pattern) = pattern {
        return pattern.is_match(line);
    }
    !literal_match_ranges(line, query, options).is_empty()
}

fn content_line_match_ranges(
    line: &str,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
) -> Vec<(usize, usize)> {
    if let Some(pattern) = pattern {
        return pattern
            .find_iter(line)
            .filter(|found| !found.is_empty())
            .map(|found| (found.start(), found.end()))
            .collect();
    }
    literal_match_ranges(line, query, options)
}

fn literal_match_ranges(line: &str, query: &str, options: SearchOptions) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    if options.match_case {
        return line
            .match_indices(query)
            .filter_map(|(start, matched)| {
                let end = start + matched.len();
                word_boundary_matches(line, start, end, options.whole_word).then_some((start, end))
            })
            .collect();
    }

    let lowered = line.to_lowercase();
    let mut source_by_lowered_character = Vec::new();
    for (source_start, character) in line.char_indices() {
        let source_range = (source_start, source_start + character.len_utf8());
        source_by_lowered_character.extend(std::iter::repeat_n(
            source_range,
            character.to_lowercase().count(),
        ));
    }
    let mut lowered_boundaries = lowered
        .char_indices()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    lowered_boundaries.push(lowered.len());
    if source_by_lowered_character.len() + 1 != lowered_boundaries.len() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let mut ranges = Vec::new();
    for (lowered_start, matched) in lowered.match_indices(&needle) {
        let lowered_end = lowered_start + matched.len();
        if !word_boundary_matches(&lowered, lowered_start, lowered_end, options.whole_word) {
            continue;
        }
        let Ok(first) = lowered_boundaries.binary_search(&lowered_start) else {
            continue;
        };
        let Ok(after_last) = lowered_boundaries.binary_search(&lowered_end) else {
            continue;
        };
        if first >= after_last {
            continue;
        }
        let range = (
            source_by_lowered_character[first].0,
            source_by_lowered_character[after_last - 1].1,
        );
        if ranges.last() != Some(&range) {
            ranges.push(range);
        }
    }
    ranges
}

fn word_boundary_matches(haystack: &str, start: usize, end: usize, whole_word: bool) -> bool {
    if !whole_word {
        return true;
    }
    let before = haystack[..start].chars().next_back();
    let after = haystack[end..].chars().next();
    !before.is_some_and(search_word_char) && !after.is_some_and(search_word_char)
}

fn content_hit_context(line: &str, ranges: &[(usize, usize)]) -> (String, Vec<(usize, usize)>) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return (String::new(), Vec::new());
    }
    let start = line.len() - line.trim_start().len();
    let end = start + trimmed.len();
    let ranges = ranges
        .iter()
        .filter_map(|&(match_start, match_end)| {
            if match_start >= end || match_end <= start {
                return None;
            }
            let match_start = match_start.max(start);
            let match_end = match_end.min(end);
            Some((match_start - start, match_end - start))
        })
        .collect();
    (trimmed.replace('\t', " "), ranges)
}

fn search_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn quick_matches(files: &[QuickFile], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..files.len()).collect();
    }
    let query_lower = query.to_lowercase();
    let mut ranked = files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| {
            fuzzy_score_lowercased(&query_lower, &file.label_lower).map(|score| (index, score))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|(left_index, left_score), (right_index, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| {
                files[*left_index]
                    .label
                    .len()
                    .cmp(&files[*right_index].label.len())
            })
            .then_with(|| files[*left_index].label.cmp(&files[*right_index].label))
    });
    ranked.into_iter().map(|(index, _)| index).collect()
}

fn fuzzy_score_lowercased(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let mut wanted = query.chars();
    let mut current = wanted.next()?;
    let mut score = 0i64;
    let mut previous_match = None;
    let mut previous_char = None;
    for (index, ch) in candidate.chars().enumerate() {
        if ch == current {
            score += 10;
            if previous_match == Some(index.saturating_sub(1)) {
                score += 8;
            }
            if index == 0
                || previous_char
                    .is_some_and(|before| matches!(before, '/' | '\\' | '-' | '_' | '.'))
            {
                score += 6;
            }
            previous_match = Some(index);
            match wanted.next() {
                Some(next) => current = next,
                None => return Some(score - candidate.chars().count() as i64),
            }
        }
        previous_char = Some(ch);
    }
    None
}

fn truncate_path_tail(label: &str, max: usize) -> String {
    let width = Span::raw(label).width();
    if width <= max {
        return label.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut used = 1;
    let mut reversed = Vec::new();
    for ch in label.chars().rev() {
        let char_width = ch.width().unwrap_or(0);
        if used + char_width > max {
            break;
        }
        used += char_width;
        reversed.push(ch);
    }
    let tail = reversed.into_iter().rev().collect::<String>();
    format!("…{tail}")
}

fn pane_focused_in(pane_list_json: &str, pane_id: &str) -> bool {
    let Ok(value) =
        serde_json::from_str::<serde_json::Value>(pane_list_json.trim_start_matches('\u{feff}'))
    else {
        return false;
    };
    value
        .get("result")
        .and_then(|result| result.get("panes"))
        .and_then(|panes| panes.as_array())
        .and_then(|panes| {
            panes
                .iter()
                .find(|pane| pane.get("pane_id").and_then(|id| id.as_str()) == Some(pane_id))
        })
        .and_then(|pane| pane.get("focused"))
        .and_then(|focused| focused.as_bool())
        .unwrap_or(false)
}

/// The right-aligned decoration for a status letter (issue #19): the letter
/// itself for a file, a filled dot for a directory whose DESCENDANTS changed
/// (VS Code's dirty-folder badge), and nothing at all for ignored paths —
/// those are conveyed by the dimmed name instead.
fn deco_marker(letter: char, is_dir: bool) -> Option<String> {
    match letter {
        'I' => None,
        _ if is_dir => Some("●".to_string()),
        c => Some(c.to_string()),
    }
}

/// The name's style for a decoration. Decorations are FOREGROUND-only on
/// purpose: selection and hover own the background, so a decorated row stays
/// readable when it is also the selected one.
fn deco_name_style(letter: Option<char>) -> Style {
    match letter {
        Some('I') => Style::default().dim(),
        Some(c) => Style::default().fg(status_color(c)),
        None => Style::default(),
    }
}

fn row_item(
    row: &Row,
    theme: IconTheme,
    hovered: bool,
    selected: bool,
    deco: Option<char>,
    width: u16,
) -> ListItem<'static> {
    let item = ListItem::new(row_line(row, theme, deco, width));
    match row_bg(hovered, selected) {
        Some(style) => item.style(style),
        None => item,
    }
}

/// The selection / hover BACKGROUND for a row, if any. Kept apart from the
/// content so a git decoration (foreground-only) can never collide with it.
fn row_bg(hovered: bool, selected: bool) -> Option<Style> {
    if selected {
        Some(selection_style(true))
    } else if hovered {
        // Subtler than the selection bg — hover is a hint, not a choice.
        Some(hover_style())
    } else {
        None
    }
}

/// One tree row's content: indent, chevron, icon, name, and the right-aligned
/// git decoration.
fn row_line(row: &Row, theme: IconTheme, deco: Option<char>, width: u16) -> Line<'static> {
    let indent = "  ".repeat(row.depth);
    let arrow = if row.is_dir {
        if row.expanded { "▾ " } else { "▸ " }
    } else {
        "  "
    };
    let icon = icon(theme, &row.name, row.is_dir, row.expanded);
    let icon_style = ui_icon_style(icon.rgb);
    // Folder and file names share the default foreground, like VS Code — the
    // chevron and icon carry the distinction. Accent-on-gray (the old blue
    // names) was hard to read against the selection/hover backgrounds. A git
    // status recolors the name on top of that.
    let mut spans = vec![
        Span::styled(format!("{indent}{arrow}"), Style::default().dim()),
        Span::styled(format!("{} ", icon.glyph), icon_style),
    ];
    let marker = deco.and_then(|letter| deco_marker(letter, row.is_dir && !row.is_symlink));
    // Row anatomy with a marker: [prefix][name][pad][marker][2 trailing]. The
    // two trailing cells keep the marker clear of the overflow scrollbar,
    // which overdraws the very last column; the name yields the 4 cells that
    // leaves (gap + marker + 2), so a narrow pane ellipsizes the NAME instead
    // of losing the status.
    let tail = if marker.is_some() { 4 } else { 0 };
    let used: usize = spans.iter().map(Span::width).sum();
    let avail = usize::from(width).saturating_sub(used + tail);
    let name = truncate_to(row.name.clone(), avail);
    let name_width = Span::raw(name.as_str()).width();
    spans.push(Span::styled(name, deco_name_style(deco)));
    if let Some(marker) = marker {
        let pad = usize::from(width).saturating_sub(used + name_width + 3);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(
            marker,
            Style::default()
                .fg(status_color(deco.unwrap_or(' ')))
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
    }
    Line::from(spans)
}

/// Next selectable (non-separator) menu index in `direction`, staying put at
/// the ends.
fn step_menu(entries: &[MenuEntry], from: usize, direction: isize) -> usize {
    let mut index = from as isize;
    loop {
        index += direction;
        if index < 0 || index >= entries.len() as isize {
            return from;
        }
        if matches!(entries[index as usize], MenuEntry::Action(..)) {
            return index as usize;
        }
    }
}

/// VS Code's creation target for the title-bar New File / New Folder buttons:
/// a selected folder itself, a selected file's parent, or the workspace root
/// when nothing is selected.
fn create_target_dir(selected: Option<&Row>, root: PathBuf) -> PathBuf {
    match selected {
        Some(row) if row.is_dir => row.path.clone(),
        Some(row) => row.path.parent().map(Path::to_path_buf).unwrap_or(root),
        None => root,
    }
}

/// True when a click at pane-local `column` lands on a row's disclosure
/// chevron (the two cells right after the depth indent).
fn hits_chevron(column: u16, depth: usize) -> bool {
    let start = (depth * 2) as u16;
    (start..start + 2).contains(&column)
}

fn folder_click_toggles(on_chevron: bool, double: bool) -> bool {
    on_chevron || !double
}

/// The row index at a pane-local mouse row given the last-drawn body
/// geometry, if it lands on an actual row.
fn row_index_at(body: BodyGeom, row_count: usize, mouse_row: u16) -> Option<usize> {
    if mouse_row < body.top || mouse_row >= body.top + body.height {
        return None;
    }
    let index = body.offset + usize::from(mouse_row - body.top);
    (index < row_count).then_some(index)
}

fn apply_shared_tree_state(
    tree: &mut Tree,
    rows: &mut Vec<Row>,
    selected: &mut Option<usize>,
    scroll: &mut usize,
    mut shared: sidebar::TreeState,
) -> bool {
    let root = tree.root_path();
    shared.expanded.retain(|path| path.starts_with(&root));
    shared.expanded.sort();
    shared.expanded.dedup();

    let current_selection = selected.and_then(|index| rows.get(index).map(|row| row.path.clone()));
    let expansion_changed = tree.expanded_paths() != shared.expanded;
    if expansion_changed {
        tree.set_expanded(shared.expanded);
        *rows = tree.rows();
    }

    let desired_selection = match shared.selected {
        Some(path) => rows.iter().position(|row| row.path == path).or_else(|| {
            current_selection
                .as_ref()
                .and_then(|path| rows.iter().position(|row| &row.path == path))
        }),
        None => None,
    };
    let desired_path = desired_selection.and_then(|index| rows.get(index).map(|row| &row.path));
    let selection_changed = current_selection.as_ref() != desired_path;
    if selection_changed {
        *selected = desired_selection;
    }
    if !expansion_changed && !selection_changed {
        return false;
    }
    *scroll = if rows.is_empty() {
        0
    } else {
        (*scroll).min(rows.len() - 1)
    };
    true
}

fn rebuild_tree_rows(
    tree: &mut Tree,
    rows: &mut Vec<Row>,
    selected: &mut Option<usize>,
    scroll: &mut usize,
) {
    let selected_path = selected.and_then(|index| rows.get(index).map(|row| row.path.clone()));
    *rows = tree.rows();
    if rows.is_empty() {
        *selected = None;
        *scroll = 0;
        return;
    }
    if let Some(path) = selected_path {
        let index = rows
            .iter()
            .position(|row| row.path == path)
            .unwrap_or_else(|| selected.unwrap_or(0).min(rows.len() - 1));
        *selected = Some(index);
    } else if let Some(index) = *selected {
        *selected = Some(index.min(rows.len() - 1));
    }
    *scroll = (*scroll).min(rows.len() - 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_button_hit_region_is_header_right_edge() {
        assert!(hits_collapse_button(30, 49, 32, 50), "footer right edge");
        assert!(hits_collapse_button(28, 49, 32, 50));
        assert!(!hits_collapse_button(27, 49, 32, 50), "left of the button");
        assert!(!hits_collapse_button(30, 0, 32, 50), "header row");
        assert!(!hits_collapse_button(30, 48, 32, 50), "tree row");
    }

    #[test]
    fn menu_navigation_skips_separators_and_clamps() {
        let entries = actions::menu_entries(Some(false), true);
        // First entry is an action; stepping up from it stays put.
        assert_eq!(step_menu(&entries, 0, -1), 0);
        // Stepping down over a separator lands on the next action.
        let sep = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Separator))
            .unwrap();
        assert_eq!(step_menu(&entries, sep - 1, 1), sep + 1);
        let last = entries.len() - 1;
        assert_eq!(step_menu(&entries, last, 1), last);
    }

    #[test]
    fn chevron_hit_region_follows_indent_depth() {
        assert!(hits_chevron(0, 0));
        assert!(hits_chevron(1, 0));
        assert!(!hits_chevron(2, 0), "icon cell");
        assert!(hits_chevron(2, 1));
        assert!(hits_chevron(3, 1));
        assert!(!hits_chevron(0, 1), "indent cell");
    }

    #[test]
    fn folder_rows_toggle_on_single_click_without_double_clicking_back() {
        assert!(folder_click_toggles(false, false));
        assert!(!folder_click_toggles(false, true));
        assert!(folder_click_toggles(true, true));
    }

    #[test]
    fn shared_tree_state_updates_expansion_and_selection() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-tree-sync-{}-{}",
            std::process::id(),
            sidebar::unix_now()
        ));
        let src = root.join("src");
        let nested = src.join("bin");
        let file = nested.join("main.rs");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let mut tree = Tree::new(root.clone());
        let mut rows = tree.rows();
        let mut selected = None;
        let mut scroll = 4;
        assert!(apply_shared_tree_state(
            &mut tree,
            &mut rows,
            &mut selected,
            &mut scroll,
            sidebar::TreeState {
                expanded: vec![nested, src],
                selected: Some(file.clone()),
            },
        ));
        assert_eq!(
            selected.and_then(|index| rows.get(index)),
            rows.iter().find(|row| row.path == file)
        );
        let mut expanded = tree.expanded_paths();
        expanded.reverse();
        assert!(!apply_shared_tree_state(
            &mut tree,
            &mut rows,
            &mut selected,
            &mut scroll,
            sidebar::TreeState {
                expanded,
                selected: Some(file),
            },
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collapsing_all_publishes_the_repointed_visible_selection() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-tree-collapse-{}-{}",
            std::process::id(),
            sidebar::unix_now()
        ));
        let src = root.join("src");
        let file = src.join("main.rs");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let mut tree = Tree::new(root.clone());
        tree.expand(&src);
        let mut rows = tree.rows();
        let mut selected = rows.iter().position(|row| row.path == file);
        let mut scroll = 0;
        tree.collapse_all();
        rebuild_tree_rows(&mut tree, &mut rows, &mut selected, &mut scroll);

        let selected_path = selected.and_then(|index| rows.get(index).map(|row| row.path.clone()));
        assert!(selected_path.is_some());
        let expanded = tree.expanded_paths();
        assert!(!apply_shared_tree_state(
            &mut tree,
            &mut rows,
            &mut selected,
            &mut scroll,
            sidebar::TreeState {
                expanded,
                selected: selected_path,
            },
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn create_target_matches_vscode_semantics() {
        let root = PathBuf::from("C:\\ws");
        let dir = Row {
            path: root.join("src"),
            name: "src".into(),
            is_dir: true,
            is_symlink: false,
            depth: 0,
            expanded: false,
        };
        let file = Row {
            path: root.join("src").join("main.rs"),
            name: "main.rs".into(),
            is_dir: false,
            is_symlink: false,
            depth: 1,
            expanded: false,
        };
        assert_eq!(
            create_target_dir(Some(&dir), root.clone()),
            root.join("src")
        );
        assert_eq!(
            create_target_dir(Some(&file), root.clone()),
            root.join("src")
        );
        assert_eq!(create_target_dir(None, root.clone()), root);
    }

    #[test]
    fn focused_pane_detection_is_scoped_to_our_pane_id() {
        let panes = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","focused":false},
            {"pane_id":"w1:p2","focused":true}
        ]}}"#;
        assert!(!pane_focused_in(panes, "w1:p1"));
        assert!(pane_focused_in(panes, "w1:p2"));
        assert!(!pane_focused_in("garbage", "w1:p2"));
    }

    /// The rendered text of a row, decorations included.
    fn rendered(row: &Row, deco: Option<char>, width: u16) -> String {
        row_line(row, IconTheme::Emoji, deco, width)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn file_row(name: &str) -> Row {
        Row {
            path: PathBuf::from("C:\\ws").join(name),
            name: name.into(),
            is_dir: false,
            is_symlink: false,
            depth: 0,
            expanded: false,
        }
    }

    fn dir_row(name: &str) -> Row {
        Row {
            path: PathBuf::from("C:\\ws").join(name),
            name: name.into(),
            is_dir: true,
            is_symlink: false,
            depth: 0,
            expanded: false,
        }
    }

    fn symlink_dir_row(name: &str) -> Row {
        Row {
            path: PathBuf::from("C:\\ws").join(name),
            name: name.into(),
            is_dir: true,
            is_symlink: true,
            depth: 0,
            expanded: false,
        }
    }

    #[test]
    fn files_render_their_status_letter_and_dirs_a_dirty_dot() {
        assert!(rendered(&file_row("app.rs"), Some('M'), 30).ends_with("M  "));
        assert!(rendered(&file_row("new.rs"), Some('A'), 30).ends_with("A  "));
        assert!(
            rendered(&symlink_dir_row("linked"), Some('U'), 30).ends_with("U  "),
            "a directory symlink is a Git file even though it expands like a folder"
        );
        assert!(rendered(&file_row("gone.rs"), Some('D'), 30).ends_with("D  "));
        assert!(rendered(&file_row("notes.md"), Some('U'), 30).ends_with("U  "));
        assert!(rendered(&file_row("merge.rs"), Some('!'), 30).ends_with("!  "));
        // A directory shows the aggregate as a dot, never a letter.
        let dir = rendered(&dir_row("src"), Some('M'), 30);
        assert!(dir.ends_with("●  "), "{dir}");
        assert!(!dir.contains('M'));
    }

    #[test]
    fn undecorated_and_ignored_rows_carry_no_marker() {
        let plain = rendered(&file_row("app.rs"), None, 30);
        assert!(plain.trim_end().ends_with("app.rs"), "{plain}");
        // Ignored is conveyed by the dimmed name alone — no trailing marker.
        let ignored = rendered(&file_row("build.log"), Some('I'), 30);
        assert!(ignored.trim_end().ends_with("build.log"), "{ignored}");
    }

    #[test]
    fn decorations_are_foreground_only_so_selection_stays_visible() {
        // Selection owns the BACKGROUND; a decoration must only recolor the
        // name, or a selected changed row would be unreadable.
        assert_eq!(
            row_bg(false, true).and_then(|s| s.bg),
            Some(palette().selection_bg)
        );
        assert_eq!(
            row_bg(true, false).and_then(|s| s.bg),
            Some(palette().hover_bg)
        );
        assert_eq!(row_bg(false, false), None);
        // The decoration itself only ever sets a foreground.
        let name_spans = row_line(&file_row("app.rs"), IconTheme::Emoji, Some('M'), 30);
        assert!(name_spans.spans.iter().all(|s| s.style.bg.is_none()));
        assert_eq!(deco_name_style(Some('M')).fg, Some(status_color('M')));
        assert_eq!(deco_name_style(Some('M')).bg, None);
        assert_eq!(deco_name_style(None).fg, None);
        assert!(
            deco_name_style(Some('I'))
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn a_decorated_name_is_truncated_to_keep_the_marker_visible() {
        let long = file_row("a-very-long-file-name-that-will-not-fit.rs");
        let text = rendered(&long, Some('M'), 20);
        assert!(
            text.ends_with("M  "),
            "marker survives a narrow pane: {text}"
        );
        assert_eq!(
            Span::raw(text.as_str()).width(),
            20,
            "and the row still fills exactly the pane width"
        );
    }

    #[test]
    fn deco_markers_follow_the_row_kind() {
        assert_eq!(deco_marker('M', false).as_deref(), Some("M"));
        assert_eq!(deco_marker('M', true).as_deref(), Some("●"));
        assert_eq!(deco_marker('!', true).as_deref(), Some("●"));
        assert_eq!(deco_marker('I', false), None);
        assert_eq!(deco_marker('I', true), None);
    }

    #[test]
    fn row_index_accounts_for_header_and_scroll() {
        let body = BodyGeom {
            top: 1,
            height: 10,
            offset: 5,
        };
        assert_eq!(row_index_at(body, 100, 0), None, "header row");
        assert_eq!(row_index_at(body, 100, 1), Some(5));
        assert_eq!(row_index_at(body, 100, 10), Some(14));
        assert_eq!(row_index_at(body, 100, 11), None, "footer row");
        assert_eq!(row_index_at(body, 6, 2), None, "past the last row");
    }

    #[test]
    fn quick_open_matches_case_insensitive_subsequences() {
        let files = vec![
            QuickFile {
                path: PathBuf::from("src/main.rs"),
                label: "src/main.rs".into(),
                label_lower: "src/main.rs".into(),
            },
            QuickFile {
                path: PathBuf::from("README.md"),
                label: "README.md".into(),
                label_lower: "readme.md".into(),
            },
        ];
        assert!(fuzzy_score_lowercased("smr", "src/main.rs").is_some());
        assert!(fuzzy_score_lowercased("smr", "readme.md").is_none());
        assert_eq!(quick_matches(&files, "read"), vec![1]);
    }

    #[test]
    fn quick_open_truncation_keeps_the_filename_visible() {
        assert_eq!(
            truncate_path_tail("src/very/deep/nested/thing.rs", 12),
            "…ed/thing.rs"
        );
        assert_eq!(truncate_path_tail("main.rs", 12), "main.rs");
        assert_eq!(truncate_path_tail("main.rs", 0), "");
        assert_eq!(truncate_path_tail("src/界面.rs", 8), "…界面.rs");
    }

    #[test]
    fn quick_open_index_skips_git_and_respects_hidden_files() {
        let root =
            std::env::temp_dir().join(format!("herdr-sidebar-quick-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join(".secret"), "").unwrap();
        std::fs::write(root.join(".git/config"), "").unwrap();
        std::fs::write(root.join("target/generated.rs"), "").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();

        let (hidden_off, truncated) = collect_quick_files(&root, false, 20);
        assert!(!truncated);
        assert_eq!(
            hidden_off
                .iter()
                .map(|file| file.label.as_str())
                .collect::<Vec<_>>(),
            vec!["src/main.rs"]
        );
        let (hidden_on, _) = collect_quick_files(&root, true, 20);
        assert_eq!(
            hidden_on
                .iter()
                .map(|file| file.label.as_str())
                .collect::<Vec<_>>(),
            vec![".gitignore", ".secret", "src/main.rs"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn content_search_groups_ignored_and_hidden_rules_with_source_lines() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-content-search-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "first\nNeedle here\nneedle again\n",
        )
        .unwrap();
        std::fs::write(root.join(".secret"), "needle\n").unwrap();
        std::fs::write(root.join("target/generated.rs"), "needle\n").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();

        let (visible, truncated) =
            collect_content_hits(&root, false, "NEEDLE", "", "", SearchOptions::default(), 20)
                .unwrap();
        assert!(!truncated);
        assert_eq!(
            visible
                .iter()
                .map(|hit| (hit.label.as_str(), hit.line))
                .collect::<Vec<_>>(),
            vec![("src/main.rs", 2), ("src/main.rs", 3)]
        );
        let (with_hidden, _) =
            collect_content_hits(&root, true, "needle", "", "", SearchOptions::default(), 20)
                .unwrap();
        assert_eq!(with_hidden.len(), 3);
        assert!(with_hidden.iter().any(|hit| hit.label == ".secret"));

        let (included, _) = collect_content_hits(
            &root,
            true,
            "needle",
            "src/**",
            "",
            SearchOptions::default(),
            20,
        )
        .unwrap();
        assert_eq!(included.len(), 2);
        let (excluded, _) = collect_content_hits(
            &root,
            true,
            "needle",
            "",
            "src/**",
            SearchOptions::default(),
            20,
        )
        .unwrap();
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].label, ".secret");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn content_search_options_match_vscode_semantics() {
        assert!(content_line_matches(
            "Needle needlebox",
            "needle",
            SearchOptions::default(),
            None,
        ));
        assert!(!content_line_matches(
            "Needle",
            "needle",
            SearchOptions {
                match_case: true,
                ..SearchOptions::default()
            },
            None,
        ));
        assert!(content_line_matches(
            "a needle here",
            "needle",
            SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
            None,
        ));
        assert!(!content_line_matches(
            "needlebox",
            "needle",
            SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
            None,
        ));

        let regex = RegexBuilder::new(r"need(le|ful)")
            .case_insensitive(true)
            .build()
            .unwrap();
        assert!(content_line_matches(
            "NEEDFUL",
            "ignored",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
            Some(&regex),
        ));
        assert!(build_search_globs("[", "include").is_err());
        let directories = build_search_globs("node_modules", "exclude")
            .unwrap()
            .unwrap();
        assert!(directories.is_match("web/node_modules/pkg/index.js"));
        assert!(directories.is_match("node_modules/pkg/index.js"));
    }

    #[test]
    fn ignored_backoff_applies_independently_of_status_refreshes() {
        let now = std::time::Instant::now();
        assert!(!ignored_scan_due(
            Some(now + std::time::Duration::from_secs(60)),
            now
        ));
        assert!(ignored_scan_due(Some(now), now));
        assert!(ignored_scan_due(None, now));
    }

    #[test]
    fn focused_empty_search_hides_its_placeholder() {
        let focused = search_input_line("", "Search", true, 20);
        assert_eq!(
            focused
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "█"
        );
        let idle = search_input_line("", "Search", false, 20);
        assert_eq!(idle.spans[0].content.as_ref(), "Search");
        assert!(idle.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn search_result_context_bolds_every_visible_match() {
        let line = "Needle plus needlebox";
        let ranges = content_line_match_ranges(line, "needle", SearchOptions::default(), None);
        let spans = highlighted_search_context(line, &ranges, 80, Style::default().bold());
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "Needle plus needlebox"
        );
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
                .count(),
            2
        );

        let line = "needle plus needlebox";
        let ranges = content_line_match_ranges(
            line,
            "needle",
            SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
            None,
        );
        let spans = highlighted_search_context(line, &ranges, 80, Style::default().bold());
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
                .count(),
            1
        );
    }

    #[test]
    fn search_highlights_follow_literal_boundaries_unicode_and_clipping() {
        let whole_word = SearchOptions {
            whole_word: true,
            ..SearchOptions::default()
        };
        let punctuation = "use C++ here";
        let ranges = content_line_match_ranges(punctuation, "C++", whole_word, None);
        assert_eq!(ranges, [(4, 7)]);

        let unicode = "İ and s ſ";
        assert_eq!(
            content_line_match_ranges(unicode, "i", SearchOptions::default(), None),
            [(0, 2)]
        );
        assert_eq!(
            content_line_match_ranges(unicode, "s", SearchOptions::default(), None),
            [(7, 8)]
        );
        assert_eq!(
            content_line_match_ranges("ΟΣ", "ΟΣ", SearchOptions::default(), None),
            [(0, 4)]
        );

        let (_, trimmed_ranges) = content_hit_context(" needle", &[(0, 1)]);
        assert!(trimmed_ranges.is_empty());

        let clipped = "needlebox needle";
        let ranges = content_line_match_ranges(clipped, "needle", whole_word, None);
        let spans = highlighted_search_context(clipped, &ranges, 8, Style::default().bold());
        assert!(
            spans
                .iter()
                .all(|span| !span.style.add_modifier.contains(Modifier::BOLD))
        );

        let ranges = content_line_match_ranges("needle", "needle", whole_word, None);
        let spans = highlighted_search_context("needle", &ranges, 5, Style::default().bold());
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "need…"
        );
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn search_toolbar_uses_full_size_codicons_in_material_mode() {
        assert_eq!(
            search_toolbar_icons(IconTheme::Material),
            ("\u{eb37}", "\u{eabf}", "\u{ea7c}")
        );
    }
}
