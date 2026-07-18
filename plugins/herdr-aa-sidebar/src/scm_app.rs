//! TUI state and rendering: the VS Code Source Control panel — commit message
//! box (with the ✨ suggest button), Commit button, collapsible Staged/Changes
//! sections, Git-Graph-style drawers (GRAPH, COMMITS, FILE HISTORY, BRANCHES,
//! REMOTES, STASHES, TAGS), theme-matched file icons, mouse support, and a
//! Ctrl+right-click context menu — kept interaction-consistent with
//! herdr-aa-filetree. No own border/title: herdr already frames the pane and
//! titles it with the pane label.
//!
//! When herdr-aa-filetree is also installed, the panel can merge with it into
//! a single "Sidebar" pane with an activity-bar view switcher (see sidebar.rs).

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use herdr_aa_sidebar::git::{FileEntry, Git, Status};
use herdr_aa_sidebar::icons::{IconTheme, icon};
use herdr_aa_sidebar::state::{self as sidebar, View};
use herdr_aa_sidebar::state::Exit;
use herdr_aa_sidebar::ui::{activity_icons, branch_icon, gear_icon, hits, sibling_panes_of, sparkle_icon, truncate_to, within, wrap_hints};
use herdr_aa_sidebar::actions::{copy_to_clipboard, reveal};
use herdr_aa_sidebar::suggest;

// VS Code's dark-theme git decoration colors.
const BUTTON_BLUE: Color = Color::Rgb(0x00, 0x78, 0xd4);
const BUTTON_BLUE_FOCUS: Color = Color::Rgb(0x02, 0x8a, 0xf0);
const BADGE_BLUE: Color = Color::Rgb(0x00, 0x78, 0xd4);
const MODIFIED: Color = Color::Rgb(0xe2, 0xc0, 0x8d);
const UNTRACKED: Color = Color::Rgb(0x73, 0xc9, 0x91);
const ADDED: Color = Color::Rgb(0x81, 0xb8, 0x8b);
const RENAMED: Color = Color::Rgb(0x73, 0xc9, 0x91);
const DELETED: Color = Color::Rgb(0xc7, 0x4e, 0x39);
const CONFLICT: Color = Color::Rgb(0xe4, 0x67, 0x6b);
const HOVER_BG: Color = Color::Rgb(48, 52, 60);


/// How many log lines the history-ish drawers fetch.
const DRAWER_LIMIT: usize = 30;

fn letter_color(letter: char) -> Color {
    match letter {
        'M' => MODIFIED,
        'U' => UNTRACKED,
        'A' => ADDED,
        'R' | 'C' => RENAMED,
        'D' => DELETED,
        '!' => CONFLICT,
        _ => Color::Reset,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Message,
    Commit,
    List,
}

/// The Git-Graph-style drawers below the Changes section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drawer {
    Graph,
    Commits,
    FileHistory,
    Branches,
    Remotes,
    Stashes,
    Tags,
}

impl Drawer {
    const ALL: [Drawer; 7] = [
        Drawer::Graph,
        Drawer::Commits,
        Drawer::FileHistory,
        Drawer::Branches,
        Drawer::Remotes,
        Drawer::Stashes,
        Drawer::Tags,
    ];

    fn title(self) -> &'static str {
        match self {
            Drawer::Graph => "GRAPH",
            Drawer::Commits => "COMMITS",
            Drawer::FileHistory => "FILE HISTORY",
            Drawer::Branches => "BRANCHES",
            Drawer::Remotes => "REMOTES",
            Drawer::Stashes => "STASHES",
            Drawer::Tags => "TAGS",
        }
    }

    fn index(self) -> usize {
        Drawer::ALL.iter().position(|d| *d == self).unwrap_or(0)
    }
}

#[derive(Default)]
struct DrawerPanel {
    expanded: bool,
    lines: Vec<String>,
}

/// One discovered repository and its per-repo view state — including its own
/// commit message, so the multi-repo view mirrors VS Code's per-repo inputs.
struct Repo {
    git: Git,
    name: String,
    status: Status,
    collapsed: bool,
    staged_collapsed: bool,
    changes_collapsed: bool,
    message: Vec<char>,
    cursor: usize,
}

impl Repo {
    fn new(git: Git) -> Self {
        Self {
            name: git.name(),
            git,
            status: Status::default(),
            collapsed: false,
            staged_collapsed: false,
            changes_collapsed: false,
            message: Vec::new(),
            cursor: 0,
        }
    }

    /// The repo row's branch decoration: `name*` when the tree is dirty.
    fn branch_decor(&self) -> String {
        let dirty = if self.status.staged.is_empty() && self.status.unstaged.is_empty() {
            ""
        } else {
            "*"
        };
        format!("{}{dirty}", self.status.branch)
    }
}

/// List rows; the first index on the repo-scoped variants is the repo.
#[derive(Clone, Copy)]
enum Row {
    /// Only rendered when more than one repository is visible.
    RepoHeader(usize),
    /// The repo's inline message box (3 screen lines) — multi-repo only.
    Message(usize),
    /// The repo's inline ✓ Commit button — multi-repo only.
    Commit(usize),
    StagedHeader(usize),
    ChangesHeader(usize),
    Staged(usize, usize),
    Unstaged(usize, usize),
    DrawerHeader(Drawer),
    DrawerLine(Drawer, usize),
}

impl Row {
    /// The repository a row belongs to (drawers follow the active repo).
    fn repo(self) -> Option<usize> {
        match self {
            Row::RepoHeader(r)
            | Row::Message(r)
            | Row::Commit(r)
            | Row::StagedHeader(r)
            | Row::ChangesHeader(r)
            | Row::Staged(r, _)
            | Row::Unstaged(r, _) => Some(r),
            Row::DrawerHeader(_) | Row::DrawerLine(..) => None,
        }
    }

    /// Screen lines this row occupies (the inline message box is bordered).
    fn height(self) -> u16 {
        match self {
            Row::Message(_) => 3,
            _ => 1,
        }
    }

    /// Keyboard navigation (j/k, wheel) skips widget rows — they are clicked,
    /// like VS Code's inputs, not list entries.
    fn selectable(self) -> bool {
        !matches!(self, Row::Message(_) | Row::Commit(_))
    }
}

/// Context-menu actions for a staged/unstaged file row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    StageOrUnstage,
    Discard,
    CopyPath,
    CopyRelativePath,
    Reveal,
}

#[derive(Clone, Copy)]
enum MenuEntry {
    Action(MenuAction, &'static str),
    Separator,
}

/// A modal layered over the list; while open it owns keyboard and mouse input.
enum Overlay {
    Menu {
        x: u16,
        y: u16,
        /// (repo, entry, staged) — the file row the menu targets.
        target: (usize, FileEntry, bool),
        entries: Vec<MenuEntry>,
        selected: usize,
        rect: Rect,
    },
    ConfirmDiscard {
        repo: usize,
        entry: FileEntry,
    },
    /// The ⚙ settings modal: mouse-toggleable panel settings.
    Settings {
        selected: usize,
        rect: Rect,
    },
}

/// One row of the Settings modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    UnifiedSidebar,
    IconTheme,
}

