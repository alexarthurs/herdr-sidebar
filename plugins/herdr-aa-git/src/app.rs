//! TUI state and rendering: the VS Code Source Control panel — commit message
//! box, Commit button, and collapsible Staged Changes / Changes sections with
//! count badges, file-type icons, dimmed parent paths, and right-aligned status
//! letters in VS Code's git colors.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use crate::git::{FileEntry, Git, Status};
use crate::icons::icon_for;

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

#[derive(Clone, Copy)]
enum Row {
    StagedHeader,
    ChangesHeader,
    Staged(usize),
    Unstaged(usize),
}

pub struct App {
    repo: Result<Git, String>,
    cwd: PathBuf,
    status: Status,
    rows: Vec<Row>,
    list: ListState,
    focus: Focus,
    message: Vec<char>,
    cursor: usize,
    staged_collapsed: bool,
    changes_collapsed: bool,
    /// One-shot footer notice: (text, is_error). Cleared on the next key press.
    flash: Option<(String, bool)>,
    /// Viewport height from the last draw, for PageUp/PageDown strides.
    page: usize,
}

impl App {
    pub fn new(cwd: PathBuf) -> Self {
        let repo = Git::discover(&cwd);
        let mut app = Self {
            repo,
            cwd,
            status: Status::default(),
            rows: Vec::new(),
            list: ListState::default(),
            focus: Focus::List,
            message: Vec::new(),
            cursor: 0,
            staged_collapsed: false,
            changes_collapsed: false,
            flash: None,
            page: 20,
        };
        app.refresh();
        app
    }

    /// Re-read git status; keeps the flash so periodic ticks don't eat notices.
    pub fn refresh(&mut self) {
        if let Ok(git) = &self.repo {
            match git.status() {
                Ok(status) => self.status = status,
                Err(e) => self.flash = Some((e, true)),
            }
        }
        self.rebuild();
    }

    /// Periodic timer tick: retry repo discovery if we started outside one,
    /// otherwise pick up external changes (edits, commits from other panes).
    pub fn tick(&mut self) {
        if self.repo.is_err() {
            self.repo = Git::discover(&self.cwd);
        }
        self.refresh();
    }

    fn rebuild(&mut self) {
        self.rows.clear();
        // Like VS Code, the Staged section only exists while something is staged.
        if !self.status.staged.is_empty() {
            self.rows.push(Row::StagedHeader);
            if !self.staged_collapsed {
                for i in 0..self.status.staged.len() {
                    self.rows.push(Row::Staged(i));
                }
            }
        }
        self.rows.push(Row::ChangesHeader);
        if !self.changes_collapsed {
            for i in 0..self.status.unstaged.len() {
                self.rows.push(Row::Unstaged(i));
            }
        }
        let index = self.list.selected().unwrap_or(0).min(self.rows.len() - 1);
        self.list.select(Some(index));
    }

