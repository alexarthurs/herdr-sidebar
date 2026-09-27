//! Experimental, file-only editor used by the preview pane.
//!
//! This is deliberately independent from diff and `git show` rendering: only
//! a strict UTF-8 file request can create an [`Editor`]. Text positions are
//! Unicode scalar indices (never byte offsets), and scrolling is in visual
//! rows produced by word-aware wrapping.

use std::fmt;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use crate::actions::{ClipboardWrite, copy_to_clipboard, paste_from_clipboard};

const TAB_WIDTH: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualRow {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
}

#[derive(Debug)]
pub enum OpenError {
    Io(std::io::Error),
    TooLarge(usize),
    TooManyLines(usize),
    Binary,
    InvalidUtf8,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot edit: {e}"),
            Self::TooLarge(n) => write!(f, "cannot edit files larger than 1 MiB ({n} bytes)"),
            Self::TooManyLines(n) => write!(f, "cannot edit files over 5000 lines ({n} lines)"),
            Self::Binary => f.write_str("binary files are read-only"),
            Self::InvalidUtf8 => f.write_str("only valid UTF-8 files can be edited"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditAction {
    None,
    Leave,
    Close,
    Save,
}

#[derive(Clone, Debug, Default)]
struct Search {
    query: String,
}

pub struct Editor {
    path: PathBuf,
    lines: Vec<String>,
    cursor: TextPos,
    anchor: Option<TextPos>,
    preferred_x: Option<usize>,
    pub scroll: usize,
    original_bytes: Vec<u8>,
    bom: bool,
    line_ending: LineEnding,
    pub dirty: bool,
    pub external_changed: bool,
    status: Option<String>,
    search: Option<Search>,
    mouse_anchor: Option<TextPos>,
}

impl Editor {
    pub fn open(path: &Path, max_bytes: usize, max_lines: usize) -> Result<Self, OpenError> {
        let bytes = std::fs::read(path).map_err(OpenError::Io)?;
        Self::from_bytes(path.to_path_buf(), bytes, max_bytes, max_lines)
    }

    fn from_bytes(
        path: PathBuf,
        bytes: Vec<u8>,
        max_bytes: usize,
        max_lines: usize,
    ) -> Result<Self, OpenError> {
        if bytes.len() > max_bytes {
            return Err(OpenError::TooLarge(bytes.len()));
        }
        if bytes.contains(&0) {
            return Err(OpenError::Binary);
        }
        let bom = bytes.starts_with(&[0xef, 0xbb, 0xbf]);
        let text_bytes = if bom { &bytes[3..] } else { &bytes };
        let text = std::str::from_utf8(text_bytes).map_err(|_| OpenError::InvalidUtf8)?;
        let line_ending = if text.contains("\r\n") {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        };
        let normalized = text.replace("\r\n", "\n");
        let lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
        if lines.len() > max_lines {
            return Err(OpenError::TooManyLines(lines.len()));
        }
        Ok(Self {
            path,
            lines,
            cursor: TextPos::default(),
            anchor: None,
            preferred_x: None,
            scroll: 0,
            original_bytes: bytes,
            bom,
            line_ending,
            dirty: false,
            external_changed: false,
            status: None,
            search: None,
            mouse_anchor: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    pub fn context(&self) -> String {
        self.path.display().to_string()
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = Some(status.into());
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn cursor(&self) -> TextPos {
        self.cursor
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn rows(&self, width: usize) -> Vec<VisualRow> {
        wrapped_rows(&self.lines, width)
    }

    pub fn on_key(&mut self, key: KeyEvent, wrap_width: usize, page: usize) -> EditAction {
        self.mouse_anchor = None;
        if self.search.is_some() {
            return self.on_search_key(key);
        }
        let shortcut = (key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT))
            || key.modifiers.contains(KeyModifiers::SUPER);
        if shortcut {
            match key.code {
                KeyCode::Char('s') => return EditAction::Save,
                KeyCode::Char('q') => return EditAction::Close,
                KeyCode::Char('f') => {
                    self.search = Some(Search::default());
                    self.status = None;
                    return EditAction::None;
                }
                KeyCode::Char('a') => {
                    self.anchor = Some(TextPos::default());
                    let line = self.lines.len().saturating_sub(1);
                    self.cursor = TextPos {
                        line,
                        col: char_len(&self.lines[line]),
                    };
                    return EditAction::None;
                }
                KeyCode::Char('c') => {
                    let text = self.selected_text().unwrap_or_else(|| {
                        let mut line = self.lines[self.cursor.line].clone();
                        if self.cursor.line + 1 < self.lines.len() {
                            line.push('\n');
                        }
                        line
                    });
                    self.status = Some(match copy_to_clipboard(&text) {
                        Ok(ClipboardWrite::Native) => "copied to clipboard".into(),
                        Ok(ClipboardWrite::Osc52Unacknowledged) => {
                            "sent to terminal clipboard".into()
                        }
                        Err(e) => format!("clipboard unavailable: {e}"),
                    });
                    return EditAction::None;
                }
                KeyCode::Char('x') => {
                    if let Some(text) = self.selected_text() {
                        match copy_to_clipboard(&text) {
                            Ok(ClipboardWrite::Native) => {
                                self.delete_selection();
                                self.mark_changed();
                                self.status = Some("cut to clipboard".into());
                            }
                            Ok(ClipboardWrite::Osc52Unacknowledged) => {
                                // The editor has no undo, and an OSC 52 write is never
                                // acknowledged by the terminal: deleting here on an
                                // unconfirmed copy could destroy text that never made
                                // it to the clipboard. Leave the selection in place.
                                self.status = Some(
                                    "sent to terminal clipboard (cut skipped: copy can't be confirmed over SSH)"
                                        .into(),
                                );
                            }
                            Err(e) => self.status = Some(format!("clipboard unavailable: {e}")),
                        }
                    } else {
                        self.status = Some("select text before cutting".into());
                    }
                    return EditAction::None;
                }
                KeyCode::Char('v') => {
                    self.status = Some(match paste_from_clipboard() {
                        Ok(text) => {
                            self.insert_text(&text);
                            "pasted from clipboard".into()
                        }
                        Err(e) => format!("clipboard unavailable: {e}"),
                    });
                    return EditAction::None;
                }
                KeyCode::Left => {
                    self.prepare_move(key.modifiers);
                    self.move_word_left();
                    return EditAction::None;
                }
                KeyCode::Right => {
                    self.prepare_move(key.modifiers);
                    self.move_word_right();
                    return EditAction::None;
                }
                _ => return EditAction::None,
            }
        }

        match key.code {
            KeyCode::Esc => EditAction::Leave,
            KeyCode::Left => {
                self.prepare_move(key.modifiers);
                self.move_left();
                EditAction::None
            }
            KeyCode::Right => {
                self.prepare_move(key.modifiers);
                self.move_right();
                EditAction::None
            }
            KeyCode::Up => {
                self.prepare_move(key.modifiers);
                self.move_visual(-1, wrap_width);
                EditAction::None
            }
            KeyCode::Down => {
                self.prepare_move(key.modifiers);
                self.move_visual(1, wrap_width);
                EditAction::None
            }
            KeyCode::PageUp => {
                self.prepare_move(key.modifiers);
                self.move_visual(-(page as isize), wrap_width);
                EditAction::None
            }
            KeyCode::PageDown => {
                self.prepare_move(key.modifiers);
                self.move_visual(page as isize, wrap_width);
                EditAction::None
            }
            KeyCode::Home => {
                self.prepare_move(key.modifiers);
                self.cursor.col = 0;
                self.preferred_x = None;
                EditAction::None
            }
            KeyCode::End => {
                self.prepare_move(key.modifiers);
                self.cursor.col = char_len(&self.lines[self.cursor.line]);
                self.preferred_x = None;
                EditAction::None
            }
            KeyCode::Backspace => {
                self.backspace();
                EditAction::None
            }
            KeyCode::Delete => {
                self.delete_forward();
                EditAction::None
            }
            KeyCode::Enter => {
                self.insert_text("\n");
                EditAction::None
            }
            KeyCode::Tab => {
                self.insert_text(&" ".repeat(TAB_WIDTH));
                EditAction::None
            }
            KeyCode::Char(c) => {
                // CONTROL+ALT is AltGr on Windows and must remain insertable.
                self.insert_text(&c.to_string());
                EditAction::None
            }
            _ => EditAction::None,
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) -> EditAction {
        let shortcut = (key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT))
            || key.modifiers.contains(KeyModifiers::SUPER);
        match key.code {
            KeyCode::Esc => self.search = None,
            KeyCode::Enter => self.find(key.modifiers.contains(KeyModifiers::SHIFT)),
            KeyCode::Backspace => {
                if let Some(search) = &mut self.search {
                    search.query.pop();
                }
            }
            KeyCode::Char('v') if shortcut => match paste_from_clipboard() {
                Ok(text) => {
                    if let Some(search) = &mut self.search {
                        search.query.push_str(text.trim_end_matches(['\r', '\n']));
                    }
                }
                Err(e) => self.status = Some(format!("clipboard unavailable: {e}")),
            },
            KeyCode::Char(c) if !shortcut => {
                if let Some(search) = &mut self.search {
                    search.query.push(c);
                }
            }
            _ => {}
        }
        EditAction::None
    }

    fn find(&mut self, reverse: bool) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            return;
        };
        if query.is_empty() {
            return;
        }
        let found = if reverse {
            self.find_reverse(&query)
        } else {
            self.find_forward(&query)
        };
        if let Some((start, end)) = found {
            self.anchor = Some(start);
            self.cursor = end;
            self.preferred_x = None;
            self.status = None;
        } else {
            self.status = Some(format!("no match for {query:?}"));
        }
    }

    fn find_forward(&self, query: &str) -> Option<(TextPos, TextPos)> {
        let mut order = (self.cursor.line..self.lines.len()).chain(0..self.cursor.line);
        order
            .find_map(|line| {
                let start_col = if line == self.cursor.line {
                    self.cursor.col.min(char_len(&self.lines[line]))
                } else {
                    0
                };
                find_from(&self.lines[line], query, start_col).map(|col| {
                    (
                        TextPos { line, col },
                        TextPos {
                            line,
                            col: col + char_len(query),
                        },
                    )
                })
            })
            .or_else(|| {
                find_from(&self.lines[self.cursor.line], query, 0)
                    .filter(|col| *col <= self.cursor.col)
                    .map(|col| {
                        (
                            TextPos {
                                line: self.cursor.line,
                                col,
                            },
                            TextPos {
                                line: self.cursor.line,
                                col: col + char_len(query),
                            },
                        )
                    })
            })
    }

    fn find_reverse(&self, query: &str) -> Option<(TextPos, TextPos)> {
        let mut order = (0..=self.cursor.line)
            .rev()
            .chain((self.cursor.line + 1..self.lines.len()).rev());
        order.find_map(|line| {
            let before = if line == self.cursor.line {
                self.cursor.col
            } else {
                char_len(&self.lines[line])
            };
            rfind_before(&self.lines[line], query, before).map(|col| {
                (
                    TextPos { line, col },
                    TextPos {
                        line,
                        col: col + char_len(query),
                    },
                )
            })
        })
    }

    fn prepare_move(&mut self, modifiers: KeyModifiers) {
        if modifiers.contains(KeyModifiers::SHIFT) {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
    }

    fn move_left(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.col = char_len(&self.lines[self.cursor.line]);
        }
        self.preferred_x = None;
    }

    fn move_right(&mut self) {
        let len = char_len(&self.lines[self.cursor.line]);
        if self.cursor.col < len {
            self.cursor.col += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.col = 0;
        }
        self.preferred_x = None;
    }

    fn move_word_left(&mut self) {
        if self.cursor.col == 0 && self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.col = char_len(&self.lines[self.cursor.line]);
        }
        let chars: Vec<char> = self.lines[self.cursor.line].chars().collect();
        while self.cursor.col > 0 && chars[self.cursor.col - 1].is_whitespace() {
            self.cursor.col -= 1;
        }
        while self.cursor.col > 0 && !chars[self.cursor.col - 1].is_whitespace() {
            self.cursor.col -= 1;
        }
        self.preferred_x = None;
    }

    fn move_word_right(&mut self) {
        let chars: Vec<char> = self.lines[self.cursor.line].chars().collect();
        while self.cursor.col < chars.len() && !chars[self.cursor.col].is_whitespace() {
            self.cursor.col += 1;
        }
        while self.cursor.col < chars.len() && chars[self.cursor.col].is_whitespace() {
            self.cursor.col += 1;
        }
        if self.cursor.col == chars.len() && self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.col = 0;
        }
        self.preferred_x = None;
    }

    fn move_visual(&mut self, delta: isize, width: usize) {
        let rows = self.rows(width);
        let current = visual_row_index(&rows, self.cursor).unwrap_or(0);
        let row = &rows[current];
        let x = self.preferred_x.unwrap_or_else(|| {
            display_width_range(&self.lines[row.line], row.start, self.cursor.col)
        });
        self.preferred_x = Some(x);
        let target = current
            .saturating_add_signed(delta)
            .min(rows.len().saturating_sub(1));
        let row = &rows[target];
        self.cursor = TextPos {
            line: row.line,
            col: col_at_display_x(&self.lines[row.line], row.start, row.end, x),
        };
    }

    fn backspace(&mut self) {
        if self.delete_selection() {
            self.mark_changed();
            return;
        }
        if self.cursor.col > 0 {
            let line = &mut self.lines[self.cursor.line];
            let start = byte_at(line, self.cursor.col - 1);
            let end = byte_at(line, self.cursor.col);
            line.replace_range(start..end, "");
            self.cursor.col -= 1;
            self.mark_changed();
        } else if self.cursor.line > 0 {
            let tail = self.lines.remove(self.cursor.line);
            self.cursor.line -= 1;
            self.cursor.col = char_len(&self.lines[self.cursor.line]);
            self.lines[self.cursor.line].push_str(&tail);
            self.mark_changed();
        }
    }

    fn delete_forward(&mut self) {
        if self.delete_selection() {
            self.mark_changed();
            return;
        }
        let len = char_len(&self.lines[self.cursor.line]);
        if self.cursor.col < len {
            let line = &mut self.lines[self.cursor.line];
            let start = byte_at(line, self.cursor.col);
            let end = byte_at(line, self.cursor.col + 1);
            line.replace_range(start..end, "");
            self.mark_changed();
        } else if self.cursor.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor.line + 1);
            self.lines[self.cursor.line].push_str(&next);
            self.mark_changed();
        }
    }