/// (setting, label, current value, enabled) — disabled rows render dimmed and
/// don't toggle.
type SettingRow = (Setting, &'static str, String, bool);

/// Where the list body was drawn last frame, for mouse hit-testing.
#[derive(Clone, Copy, Default)]
struct BodyGeom {
    top: u16,
    height: u16,
    offset: usize,
}

/// Clickable regions of the activity bar / header / message box, from the
/// last draw.
#[derive(Clone, Copy, Default)]
struct ClickZones {
    activity_row: u16,
    explorer: (u16, u16),
    source_control: (u16, u16),
    /// The ⚙ button (activity bar in unified mode, header otherwise).
    gear: Rect,
    message: Rect,
    sparkle: Rect,
    button: Rect,
    /// The Sync Changes row (zero-sized when hidden).
    sync: Rect,
}

/// Handle for identity/label control of our own pane over the socket API.
struct PaneCtl {
    pane_id: String,
}

impl PaneCtl {
    fn from_env() -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|id| !id.is_empty())?;
        Some(Self { pane_id })
    }

    fn set_label(&self, label: &str) {
        let _ = herdr_aa_sidebar::ipc::call_text(
            "pane.rename",
            serde_json::json!({ "pane_id": self.pane_id, "label": label }),
        );
    }

    /// Resize our pane to `target` terminal columns over the socket API
    /// (`pane.resize` takes a split-RATIO delta; the plan converts columns).
    fn resize_to(&self, current: u16, target: u16) {
        let Ok(layout) = herdr_aa_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        ) else {
            return;
        };
        let Some(step) =
            herdr_aa_sidebar::launch::resize_plan(&layout, &self.pane_id, current, target)
        else {
            return;
        };
        let _ = herdr_aa_sidebar::ipc::call_text(
            "pane.resize",
            serde_json::json!({
                "pane_id": self.pane_id,
                "direction": step.direction,
                "amount": step.amount,
            }),
        );
    }

    /// Report identity tokens: always our own; in merged mode also the other
    /// view's (one Sidebar pane satisfies both plugins' launchers), otherwise
    /// clear the other view's token.
    fn report_tokens(&self, my: View, merged: bool) {
        let mine = serde_json::json!({ my.plugin_id(): my.token() });
        let _ = herdr_aa_sidebar::ipc::call_text(
            "pane.report_metadata",
            serde_json::json!({ "pane_id": self.pane_id, "source": my.plugin_id(), "tokens": mine }),
        );
        let other = my.other();
        // Clearing needs an explicit null VALUE: report_metadata MERGES the
        // token map, so an empty map is a no-op (verified live, herdr 0.7.1).
        let other_tokens = if merged {
            serde_json::json!({ other.plugin_id(): other.token() })
        } else {
            serde_json::json!({ other.plugin_id(): serde_json::Value::Null })
        };
        let _ = herdr_aa_sidebar::ipc::call_text(
            "pane.report_metadata",
            serde_json::json!({
                "pane_id": self.pane_id,
                "source": other.plugin_id(),
                "tokens": other_tokens,
            }),
        );
    }
}

pub struct App {
    /// Every repository visible from the cwd (VS Code style: the containing
    /// repo plus child repos). Empty = "not a git repository".
    repos: Vec<Repo>,
    /// Why discovery came up empty, for the placeholder screen.
    discover_err: String,
    /// The repo the commit box / drawers / sync act on: the one the selection
    /// is in.
    active: usize,
    cwd: PathBuf,
    rows: Vec<Row>,
    list: ListState,
    focus: Focus,
    theme: IconTheme,
    drawers: [DrawerPanel; 7],
    /// The file the FILE HISTORY drawer follows: the last selected file row.
    history_target: Option<String>,
    /// One-shot footer notice: (text, is_error). Cleared on the next key press.
    flash: Option<(String, bool)>,
    /// Pending ✧ commit-message generation, polled from tick().
    suggesting: Option<Receiver<String>>,
    /// Pending Sync Changes run, polled from tick().
    syncing: Option<Receiver<Result<String, String>>>,
    overlay: Option<Overlay>,
    hovered: Option<usize>,
    body: BodyGeom,
    zones: ClickZones,
    page: usize,
    last_width: u16,
    // Merged-sidebar state.
    sidebar_state: sidebar::State,
    other_exe: Option<PathBuf>,
    pane_ctl: Option<PaneCtl>,
}

const MY_VIEW: View = View::SourceControl;

impl App {
    pub fn new(cwd: PathBuf) -> Self {
        let repos: Vec<Repo> = Git::discover_all(&cwd).into_iter().map(Repo::new).collect();
        let discover_err = if repos.is_empty() {
            Git::discover(&cwd).err().unwrap_or_else(|| "no repositories found".to_string())
        } else {
            String::new()
        };
        let theme = IconTheme::from_env(
            std::env::var("HERDR_AA_GIT_ICONS")
                .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
                .ok()
                .as_deref(),
        );
        // The other view ships in this same binary — always available.
        let other_exe = std::env::current_exe().ok();
        let sidebar_state = sidebar::load_state();
        let pane_ctl = PaneCtl::from_env();
        let mut app = Self {
            repos,
            discover_err,
            active: 0,
            cwd,
            rows: Vec::new(),
            list: ListState::default(),
            focus: Focus::List,
            theme,
            drawers: Default::default(),
            history_target: None,
            flash: None,
            suggesting: None,
            syncing: None,
            overlay: None,
            hovered: None,
            body: BodyGeom::default(),
            zones: ClickZones::default(),
            page: 20,
            last_width: 40,
            sidebar_state,
            other_exe,
            pane_ctl,
        };
        app.apply_identity();
        app.refresh();
        app
    }

    fn active_repo(&self) -> Option<&Repo> {
        self.repos.get(self.active)
    }

    fn active_repo_mut(&mut self) -> Option<&mut Repo> {
        let i = self.active;
        self.repos.get_mut(i)
    }

    /// More than one repo: VS Code-style per-repo inline inputs in the list.
    fn multi(&self) -> bool {
        self.repos.len() > 1
    }

    /// The merged sidebar is on and actually usable (other plugin present).
    fn merged(&self) -> bool {
        self.sidebar_state.merged && self.other_exe.is_some()
    }

    /// Push our label + metadata tokens to herdr for the current mode.
    fn apply_identity(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        let label = if self.merged() { sidebar::SIDEBAR_LABEL } else { MY_VIEW.label() };
        ctl.set_label(label);
        ctl.report_tokens(MY_VIEW, self.merged());
    }

    /// Re-read every repo's git status (this is the change auto-detection —
    /// tick() calls it every [`crate::REFRESH_EVERY`]); keeps the flash so
    /// periodic ticks don't eat notices.
    pub fn refresh(&mut self) {
        let mut error = None;
        for repo in &mut self.repos {
            match repo.git.status() {
                Ok(status) => repo.status = status,
                Err(e) => error = Some(e),
            }
        }
        if let Some(e) = error {
            self.flash = Some((e, true));
        }
        self.reload_expanded_drawers();
        self.rebuild();
    }

    /// Periodic timer tick: retry repo discovery if we started outside one,
    /// pick up external changes, and collect finished ✧ suggestion / sync runs.
    pub fn tick(&mut self) {
        if self.repos.is_empty() {
            self.repos = Git::discover_all(&self.cwd).into_iter().map(Repo::new).collect();
            if !self.repos.is_empty() {
                self.discover_err.clear();
            }
        }
        if let Some(rx) = &self.suggesting {
            match rx.try_recv() {
                Ok(message) => {
                    if let Some(repo) = self.active_repo_mut() {
                        repo.message = message.chars().collect();
                        repo.cursor = repo.message.len();
                    }
                    self.focus = Focus::Message;
                    self.flash = Some(("✧ suggestion ready — edit or ⏎ to commit".into(), false));
                    self.suggesting = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.flash = Some(("✧ generation failed".into(), true));
                    self.suggesting = None;
                }
            }
        }
        if let Some(rx) = &self.syncing {
            match rx.try_recv() {
                Ok(Ok(summary)) => {
                    self.flash = Some((summary, false));
                    self.syncing = None;
                }
                Ok(Err(e)) => {
                    self.flash = Some((e, true));
                    self.syncing = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.flash = Some(("sync failed".into(), true));
                    self.syncing = None;
                }
            }
        }
        self.refresh();
    }

