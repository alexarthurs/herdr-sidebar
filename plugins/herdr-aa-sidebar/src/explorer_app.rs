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
use herdr_aa_sidebar::ui::{activity_icons, gear_icon, sibling_panes_of, wrap_hints};
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
        };
        app.apply_identity();
        app
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
    /// visible, like VS Code's editor area): write the target into the
    /// viewer's control file, reusing the running viewer pane when one is
    /// open in this tab, otherwise spawning one next to us.
    fn open_preview(&mut self, path: &Path) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone()) else {
            self.notice = Some("preview needs a herdr pane".into());
            return;
        };
        let control = herdr_aa_sidebar::viewer::control_path(&pane_id);
        if let Err(e) = std::fs::write(&control, path.display().to_string()) {
            self.notice = Some(format!("preview failed: {e}"));
            return;
        }
        // A running viewer in this tab follows the control file by itself.
        if let Ok(json) = herdr_aa_sidebar::ipc::call_text("pane.list", serde_json::json!({}))
            && viewer_pane_in_tab(&json, &pane_id).is_some()
        {
            return;
        }
        self.spawn_viewer_pane(&pane_id, &control);
    }

    /// Split a viewer pane directly to our right: split our right NEIGHBOR
    /// and swap the fresh pane into its left slot (split only goes
    /// right/down), so the layout reads sidebar | preview | rest.
    fn spawn_viewer_pane(&mut self, pane_id: &str, control: &Path) {
        let ipc = herdr_aa_sidebar::ipc::call_text;
        let layout = ipc("pane.layout", serde_json::json!({ "pane_id": pane_id })).ok();
        let neighbor = layout.as_deref().and_then(|json| right_neighbor(json, pane_id));
        // No neighbor (the sidebar is alone in the tab): split ourselves and
        // give the viewer the lion's share.
        let (target, ratio, needs_swap) = match &neighbor {
            Some(id) => (id.clone(), 0.5, true),
            None => (pane_id.to_string(), 0.3, false),
        };
        let response = ipc(
            "pane.split",
            serde_json::json!({
                "target_pane_id": target,
                "direction": "right",
                "ratio": ratio,
                "focus": false,
                "cwd": self.tree.root_path().display().to_string(),
            }),
        );
        let Some(new_pane) =
            response.ok().and_then(|r| herdr_aa_sidebar::launch::split_pane_id(&r))
        else {
            self.notice = Some("preview pane failed to open".into());
            return;
        };
        if needs_swap {
            let _ = ipc(
                "pane.swap",
                serde_json::json!({ "source_pane_id": new_pane, "target_pane_id": target }),
            );
        }
        let exe = std::env::current_exe().ok();
        let exe = exe
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "herdr-aa-filetree".to_string());
        #[cfg(windows)]
        let command = format!("& \"{exe}\" --preview \"{}\"", control.display());
        #[cfg(not(windows))]
        let command = format!("exec \"{exe}\" --preview \"{}\"", control.display());
        let _ = ipc(
            "pane.send_input",
            serde_json::json!({ "pane_id": new_pane, "text": command, "keys": ["Enter"] }),
        );
        let _ = ipc(
            "pane.rename",
            serde_json::json!({ "pane_id": new_pane, "label": "Preview" }),
        );
        // The split/swap can move focus with the slot; stay in the explorer
        // so the user keeps clicking files.
        let _ = ipc("pane.focus", serde_json::json!({ "pane_id": pane_id }));
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
            // Sliver mode: only expand or quit.
            match key.code {
                KeyCode::Char('q') => return Some(Exit::Quit),
                _ => self.expand(),
            }
            return None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(Exit::Quit),
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
                self.expand();
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
                _ => wrap_hints(&self.hints(), frame.area().width, 3),
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
        if self.notice.is_some() || self.overlay.is_some() {
            return 1;
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
            Paragraph::new(sliver_lines(self.theme)).alignment(Alignment::Center),
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

/// The viewer pane in the same tab as `my_pane_id`, found by its metadata
/// token, from a `pane.list` response.
fn viewer_pane_in_tab(pane_list_json: &str, my_pane_id: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(serde::Deserialize)]
    struct Res {
        #[serde(default)]
        panes: Vec<Pane>,
    }
    #[derive(serde::Deserialize)]
    struct Pane {
        pane_id: Option<String>,
        tab_id: Option<String>,
        #[serde(default)]
        tokens: serde_json::Map<String, serde_json::Value>,
    }
    let msg: Msg = serde_json::from_str(pane_list_json.trim_start_matches('\u{feff}')).ok()?;
    let panes = &msg.result.panes;
    let my_tab = panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(my_pane_id))?
        .tab_id
        .clone()?;
    panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(my_tab.as_str()))
        .find(|p| p.tokens.contains_key(herdr_aa_sidebar::viewer::METADATA_SOURCE))
        .and_then(|p| p.pane_id.clone())
}

/// The pane directly to the right of `pane_id` (sharing its top edge or
/// overlapping vertically), from a `pane.layout` response.
fn right_neighbor(layout_json: &str, pane_id: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(serde::Deserialize)]
    struct Res {
        layout: L,
    }
    #[derive(serde::Deserialize)]
    struct L {
        #[serde(default)]
        panes: Vec<P>,
    }
    #[derive(serde::Deserialize)]
    struct P {
        pane_id: Option<String>,
        rect: Option<R>,
    }
    #[derive(serde::Deserialize)]
    struct R {
        x: i64,
        y: i64,
        width: i64,
        height: i64,
    }
    let msg: Msg = serde_json::from_str(layout_json.trim_start_matches('\u{feff}')).ok()?;
    let panes = &msg.result.layout.panes;
    let me = panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    let (my_right, my_top, my_bottom) = (me.x + me.width, me.y, me.y + me.height);
    panes
        .iter()
        .filter(|p| p.pane_id.as_deref() != Some(pane_id))
        .filter_map(|p| Some((p.pane_id.clone()?, p.rect.as_ref()?)))
        .find(|(_, r)| r.x == my_right && r.y < my_bottom && r.y + r.height > my_top)
        .map(|(id, _)| id)
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

/// True when a click at pane-local (column, row) lands on the `«` button: the
/// 3-cell region at the right end of the footer (bottom) line, mirroring
/// herdr's own sidebar collapse control.
fn hits_collapse_button(column: u16, row: u16, pane_width: u16, pane_height: u16) -> bool {
    row == pane_height.saturating_sub(1) && column >= pane_width.saturating_sub(4)
}

/// The sliver's content: a single folder icon in the active theme.
fn sliver_lines(theme: IconTheme) -> Vec<Line<'static>> {
    let icon = icon(theme, "", true, false);
    let style = match icon.rgb {
        Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)),
        None => Style::default(),
    };
    vec![
        Line::raw(""),
        Line::from(Span::styled(icon.glyph, style)),
    ]
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
    fn sliver_is_a_single_theme_icon() {
        let material: Vec<String> = sliver_lines(IconTheme::Material)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(material, ["", "\u{f07b}"]);
        let emoji: Vec<String> = sliver_lines(IconTheme::Emoji)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(emoji, ["", "📁"]);
    }
}