    pub fn insert_text(&mut self, text: &str) {
        self.delete_selection();
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let parts: Vec<&str> = normalized.split('\n').collect();
        let line = &mut self.lines[self.cursor.line];
        let at = byte_at(line, self.cursor.col);
        let tail = line[at..].to_string();
        line.truncate(at);
        line.push_str(parts[0]);
        if parts.len() == 1 {
            self.cursor.col += char_len(parts[0]);
            self.lines[self.cursor.line].push_str(&tail);
        } else {
            for (offset, part) in parts[1..].iter().enumerate() {
                self.lines
                    .insert(self.cursor.line + 1 + offset, (*part).to_string());
            }
            self.cursor.line += parts.len() - 1;
            self.cursor.col = char_len(parts.last().copied().unwrap_or_default());
            self.lines[self.cursor.line].push_str(&tail);
        }
        self.mark_changed();
    }

    fn mark_changed(&mut self) {
        self.dirty = true;
        self.anchor = None;
        self.preferred_x = None;
        self.status = None;
    }

    fn selection(&self) -> Option<(TextPos, TextPos)> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| {
            if anchor < self.cursor {
                (anchor, self.cursor)
            } else {
                (self.cursor, anchor)
            }
        })
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        if start.line == end.line {
            return Some(char_slice(&self.lines[start.line], start.col, end.col));
        }
        let mut out = char_slice(
            &self.lines[start.line],
            start.col,
            char_len(&self.lines[start.line]),
        );
        out.push('\n');
        for line in start.line + 1..end.line {
            out.push_str(&self.lines[line]);
            out.push('\n');
        }
        out.push_str(&char_slice(&self.lines[end.line], 0, end.col));
        Some(out)
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        if start.line == end.line {
            let line = &mut self.lines[start.line];
            line.replace_range(byte_at(line, start.col)..byte_at(line, end.col), "");
        } else {
            let prefix = char_slice(&self.lines[start.line], 0, start.col);
            let suffix = char_slice(
                &self.lines[end.line],
                end.col,
                char_len(&self.lines[end.line]),
            );
            self.lines
                .splice(start.line..=end.line, [format!("{prefix}{suffix}")]);
        }
        self.cursor = start;
        self.anchor = None;
        true
    }

    fn encoded_bytes(&self) -> Vec<u8> {
        let mut text = self.text();
        if self.line_ending == LineEnding::CrLf {
            text = text.replace('\n', "\r\n");
        }
        let mut bytes = Vec::with_capacity(text.len() + usize::from(self.bom) * 3);
        if self.bom {
            bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
        }
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    fn disk_changed(&self) -> bool {
        std::fs::read(&self.path).map_or(true, |bytes| bytes != self.original_bytes)
    }

    pub fn save(&mut self, overwrite_external: bool) -> std::io::Result<SaveOutcome> {
        if !overwrite_external && self.disk_changed() {
            self.external_changed = true;
            return Ok(SaveOutcome::Conflict);
        }
        let bytes = self.encoded_bytes();
        std::fs::write(&self.path, &bytes)?;
        self.original_bytes = bytes;
        self.dirty = false;
        self.external_changed = false;
        self.status = Some("saved".into());
        Ok(SaveOutcome::Saved)
    }

    pub fn poll_external(&mut self, max_bytes: usize, max_lines: usize) {
        if !self.disk_changed() {
            self.external_changed = false;
            return;
        }
        if self.dirty {
            self.external_changed = true;
            self.status = Some("file changed on disk — save will ask before overwriting".into());
            return;
        }
        match std::fs::read(&self.path)
            .map_err(OpenError::Io)
            .and_then(|bytes| Self::from_bytes(self.path.clone(), bytes, max_bytes, max_lines))
        {
            Ok(mut fresh) => {
                fresh.status = Some("reloaded after external change".into());
                *self = fresh;
            }
            Err(e) => {
                self.external_changed = true;
                self.status = Some(format!("external change: {e}"));
            }
        }
    }

    pub fn reload(&mut self, max_bytes: usize, max_lines: usize) -> Result<(), OpenError> {
        let mut fresh = Self::open(&self.path, max_bytes, max_lines)?;
        fresh.status = Some("reloaded from disk".into());
        *self = fresh;
        Ok(())
    }

    pub fn scroll_by(&mut self, delta: isize, width: usize, height: usize) {
        let count = self.rows(width).len();
        let max = count.saturating_sub(height.max(1));
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    pub fn on_mouse(&mut self, mouse: &MouseEvent, body: Rect) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(position) = self.text_position_at(body, mouse.column, mouse.row) else {
                    return;
                };
                let extending = mouse.modifiers.contains(KeyModifiers::SHIFT);
                let anchor = if extending {
                    self.anchor.unwrap_or(self.cursor)
                } else {
                    position
                };
                self.anchor = extending.then_some(anchor);
                self.cursor = position;
                self.mouse_anchor = Some(anchor);
                self.preferred_x = None;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(anchor) = self.mouse_anchor else {
                    return;
                };
                let Some(position) = self.text_position_at(body, mouse.column, mouse.row) else {
                    return;
                };
                self.anchor = Some(anchor);
                self.cursor = position;
                self.preferred_x = None;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(position) = self.text_position_at(body, mouse.column, mouse.row) {
                    self.cursor = position;
                    self.preferred_x = None;
                }
                self.mouse_anchor = None;
            }
            _ => {}
        }
    }

    fn text_position_at(&self, body: Rect, column: u16, row: u16) -> Option<TextPos> {
        if column < body.x || column >= body.right() || row < body.y || row >= body.bottom() {
            return None;
        }
        let gutter = self.lines.len().to_string().len() + 1;
        let width = usize::from(body.width).saturating_sub(gutter).max(1);
        let rows = self.rows(width);
        let visual = self.scroll + usize::from(row - body.y);
        let visual_row = rows.get(visual)?;
        let text_x = usize::from(column - body.x).saturating_sub(gutter);
        Some(TextPos {
            line: visual_row.line,
            col: col_at_display_x(
                &self.lines[visual_row.line],
                visual_row.start,
                visual_row.end,
                text_x,
            ),
        })
    }

    pub fn draw(&mut self, frame: &mut Frame, body: Rect, footer: Rect, prompt: Option<&str>) {
        let number_width = self.lines.len().to_string().len();
        let gutter = number_width + 1;
        let width = usize::from(body.width).saturating_sub(gutter).max(1);
        let rows = self.rows(width);
        let cursor_row = visual_row_index(&rows, self.cursor).unwrap_or(0);
        let height = usize::from(body.height).max(1);
        if cursor_row < self.scroll {
            self.scroll = cursor_row;
        } else if cursor_row >= self.scroll + height {
            self.scroll = cursor_row + 1 - height;
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(height));
        let selection = self.selection();
        let rendered: Vec<Line> = rows
            .iter()
            .skip(self.scroll)
            .take(height)
            .map(|row| {
                let gutter_text = if row.start == 0 {
                    format!("{:>number_width$} ", row.line + 1)
                } else {
                    " ".repeat(gutter)
                };
                let mut spans = vec![Span::styled(gutter_text, Style::default().dim())];
                let mut x = 0;
                for (col, ch) in self.lines[row.line]
                    .chars()
                    .enumerate()
                    .skip(row.start)
                    .take(row.end - row.start)
                {
                    let selected = selection.is_some_and(|(start, end)| {
                        TextPos {
                            line: row.line,
                            col,
                        } >= start
                            && TextPos {
                                line: row.line,
                                col,
                            } < end
                    });
                    let style = if selected {
                        Style::default().bg(crate::ui::palette().text_selection_bg)
                    } else {
                        Style::default()
                    };
                    if ch == '\t' {
                        let spaces = TAB_WIDTH - (x % TAB_WIDTH);
                        spans.push(Span::styled(" ".repeat(spaces), style));
                        x += spaces;
                    } else {
                        spans.push(Span::styled(ch.to_string(), style));
                        x += char_width(ch, x);
                    }
                }
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(rendered), body);

        let footer_text = if let Some(prompt) = prompt {
            prompt.to_string()
        } else if let Some(search) = &self.search {
            format!(
                " Find: {}  Enter next  Shift+Enter previous  Esc done",
                search.query
            )
        } else if let Some(status) = &self.status {
            format!(" {status}")
        } else {
            " click/drag select  arrows move  Ctrl/Cmd+F find  Ctrl/Cmd+S save  Esc preview".into()
        };
        frame.render_widget(Paragraph::new(Line::from(footer_text).dim()), footer);

        if prompt.is_none() && self.search.is_none() && body.width > gutter as u16 {
            let row = &rows[cursor_row];
            let x = display_width_range(&self.lines[row.line], row.start, self.cursor.col);
            let screen_y =
                body.y + u16::try_from(cursor_row.saturating_sub(self.scroll)).unwrap_or(0);
            let screen_x =
                body.x + u16::try_from(gutter + x).unwrap_or(body.width.saturating_sub(1));
            frame.set_cursor_position(Position::new(
                screen_x.min(body.right().saturating_sub(1)),
                screen_y.min(body.bottom().saturating_sub(1)),
            ));
        }
    }
}