    fn reload_expanded_drawers(&mut self) {
        let Some(git) = self.active_repo().map(|r| r.git.clone()) else { return };
        let git = &git;
        for kind in Drawer::ALL {
            if !self.drawers[kind.index()].expanded {
                continue;
            }
            let lines = match kind {
                Drawer::Graph => git.graph(DRAWER_LIMIT),
                Drawer::Commits => git.commits(DRAWER_LIMIT),
                Drawer::FileHistory => match &self.history_target {
                    Some(path) => git.file_history(path, DRAWER_LIMIT),
                    None => Ok(vec!["(select a file above)".to_string()]),
                },
                Drawer::Branches => git.branches(),
                Drawer::Remotes => git.remotes(),
                Drawer::Stashes => git.stashes(),
                Drawer::Tags => git.tags(),
            };
            self.drawers[kind.index()].lines = match lines {
                Ok(lines) if lines.is_empty() => vec!["(none)".to_string()],
                Ok(lines) => lines,
                Err(e) => vec![format!("({e})")],
            };
        }
    }

    fn rebuild(&mut self) {
        self.hovered = None;
        self.rows.clear();
        let multi = self.repos.len() > 1;
        for (r, repo) in self.repos.iter().enumerate() {
            if multi {
                self.rows.push(Row::RepoHeader(r));
                if repo.collapsed {
                    continue;
                }
                // VS Code gives every repo its own message box and Commit
                // button, inline in the list.
                self.rows.push(Row::Message(r));
                self.rows.push(Row::Commit(r));
            }
            // Like VS Code, the Staged section only exists while something is staged.
            if !repo.status.staged.is_empty() {
                self.rows.push(Row::StagedHeader(r));
                if !repo.staged_collapsed {
                    for i in 0..repo.status.staged.len() {
                        self.rows.push(Row::Staged(r, i));
                    }
                }
            }
            self.rows.push(Row::ChangesHeader(r));
            if !repo.changes_collapsed {
                for i in 0..repo.status.unstaged.len() {
                    self.rows.push(Row::Unstaged(r, i));
                }
            }
        }
        for kind in Drawer::ALL {
            self.rows.push(Row::DrawerHeader(kind));
            if self.drawers[kind.index()].expanded {
                for i in 0..self.drawers[kind.index()].lines.len() {
                    self.rows.push(Row::DrawerLine(kind, i));
                }
            }
        }
        if self.rows.is_empty() {
            self.list.select(None);
            return;
        }
        let index = self.list.selected().unwrap_or(0).min(self.rows.len() - 1);
        self.list.select(Some(self.nearest_selectable(index)));
        self.follow_selection();
    }

    /// The closest keyboard-selectable row to `from` (widget rows — inline
    /// message boxes and commit buttons — are skipped).
    fn nearest_selectable(&self, from: usize) -> usize {
        if self.rows.get(from).is_some_and(|r| r.selectable()) {
            return from;
        }
        let after = (from..self.rows.len()).find(|&i| self.rows[i].selectable());
        let before = (0..from).rev().find(|&i| self.rows[i].selectable());
        after.or(before).unwrap_or(0)
    }

    /// Keep the active repo and the FILE HISTORY drawer following the
    /// selection: drawers, commit box, and sync all act on the selected
    /// row's repository.
    fn follow_selection(&mut self) {
        let selected = self.list.selected().and_then(|i| self.rows.get(i)).copied();
        if let Some(r) = selected.and_then(Row::repo)
            && r != self.active
            && r < self.repos.len()
        {
            self.active = r;
            self.history_target = None;
            self.reload_expanded_drawers();
            let keep = self.list.selected();
            self.rebuild();
            if let Some(i) = keep {
                self.list.select(Some(i.min(self.rows.len().saturating_sub(1))));
            }
            return;
        }
        let path = match selected {
            Some(Row::Staged(r, i)) if r == self.active => {
                self.repos[r].status.staged.get(i).map(|e| e.path.clone())
            }
            Some(Row::Unstaged(r, i)) if r == self.active => {
                self.repos[r].status.unstaged.get(i).map(|e| e.path.clone())
            }
            _ => return, // keep the last file while browsing elsewhere
        };
        if path.is_some() && path != self.history_target {
            self.history_target = path;
            if self.drawers[Drawer::FileHistory.index()].expanded {
                self.reload_expanded_drawers();
                // Line count may have changed; rebuild WITHOUT re-entering
                // follow_selection (path is unchanged now).
                let selected = self.list.selected();
                self.rebuild();
                if let Some(i) = selected {
                    self.list.select(Some(i.min(self.rows.len() - 1)));
                }
            }
        }
    }