    /// Handle one key press; returns `false` when the app should exit.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return true;
        }
        self.flash = None;
        match self.focus {
            Focus::Message => self.on_message_key(key),
            Focus::Commit => self.on_button_key(key),
            Focus::List => return self.on_list_key(key),
        }
        true
    }

    fn on_message_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.commit(),
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Tab => self.focus = Focus::Commit,
            KeyCode::BackTab => self.focus = Focus::List,
            KeyCode::Down => self.focus = Focus::Commit,
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.message.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.message.len() {
                    self.message.remove(self.cursor);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.message.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.message.len(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.message.clear();
                self.cursor = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.message.insert(self.cursor, c);
                self.cursor += 1;
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

    fn on_list_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
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
            _ => {}
        }
        true
    }

    fn select(&mut self, index: usize) {
        if !self.rows.is_empty() {
            self.list.select(Some(index.min(self.rows.len() - 1)));
        }
    }

    fn move_by(&mut self, delta: isize) {
        let current = self.list.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.rows.len().saturating_sub(1) as isize);
        self.select(next as usize);
    }

    /// Enter/Space on the selected row: toggle a section, or move a file
    /// between the staged and unstaged lists.
    fn activate(&mut self) {
        let Some(&row) = self.list.selected().and_then(|i| self.rows.get(i)) else {
            return;
        };
        match row {
            Row::StagedHeader => {
                self.staged_collapsed = !self.staged_collapsed;
                self.rebuild();
            }
            Row::ChangesHeader => {
                self.changes_collapsed = !self.changes_collapsed;
                self.rebuild();
            }
            Row::Staged(i) => self.run_op(|git, e| git.unstage(e), i, true),
            Row::Unstaged(i) => self.run_op(|git, e| git.stage(e), i, false),
        }
    }

    fn run_op(
        &mut self,
        op: impl Fn(&Git, &FileEntry) -> Result<(), String>,
        index: usize,
        staged: bool,
    ) {
        let Ok(git) = &self.repo else { return };
        let list = if staged { &self.status.staged } else { &self.status.unstaged };
        let Some(entry) = list.get(index) else { return };
        if let Err(e) = op(git, entry) {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    fn stage_all(&mut self) {
        let Ok(git) = &self.repo else { return };
        if let Err(e) = git.stage_all() {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    fn unstage_all(&mut self) {
        let Ok(git) = &self.repo else { return };
        if let Err(e) = git.unstage_all() {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    fn commit(&mut self) {
        let Ok(git) = &self.repo else { return };
        let message: String = self.message.iter().collect();
        if message.trim().is_empty() {
            self.flash = Some(("Commit message is empty.".to_string(), true));
            self.focus = Focus::Message;
            return;
        }
        if self.status.staged.is_empty() {
            self.flash = Some(("No staged changes to commit.".to_string(), true));
            return;
        }
        match git.commit(message.trim()) {
            Ok(summary) => {
                self.flash = Some((summary, false));
                self.message.clear();
                self.cursor = 0;
                self.focus = Focus::List;
            }
            Err(e) => self.flash = Some((e, true)),
        }
        self.refresh();
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let block = Block::bordered().title(" SOURCE CONTROL ".bold());
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());

        if let Err(e) = &self.repo {
            let text = format!(
                "Not a git repository.\n\n{e}\n\nOpen this pane inside a repo,\nor press q to quit.",
            );
            frame.render_widget(Paragraph::new(text).dim().wrap(Wrap { trim: false }), inner);
            return;
        }

        let [header, message, button, list, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(inner);
        self.page = list.height.saturating_sub(1).max(1) as usize;

        self.draw_header(frame, header);
        self.draw_message(frame, message);
        self.draw_button(frame, button);
        self.draw_list(frame, list);
        self.draw_footer(frame, footer);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let left = Span::styled(" ▾ CHANGES", Style::default().bold());
        let branch = Span::styled(format!("{} ", self.status.branch), Style::default().dim());
        let pad = (area.width as usize)
            .saturating_sub(left.width() + branch.width())
            .max(1);
        let line = Line::from(vec![left, Span::raw(" ".repeat(pad)), branch]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_message(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Message;
        let border = if focused {
            Style::default().fg(BUTTON_BLUE)
        } else {
            Style::default().dim()
        };
        let boxed = Block::bordered().border_style(border);
        let inner = boxed.inner(area);
        frame.render_widget(boxed, area);

        if self.message.is_empty() && !focused {
            let placeholder =
                format!("Message (Ctrl+Enter to commit on \"{}\")", self.status.branch);
            frame.render_widget(Paragraph::new(placeholder).dim().italic(), inner);
            return;
        }

        // Single-line input with horizontal scroll keeping the cursor visible.
        let width = inner.width.saturating_sub(1) as usize;
        let start = self.cursor.saturating_sub(width);
        let visible: String = self.message.iter().skip(start).take(width.max(1)).collect();
        frame.render_widget(Paragraph::new(visible), inner);
        if focused {
            frame.set_cursor_position(Position::new(
                inner.x + (self.cursor - start) as u16,
                inner.y,
            ));
        }
    }

    fn draw_button(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Commit;
        let bg = if focused { BUTTON_BLUE_FOCUS } else { BUTTON_BLUE };
        let mut style = Style::default().bg(bg).fg(Color::White);
        if focused {
            style = style.add_modifier(Modifier::BOLD);
        }
        frame.render_widget(
            Paragraph::new("✓ Commit").centered().style(style),
            area,
        );
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width as usize;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| match *row {
                Row::StagedHeader => section_item(
                    "Staged Changes",
                    self.staged_collapsed,
                    self.status.staged.len(),
                    width,
                ),
                Row::ChangesHeader => section_item(
                    "Changes",
                    self.changes_collapsed,
                    self.status.unstaged.len(),
                    width,
                ),
                Row::Staged(i) => file_item(&self.status.staged[i], width),
                Row::Unstaged(i) => file_item(&self.status.unstaged[i], width),
            })
            .collect();
        let highlight = if self.focus == Focus::List {
            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::Rgb(0x2a, 0x2d, 0x2e))
        };
        let list = List::new(items).highlight_style(highlight);
        frame.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let line: Line = match &self.flash {
            Some((text, true)) => Line::styled(format!(" {text}"), Style::default().fg(DELETED)),
            Some((text, false)) => {
                Line::styled(format!(" ✓ {text}"), Style::default().fg(UNTRACKED))
            }
            None => Line::from(Span::styled(
                " ⏎ un/stage  a all  u none  c msg  r refresh  q quit",
                Style::default().dim(),
            )),
        };
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// A collapsible section header with a right-aligned count badge.
fn section_item(title: &str, collapsed: bool, count: usize, width: usize) -> ListItem<'static> {
    let arrow = if collapsed { "▸" } else { "▾" };
    let left = Span::styled(format!(" {arrow} {title}"), Style::default().bold());
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

/// A file row: icon, name colored by status, dimmed parent directory, and a
/// right-aligned status letter — VS Code Source Control's row anatomy.
fn file_item(entry: &FileEntry, width: usize) -> ListItem<'static> {
    let (dir, name) = match entry.path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, entry.path.as_str()),
    };
    let color = letter_color(entry.letter);
    let mut spans = vec![
        Span::raw(format!("   {} ", icon_for(name))),
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

/// Cut `s` down to at most `max` display columns, ending in `…` when trimmed.
/// Empty when even the ellipsis wouldn't fit.
fn truncate_to(s: String, max: usize) -> String {
    if Span::raw(s.as_str()).width() <= max {
        return s;
    }
    if max < 2 {
        return String::new();
    }
    let mut out = String::new();
    for c in s.chars() {
        let mut candidate = out.clone();
        candidate.push(c);
        if Span::raw(candidate.as_str()).width() + 1 > max {
            break;
        }
        out = candidate;
    }
    out.push('…');
    out
}
