use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};

use crate::git::{Branch, Git, Status};
use crate::icons::IconTheme;
use crate::ui::{
    branch_icon, hits, hover_style, input_tail, keep_visible_scroll, palette, selection_style,
    truncate_to,
};

const SYNC_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
const SYNC_FRAME_MILLIS: u128 = 120;

pub fn sync_glyph(syncing: bool) -> &'static str {
    if !syncing {
        return "⟳";
    }
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    SYNC_FRAMES[(elapsed / SYNC_FRAME_MILLIS) as usize % SYNC_FRAMES.len()]
}

pub enum PickerAction {
    None,
    Close,
    Checkout(Branch),
    Create(String),
}

/// Mirrors the Explorer's New File / New Folder prompts: a row that opens a
/// one-line name field.
const NEW_BRANCH_ROW: &str = "New branch…";

pub struct BranchPicker {
    pub git: Git,
    branches: Vec<Branch>,
    selected: usize,
    scroll: usize,
    rect: Rect,
    /// `Some` while the name field is open; the picker list is frozen behind
    /// it and every keystroke belongs to the field.
    naming: Option<String>,
}

impl BranchPicker {
    pub fn open(git: Git) -> Result<Self, String> {
        let branches = git.branch_choices()?;
        if branches.is_empty() {
            return Err("no branches found".to_string());
        }
        let selected = branches
            .iter()
            .position(|branch| branch.current)
            .map_or(0, |index| index + 1);
        Ok(Self {
            git,
            branches,
            selected,
            scroll: 0,
            rect: Rect::default(),
            naming: None,
        })
    }

    fn row_count(&self) -> usize {
        self.branches.len() + 1
    }

    fn branch_at(&self, row: usize) -> Option<&Branch> {
        self.branches.get(row.checked_sub(1)?)
    }