    /// Handle one key press; `Some(exit)` ends the event loop.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Exit> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        self.flash = None;
        if self.overlay.is_some() {
            self.overlay_key(key);
            return None;
        }
        match self.focus {
            Focus::Message => self.on_message_key(key),
            Focus::Commit => self.on_button_key(key),
            Focus::List => return self.on_list_key(key),
        }
        None
    }

    fn on_message_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.commit();
                return;
            }
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Tab => self.focus = Focus::Commit,
            KeyCode::BackTab => self.focus = Focus::List,
            KeyCode::Down => self.focus = Focus::Commit,
            _ => {}
        }
        let Some(repo) = self.active_repo_mut() else { return };
        match key.code {
            KeyCode::Backspace => {
                if repo.cursor > 0 {
                    repo.cursor -= 1;
                    repo.message.remove(repo.cursor);
                }
            }
            KeyCode::Delete => {
                if repo.cursor < repo.message.len() {
                    repo.message.remove(repo.cursor);
                }
            }
            KeyCode::Left => repo.cursor = repo.cursor.saturating_sub(1),
            KeyCode::Right => repo.cursor = (repo.cursor + 1).min(repo.message.len()),
            KeyCode::Home => repo.cursor = 0,
            KeyCode::End => repo.cursor = repo.message.len(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                repo.message.clear();
                repo.cursor = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                repo.message.insert(repo.cursor, c);
                repo.cursor += 1;
            }
            _ => {}
        }
    }

    fn on_button_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => self.commit(),
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Tab | KeyCode::Down => self.focus = Focus::List,
            KeyCode::BackTab | KeyCode::Up => self.focus = Focus::Message,
            _ => {}
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) -> Option<Exit> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(Exit::Quit),
            KeyCode::Tab => self.focus = Focus::Message,
            KeyCode::BackTab => self.focus = Focus::Commit,
            KeyCode::Char('c') => self.focus = Focus::Message,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.page as isize)),
            KeyCode::PageDown => self.move_by(self.page as isize),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(self.rows.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
            KeyCode::Char('a') => self.stage_all(),
            KeyCode::Char('u') => self.unstage_all(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('i') => self.theme = self.theme.toggled(),
            KeyCode::Char('A') => self.suggest_message(),
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('S') => self.sync_changes(),
            KeyCode::Char('1') => return self.switch_to(View::Explorer),
            KeyCode::Char('2') => return self.switch_to(View::SourceControl),
            _ => {}
        }
        None
    }

    /// `Some(exit)` ends the event loop, mirroring on_key.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit> {
        if self.overlay.is_some() {
            self.overlay_mouse(mouse);
            return None;
        }
        match mouse.kind {
            MouseEventKind::Moved => {
                self.hovered = self.row_at(mouse.row);
            }
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::ScrollDown => self.move_by(3),
            MouseEventKind::Down(MouseButton::Left) => return self.left_click(mouse),
            MouseEventKind::Down(MouseButton::Right) => {
                // Reaches us only as Ctrl+right-click (herdr's passthrough
                // modifier); plain right-click opens herdr's own pane menu.
                self.flash = None;
                self.open_context_menu(mouse.column, mouse.row);
            }
            _ => {}
        }
        None
    }

    fn left_click(&mut self, mouse: MouseEvent) -> Option<Exit> {
        self.flash = None;
        let (x, y) = (mouse.column, mouse.row);
        let z = self.zones;
        if self.merged() && y == z.activity_row {
            if within(x, z.explorer) {
                return self.switch_to(View::Explorer);
            }
            if within(x, z.source_control) {
                return self.switch_to(View::SourceControl);
            }
        }
        if hits(z.gear, x, y) {
            self.open_settings();
            return None;
        }
        if hits(z.sparkle, x, y) {
            self.suggest_message();
            return None;
        }
        if hits(z.message, x, y) {
            self.focus = Focus::Message;
            return None;
        }
        if hits(z.button, x, y) {
            self.focus = Focus::Commit;
            self.commit();
            return None;
        }
        if hits(z.sync, x, y) {
            self.sync_changes();
            return None;
        }
        if let Some((index, line)) = self.row_hit(y) {
            match self.rows[index] {
                // The inline widgets: click focuses/acts without selecting.
                Row::Message(r) => {
                    self.active = r;
                    // The box's middle line holds the input and the ✧ button.
                    if line == 1 && x >= self.last_width.saturating_sub(4) {
                        self.suggest_message();
                    } else {
                        self.focus = Focus::Message;
                    }
                    self.follow_selection();
                }
                Row::Commit(r) => self.commit_repo(r),
                Row::RepoHeader(r) => {
                    self.focus = Focus::List;
                    self.select(index);
                    // Right-side action icons: ⟳ sync · ✓ commit (fixed
                    // offsets from the right edge, see repo_header_item).
                    let w = self.last_width;
                    if x >= w.saturating_sub(3) && x < w {
                        self.commit_repo(r);
                    } else if x >= w.saturating_sub(6) && x < w.saturating_sub(3) {
                        self.sync_repo(r);
                    } else {
                        self.activate();
                    }
                }
                Row::StagedHeader(_) | Row::ChangesHeader(_) | Row::DrawerHeader(_) => {
                    self.focus = Focus::List;
                    self.select(index);
                    self.activate();
                }
                _ => {
                    self.focus = Focus::List;
                    self.select(index);
                }
            }
        }
        None
    }

    /// Ctrl+right-click: the VS Code-style context menu for a file row.
    fn open_context_menu(&mut self, x: u16, y: u16) {
        let Some(index) = self.row_at(y) else { return };
        self.select(index);
        let (repo, entry, staged) = match self.rows[index] {
            Row::Staged(r, i) => (r, self.repos[r].status.staged.get(i), true),
            Row::Unstaged(r, i) => (r, self.repos[r].status.unstaged.get(i), false),
            _ => return, // headers and drawer lines have no menu
        };
        let Some(entry) = entry.cloned() else { return };
        let mut entries = vec![MenuEntry::Action(
            MenuAction::StageOrUnstage,
            if staged { "Unstage Changes" } else { "Stage Changes" },
        )];
        if !staged {
            entries.push(MenuEntry::Action(MenuAction::Discard, "Discard Changes…"));
        }
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
            MenuEntry::Action(MenuAction::CopyRelativePath, "Copy Relative Path"),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
        ]);
        self.overlay = Some(Overlay::Menu {
            x,
            y,
            target: (repo, entry, staged),
            entries,
            selected: 0,
            rect: Rect::default(),
        });
    }

    fn overlay_key(&mut self, key: KeyEvent) {
        enum Cmd {
            Nothing,
            Close,
            Activate,
            ToggleSetting(usize),
            DiscardConfirmed(usize, FileEntry),
        }
        let row_count = self.settings_rows().len();
        let cmd = match self.overlay.as_mut() {
            Some(Overlay::Menu { entries, selected, .. }) => match key.code {
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
                KeyCode::Enter | KeyCode::Char(' ') => Cmd::ToggleSetting(*selected),
                _ => Cmd::Nothing,
            },
            Some(Overlay::ConfirmDiscard { repo, entry }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Cmd::DiscardConfirmed(*repo, entry.clone())
                }
                _ => Cmd::Close,
            },
            None => Cmd::Nothing,
        };
        match cmd {
            Cmd::Nothing => {}
            Cmd::Close => self.overlay = None,
            Cmd::Activate => self.activate_menu_entry(),
            Cmd::ToggleSetting(index) => self.toggle_setting(index),
            Cmd::DiscardConfirmed(repo, entry) => {
                self.overlay = None;
                let result = match self.repos.get(repo) {
                    Some(r) => r.git.discard(&entry),
                    None => Err("repository is gone".to_string()),
                };
                match result {
                    Ok(()) => self.flash = Some((format!("discarded {}", entry.path), false)),
                    Err(e) => self.flash = Some((e, true)),
                }
                self.refresh();
            }
        }
    }

    fn overlay_mouse(&mut self, mouse: MouseEvent) {
        enum Cmd {
            Nothing,
            Close,
            Activate,
            ToggleSetting(usize),
            Reopen(u16, u16),
        }
        let row_count = self.settings_rows().len();
        let cmd = match self.overlay.as_mut() {
            Some(Overlay::Settings { selected, rect }) => {
                // Rows start just inside the top border (the title renders ON
                // the border, not on its own line).
                let row_at = |row: u16, col: u16| -> Option<usize> {
                    (col > rect.x
                        && col < rect.x + rect.width.saturating_sub(1)
                        && row > rect.y
                        && row < rect.y + 1 + row_count as u16)
                        .then(|| usize::from(row - rect.y - 1))
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
                            None if hits(*rect, mouse.column, mouse.row) => Cmd::Nothing,
                            None => Cmd::Close,
                        }
                    }
                    _ => Cmd::Nothing,
                }
            }
            Some(Overlay::Menu { entries, selected, rect, .. }) => {
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
                    MouseEventKind::Down(MouseButton::Right) => Cmd::Reopen(mouse.column, mouse.row),
                    _ => Cmd::Nothing,
                }
            }
            // The discard confirm is keyboard-driven (y/N); clicks do nothing.
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
        }
    }

    // ---- Settings modal ----

    fn open_settings(&mut self) {
        self.overlay = Some(Overlay::Settings { selected: 0, rect: Rect::default() });
    }

    /// The modal's rows for the current state.
    fn settings_rows(&self) -> Vec<SettingRow> {
        vec![
            (
                Setting::UnifiedSidebar,
                "Unified sidebar",
                if self.merged() { "on" } else { "off" }.to_string(),
                self.other_exe.is_some(),
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
            Setting::UnifiedSidebar => {
                // The pane layout changes underneath the modal; close it.
                self.overlay = None;
                let on = !self.merged();
                self.set_unified(on);
            }
            Setting::IconTheme => self.theme = self.theme.toggled(),
        }
    }

    /// Render the centered Settings popup and remember its rect for clicks.
    fn draw_settings(&mut self, frame: &mut Frame) {
        let rows = self.settings_rows();
        let Some(Overlay::Settings { selected, rect }) = self.overlay.as_mut() else {
            return;
        };
        let area = frame.area();
        let width = 30.min(area.width);
        let height = (rows.len() as u16 + 3).min(area.height);
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 3,
            width,
            height,
        );
        *rect = popup;

        let inner_w = usize::from(width.saturating_sub(2));
        let mut lines: Vec<Line> = Vec::new();
        for (i, (_, label, value, enabled)) in rows.iter().enumerate() {
            let pad = inner_w.saturating_sub(label.chars().count() + value.chars().count() + 2);
            let text = format!(" {label}{}{value} ", " ".repeat(pad.max(1)));
            let style = if !enabled {
                Style::default().dim()
            } else if i == *selected {
                Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::styled(text, style));
        }
        lines.push(Line::from(" click/⏎ toggle · esc close".dim()));

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .title(" Settings ")
                    .border_style(Style::default().dim()),
            ),
            popup,
        );
    }

    fn activate_menu_entry(&mut self) {
        let Some(Overlay::Menu { target, entries, selected, .. }) = self.overlay.take() else {
            return;
        };
        let MenuEntry::Action(action, _) = entries[selected] else { return };
        let (repo, entry, staged) = target;
        let repo_root = self.repos.get(repo).map(|r| r.git.root().to_path_buf());
        match action {
            MenuAction::StageOrUnstage => {
                let result = match self.repos.get(repo) {
                    Some(r) if staged => r.git.unstage(&entry),
                    Some(r) => r.git.stage(&entry),
                    None => Err("repository is gone".to_string()),
                };
                if let Err(e) = result {
                    self.flash = Some((e, true));
                }
                self.refresh();
            }
            MenuAction::Discard => self.overlay = Some(Overlay::ConfirmDiscard { repo, entry }),
            MenuAction::CopyPath | MenuAction::CopyRelativePath => {
                let rel = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
                let text = if action == MenuAction::CopyPath {
                    repo_root.unwrap_or_else(|| self.cwd.clone()).join(&rel).display().to_string()
                } else {
                    rel
                };
                self.flash = Some(match copy_to_clipboard(&text) {
                    Ok(()) => (format!("copied: {text}"), false),
                    Err(err) => (format!("copy failed: {err}"), true),
                });
            }
            MenuAction::Reveal => {
                let rel = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
                let path = repo_root.unwrap_or_else(|| self.cwd.clone()).join(rel);
                reveal(&path);
            }
        }
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
        self.sidebar_state = sidebar::State { merged: on, active: MY_VIEW };
        sidebar::save_state(self.sidebar_state);
        self.apply_identity();
        if on {
            self.close_other_standalone_pane();
        } else {
            self.spawn_other_pane();
        }
    }

    /// Hand the pane to the other view (the supervisor swaps processes).
    fn switch_to(&mut self, view: View) -> Option<Exit> {
        if !self.merged() || view == MY_VIEW {
            return None;
        }
        self.sidebar_state.active = view;
        sidebar::save_state(self.sidebar_state);
        Some(Exit::Switch)
    }

    /// Close the other panel's standalone pane in our tab, if one is open.
    fn close_other_standalone_pane(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        let Ok(json) = herdr_aa_sidebar::ipc::call_text("pane.list", serde_json::json!({})) else {
            return;
        };
        for id in sibling_panes_of(&json, &ctl.pane_id, MY_VIEW.other()) {
            let _ = herdr_aa_sidebar::ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }));
        }
    }

    /// Open the other view in a fresh pane beside this one (detach).
    fn spawn_other_pane(&self) {
        let (Some(ctl), Some(exe)) = (&self.pane_ctl, &self.other_exe) else { return };
        // Grow to double width FIRST, then split 50/50 — each separated panel
        // keeps the width the unified sidebar had, instead of halving.
        ctl.resize_to(self.last_width, self.last_width.saturating_mul(2).saturating_add(1));
        let response = herdr_aa_sidebar::ipc::call_text(
            "pane.split",
            serde_json::json!({
                "target_pane_id": ctl.pane_id,
                "direction": "right",
                "ratio": 0.5,
                "focus": false,
                "cwd": self.cwd.display().to_string(),
            }),
        );
        let Some(new_pane) = response.ok().and_then(|r| herdr_aa_sidebar::launch::split_pane_id(&r)) else {
            return;
        };
        let flag = MY_VIEW.other().view_flag();
        #[cfg(windows)]
        let command = format!("& \"{}\" --view {flag}", exe.display());
        #[cfg(not(windows))]
        let command = format!("exec \"{}\" --view {flag}", exe.display());
        let _ = herdr_aa_sidebar::ipc::call_text(
            "pane.send_input",
            serde_json::json!({ "pane_id": new_pane, "text": command, "keys": ["Enter"] }),
        );
        let _ = herdr_aa_sidebar::ipc::call_text(
            "pane.rename",
            serde_json::json!({ "pane_id": new_pane, "label": MY_VIEW.other().label() }),
        );
    }

    // ---- Git operations ----

    fn select(&mut self, index: usize) {
        if !self.rows.is_empty() {
            self.list.select(Some(index.min(self.rows.len() - 1)));
            self.follow_selection();
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let current = self.list.selected().unwrap_or(0) as isize;
        let step = if delta >= 0 { 1 } else { -1 };
        let mut next = (current + delta).clamp(0, len - 1);
        // Widget rows aren't keyboard stops: keep going in the same direction,
        // falling back to the nearest stop at the ends.
        while (0..len).contains(&next) && !self.rows[next as usize].selectable() {
            next += step;
        }
        let next = if (0..len).contains(&next) {
            next as usize
        } else {
            self.nearest_selectable((current + delta).clamp(0, len - 1) as usize)
        };
        self.select(next);
    }

    /// Enter/Space on the selected row: toggle a section/drawer, or move a
    /// file between the staged and unstaged lists.
    fn activate(&mut self) {
        let Some(&row) = self.list.selected().and_then(|i| self.rows.get(i)) else {
            return;
        };
        match row {
            // Widget rows aren't keyboard-selectable; nothing to activate.
            Row::Message(_) | Row::Commit(_) => {}
            Row::RepoHeader(r) => {
                self.repos[r].collapsed = !self.repos[r].collapsed;
                self.rebuild();
            }
            Row::StagedHeader(r) => {
                self.repos[r].staged_collapsed = !self.repos[r].staged_collapsed;
                self.rebuild();
            }
            Row::ChangesHeader(r) => {
                self.repos[r].changes_collapsed = !self.repos[r].changes_collapsed;
                self.rebuild();
            }
            Row::DrawerHeader(kind) => {
                self.drawers[kind.index()].expanded = !self.drawers[kind.index()].expanded;
                self.reload_expanded_drawers();
                self.rebuild();
            }
            Row::DrawerLine(..) => {}
            Row::Staged(r, i) => self.run_op(|git, e| git.unstage(e), r, i, true),
            Row::Unstaged(r, i) => self.run_op(|git, e| git.stage(e), r, i, false),
        }
    }

    fn run_op(
        &mut self,
        op: impl Fn(&Git, &FileEntry) -> Result<(), String>,
        repo: usize,
        index: usize,
        staged: bool,
    ) {
        let Some(repo) = self.repos.get(repo) else { return };
        let list = if staged { &repo.status.staged } else { &repo.status.unstaged };
        let Some(entry) = list.get(index) else { return };
        if let Err(e) = op(&repo.git, entry) {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    fn stage_all(&mut self) {
        let Some(repo) = self.active_repo() else { return };
        if let Err(e) = repo.git.stage_all() {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    fn unstage_all(&mut self) {
        let Some(repo) = self.active_repo() else { return };
        if let Err(e) = repo.git.unstage_all() {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    /// Kick off ✧ commit-message generation in the background.
    fn suggest_message(&mut self) {
        if self.suggesting.is_some() {
            return;
        }
        let Some(repo) = self.active_repo() else { return };
        match repo.git.diff_for_message() {
            Ok((diff, files)) if diff.trim().is_empty() && files.is_empty() => {
                self.flash = Some(("no changes to describe".into(), true));
            }
            Ok((diff, files)) => {
                self.suggesting = Some(suggest::spawn(diff, files));
                self.flash = Some(("✧ generating commit message…".into(), false));
            }
            Err(e) => self.flash = Some((e, true)),
        }
    }

    /// VS Code's Sync Changes (pull --rebase, then push) on a background
    /// thread; tick() collects the outcome.
    fn sync_changes(&mut self) {
        self.sync_repo(self.active);
    }

    fn sync_repo(&mut self, index: usize) {
        if self.syncing.is_some() {
            return;
        }
        let Some(repo) = self.repos.get(index) else { return };
        if !repo.status.has_upstream {
            self.flash = Some(("no upstream to sync with".into(), true));
            return;
        }
        let git = repo.git.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(git.sync());
        });
        self.syncing = Some(rx);
    }

    fn commit(&mut self) {
        self.commit_repo(self.active);
    }

    fn commit_repo(&mut self, index: usize) {
        let Some(repo) = self.repos.get_mut(index) else { return };
        let message: String = repo.message.iter().collect();
        if message.trim().is_empty() {
            self.active = index;
            self.flash = Some(("Commit message is empty.".to_string(), true));
            self.focus = Focus::Message;
            return;
        }
        if repo.status.staged.is_empty() {
            self.flash = Some(("No staged changes to commit.".to_string(), true));
            return;
        }
        match repo.git.commit(message.trim()) {
            Ok(summary) => {
                self.flash = Some((summary, false));
                repo.message.clear();
                repo.cursor = 0;
                self.focus = Focus::List;
            }
            Err(e) => self.flash = Some((e, true)),
        }
        self.refresh();
    }

    /// The visible row at a pane-local mouse row plus the line within it
    /// (rows vary in height: the inline message boxes are 3 lines tall).
    fn row_hit(&self, mouse_row: u16) -> Option<(usize, u16)> {
        if mouse_row < self.body.top || mouse_row >= self.body.top + self.body.height {
            return None;
        }
        let mut y = self.body.top;
        for index in self.body.offset..self.rows.len() {
            let h = self.rows[index].height();
            if mouse_row < y + h {
                return Some((index, mouse_row - y));
            }
            y += h;
        }
        None
    }

    /// The visible row index at a pane-local mouse row, if it lands on one.
    fn row_at(&self, mouse_row: u16) -> Option<usize> {
        self.row_hit(mouse_row).map(|(index, _)| index)
    }

    /// The screen row where `index`'s first line is drawn, if visible.
    fn row_y(&self, index: usize) -> Option<u16> {
        let mut y = self.body.top;
        for i in self.body.offset..self.rows.len() {
            if i == index {
                return (y < self.body.top + self.body.height).then_some(y);
            }
            y += self.rows[i].height();
        }
        None
    }

    // ---- Rendering ----

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.last_width = area.width;

        if self.repos.is_empty() {
            let text = format!(
                "Not a git repository.\n\n{}\n\nOpen this pane inside a repo,\nor press q to quit.",
                self.discover_err,
            );
            frame.render_widget(Paragraph::new(text).dim().wrap(Wrap { trim: false }), area);
            return;
        }

        // With several repos, VS Code puts a message box + Commit button
        // INSIDE each repo's section (rendered as list rows); the single-repo
        // view keeps them fixed at the top. The Sync Changes row only appears
        // when there is something to sync (or a sync is running).
        let multi = self.multi();
        let message_height = if multi { 0 } else { 3 };
        let button_height = u16::from(!multi);
        let sync_height = u16::from(!multi && self.sync_label().is_some());
        let footer_lines = self.footer_lines(area.width);
        // A breathing row above and below the icons keeps the activity bar
        // from crowding the pane border.
        let activity_height = if self.merged() { 3 } else { 0 };
        let [activity, header, message, button, sync, list, footer] = Layout::vertical([
            Constraint::Length(activity_height),
            Constraint::Length(1),
            Constraint::Length(message_height),
            Constraint::Length(button_height),
            Constraint::Length(sync_height),
            Constraint::Min(0),
            Constraint::Length(footer_lines.len() as u16),
        ])
        .areas(area);
        self.page = list.height.saturating_sub(1).max(1) as usize;

        if self.merged() {
            self.draw_activity_bar(frame, activity);
        }
        self.draw_header(frame, header);
        if !multi {
            self.draw_message(frame, message);
            self.draw_button(frame, button);
            self.draw_sync(frame, sync);
        } else {
            self.zones.message = Rect::default();
            self.zones.sparkle = Rect::default();
            self.zones.button = Rect::default();
            self.zones.sync = Rect::default();
        }
        self.draw_list(frame, list);
        frame.render_widget(Paragraph::new(footer_lines), footer);

        match self.overlay {
            Some(Overlay::Menu { .. }) => self.draw_menu(frame),
            Some(Overlay::Settings { .. }) => self.draw_settings(frame),
            _ => {}
        }
    }

    /// The VS Code activity bar: view-switcher icons plus a detach button.
    /// The area is three rows tall — icons on the middle one, one blank
    /// spacer row each side.
    fn draw_activity_bar(&mut self, frame: &mut Frame, area: Rect) {
        // Three rows in the plain pane background; only the ACTIVE icon's
        // highlight chip extends into the outer rows by a half block — a tall
        // button with built-in breathing room, no strip container.
        let outer_top = area.y;
        let outer_bottom = area.y + 2;
        let area = Rect::new(area.x, area.y + 1, area.width, 1);
        let (exp_icon, git_icon) = activity_icons(self.theme);
        let active = |on: bool| {
            if on {
                Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else {
                Style::default().dim()
            }
        };
        // Both FA glyphs (folder, code-fork) render two cells wide in the
        // non-Mono Nerd Font; reserve the second cell in each chip so the
        // highlights are equal-sized with centered icons.
        let slack = if self.theme == IconTheme::Material { " " } else { "" };
        let spans = [
            Span::raw(" "),
            Span::styled(format!(" {exp_icon}{slack} "), active(false)),
            Span::raw(" "),
            Span::styled(format!(" {git_icon}{slack} "), active(true)),
        ];
        // Hit zones from the actual span widths (emoji vs nerd-glyph widths differ).
        let mut x = area.x;
        let mut bounds = Vec::new();
        for span in &spans {
            let w = span.width() as u16;
            bounds.push((x, x + w));
            x += w;
        }
        self.zones.activity_row = area.y;
        self.zones.explorer = bounds[1];
        self.zones.source_control = bounds[3];
        // Symmetric half-block caps: a 2-cell button with the icon in its
        // vertical center.
        let (chip_start, chip_end) = bounds[3];
        let chip_w = chip_end.saturating_sub(chip_start);
        let cap = |glyph: &str| {
            Paragraph::new(glyph.repeat(usize::from(chip_w)))
                .style(Style::default().fg(Color::DarkGray))
        };
        frame.render_widget(cap("▄"), Rect::new(chip_start, outer_top, chip_w, 1));
        frame.render_widget(cap("▀"), Rect::new(chip_start, outer_bottom, chip_w, 1));
        let gear = Span::styled(format!(" {} ", gear_icon(self.theme)), Style::default().dim());
        let gear_w = gear.width() as u16;
        let gear_x = area.x + area.width.saturating_sub(gear_w);
        self.zones.gear = Rect::new(gear_x, area.y, gear_w, 1);

        let pad = usize::from(area.width)
            .saturating_sub(spans.iter().map(Span::width).sum::<usize>() + usize::from(gear_w));
        let mut line = spans.to_vec();
        line.push(Span::raw(" ".repeat(pad)));
        line.push(gear);
        frame.render_widget(Paragraph::new(Line::from(line)), area);
    }

    fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
        let left = Span::styled(" ▾ CHANGES", Style::default().bold());
        // With several repos visible, the header names the one the commit box
        // and sync act on.
        let right_text = match self.active_repo() {
            Some(repo) if self.repos.len() > 1 => {
                format!("{} · {} ", repo.name, repo.status.branch)
            }
            Some(repo) => format!("{} ", repo.status.branch),
            None => String::new(),
        };
        let branch = Span::styled(right_text, Style::default().dim());
        // In unified mode the ⚙ lives in the activity bar; standalone puts it
        // at the header's right edge.
        let gear = if self.merged() {
            None
        } else {
            Some(Span::styled(format!("{} ", gear_icon(self.theme)), Style::default().dim()))
        };
        let gear_w = gear.as_ref().map(Span::width).unwrap_or(0);
        let pad = (area.width as usize)
            .saturating_sub(left.width() + branch.width() + gear_w)
            .max(1);
        let mut spans = vec![left, Span::raw(" ".repeat(pad)), branch];
        if let Some(gear) = gear {
            let gx = area.x + area.width.saturating_sub(gear_w as u16);
            self.zones.gear = Rect::new(gx, area.y, gear_w as u16, 1);
            spans.push(gear);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_message(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Message;
        let border = if focused {
            Style::default().fg(BUTTON_BLUE)
        } else {
            Style::default().dim()
        };
        let boxed = Block::bordered().border_style(border);
        let inner = boxed.inner(area);
        frame.render_widget(boxed, area);
        self.zones.message = area;

        // The suggest button lives at the right end of the input line — a
        // monochrome OUTLINE of the ✨ sparkles shape (MDI "creation" in the
        // material theme) in the normal foreground, never the colored emoji.
        let sparkle_glyph =
            if self.suggesting.is_some() { "…" } else { sparkle_icon(self.theme) };
        let sparkle_w = Span::raw(sparkle_glyph).width() as u16 + 1;
        let [text_area, sparkle_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(sparkle_w)]).areas(inner);
        frame.render_widget(Paragraph::new(sparkle_glyph), sparkle_area);
        self.zones.sparkle = sparkle_area;

        let (message, cursor, branch) = match self.active_repo() {
            Some(r) => (r.message.clone(), r.cursor, r.status.branch.clone()),
            None => (Vec::new(), 0, String::new()),
        };
        if message.is_empty() && !focused {
            let placeholder = format!("Message (⏎ to commit on \"{branch}\")");
            frame.render_widget(Paragraph::new(placeholder).dim().italic(), text_area);
            return;
        }

        // Single-line input with horizontal scroll keeping the cursor visible.
        let width = text_area.width.saturating_sub(1) as usize;
        let start = cursor.saturating_sub(width);
        let visible: String = message.iter().skip(start).take(width.max(1)).collect();
        frame.render_widget(Paragraph::new(visible), text_area);
        if focused {
            frame.set_cursor_position(Position::new(
                text_area.x + (cursor - start) as u16,
                text_area.y,
            ));
        }
    }

    fn draw_button(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Commit;
        let bg = if focused { BUTTON_BLUE_FOCUS } else { BUTTON_BLUE };
        let mut style = Style::default().bg(bg).fg(Color::White);
        if focused {
            style = style.add_modifier(Modifier::BOLD);
        }
        frame.render_widget(Paragraph::new("✓ Commit").centered().style(style), area);
        self.zones.button = area;
    }

    /// The Sync Changes label, or `None` while there is nothing to sync
    /// (which hides the row entirely).
    fn sync_label(&self) -> Option<String> {
        if self.syncing.is_some() {
            return Some("⇅ Syncing…".to_string());
        }
        let status = &self.active_repo()?.status;
        if !status.has_upstream || status.ahead + status.behind == 0 {
            return None;
        }
        Some(format!("⇅ Sync Changes  {}↑ {}↓", status.ahead, status.behind))
    }

    /// A secondary button below Commit, VS Code's Sync Changes: pull + push
    /// with the outgoing↑ / incoming↓ counts.
    fn draw_sync(&mut self, frame: &mut Frame, area: Rect) {
        self.zones.sync = area;
        let Some(label) = self.sync_label() else { return };
        let style = if self.syncing.is_some() {
            Style::default().bg(Color::Rgb(0x2d, 0x2d, 0x33)).fg(Color::Gray)
        } else {
            Style::default().bg(Color::Rgb(0x3a, 0x3d, 0x41)).fg(Color::White)
        };
        frame.render_widget(Paragraph::new(label).centered().style(style), area);
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width as usize;
        let theme = self.theme;
        let hovered = self.hovered;
        let active = self.active;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let item = match *row {
                    Row::RepoHeader(r) => {
                        repo_header_item(&self.repos[r], r == active, theme, width)
                    }
                    Row::Message(r) => message_box_item(
                        &self.repos[r],
                        r == active && self.focus == Focus::Message,
                        theme,
                        width,
                    ),
                    Row::Commit(r) => commit_button_item(
                        r == active,
                        r == active && self.focus == Focus::Commit,
                        width,
                    ),
                    Row::StagedHeader(r) => section_item(
                        "Staged Changes",
                        self.repos[r].staged_collapsed,
                        Some(self.repos[r].status.staged.len()),
                        width,
                    ),
                    Row::ChangesHeader(r) => section_item(
                        "Changes",
                        self.repos[r].changes_collapsed,
                        Some(self.repos[r].status.unstaged.len()),
                        width,
                    ),
                    Row::DrawerHeader(kind) => {
                        let mut item = section_item(
                            kind.title(),
                            !self.drawers[kind.index()].expanded,
                            None,
                            width,
                        );
                        if kind == Drawer::FileHistory
                            && let Some(target) = &self.history_target
                        {
                            let name = target.rsplit('/').next().unwrap_or(target);
                            item = file_history_header(
                                !self.drawers[kind.index()].expanded,
                                name,
                            );
                        }
                        item
                    }
                    Row::DrawerLine(kind, i) => {
                        drawer_line(kind, &self.drawers[kind.index()].lines[i])
                    }
                    Row::Staged(r, i) => file_item(&self.repos[r].status.staged[i], width, theme),
                    Row::Unstaged(r, i) => {
                        file_item(&self.repos[r].status.unstaged[i], width, theme)
                    }
                };
                if hovered == Some(i) {
                    item.style(Style::default().bg(HOVER_BG))
                } else {
                    item
                }
            })
            .collect();
        let highlight = if self.focus == Focus::List {
            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::Rgb(0x2a, 0x2d, 0x2e))
        };
        let list = List::new(items).highlight_style(highlight);
        frame.render_stateful_widget(list, area, &mut self.list);
        self.body = BodyGeom {
            top: area.y,
            height: area.height,
            offset: self.list.offset(),
        };

        // Terminal cursor inside the focused INLINE message box (multi-repo).
        if self.multi() && self.focus == Focus::Message {
            let target = self
                .rows
                .iter()
                .position(|row| matches!(row, Row::Message(r) if *r == self.active));
            if let Some(index) = target
                && let Some(y) = self.row_y(index)
                && y + 1 < area.y + area.height
                && let Some(repo) = self.active_repo()
            {
                let field = usize::from(inline_field_width(self.last_width));
                let start = repo.cursor.saturating_sub(field.saturating_sub(1));
                frame.set_cursor_position(Position::new(
                    area.x + 1 + (repo.cursor - start) as u16,
                    y + 1,
                ));
            }
        }
    }

    /// Footer content: a flash message, or the hotkey hints wrapped so they
    /// never clip in a narrow sidebar.
    fn footer_lines(&self, width: u16) -> Vec<Line<'static>> {
        if let Some(Overlay::ConfirmDiscard { entry, .. }) = &self.overlay {
            return vec![Line::styled(
                format!(" Discard changes to '{}'? (y/N)", entry.path),
                Style::default().fg(DELETED),
            )];
        }
        if let Some((text, is_error)) = &self.flash {
            let color = if *is_error { DELETED } else { UNTRACKED };
            let prefix = if *is_error { " " } else { " ✓ " };
            return vec![Line::styled(
                format!("{prefix}{text}"),
                Style::default().fg(color),
            )];
        }
        let mut hints: Vec<(&'static str, &'static str)> = vec![
            ("⏎", "stage"),
            ("a", "all"),
            ("u", "none"),
            ("c", "msg"),
            ("A", "suggest"),
            ("S", "sync"),
            ("s", "settings"),
            ("r", "refresh"),
            ("q", "quit"),
        ];
        if self.merged() {
            hints.extend([("1", "files"), ("2", "git")]);
        }
        wrap_hints(&hints, width, 0)
    }

    /// Render the context-menu popup near its anchor, clamped inside the pane,
    /// and remember its rect for mouse hit-testing.
    fn draw_menu(&mut self, frame: &mut Frame) {
        let Some(Overlay::Menu { x, y, entries, selected, rect, .. }) = self.overlay.as_mut()
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
                        ListItem::new(line).style(
                            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD),
                        )
                    } else {
                        ListItem::new(line)
                    }
                }
            })
            .collect();
        frame.render_widget(Clear, popup);
        frame.render_widget(
            List::new(items)
                .block(Block::bordered().border_style(Style::default().dim())),
            popup,
        );
    }
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