pub fn wrapped_rows(lines: &[String], width: usize) -> Vec<VisualRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for (line_idx, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            rows.push(VisualRow {
                line: line_idx,
                start: 0,
                end: 0,
            });
            continue;
        }
        let mut start = 0;
        while start < chars.len() {
            let mut x = 0;
            let mut end = start;
            let mut last_break = None;
            while end < chars.len() {
                let w = char_width(chars[end], x);
                if x + w > width && end > start {
                    break;
                }
                x += w;
                end += 1;
                if chars[end - 1].is_whitespace() {
                    last_break = Some(end);
                }
                if x >= width {
                    break;
                }
            }
            if end < chars.len()
                && let Some(boundary) = last_break
                && boundary > start
            {
                end = boundary;
            }
            if end == start {
                end += 1;
            }
            rows.push(VisualRow {
                line: line_idx,
                start,
                end,
            });
            start = end;
        }
    }
    rows
}

fn visual_row_index(rows: &[VisualRow], pos: TextPos) -> Option<usize> {
    rows.iter().enumerate().find_map(|(idx, row)| {
        if row.line != pos.line {
            return None;
        }
        let last_for_line = rows.get(idx + 1).is_none_or(|next| next.line != row.line);
        (pos.col < row.end || (last_for_line && pos.col == row.end)).then_some(idx)
    })
}

