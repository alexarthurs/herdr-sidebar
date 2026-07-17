//! TUI state and rendering: a VS Code Explorer-style tree with disclosure arrows,
//! nested indentation, and per-file-type emoji icons.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use crate::icons::{IconTheme, icon};
use crate::tree::{Row, Tree};

pub struct App {
    tree: Tree,
    rows: Vec<Row>,
    state: ListState,
    theme: IconTheme,
    /// Viewport height from the last draw, for PageUp/PageDown strides.
    page: usize,
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
        Self { tree, rows, state, theme, page: 20 }
    }

    /// Handle one key press; returns `false` when the app should exit.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
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
            _ => {}
        }
        true
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
        let block = Block::bordered().title(" EXPLORER ".bold());
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());

        let [header, body, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
                .areas(inner);
        self.page = body.height.saturating_sub(1).max(1) as usize;

        let root_label = format!(" 🗂  {}", self.tree.root_name().to_uppercase());
        frame.render_widget(
            Paragraph::new(root_label.bold().fg(Color::LightBlue)),
            header,
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
                " ↑↓ move  ←→ fold  ⏎ toggle  r refresh  . dotfiles  i icons  q quit".dim(),
            ),
            footer,
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
