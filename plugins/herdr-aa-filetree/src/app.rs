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

use herdr_aa_filetree::actions::{self, MenuAction, MenuEntry};
use herdr_aa_filetree::icons::{IconTheme, icon};
use herdr_aa_filetree::tree::{Row, Tree};

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

    /// Tag our pane with the Explorer metadata token so the ensure logic can
    /// recognize it even while the (cosmetic) label is cleared.
    fn report_identity(&self) {
        let _ = herdr_aa_filetree::ipc::call_text(
            "pane.report_metadata",
            serde_json::json!({
                "pane_id": self.pane_id,
                "source": herdr_aa_filetree::launch::METADATA_SOURCE,
                "tokens": { herdr_aa_filetree::launch::METADATA_SOURCE: "explorer" },
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
        let _ = herdr_aa_filetree::ipc::call_text("pane.rename", params);
    }

    /// Resize our pane to `target` terminal columns over the socket API.
    /// `pane.resize`'s amount is a split-RATIO delta, so the exact amount comes
    /// from the live layout via [`herdr_aa_filetree::launch::resize_plan`].
    fn resize_to(&self, current: u16, target: u16) {
        let Ok(layout) = herdr_aa_filetree::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        ) else {
            return;
        };
        let Some(step) =
            herdr_aa_filetree::launch::resize_plan(&layout, &self.pane_id, current, target)
        else {
            return;
        };
        let _ = herdr_aa_filetree::ipc::call_text(
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
}

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
        if let Some(ctl) = &pane_ctl {
            ctl.report_identity();
        }
        Self {
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
        }
    }

    /// Collapsed by the button, or manually dragged down to a sliver.
    fn collapsed(&self) -> bool {
        self.collapsed || self.last_width < SLIVER_THRESHOLD
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
            ctl.set_label(Some(herdr_aa_filetree::launch::PANE_LABEL));
            ctl.resize_to(
                self.last_width,
                self.expanded_width.max(DEFAULT_EXPANDED_WIDTH),
            );
        }
    }

    /// Handle one key press; returns `false` when the app should exit.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return true;
        }
        self.notice = None;
        if self.overlay.is_some() {
            self.overlay_key(key);
            return true;
        }
        if self.collapsed() {
            // Sliver mode: only expand or quit.
            match key.code {
                KeyCode::Char('q') => return false,
                _ => self.expand(),
            }
            return true;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
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
            _ => {}
        }
        true
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.collapsed() {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                self.expand();
            }
            return;
        }
        if self.overlay.is_some() {
            self.overlay_mouse(mouse);
            return;
        }
        match mouse.kind {
            MouseEventKind::Moved => {
                self.hovered = self.row_at(mouse.row);
            }
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::ScrollDown => self.move_by(3),
            MouseEventKind::Down(MouseButton::Left) => {
                if hits_collapse_button(mouse.column, mouse.row, self.last_width, self.last_height)
                {
                    self.collapse();
                    return;
                }
                let Some(index) = self.row_at(mouse.row) else { return };
                self.select(index);
                let row = &self.rows[index];
                if row.is_dir && hits_chevron(mouse.column, row.depth) {
                    self.toggle();
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.notice = None;
                self.open_context_menu(mouse.column, mouse.row);
            }
            _ => {}
        }
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
            DeleteConfirmed(PathBuf, bool),
        }
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
            Reopen(u16, u16),
        }
        let cmd = match self.overlay.as_mut() {
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
            Cmd::Reopen(x, y) => {
                self.overlay = None;
                self.open_context_menu(x, y);
            }
        }
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
        if !row.is_dir {
            return;
        }
        let path = row.path.clone();
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
        // the pane label ("Explorer") — a second border read as a double frame.
        let [header, body, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
                .areas(frame.area());
        self.page = body.height.saturating_sub(1).max(1) as usize;

        let root_label = format!(" {}", self.tree.root_name().to_uppercase());
        frame.render_widget(Paragraph::new(root_label.bold().fg(Color::LightBlue)), header);

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

        // Collapse button at the bottom-right, mirroring herdr's own sidebar.
        let [footer_left, footer_button] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(3)]).areas(footer);
        frame.render_widget(
            Paragraph::new("«".bold().fg(Color::LightBlue)).alignment(Alignment::Center),
            footer_button,
        );
        let footer = footer_left;
        let footer_line: Line = if let Some(notice) = &self.notice {
            format!(" {notice}").fg(Color::Yellow).into()
        } else {
            match &self.overlay {
                Some(Overlay::Prompt { title, input, .. }) => Line::from(vec![
                    Span::styled(format!(" {title}: "), Style::default().bold()),
                    Span::raw(input.clone()),
                    Span::styled("█", Style::default().dim()),
                    Span::styled("  (⏎ ok · esc cancel)", Style::default().dim()),
                ]),
                Some(Overlay::ConfirmDelete { path, .. }) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    format!(" Delete '{name}' permanently? (y/N)").fg(Color::Red).into()
                }
                _ => {
                    " ↑↓ move  ←→ fold  ⏎ toggle  r refresh  . dotfiles  i icons  « b collapse  q quit"
                        .dim()
                        .into()
                }
            }
        };
        frame.render_widget(Paragraph::new(footer_line), footer);

        if matches!(self.overlay, Some(Overlay::Menu { .. })) {
            self.draw_menu(frame);
        }
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
    let name_style = if row.is_dir {
        Style::default().fg(Color::LightBlue)
    } else {
        Style::default()
    };
    let item = ListItem::new(Line::from(vec![
        Span::styled(format!("{indent}{arrow}"), Style::default().dim()),
        Span::styled(format!("{} ", icon.glyph), icon_style),
        Span::styled(row.name.clone(), name_style),
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