/// A repository row, matching VS Code's multi-repo Source Control: disclosure
/// arrow, repo icon and name on the left; branch (starred when dirty) and the
/// ⟳ sync / ✓ commit action icons on the right. The right-edge icon columns
/// are FIXED (last 6 cells) — left_click's hit zones rely on that.
fn repo_header_item(repo: &Repo, active: bool, theme: IconTheme, width: usize) -> ListItem<'static> {
    let arrow = if repo.collapsed { "▸" } else { "▾" };
    let repo_icon = icon(theme, "", true, false);
    let name_style = if active { Style::default().bold() } else { Style::default().dim().bold() };
    let left = vec![
        Span::styled(format!(" {arrow} "), Style::default().bold()),
        Span::raw(format!("{} ", repo_icon.glyph)),
        Span::styled(repo.name.clone(), name_style),
    ];
    let branch = Span::styled(
        format!("{} {}", branch_icon(theme), repo.branch_decor()),
        Style::default().dim(),
    );
    let icons = Span::styled(" ⟳  ✓ ", Style::default().dim());
    let used: usize =
        left.iter().map(Span::width).sum::<usize>() + branch.width() + icons.width();
    let pad = width.saturating_sub(used).max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(branch);
    spans.push(icons);
    ListItem::new(Line::from(spans))
}

