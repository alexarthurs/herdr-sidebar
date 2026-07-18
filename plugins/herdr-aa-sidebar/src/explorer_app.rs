//! TUI state and rendering: a VS Code Explorer-style tree with disclosure arrows,
//! nested indentation, per-file-type icons, and a VS Code-like collapse-to-sliver
//! (the `«` button, or `b`): the pane narrows to a strip with EXPLORER written
//! sideways, resized through the herdr CLI since only the host controls pane size.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use herdr_aa_sidebar::actions::{self, MenuAction, MenuEntry};
use herdr_aa_sidebar::icons::{IconTheme, icon};
use herdr_aa_sidebar::state::{self as sidebar, View};
use herdr_aa_sidebar::ui::{
    activity_icons, gear_icon, hits_collapse_button, sibling_panes_of, sliver_lines,
    sliver_view_at, wrap_hints,
};
use herdr_aa_sidebar::tree::{Row, Tree};

use herdr_aa_sidebar::state::Exit;

const MY_VIEW: View = View::Explorer;

/// Below this pane width the explorer renders as the collapsed sliver.
const SLIVER_THRESHOLD: u16 = 14;
/// Width we ask herdr for when collapsing (herdr's 0.1 min split ratio may keep
/// the pane a little wider — the sliver rendering adapts to whatever we get).
const SLIVER_TARGET: u16 = 5;
/// Expanded width to restore when nothing better is known.
const DEFAULT_EXPANDED_WIDTH: u16 = 32;

/// Handle for resizing our own pane through the herdr socket API.
struct PaneCtl {
    pane_id: String,
}

impl PaneCtl {
    fn from_env() -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|id| !id.is_empty())?;
        Some(Self { pane_id })
    }

    /// Report identity tokens: always our own (so the ensure logic recognizes
    /// this pane even while the cosmetic label is cleared); in merged mode
    /// also the other view's — one Sidebar pane satisfies both plugins'
    /// launchers — otherwise clear the other view's token.
    fn report_tokens(&self, my: View, merged: bool) {
        // Token value = heartbeat timestamp; launchers treat stale stamps as
        // dead panes and replace them (see launch::HEARTBEAT_STALE_SECS).
        let now = sidebar::unix_now().to_string();
        let mine = serde_json::json!({ my.plugin_id(): now });
        let _ = herdr_aa_sidebar::ipc::call_text(
            "pane.report_metadata",
            serde_json::json!({ "pane_id": self.pane_id, "source": my.plugin_id(), "tokens": mine }),
        );
        let other = my.other();
        // Clearing needs an explicit null VALUE: report_metadata MERGES the
        // token map, so an empty map is a no-op (verified live, herdr 0.7.1).
        let other_tokens = if merged {
            serde_json::json!({ other.plugin_id(): now })
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

    /// Set or clear the pane label — cleared while collapsed so the sliver has
    /// no border title (herdr shows nothing when label and metadata title are
    /// both absent).
    fn set_label(&self, label: Option<&str>) {
        let mut params = serde_json::json!({ "pane_id": self.pane_id });
        if let Some(label) = label {
            params["label"] = serde_json::Value::String(label.to_string());
        }
        let _ = herdr_aa_sidebar::ipc::call_text("pane.rename", params);
    }

    /// Resize our pane to `target` terminal columns over the socket API.
    /// `pane.resize`'s amount is a split-RATIO delta, so the exact amount comes
    /// from the live layout via [`herdr_aa_sidebar::launch::resize_plan`].
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
}

/// A modal layered over the tree: the context menu, a name prompt, or a
/// delete confirmation. While one is open it owns keyboard and mouse input.
enum Overlay {
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
    },
}

/// One row of the Settings modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    UnifiedSidebar,
    IconTheme,
    HiddenFiles,
    Hotkeys,
}