    fn activate(&mut self, row: usize) -> PickerAction {
        match self.branch_at(row) {
            Some(branch) => PickerAction::Checkout(branch.clone()),
            None => {
                self.naming = Some(String::new());
                PickerAction::None
            }
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> PickerAction {
        // The name field owns every key while it is open, so a branch called
        // `k` cannot be typed into a list that reads k as "move up".
        if let Some(name) = &mut self.naming {
            return match key.code {
                KeyCode::Esc => {
                    self.naming = None;
                    PickerAction::None
                }
                KeyCode::Enter => {
                    let typed = name.trim().to_string();
                    if typed.is_empty() {
                        PickerAction::None
                    } else {
                        self.naming = None;
                        PickerAction::Create(typed)
                    }
                }
                KeyCode::Backspace => {
                    name.pop();
                    PickerAction::None
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    name.push(c);
                    PickerAction::None
                }
                _ => PickerAction::None,
            };
        }

        match key.code {
            KeyCode::Esc => PickerAction::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                PickerAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.row_count().saturating_sub(1));
                PickerAction::None
            }
            KeyCode::Home => {
                self.selected = 0;
                PickerAction::None
            }
            KeyCode::End => {
                self.selected = self.row_count().saturating_sub(1);
                PickerAction::None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(self.selected),
            _ => PickerAction::None,
        }
    }

    pub fn mouse(&mut self, mouse: MouseEvent) -> PickerAction {
        // While naming, the list behind the field is inert: a stray click or
        // scroll must not switch branches out from under a half-typed name.
        if self.naming.is_some() {
            return match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if !hits(self.rect, mouse.column, mouse.row) =>
                {
                    self.naming = None;
                    PickerAction::Close
                }
                _ => PickerAction::None,
            };
        }

        let inner = self.rect.inner(ratatui::layout::Margin::new(1, 1));
        let rows = self.row_count();
        let item_at = |row: u16, col: u16| {
            (col >= inner.x
                && col < inner.x + inner.width
                && row >= inner.y
                && row < inner.y + inner.height)
                .then(|| self.scroll + usize::from(row - inner.y))
                .filter(|index| *index < rows)
        };
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(index) = item_at(mouse.row, mouse.column) {
                    self.selected = index;
                }
                PickerAction::None
            }
            MouseEventKind::ScrollUp => {
                self.selected = self.selected.saturating_sub(3);
                PickerAction::None
            }
            MouseEventKind::ScrollDown => {
                self.selected = (self.selected + 3).min(rows.saturating_sub(1));
                PickerAction::None
            }
            MouseEventKind::Down(MouseButton::Left) => match item_at(mouse.row, mouse.column) {
                Some(index) => self.activate(index),
                None if hits(self.rect, mouse.column, mouse.row) => PickerAction::None,
                None => PickerAction::Close,
            },
            _ => PickerAction::None,
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let desired_width = self
            .branches
            .iter()
            .map(|branch| Span::raw(branch.name.as_str()).width() + 13)
            .chain(std::iter::once(Span::raw(NEW_BRANCH_ROW).width() + 13))
            .max()
            .unwrap_or(28)
            .max(28) as u16;
        let width = desired_width.min(area.width);
        let height = (self.row_count() as u16 + 2).min(area.height).min(18);
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 3,
            width,
            height,
        );
        self.rect = popup;
        let visible = usize::from(height.saturating_sub(2));
        self.scroll = keep_visible_scroll(self.selected, visible, self.row_count());
        let inner_width = usize::from(width.saturating_sub(2));

        if let Some(name) = &self.naming {
            // The list is only as wide as its longest branch name, which is far
            // too narrow for a name being typed — and a block TITLE is silently
            // clipped rather than wrapped, so keep the title short and give the
            // field its own minimum width.
            const FIELD_MIN: u16 = 40;
            let field_width = popup.width.max(FIELD_MIN).min(area.width);
            let field = Rect::new(
                (area.width.saturating_sub(field_width)) / 2,
                popup.y,
                field_width,
                3.min(area.height),
            );
            self.rect = field;
            let field_inner = usize::from(field_width.saturating_sub(2));

            // Drop the hint before the name loses room, the way the Explorer's
            // prompts do.
            let hint = "  ⏎ ok · esc cancel";
            let typed = input_tail(name, field_inner.saturating_sub(2));
            let hint_fits =
                Span::raw(typed.as_str()).width() + Span::raw(hint).width() + 2 <= field_inner;

            let mut spans = vec![
                Span::raw(" "),
                // Tail, not head: a long name must keep its end — where the
                // cursor is — on screen, exactly like the Explorer prompts.
                Span::raw(typed),
                Span::styled("█", Style::default().dim()),
            ];
            if hint_fits {
                spans.push(Span::styled(hint, Style::default().dim()));
            }

            frame.render_widget(Clear, field);
            frame.render_widget(
                Paragraph::new(Line::from(spans)).block(
                    Block::bordered()
                        .title(" New branch ")
                        .border_style(Style::default().fg(palette().accent)),
                ),
                field,
            );
            return;
        }

        let new_row = std::iter::once({
            let label = truncate_to(NEW_BRANCH_ROW.to_string(), inner_width.saturating_sub(3));
            let line = Line::from(vec![
                Span::raw("+ "),
                Span::styled(label, Style::default().fg(palette().accent)),
            ]);
            if self.selected == 0 {
                ListItem::new(line).style(selection_style(true))
            } else {
                ListItem::new(line)
            }
        });

        let branch_rows = self
            .branches
            .iter()
            .enumerate()
            .map(|(index, branch)| (index + 1, branch))
            .map(|(index, branch)| {
                let mark = if branch.current { "✓ " } else { "  " };
                let remote = if branch.remote { "  remote" } else { "" };
                let mark_width = Span::raw(mark).width();
                let remote_width = Span::raw(remote).width();
                let reserved = mark_width + remote_width + 1;
                let name = truncate_to(branch.name.clone(), inner_width.saturating_sub(reserved));
                let name_width = Span::raw(name.as_str()).width();
                let pad = inner_width.saturating_sub(mark_width + name_width + remote_width);
                let line = Line::from(vec![
                    Span::raw(mark),
                    Span::raw(name),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(remote, Style::default().dim()),
                ]);
                if index == self.selected {
                    ListItem::new(line).style(selection_style(true))
                } else {
                    ListItem::new(line)
                }
            });

        // Scrolling covers the combined list; New branch… is not pinned.
        let items: Vec<ListItem> = new_row
            .chain(branch_rows)
            .skip(self.scroll)
            .take(visible)
            .collect();
        frame.render_widget(Clear, popup);
        frame.render_widget(
            List::new(items).block(
                Block::bordered()
                    .title(" Switch Branch ")
                    .border_style(Style::default().fg(palette().accent)),
            ),
            popup,
        );
    }
}

#[derive(Clone, Copy, Default)]
pub struct FooterZones {
    pub branch: Rect,
    pub sync: Rect,
}