/// Columns the inline message box's input field spans (between the left
/// border and the ✧ button).
fn inline_field_width(pane_width: u16) -> u16 {
    pane_width.saturating_sub(2 + 3)
}

/// A repo's inline message box, VS Code style: a 3-line bordered input with
/// the ✧ suggest button at its right end.
fn message_box_item(
    repo: &Repo,
    focused: bool,
    theme: IconTheme,
    width: usize,
) -> ListItem<'static> {
    let border = if focused {
        Style::default().fg(BUTTON_BLUE)
    } else {
        Style::default().dim()
    };
    let horizontal = "─".repeat(width.saturating_sub(2));
    let field = usize::from(inline_field_width(width as u16));

    let content: Span = if repo.message.is_empty() && !focused {
        let placeholder = truncate_to(
            format!("Message (⏎ to commit on \"{}\")", repo.status.branch),
            field,
        );
        Span::styled(placeholder, Style::default().dim().italic())
    } else {
        let start = repo.cursor.saturating_sub(field.saturating_sub(1));
        let visible: String = repo.message.iter().skip(start).take(field.max(1)).collect();
        Span::raw(visible)
    };
    let pad = field.saturating_sub(content.width());
    let middle = Line::from(vec![
        Span::styled("│", border),
        content,
        Span::raw(" ".repeat(pad)),
        Span::raw(format!("{} ", sparkle_icon(theme))),
        Span::styled("│", border),
    ]);
    ListItem::new(vec![
        Line::from(Span::styled(format!("┌{horizontal}┐"), border)),
        middle,
        Line::from(Span::styled(format!("└{horizontal}┘"), border)),
    ])
}