fn char_width(ch: char, x: usize) -> usize {
    if ch == '\t' {
        TAB_WIDTH - (x % TAB_WIDTH)
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

fn display_width_range(line: &str, start: usize, end: usize) -> usize {
    line.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .fold(0, |x, ch| x + char_width(ch, x))
}

fn col_at_display_x(line: &str, start: usize, end: usize, wanted: usize) -> usize {
    let mut x = 0;
    for (offset, ch) in line.chars().skip(start).take(end - start).enumerate() {
        let width = char_width(ch, x);
        if x + width > wanted {
            return start + offset;
        }
        x += width;
    }
    end
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn byte_at(s: &str, char_col: usize) -> usize {
    s.char_indices()
        .nth(char_col)
        .map_or(s.len(), |(idx, _)| idx)
}

fn char_slice(s: &str, start: usize, end: usize) -> String {
    s[byte_at(s, start)..byte_at(s, end)].to_string()
}

fn find_from(haystack: &str, needle: &str, start_col: usize) -> Option<usize> {
    let start = byte_at(haystack, start_col);
    haystack[start..]
        .find(needle)
        .map(|offset| start_col + haystack[start..start + offset].chars().count())
}

fn rfind_before(haystack: &str, needle: &str, before_col: usize) -> Option<usize> {
    let end = byte_at(haystack, before_col);
    haystack[..end]
        .rfind(needle)
        .map(|offset| haystack[..offset].chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> Editor {
        Editor::from_bytes(
            PathBuf::from("test.txt"),
            text.as_bytes().to_vec(),
            1_000_000,
            5000,
        )
        .unwrap()
    }

    #[test]
    fn word_wrap_prefers_whitespace_and_hard_wraps_long_words() {
        let rows = wrapped_rows(&["one two enormousword".into()], 7);
        let pieces: Vec<String> = rows
            .iter()
            .map(|r| char_slice("one two enormousword", r.start, r.end))
            .collect();
        assert_eq!(pieces, ["one ", "two ", "enormou", "sword"]);
    }

    #[test]
    fn insert_delete_and_newlines_are_unicode_safe() {
        let mut e = editor("héllo\nworld");
        e.cursor = TextPos { line: 0, col: 2 };
        e.insert_text("🙂\nnew");
        assert_eq!(e.text(), "hé🙂\nnewllo\nworld");
        e.backspace();
        assert_eq!(e.text(), "hé🙂\nnello\nworld");
    }

    #[test]
    fn multiline_selection_is_replaced() {
        let mut e = editor("alpha\nbeta\ngamma");
        e.anchor = Some(TextPos { line: 0, col: 2 });
        e.cursor = TextPos { line: 2, col: 2 };
        e.insert_text("X");
        assert_eq!(e.text(), "alXmma");
        assert_eq!(e.cursor, TextPos { line: 0, col: 3 });
    }

    #[test]
    fn invalid_utf8_and_binary_files_cannot_enter_edit_mode() {
        assert!(matches!(
            Editor::from_bytes(PathBuf::from("x"), vec![0xff], 100, 10),
            Err(OpenError::InvalidUtf8)
        ));
        assert!(matches!(
            Editor::from_bytes(PathBuf::from("x"), b"a\0b".to_vec(), 100, 10),
            Err(OpenError::Binary)
        ));
        let mut late_nul = vec![b'a'; 9000];
        late_nul.push(0);
        assert!(matches!(
            Editor::from_bytes(PathBuf::from("x"), late_nul, 10_000, 10),
            Err(OpenError::Binary)
        ));
    }

    #[test]
    fn visual_navigation_moves_across_wrapped_rows() {
        let mut e = editor("one two three\nlast");
        e.cursor = TextPos { line: 0, col: 1 };
        e.move_visual(1, 5);
        assert_eq!(e.cursor, TextPos { line: 0, col: 5 });
        e.move_visual(2, 5);
        assert_eq!(e.cursor, TextPos { line: 1, col: 1 });
    }

    #[test]
    fn mouse_click_and_drag_follow_wrapped_rows() {
        let mut e = editor("one two three");
        let body = Rect::new(5, 7, 10, 3);
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 7,
            row: 7,
            modifiers: KeyModifiers::NONE,
        };
        e.on_mouse(&down, body);
        assert_eq!(e.cursor, TextPos { line: 0, col: 0 });

        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 9,
            row: 8,
            modifiers: KeyModifiers::NONE,
        };
        e.on_mouse(&drag, body);
        assert_eq!(e.cursor, TextPos { line: 0, col: 10 });
        assert_eq!(
            e.selection(),
            Some((TextPos { line: 0, col: 0 }, TextPos { line: 0, col: 10 }))
        );

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..drag
        };
        e.on_mouse(&up, body);
        assert!(e.mouse_anchor.is_none());
    }

    #[test]
    fn shift_click_extends_the_existing_selection() {
        let mut e = editor("one two three");
        e.cursor = TextPos { line: 0, col: 3 };
        let body = Rect::new(0, 0, 10, 3);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 1,
            modifiers: KeyModifiers::SHIFT,
        };
        e.on_mouse(&click, body);
        assert_eq!(
            e.selection(),
            Some((TextPos { line: 0, col: 3 }, TextPos { line: 0, col: 8 }))
        );
    }

    #[test]
    fn find_selects_unicode_text_and_wraps_to_the_first_match() {
        let mut e = editor("🙂 first\nsecond 🙂");
        e.cursor = TextPos { line: 1, col: 8 };
        e.search = Some(Search {
            query: "🙂".into()
        });
        e.find(false);
        assert_eq!(
            e.selection(),
            Some((TextPos::default(), TextPos { line: 0, col: 1 }))
        );
    }

    #[test]
    fn save_detects_external_changes_and_preserves_bom_and_crlf() {
        let dir = std::env::temp_dir().join(format!("herdr-editor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("utf8.txt");
        std::fs::write(&path, b"\xef\xbb\xbfone\r\ntwo\r\n").unwrap();
        let mut e = Editor::open(&path, 1000, 100).unwrap();
        e.cursor = TextPos { line: 0, col: 3 };
        e.insert_text("!");
        std::fs::write(&path, b"external\n").unwrap();
        assert_eq!(e.save(false).unwrap(), SaveOutcome::Conflict);
        assert_eq!(std::fs::read(&path).unwrap(), b"external\n");
        assert_eq!(e.save(true).unwrap(), SaveOutcome::Saved);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xef\xbb\xbfone!\r\ntwo\r\n"
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn clean_buffer_reloads_an_external_utf8_change() {
        let dir = std::env::temp_dir().join(format!("herdr-editor-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reload.txt");
        std::fs::write(&path, "before").unwrap();
        let mut e = Editor::open(&path, 1000, 100).unwrap();
        std::fs::write(&path, "after 🙂").unwrap();
        e.poll_external(1000, 100);
        assert_eq!(e.text(), "after 🙂");
        assert!(!e.external_changed);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