/// (setting, label, current value, enabled) — disabled rows render dimmed and
/// don't toggle.
type SettingRow = (Setting, &'static str, String, bool);

pub struct App {
    tree: Tree,
    rows: Vec<Row>,
    state: ListState,
    theme: IconTheme,
    pane_ctl: Option<PaneCtl>,
    /// Pane size from the last draw; sizing decisions and PageUp/PageDown
    /// strides are based on what was actually rendered.
    last_width: u16,
    last_height: u16,
    page: usize,
    /// Width to restore on expand, remembered at collapse time.
    expanded_width: u16,
    /// Explicitly collapsed via the button/key. Herdr's 0.1 minimum split
    /// ratio can leave the collapsed pane wider than the sliver threshold on
    /// large windows, so collapse state can't be inferred from width alone.
    collapsed: bool,
    /// Row index under the mouse cursor, for the hover highlight.
    hovered: Option<usize>,
    body: BodyGeom,
    overlay: Option<Overlay>,
    /// Transient status/error line shown in the footer until the next action.
    notice: Option<String>,
    // Merged-sidebar state.
    sidebar_state: sidebar::State,
    other_exe: Option<std::path::PathBuf>,
    activity: ActivityZones,
    /// The ⚙ button's rect from the last draw (activity bar in unified mode,
    /// header row otherwise).
    gear: Rect,
    /// Last left-click (row index, when) for double-click detection.
    last_click: Option<(usize, std::time::Instant)>,
    /// Last heartbeat stamp, throttling the token refresh.
    last_beat: std::time::Instant,
}

/// How long two clicks on the same row still count as a double click.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(450);


/// Activity-bar click zones from the last draw: the bar's row and the column
/// ranges of the explorer / source-control icons.
#[derive(Clone, Copy)]
struct ActivityZones {
    row: u16,
    explorer: (u16, u16),
    source_control: (u16, u16),
}

impl Default for ActivityZones {
    fn default() -> Self {
        // row = MAX: nothing hit-tests true before the first draw.
        Self { row: u16::MAX, explorer: (0, 0), source_control: (0, 0) }
    }
}

impl App {
    pub fn new(root: PathBuf) -> Self {
        let mut tree = Tree::new(root);
        let rows = tree.rows();
        let mut state = ListState::default();
        if !rows.is_empty() {
            state.select(Some(0));
        }
        let theme = IconTheme::from_env(std::env::var("HERDR_AA_FILETREE_ICONS").ok().as_deref());
        let pane_ctl = PaneCtl::from_env();
        // The other view ships in this same binary — always available.
        let other_exe = std::env::current_exe().ok();
        let sidebar_state = sidebar::load_state();
        let app = Self {
            tree,
            rows,
            state,
            theme,
            pane_ctl,
            last_width: DEFAULT_EXPANDED_WIDTH,
            last_height: 24,
            page: 20,
            expanded_width: DEFAULT_EXPANDED_WIDTH,
            collapsed: false,
            hovered: None,
            body: BodyGeom::default(),
            overlay: None,
            notice: None,
            sidebar_state,
            other_exe,
            activity: ActivityZones::default(),
            gear: Rect::default(),
            last_click: None,
            last_beat: std::time::Instant::now(),
        };
        app.apply_identity();
        app
    }

    /// Re-stamp the identity tokens so launchers know this pane is alive.
    /// Cheap (two socket round-trips); the event loop calls this every few
    /// seconds.
    pub fn heartbeat(&mut self) {
        if self.last_beat.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        self.last_beat = std::time::Instant::now();
        if let Some(ctl) = &self.pane_ctl {
            ctl.report_tokens(MY_VIEW, self.merged());
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
            herdr_aa_sidebar::launch::PANE_LABEL
        }
    }

    /// Push our label + metadata tokens to herdr for the current mode.
    fn apply_identity(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        if !self.collapsed {
            ctl.set_label(Some(self.pane_label()));
        }
        ctl.report_tokens(MY_VIEW, self.merged());
    }

    /// Collapsed by the button, or manually dragged down to a sliver.
    fn collapsed(&self) -> bool {
        self.collapsed || self.last_width < SLIVER_THRESHOLD
    }

    /// Open a file in the preview pane BESIDE the sidebar (the tree stays
    /// visible): the shared viewer client reuses the tab's viewer pane or
    /// spawns one next to us.
    fn open_preview(&mut self, path: &Path) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone()) else {
            self.notice = Some("preview needs a herdr pane".into());
            return;
        };
        let payload = herdr_aa_sidebar::viewer::file_request(path);
        if let Err(e) = herdr_aa_sidebar::viewer::open_in_pane(
            &pane_id,
            &self.tree.root_path(),
            &payload,
        ) {
            self.notice = Some(e);
        }
    }

    fn collapse(&mut self) {
        if self.collapsed() {
            return;
        }
        self.expanded_width = self.last_width;
        self.collapsed = true;
        if let Some(ctl) = &self.pane_ctl {
            ctl.set_label(None);
            ctl.resize_to(self.last_width, SLIVER_TARGET);
        }
    }

    fn expand(&mut self) {
        if !self.collapsed() {
            return;
        }
        self.collapsed = false;
        if let Some(ctl) = &self.pane_ctl {
            ctl.set_label(Some(self.pane_label()));
            ctl.resize_to(
                self.last_width,
                self.expanded_width.max(DEFAULT_EXPANDED_WIDTH),
            );
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
        self.sidebar_state =
            sidebar::State { merged: on, active: MY_VIEW, ..self.sidebar_state };
        sidebar::save_state(self.sidebar_state);
        self.apply_identity();
        if on {
            // Mirror the detach growth: absorbing the sibling leaves the
            // survivor at roughly double width — shrink back to one panel.
            let width = self.last_width;
            self.close_other_standalone_pane();
            if let Some(ctl) = &self.pane_ctl {
                ctl.resize_to(width.saturating_mul(2).saturating_add(1), width);
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
        self.sidebar_state.active = view;
        sidebar::save_state(self.sidebar_state);
        Some(Exit::Switch)
    }

    /// Close the other panel's standalone pane in our tab, if one is open.
    fn close_other_standalone_pane(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        let Ok(json) = herdr_aa_sidebar::ipc::call_text("pane.list", serde_json::json!({}))
        else {
            return;
        };
        for id in sibling_panes_of(&json, &ctl.pane_id, MY_VIEW.other()) {
            let _ =
                herdr_aa_sidebar::ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }));
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
                "cwd": self.tree.root_path().display().to_string(),
            }),
        );
        let Some(new_pane) =
            response.ok().and_then(|r| herdr_aa_sidebar::launch::split_pane_id(&r))
        else {
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

    /// Handle one key press; `Some(exit)` ends the event loop.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Exit> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        self.notice = None;
        if self.overlay.is_some() {
            self.overlay_key(key);
            return None;
        }
        if self.collapsed() {
            // Sliver mode: expand, deep-link to the other view, or quit.
            match key.code {
                KeyCode::Char('q') => return Some(Exit::Quit),
                KeyCode::Char('2') => {
                    self.expand();
                    return self.switch_to(View::SourceControl);
                }
                _ => self.expand(),
            }
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
                self.tree.refresh();
                self.rebuild();
            }
            KeyCode::Char('.') => {
                self.tree.show_hidden = !self.tree.show_hidden;
                self.rebuild();
            }
            KeyCode::Char('i') => self.theme = self.theme.toggled(),
            KeyCode::Char('b') => self.collapse(),
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('1') => return self.switch_to(View::Explorer),
            KeyCode::Char('2') => return self.switch_to(View::SourceControl),
            _ => {}
        }
        None
    }

    /// `Some(exit)` ends the event loop, mirroring on_key.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit> {
        if self.collapsed() {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                // A view icon in the sliver deep-links: expand INTO that view.
                let target = sliver_view_at(mouse.row, MY_VIEW, self.merged());
                self.expand();
                if let Some(view) = target {
                    return self.switch_to(view);
                }
            }
            return None;
        }
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
            MouseEventKind::Down(MouseButton::Left) => {
                let zones = self.activity;
                if self.merged() && mouse.row == zones.row {
                    if (zones.explorer.0..zones.explorer.1).contains(&mouse.column) {
                        return self.switch_to(View::Explorer);
                    }
                    if (zones.source_control.0..zones.source_control.1).contains(&mouse.column) {
                        return self.switch_to(View::SourceControl);
                    }
                }
                let g = self.gear;
                if mouse.column >= g.x
                    && mouse.column < g.x + g.width
                    && mouse.row >= g.y
                    && mouse.row < g.y + g.height
                {
                    self.open_settings();
                    return None;
                }
                if hits_collapse_button(mouse.column, mouse.row, self.last_width, self.last_height)
                {
                    self.collapse();
                    return None;
                }
                let index = self.row_at(mouse.row)?;
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
                    // Chevron always toggles; the name toggles on double click.
                    if on_chevron || double {
                        self.toggle();
                    }
                } else {
                    // A click on a file zooms the pane into its preview.
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

    /// Open the file context menu at the click position, targeting the row
    /// under the cursor (or the workspace root on empty space).
    fn open_context_menu(&mut self, x: u16, y: u16) {
        let target = self.row_at(y).map(|index| {
            self.select(index);
            let row = &self.rows[index];
            (row.path.clone(), row.is_dir)
        });
        let entries = actions::menu_entries(target.is_none());
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
            ToggleSetting(usize),
            DeleteConfirmed(PathBuf, bool),
        }
        let row_count = self.settings_rows().len();
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
                KeyCode::Enter | KeyCode::Char(' ') => Cmd::ToggleSetting(*selected),
                _ => Cmd::Nothing,
            },
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
            Cmd::Close => self.overlay = None,
            Cmd::Activate => self.activate_menu_entry(),
            Cmd::ConfirmPrompt => self.confirm_prompt(),
            Cmd::ToggleSetting(index) => self.toggle_setting(index),
            Cmd::DeleteConfirmed(path, is_dir) => {
                self.overlay = None;
                match actions::delete(&path, is_dir) {
                    Ok(()) => self.refresh_tree(),
                    Err(err) => self.notice = Some(format!("delete failed: {err}")),
                }
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
                    MouseEventKind::Down(MouseButton::Right) => {
                        Cmd::Reopen(mouse.column, mouse.row)
                    }
                    _ => Cmd::Nothing,
                }
            }
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
            (
                Setting::HiddenFiles,
                "Hidden files",
                if self.tree.show_hidden { "shown" } else { "hidden" }.to_string(),
                true,
            ),
            (
                Setting::Hotkeys,
                "Footer hotkeys",
                if self.show_hotkeys() { "shown" } else { "hidden" }.to_string(),
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
            Setting::HiddenFiles => {
                self.tree.show_hidden = !self.tree.show_hidden;
                self.rebuild();
            }
            Setting::Hotkeys => {
                self.sidebar_state.show_hotkeys = !self.sidebar_state.show_hotkeys;
                sidebar::save_state(self.sidebar_state);
            }
        }
    }

    /// Render the centered Settings popup and remember its rect for clicks.
    fn draw_settings(&mut self, frame: &mut Frame) {
        let rows = self.settings_rows();
        // The hotkey reference lives here now; the footer chips are opt-in.
        let hint_lines = wrap_hints(&self.hints(), 28, 0);
        let Some(Overlay::Settings { selected, rect }) = self.overlay.as_mut() else {
            return;
        };
        let area = frame.area();
        let width = 30.min(area.width);
        let height =
            (rows.len() as u16 + 5 + hint_lines.len() as u16).min(area.height);
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
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(" Hotkeys", Style::default().bold())));
        lines.extend(hint_lines);
        lines.push(Line::from(" click/⏎ toggle · esc close".dim()));

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).block(
                ratatui::widgets::Block::bordered()
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
        // Creation targets: the folder itself, a file's parent, or the root.
        let create_dir = match &target {
            Some((path, true)) => path.clone(),
            Some((path, false)) => {
                path.parent().map(Path::to_path_buf).unwrap_or_else(|| self.tree.root_path())
            }
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
                    Ok(()) => format!("copied: {text}"),
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
            MenuAction::Reveal => {
                let path = target.map(|(p, _)| p).unwrap_or_else(|| self.tree.root_path());
                actions::reveal(&path);
            }
        }
    }

    fn confirm_prompt(&mut self) {
        let Some(Overlay::Prompt { input, kind, .. }) = self.overlay.take() else { return };
        let Some(name) = actions::validate_name(&input) else {
            self.notice = Some("invalid name".into());
            return;
        };
        let result = match &kind {
            PromptKind::NewFile(dir) => actions::create_file(dir, name),
            PromptKind::NewFolder(dir) => actions::create_folder(dir, name),
            PromptKind::Rename(path) => actions::rename(path, name),
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

    fn refresh_tree(&mut self) {
        self.tree.refresh();
        self.rebuild();
    }

    /// The visible row index at a pane-local mouse row, if it lands on one.
    fn row_at(&self, mouse_row: u16) -> Option<usize> {
        row_index_at(self.body, self.rows.len(), mouse_row)
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.state.selected()?)
    }

    fn select(&mut self, index: usize) {
        if !self.rows.is_empty() {
            self.state.select(Some(index.min(self.rows.len() - 1)));
        }
    }

    fn move_by(&mut self, delta: isize) {
        let current = self.state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.rows.len().saturating_sub(1) as isize);
        self.select(next as usize);
    }

    /// Right/l: expand a collapsed directory, step into an expanded one.
    fn expand_or_enter(&mut self) {
        let Some(row) = self.selected_row() else { return };
        if !row.is_dir {
            return;
        }
        if row.expanded {
            // First child, if any, sits directly below at depth + 1.
            let index = self.state.selected().unwrap_or(0);
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
        let Some(row) = self.selected_row() else { return };
        if row.is_dir && row.expanded {
            let path = row.path.clone();
            self.tree.collapse(&path);
            self.rebuild();
            return;
        }
        let index = self.state.selected().unwrap_or(0);
        let depth = row.depth;
        if depth == 0 {
            return;
        }
        if let Some(parent) = self.rows[..index].iter().rposition(|r| r.depth == depth - 1) {
            self.select(parent);
        }
    }

    fn toggle(&mut self) {
        let Some(row) = self.selected_row() else { return };
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
        let selected_path = self.selected_row().map(|r| r.path.clone());
        self.rows = self.tree.rows();
        if self.rows.is_empty() {
            self.state.select(None);
            return;
        }
        let index = selected_path
            .and_then(|p| self.rows.iter().position(|r| r.path == p))
            .unwrap_or_else(|| {
                self.state
                    .selected()
                    .unwrap_or(0)
                    .min(self.rows.len() - 1)
            });
        self.state.select(Some(index));
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.last_width = frame.area().width;
        self.last_height = frame.area().height;
        if self.collapsed() {
            self.draw_sliver(frame);
            return;
        }

        // No own border/title: herdr already frames the pane and titles it with
        // the pane label ("Explorer"/"Sidebar") — a second border read as a
        // double frame.
        let footer_height = self.footer_height(frame.area().width);
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
        self.draw_header(frame, header);

        if self.rows.is_empty() {
            frame.render_widget(Paragraph::new("  (empty)".dim().italic()), body);
        } else {
            let theme = self.theme;
            let hovered = self.hovered;
            let items: Vec<ListItem> = self
                .rows
                .iter()
                .enumerate()
                .map(|(i, r)| row_item(r, theme, hovered == Some(i)))
                .collect();
            let list = List::new(items).highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, body, &mut self.state);
        }
        self.body = BodyGeom {
            top: body.y,
            height: body.height,
            offset: self.state.offset(),
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
            Paragraph::new("«".bold().fg(Color::LightBlue)).alignment(Alignment::Center),
            footer_button,
        );
        let footer_lines: Vec<Line> = if let Some(notice) = &self.notice {
            vec![format!(" {notice}").fg(Color::Yellow).into()]
        } else {
            match &self.overlay {
                Some(Overlay::Prompt { title, input, .. }) => vec![Line::from(vec![
                    Span::styled(format!(" {title}: "), Style::default().bold()),
                    Span::raw(input.clone()),
                    Span::styled("█", Style::default().dim()),
                    Span::styled("  (⏎ ok · esc cancel)", Style::default().dim()),
                ])],
                Some(Overlay::ConfirmDelete { path, .. }) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    vec![format!(" Delete '{name}' permanently? (y/N)").fg(Color::Red).into()]
                }
                _ if self.show_hotkeys() => {
                    wrap_hints(&self.hints(), frame.area().width, 3)
                }
                _ => Vec::new(),
            }
        };
        frame.render_widget(Paragraph::new(footer_lines), footer);

        match self.overlay {
            Some(Overlay::Menu { .. }) => self.draw_menu(frame),
            Some(Overlay::Settings { .. }) => self.draw_settings(frame),
            _ => {}
        }
    }

    /// The workspace-name header; standalone mode puts the ⚙ at its right edge
    /// (unified mode's ⚙ lives in the activity bar instead).
    fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
        let root_label = format!(" {}", self.tree.root_name().to_uppercase());
        let name = Span::styled(root_label, Style::default().bold().fg(Color::LightBlue));
        let mut spans = vec![name];
        if !self.merged() {
            let gear =
                Span::styled(format!("{} ", gear_icon(self.theme)), Style::default().dim());
            let gear_w = gear.width() as u16;
            let gx = area.x + area.width.saturating_sub(gear_w);
            self.gear = Rect::new(gx, area.y, gear_w, 1);
            let pad = usize::from(area.width)
                .saturating_sub(spans[0].width() + usize::from(gear_w));
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(gear);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The persisted "show hotkeys in the footer" setting.
    fn show_hotkeys(&self) -> bool {
        self.sidebar_state.show_hotkeys
    }

    /// Esc: close the preview pane beside us, if one is open.
    fn close_preview(&mut self) {
        if let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone()) {
            herdr_aa_sidebar::viewer::close_in_tab(&pane_id);
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
            ("s", "settings"),
            ("b", "collapse"),
            ("q", "quit"),
        ];
        if self.merged() {
            hints.extend([("1", "files"), ("2", "git")]);
        }
        hints
    }

    /// Rows the footer needs at `width` (the hint lines; notices and prompts
    /// always fit in one of them).
    fn footer_height(&self, width: u16) -> u16 {
        if self.notice.is_some() || self.overlay.is_some() || !self.show_hotkeys() {
            return 1; // notices, prompts, and the « button share one line
        }
        wrap_hints(&self.hints(), width, 3).len() as u16
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
            Span::styled(format!(" {exp_icon}{slack} "), active(true)),
            Span::raw(" "),
            Span::styled(format!(" {git_icon}{slack} "), active(false)),
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
            source_control: bounds[3],
        };
        // Symmetric half-block caps: a 2-cell button with the icon in its
        // vertical center.
        let (chip_start, chip_end) = bounds[1];
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
        self.gear = Rect::new(gear_x, area.y, gear_w, 1);

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
            List::new(items).block(
                ratatui::widgets::Block::bordered().border_style(Style::default().dim()),
            ),
            popup,
        );
    }

    /// The collapsed strip: just the explorer's icon (theme-matched) — any
    /// click or key expands, so the icon is the whole affordance.
    fn draw_sliver(&mut self, frame: &mut Frame) {
        frame.render_widget(
            Paragraph::new(sliver_lines(self.theme, MY_VIEW, self.merged()))
                .alignment(Alignment::Center),
            frame.area(),
        );
    }
}