/// A repo's inline ✓ Commit button with the VS Code dropdown chevron at its
/// right end; only the active repo's button is fully lit.
fn commit_button_item(active: bool, focused: bool, width: usize) -> ListItem<'static> {
    let (bg, fg) = match (active, focused) {
        (true, true) => (BUTTON_BLUE_FOCUS, Color::White),
        (true, false) => (BUTTON_BLUE, Color::White),
        (false, _) => (Color::Rgb(0x24, 0x45, 0x5c), Color::Rgb(0x9a, 0xb2, 0xc2)),
    };
    let label = "✓ Commit";
    let body_w = width.saturating_sub(2);
    let left_pad = body_w.saturating_sub(label.chars().count()) / 2;
    let right_pad = body_w.saturating_sub(left_pad + label.chars().count());
    let mut style = Style::default().bg(bg).fg(fg);
    if focused {
        style = style.add_modifier(Modifier::BOLD);
    }
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{}{label}{}", " ".repeat(left_pad), " ".repeat(right_pad)),
            style,
        ),
        Span::styled("│∨", style.dim()),
    ]))
}

/// A collapsible section header; `count` renders as a right-aligned badge
/// (the drawers have no badge, like Git Graph's).
fn section_item(
    title: &str,
    collapsed: bool,
    count: Option<usize>,
    width: usize,
) -> ListItem<'static> {
    let arrow = if collapsed { "▸" } else { "▾" };
    let left = Span::styled(format!(" {arrow} {title}"), Style::default().bold());
    let Some(count) = count else {
        return ListItem::new(Line::from(left));
    };
    let badge = Span::styled(
        format!(" {count} "),
        Style::default().bg(BADGE_BLUE).fg(Color::White),
    );
    let pad = width.saturating_sub(left.width() + badge.width() + 1).max(1);
    ListItem::new(Line::from(vec![
        left,
        Span::raw(" ".repeat(pad)),
        badge,
        Span::raw(" "),
    ]))
}

