//! TUI state and rendering: a VS Code Explorer-style tree with disclosure arrows,
//! nested indentation, per-file-type icons, and a VS Code-like collapse-to-sliver
//! (the `«` button, or `b`): the pane narrows to a strip with EXPLORER written
//! sideways, resized through the herdr CLI since only the host controls pane size.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use herdr_aa_filetree::icons::{IconTheme, icon};
use herdr_aa_filetree::tree::{Row, Tree};

/// Below this pane width the explorer renders as the collapsed sliver.
const SLIVER_THRESHOLD: u16 = 14;
/// Width we ask herdr for when collapsing (herdr's 0.1 min split ratio may keep
/// the pane a little wider — the sliver rendering adapts to whatever we get).
const SLIVER_TARGET: u16 = 5;
/// Expanded width to restore when nothing better is known.
const DEFAULT_EXPANDED_WIDTH: u16 = 32;

/// Handle for resizing our own pane through the herdr CLI.
struct PaneCtl {
    herdr_bin: String,
    pane_id: String,
}

impl PaneCtl {
    fn from_env() -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|id| !id.is_empty())?;
        let herdr_bin = std::env::var("HERDR_BIN_PATH")
            .ok()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "herdr".to_string());
        Some(Self { herdr_bin, pane_id })
    }

    /// Resize our pane to `target` terminal columns. `pane resize --amount` is
    /// a split-RATIO delta, so the exact amount comes from the live layout via
    /// [`herdr_aa_filetree::launch::resize_plan`]. Blocking on purpose: both CLI calls are
    /// ~instant, and waiting avoids leaving zombie children behind on unix.
    fn resize_to(&self, current: u16, target: u16) {
        let layout = std::process::Command::new(&self.herdr_bin)
            .args(["pane", "layout", "--pane", &self.pane_id])
            .stderr(std::process::Stdio::null())
            .output();
        let Ok(layout) = layout else { return };
        let layout_json = String::from_utf8_lossy(&layout.stdout);
        let Some(step) =
            herdr_aa_filetree::launch::resize_plan(&layout_json, &self.pane_id, current, target)
        else {
            return;
        };
        let _ = std::process::Command::new(&self.herdr_bin)
            .args([
                "pane",
                "resize",
                "--direction",
                step.direction,
                "--amount",
                &format!("{:.4}", step.amount),
                "--pane",
                &self.pane_id,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
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
    page: usize,
    /// Width to restore on expand, remembered at collapse time.
    expanded_width: u16,
    /// Explicitly collapsed via the button/key. Herdr's 0.1 minimum split
    /// ratio can leave the collapsed pane wider than the sliver threshold on
    /// large windows, so collapse state can't be inferred from width alone.
    collapsed: bool,
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
        Self {
            tree,
            rows,
            state,
            theme,
            pane_ctl: PaneCtl::from_env(),
            last_width: DEFAULT_EXPANDED_WIDTH,
            page: 20,
            expanded_width: DEFAULT_EXPANDED_WIDTH,
            collapsed: false,
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
            ctl.resize_to(self.last_width, SLIVER_TARGET);
        }
    }

    fn expand(&mut self) {
        if !self.collapsed() {
            return;
        }
        self.collapsed = false;
        if let Some(ctl) = &self.pane_ctl {
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
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        if self.collapsed() {
            self.expand();
        } else if hits_collapse_button(mouse.column, mouse.row, self.last_width) {
            self.collapse();
        }
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
        if self.collapsed() {
            self.draw_sliver(frame);
            return;
        }

        let block = Block::bordered().title(" EXPLORER ".bold());
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());

        let [header, body, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
                .areas(inner);
        self.page = body.height.saturating_sub(1).max(1) as usize;

        // Root name on the left, the collapse button on the right.
        let [root_area, button_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(3)]).areas(header);
        let root_label = format!(" 🗂  {}", self.tree.root_name().to_uppercase());
        frame.render_widget(Paragraph::new(root_label.bold().fg(Color::LightBlue)), root_area);
        frame.render_widget(
            Paragraph::new("«".bold().fg(Color::LightBlue)).alignment(Alignment::Center),
            button_area,
        );

        if self.rows.is_empty() {
            frame.render_widget(Paragraph::new("  (empty)".dim().italic()), body);
        } else {
            let theme = self.theme;
            let items: Vec<ListItem> = self.rows.iter().map(|r| row_item(r, theme)).collect();
            let list = List::new(items).highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, body, &mut self.state);
        }

        frame.render_widget(
            Paragraph::new(
                " ↑↓ move  ←→ fold  ⏎ toggle  r refresh  . dotfiles  i icons  « b collapse  q quit"
                    .dim(),
            ),
            footer,
        );
    }

    /// The collapsed strip: `»` on top, EXPLORER written sideways beneath it.
    fn draw_sliver(&mut self, frame: &mut Frame) {
        let height = frame.area().height as usize;
        let lines = sliver_lines(height);
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            frame.area(),
        );
    }
}

fn row_item(row: &Row, theme: IconTheme) -> ListItem<'static> {
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
    ListItem::new(Line::from(vec![
        Span::styled(format!("{indent}{arrow}"), Style::default().dim()),
        Span::styled(format!("{} ", icon.glyph), icon_style),
        Span::styled(row.name.clone(), name_style),
    ]))
}

/// True when a click at pane-local (column, row) lands on the `«` button, which
/// sits in the 3-cell region at the right end of the header line (row 1, inside
/// the border).
fn hits_collapse_button(column: u16, row: u16, pane_width: u16) -> bool {
    row == 1 && column >= pane_width.saturating_sub(4)
}

/// The sliver's lines for a pane of the given height: expand button on top,
/// then E X P L O R E R vertically, truncated on tiny panes.
fn sliver_lines(height: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = vec![
        Line::from("»".bold().fg(Color::LightBlue)),
        Line::raw(""),
    ];
    for ch in "EXPLORER".chars().take(height.saturating_sub(2)) {
        lines.push(Line::from(ch.to_string().bold().fg(Color::LightBlue)));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_button_hit_region_is_header_right_edge() {
        assert!(hits_collapse_button(30, 1, 32));
        assert!(hits_collapse_button(28, 1, 32));
        assert!(!hits_collapse_button(27, 1, 32), "left of the button");
        assert!(!hits_collapse_button(30, 0, 32), "border row");
        assert!(!hits_collapse_button(30, 2, 32), "tree row");
    }

    #[test]
    fn sliver_spells_explorer_and_truncates_on_short_panes() {
        let lines = sliver_lines(20);
        let chars: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(chars[0], "»");
        assert_eq!(chars[2..], ["E", "X", "P", "L", "O", "R", "E", "R"]);
        assert_eq!(sliver_lines(5).len(), 5, "never exceeds the pane height");
    }
}