fn row_item(row: &Row, theme: IconTheme, hovered: bool) -> ListItem<'static> {
    let indent = "  ".repeat(row.depth);
    let arrow = if row.is_dir {
        if row.expanded { "▾ " } else { "▸ " }
    } else {
        "  "
    };
    let icon = icon(theme, &row.name, row.is_dir, row.expanded);
    let icon_style = match icon.rgb {
        Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)),
        None => Style::default(),
    };
    // Folder and file names share the default foreground, like VS Code — the
    // chevron and icon carry the distinction. Accent-on-gray (the old blue
    // names) was hard to read against the selection/hover backgrounds.
    let item = ListItem::new(Line::from(vec![
        Span::styled(format!("{indent}{arrow}"), Style::default().dim()),
        Span::styled(format!("{} ", icon.glyph), icon_style),
        Span::raw(row.name.clone()),
    ]));
    if hovered {
        // Subtler than the DarkGray selection bg; the selection's
        // highlight_style still wins on the selected row.
        item.style(Style::default().bg(Color::Rgb(48, 52, 60)))
    } else {
        item
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

/// True when a click at pane-local `column` lands on a row's disclosure
/// chevron (the two cells right after the depth indent).
fn hits_chevron(column: u16, depth: usize) -> bool {
    let start = (depth * 2) as u16;
    (start..start + 2).contains(&column)
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
        let entries = actions::menu_entries(false);
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
    fn row_index_accounts_for_header_and_scroll() {
        let body = BodyGeom { top: 1, height: 10, offset: 5 };
        assert_eq!(row_index_at(body, 100, 0), None, "header row");
        assert_eq!(row_index_at(body, 100, 1), Some(5));
        assert_eq!(row_index_at(body, 100, 10), Some(14));
        assert_eq!(row_index_at(body, 100, 11), None, "footer row");
        assert_eq!(row_index_at(body, 6, 2), None, "past the last row");
    }

    #[test]
    fn sliver_shows_reachable_view_icons_and_deep_links() {
        let solo: Vec<String> = sliver_lines(IconTheme::Material, View::Explorer, false)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(solo, ["", "\u{f07b}"]);
        let both: Vec<String> = sliver_lines(IconTheme::Material, View::SourceControl, true)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(both, ["", "\u{f07b}", "", "\u{f126}"]);
        // Clicking an icon routes to its view; blanks just expand.
        assert_eq!(sliver_view_at(1, View::Explorer, true), Some(View::Explorer));
        assert_eq!(sliver_view_at(3, View::Explorer, true), Some(View::SourceControl));
        assert_eq!(sliver_view_at(2, View::Explorer, true), None);
        assert_eq!(sliver_view_at(3, View::Explorer, false), None);
        assert_eq!(
            sliver_view_at(1, View::SourceControl, false),
            Some(View::SourceControl)
        );
    }
}