/// The FILE HISTORY header with the followed file's name appended, dimmed.
fn file_history_header(collapsed: bool, file: &str) -> ListItem<'static> {
    let arrow = if collapsed { "▸" } else { "▾" };
    ListItem::new(Line::from(vec![
        Span::styled(format!(" {arrow} FILE HISTORY"), Style::default().bold()),
        Span::styled(format!("  {file}"), Style::default().dim()),
    ]))
}

/// One content line inside an expanded drawer. Branch lines highlight the
/// current branch (git's `%(HEAD)` renders it as `* name`).
fn drawer_line(kind: Drawer, text: &str) -> ListItem<'static> {
    let style = match kind {
        Drawer::Branches if text.starts_with('*') => {
            Style::default().fg(UNTRACKED).bold()
        }
        _ => Style::default(),
    };
    ListItem::new(Line::from(Span::styled(format!("   {text}"), style)))
}

/// A file row: icon, name colored by status, dimmed parent directory, and a
/// right-aligned status letter — VS Code Source Control's row anatomy.
fn file_item(entry: &FileEntry, width: usize, theme: IconTheme) -> ListItem<'static> {
    let (dir, name) = match entry.path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, entry.path.as_str()),
    };
    let color = letter_color(entry.letter);
    let file_icon = icon(theme, name, false, false);
    let icon_style = match file_icon.rgb {
        Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)),
        None => Style::default(),
    };
    let mut spans = vec![
        Span::raw("   "),
        Span::styled(format!("{} ", file_icon.glyph), icon_style),
        Span::styled(name.to_string(), Style::default().fg(color)),
    ];
    if let Some(dir) = dir {
        let sep = std::path::MAIN_SEPARATOR.to_string();
        // The status letter must survive narrow panes: give the dimmed dir only
        // the room left after icon + name + letter, ellipsizing like VS Code.
        let used: usize = spans.iter().map(Span::width).sum();
        let avail = width.saturating_sub(used + 3);
        let text = truncate_to(format!(" {}", dir.replace('/', &sep)), avail);
        if !text.is_empty() {
            spans.push(Span::styled(text, Style::default().dim()));
        }
    }
    let letter = Span::styled(entry.letter.to_string(), Style::default().fg(color).bold());
    let left_width: usize = spans.iter().map(Span::width).sum();
    let pad = width.saturating_sub(left_width + 2).max(1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(letter);
    spans.push(Span::raw(" "));
    ListItem::new(Line::from(spans))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_navigation_skips_separators_and_clamps() {
        let entries = [
            MenuEntry::Action(MenuAction::StageOrUnstage, "Stage Changes"),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
        ];
        assert_eq!(step_menu(&entries, 0, -1), 0);
        assert_eq!(step_menu(&entries, 0, 1), 2, "skips the separator");
        assert_eq!(step_menu(&entries, 2, 1), 2);
    }

    #[test]
    fn drawer_titles_match_git_graph() {
        let titles: Vec<&str> = Drawer::ALL.iter().map(|d| d.title()).collect();
        assert_eq!(
            titles,
            ["GRAPH", "COMMITS", "FILE HISTORY", "BRANCHES", "REMOTES", "STASHES", "TAGS"]
        );
    }

}