pub fn draw_git_footer(
    frame: &mut Frame,
    area: Rect,
    theme: IconTheme,
    status: &Status,
    syncing: bool,
    mouse_pos: Option<(u16, u16)>,
) -> FooterZones {
    let branch_text = format!(" {} {} ", branch_icon(theme), status.branch);
    let sync_icon = sync_glyph(syncing);
    let sync_text = if status.has_upstream {
        format!("{sync_icon} {}↓ {}↑", status.behind, status.ahead)
    } else {
        sync_icon.to_string()
    };
    let sync_width = Span::raw(sync_text.as_str())
        .width()
        .min(area.width as usize) as u16;
    let branch_width = Span::raw(branch_text.as_str())
        .width()
        .min(area.width.saturating_sub(sync_width) as usize) as u16;
    let branch = Rect::new(area.x, area.y, branch_width, 1);
    let sync = Rect::new(area.x + branch_width, area.y, sync_width, 1);
    let button_style = |rect| {
        if mouse_pos.is_some_and(|(x, y)| hits(rect, x, y)) {
            hover_style()
        } else {
            Style::default().dim()
        }
    };
    frame.render_widget(
        Paragraph::new(truncate_to(branch_text, usize::from(branch_width)))
            .style(button_style(branch))
            .alignment(Alignment::Left),
        branch,
    );
    frame.render_widget(
        Paragraph::new(truncate_to(sync_text, usize::from(sync_width)))
            .style(button_style(sync))
            .alignment(Alignment::Left),
        sync,
    );
    FooterZones { branch, sync }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn picker_starts_on_current_branch() {
        let git = Git::discover(std::path::Path::new(".")).unwrap();
        let picker = BranchPicker::open(git).unwrap();
        assert!(picker.branch_at(picker.selected).unwrap().current);
    }

    #[test]
    fn row_zero_is_new_branch_and_opens_the_name_field() {
        let git = Git::discover(std::path::Path::new(".")).unwrap();
        let mut picker = BranchPicker::open(git).unwrap();
        assert!(picker.branch_at(0).is_none(), "row 0 is not a branch");

        picker.selected = 0;
        assert!(matches!(
            picker.key(key(KeyCode::Enter)),
            PickerAction::None
        ));
        assert_eq!(picker.naming.as_deref(), Some(""));
    }

    #[test]
    fn typing_a_name_survives_keys_the_list_would_have_eaten() {
        let git = Git::discover(std::path::Path::new(".")).unwrap();
        let mut picker = BranchPicker::open(git).unwrap();
        picker.selected = 0;
        picker.key(key(KeyCode::Enter));

        for c in "kj/fix".chars() {
            picker.key(key(KeyCode::Char(c)));
        }
        picker.key(key(KeyCode::Backspace));
        assert_eq!(picker.naming.as_deref(), Some("kj/fi"));

        match picker.key(key(KeyCode::Enter)) {
            PickerAction::Create(name) => assert_eq!(name, "kj/fi"),
            _ => panic!("Enter on a typed name must create it"),
        }
        assert!(picker.naming.is_none(), "the field closes after creating");
    }

    #[test]
    fn an_empty_name_neither_creates_nor_closes_the_picker() {
        let git = Git::discover(std::path::Path::new(".")).unwrap();
        let mut picker = BranchPicker::open(git).unwrap();
        picker.selected = 0;
        picker.key(key(KeyCode::Enter));

        picker.key(key(KeyCode::Char(' ')));
        assert!(matches!(
            picker.key(key(KeyCode::Enter)),
            PickerAction::None
        ));
        assert!(picker.naming.is_some(), "still typing");

        assert!(matches!(picker.key(key(KeyCode::Esc)), PickerAction::None));
        assert!(picker.naming.is_none());
        assert!(matches!(picker.key(key(KeyCode::Esc)), PickerAction::Close));
    }

    #[test]
    fn navigation_reaches_every_row_including_the_new_branch_row() {
        let git = Git::discover(std::path::Path::new(".")).unwrap();
        let mut picker = BranchPicker::open(git).unwrap();
        let last = picker.row_count() - 1;

        picker.key(key(KeyCode::End));
        assert_eq!(picker.selected, last);
        assert!(picker.branch_at(picker.selected).is_some());

        picker.key(key(KeyCode::Home));
        assert_eq!(picker.selected, 0);
        picker.key(key(KeyCode::Up));
        assert_eq!(picker.selected, 0, "cannot move above New branch…");
    }

    #[test]
    fn sync_glyph_restores_refresh_icon_when_idle() {
        assert_eq!(sync_glyph(false), "⟳");
        assert!(SYNC_FRAMES.contains(&sync_glyph(true)));
    }
}
