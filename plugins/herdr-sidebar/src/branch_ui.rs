use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};

use crate::git::{Branch, Git, Status};
use crate::icons::IconTheme;
use crate::ui::{
    branch_icon, hits, hover_style, keep_visible_scroll, palette, selection_style, truncate_to,
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
}

pub struct BranchPicker {
    pub git: Git,
    branches: Vec<Branch>,
    selected: usize,
    scroll: usize,
    rect: Rect,
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
            .unwrap_or(0);
        Ok(Self {
            git,
            branches,
            selected,
            scroll: 0,
            rect: Rect::default(),
        })
    }

    pub fn key(&mut self, key: KeyEvent) -> PickerAction {
        match key.code {
            KeyCode::Esc => PickerAction::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                PickerAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.branches.len().saturating_sub(1));
                PickerAction::None
            }
            KeyCode::Home => {
                self.selected = 0;
                PickerAction::None
            }
            KeyCode::End => {
                self.selected = self.branches.len().saturating_sub(1);
                PickerAction::None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self
                .branches
                .get(self.selected)
                .cloned()
                .map(PickerAction::Checkout)
                .unwrap_or(PickerAction::None),
            _ => PickerAction::None,
        }
    }

    pub fn mouse(&mut self, mouse: MouseEvent) -> PickerAction {
        let inner = self.rect.inner(ratatui::layout::Margin::new(1, 1));
        let item_at = |row: u16, col: u16| {
            (col >= inner.x
                && col < inner.x + inner.width
                && row >= inner.y
                && row < inner.y + inner.height)
                .then(|| self.scroll + usize::from(row - inner.y))
                .filter(|index| *index < self.branches.len())
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
                self.selected = (self.selected + 3).min(self.branches.len().saturating_sub(1));
                PickerAction::None
            }
            MouseEventKind::Down(MouseButton::Left) => item_at(mouse.row, mouse.column)
                .and_then(|index| self.branches.get(index).cloned())
                .map(PickerAction::Checkout)
                .unwrap_or_else(|| {
                    if hits(self.rect, mouse.column, mouse.row) {
                        PickerAction::None
                    } else {
                        PickerAction::Close
                    }
                }),
            _ => PickerAction::None,
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let desired_width = self
            .branches
            .iter()
            .map(|branch| Span::raw(branch.name.as_str()).width() + 13)
            .max()
            .unwrap_or(28)
            .max(28) as u16;
        let width = desired_width.min(area.width);
        let height = (self.branches.len() as u16 + 2).min(area.height).min(18);
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 3,
            width,
            height,
        );
        self.rect = popup;
        let visible = usize::from(height.saturating_sub(2));
        self.scroll = keep_visible_scroll(self.selected, visible, self.branches.len());
        let inner_width = usize::from(width.saturating_sub(2));
        let items: Vec<ListItem> = self
            .branches
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible)
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
            })
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

    #[test]
    fn picker_starts_on_current_branch() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-branch-picker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            &["init", "-q"][..],
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "initial",
            ][..],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let git = Git::discover(&root).unwrap();
        let picker = BranchPicker::open(git).unwrap();
        assert!(picker.branches[picker.selected].current);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_glyph_restores_refresh_icon_when_idle() {
        assert_eq!(sync_glyph(false), "⟳");
        assert!(SYNC_FRAMES.contains(&sync_glyph(true)));
    }
}
