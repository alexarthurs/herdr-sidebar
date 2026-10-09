//! Preview/editor tabs for file contents, git diffs, and history. One
//! ephemeral tab is reused until a double-click pins it; every preview tab
//! gets its own docked sidebar. A small control file steers each running
//! viewer so repeated clicks update in place without shell-specific launch
//! quoting. Diff requests re-run git every couple of seconds.
//! `q`/Esc (or clicking the ✕ header) closes the pane itself.
//!
//! The tail of this module is the CLIENT side — the request format plus the
//! ensure-a-viewer-pane logic both sidebar views share.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use crate::ansi;
use crate::editor::{EditAction, Editor, SaveOutcome};
use crate::icons::{IconTheme, icon};
use crate::ipc;
use crate::ui::{icon_style as ui_icon_style, palette};

/// Metadata source/token that marks the viewer pane, so the sidebar can find
/// and reuse it (distinct from the sidebar's own identity tokens).
pub const METADATA_SOURCE: &str = "herdr-sidebar-preview";

/// How often the control file is re-checked while idle.
const POLL: Duration = Duration::from_millis(250);
/// Once a worker is active, collect it promptly instead of sleeping for a
/// second idle interval before replacing the loading frame.
const LOAD_POLL: Duration = Duration::from_millis(16);

/// Preview size guards: don't slurp huge files into a pane.
const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 5000;

/// Body width from the most recent `draw_doc` call. Markdown loads happen on
/// a worker before layout is available, so glow uses this snapshot and the
/// event loop reloads a glow document if its actual body width differs.
static LAST_BODY_WIDTH: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(80);

/// Directory for the sidebar's private scratch files (viewer control files).
/// Lives in the plugin's per-user state dir (see `rundir`) rather than the
/// shared OS temp dir, so another local user can never share it with us.
/// Pure: does not create or chmod anything. Callers that are about to WRITE
/// through a path under it must call `rundir::ensure_private` first.
fn scratch_dir() -> PathBuf {
    crate::rundir::dir("scratch")
}

fn legacy_scratch_dir() -> PathBuf {
    std::env::temp_dir().join("herdr-sidebar-scratch")
}

/// Is `path` a control file CONFINED to a recognized private scratch directory?
/// The state-dir location is current; the old temp location remains accepted
/// only when it still verifies as private, so already-running preview panes can
/// survive one upgrade. Every
/// control-file operation gates on this rather than trusting a path just
/// because its immediate parent happens to be private — a metadata token can
/// name any path, including one outside `scratch_dir()` that we also happen
/// to own privately (an attacker's own `~/.ssh`, say, if it is 0700). Three
/// checks, all re-verified fresh on every call:
/// - `path`'s parent is LEXICALLY the expected directory — a plain path comparison,
///   never `canonicalize`, which would follow a symlink and could make a
///   symlinked scratch dir compare equal to somewhere it shouldn't.
/// - that parent still verifies as `rundir::is_private` (ownership/mode can
///   change between calls, not just at creation).
/// - `path`'s final component is a plain filename (`Component::Normal`), not
///   `.`/`..`/a root/a prefix: `scratch_dir().join("..")` has a parent EQUAL
///   to `scratch_dir()` (`Path::parent` only strips the last component, it
///   does not resolve `..`), so without this check that "file" would
///   resolve outside `scratch_dir()` entirely.
fn control_path_ok(path: &Path) -> bool {
    control_path_ok_in(path, &scratch_dir()) || control_path_ok_in(path, &legacy_scratch_dir())
}

fn control_path_ok_in(path: &Path, expected_dir: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    parent == expected_dir
        && crate::rundir::is_private(parent)
        && matches!(
            path.components().next_back(),
            Some(std::path::Component::Normal(_))
        )
}

/// Delete a control file, but only when it passes [`control_path_ok`] —
/// never remove_file a path outside the confined scratch directory.
fn remove_control_file(path: &Path) {
    if control_path_ok(path) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(all(test, unix))]
fn remove_control_file_in(path: &Path, expected_dir: &Path) {
    if control_path_ok_in(path, expected_dir) {
        let _ = std::fs::remove_file(path);
    }
}

/// The error a control-file operation reports when [`control_path_ok`]
/// refuses `path`.
fn control_path_err(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "control path {} is not confined to a private sidebar scratch directory (current: {})",
            path.display(),
            scratch_dir().display()
        ),
    )
}

/// Write `contents` to `path`, refusing to follow a pre-existing symlink at
/// that location (defense in depth alongside the scratch dir's 0700 perms),
/// and refusing to write at all unless `path` is confined to the private
/// scratch directory.
fn write_scratch_file(path: &Path, contents: &str) -> std::io::Result<()> {
    if !control_path_ok(path) {
        return Err(control_path_err(path));
    }
    let Some(parent) = path.parent() else {
        return Err(control_path_err(path));
    };
    write_scratch_file_in(path, contents, parent)
}

pub(crate) fn write_scratch_file_in(
    path: &Path,
    contents: &str,
    expected_dir: &Path,
) -> std::io::Result<()> {
    if !control_path_ok_in(path, expected_dir) {
        return Err(control_path_err(path));
    }
    if std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        std::fs::remove_file(path)?;
    }
    std::fs::write(path, contents)
}

/// Legacy/fallback control path for previews created before control paths
/// were carried in pane metadata.
pub fn control_path_for_pane(preview_pane_id: &str) -> PathBuf {
    scratch_dir().join(format!(
        "herdr-sidebar-preview-{}.ctl",
        preview_pane_id.replace(':', "_")
    ))
}

fn fresh_control_path() -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    scratch_dir().join(format!(
        "p-{}-{stamp:x}-{sequence:x}.ctl",
        std::process::id()
    ))
}

fn control_token(control: &Path) -> String {
    control
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| control.display().to_string())
}

fn control_from_token(token: &str) -> PathBuf {
    control_from_token_in(token, &scratch_dir(), &legacy_scratch_dir())
}

fn control_from_token_in(token: &str, current_dir: &Path, legacy_dir: &Path) -> PathBuf {
    let path = Path::new(token);
    if path.components().count() == 1 {
        let current = current_dir.join(path);
        let legacy = legacy_dir.join(path);
        if !current.exists() && legacy.exists() && control_path_ok_in(&legacy, legacy_dir) {
            legacy
        } else {
            current
        }
    } else {
        path.to_path_buf()
    }
}

pub(crate) fn document_token(doc_key: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in doc_key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Identity of the document a preview shows, stamped into the preview
/// pane's `hs-preview-path` token. A file, a diff OF that file, and a
/// `git show` touching it are three different documents with three tabs.
pub fn doc_key_for_file(path: &Path) -> String {
    path.display().to_string()
}

pub fn doc_key_for_diff(root: &Path, rel: &str, kind: &str) -> String {
    format!("diff:{}:{kind}", root.join(rel).display())
}

pub fn doc_key_for_show(root: &Path, spec: &str, path: Option<&str>) -> String {
    match path {
        Some(p) => format!("show:{}:{spec}:{p}", root.display()),
        None => format!("show:{}:{spec}", root.display()),
    }
}

/// The tab name for a document. A textual `· preview` suffix marks the tab as
/// ephemeral without borrowing `*`, which users reasonably read as an unsaved
/// edit marker. A pinned tab reads as a plain name. `tab.rename` is the only
/// display lever herdr gives a plugin.
pub fn tab_label(doc_key: &str, pinned: bool) -> String {
    let display_key = if let Some(diff) = doc_key.strip_prefix("diff:") {
        diff.rsplit_once(':').map(|(path, _)| path).unwrap_or(diff)
    } else if let Some(show) = doc_key.strip_prefix("show:") {
        show.rsplit(':').next().unwrap_or(show)
    } else {
        doc_key
    };
    let name = display_key
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(doc_key);
    if pinned {
        name.to_string()
    } else {
        format!("{name} · preview")
    }
}

/// Inverse of [`tab_label`]: the displayed name and whether it is pinned.
pub fn parse_tab_label(label: &str) -> (String, bool) {
    match label.strip_suffix(" · preview") {
        Some(rest) => (rest.to_string(), false),
        None => (label.to_string(), true),
    }
}

impl Request {
    /// The document identity this request renders.
    fn doc_key(&self) -> String {
        match self {
            Self::Close => String::new(),
            Self::File { path, .. } => doc_key_for_file(path),
            Self::Diff { root, rel, kind } => doc_key_for_diff(root, rel, kind),
            Self::Show { root, spec, path } => doc_key_for_show(root, spec, path.as_deref()),
        }
    }
}

/// What the sidebar asked the viewer to show.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Request {
    /// Graceful close request from the sidebar. The viewer gets a chance to
    /// confirm unsaved edits before it closes its own pane.
    Close,
    File {
        path: PathBuf,
        /// One-based source line to place at the top of the preview.
        line: Option<usize>,
    },
    Diff {
        root: PathBuf,
        rel: String,
        /// "staged" | "worktree" | "untracked" — which diff to run.
        kind: String,
    },
    /// `git show <spec>` — a commit, stash, tag, or branch tip, optionally
    /// narrowed to one file.
    Show {
        root: PathBuf,
        spec: String,
        path: Option<String>,
    },
}

/// Control-file payload for a file preview.
pub fn file_request(path: &Path) -> String {
    format!("file\t{}", path.display())
}

/// Control-file payload for a file preview anchored to a one-based source line.
pub fn file_request_at(path: &Path, line: usize) -> String {
    format!("file\t{}\t{line}", path.display())
}

/// Control-file payload for a git diff (`kind`: staged | worktree | untracked).
pub fn diff_request(root: &Path, rel: &str, kind: &str) -> String {
    format!("diff\t{}\t{rel}\t{kind}", root.display())
}

/// Control-file payload for `git show <spec>` (commit hash, stash@{n}, tag…),
/// optionally narrowed to one file.
pub fn show_request(root: &Path, spec: &str, path: Option<&str>) -> String {
    format!("show\t{}\t{spec}\t{}", root.display(), path.unwrap_or(""))
}

fn parse_request(raw: &str) -> Option<Request> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut parts = raw.split('\t');
    match parts.next() {
        Some("close") => Some(Request::Close),
        Some("diff") => {
            let root = PathBuf::from(parts.next()?);
            let rel = parts.next()?.to_string();
            let kind = parts.next().unwrap_or("worktree").to_string();
            Some(Request::Diff { root, rel, kind })
        }
        Some("show") => {
            let root = PathBuf::from(parts.next()?);
            let spec = parts.next()?.to_string();
            let path = parts.next().filter(|p| !p.is_empty()).map(str::to_string);
            Some(Request::Show { root, spec, path })
        }
        Some("file") => Some(Request::File {
            path: PathBuf::from(parts.next()?),
            line: parts
                .next()
                .and_then(|line| line.parse().ok())
                .filter(|line| *line > 0),
        }),
        // Legacy: a bare path.
        _ => Some(Request::File {
            path: PathBuf::from(raw),
            line: None,
        }),
    }
}

fn request_payload(request: &Request) -> String {
    match request {
        Request::Close => "close".into(),
        Request::File { path, line } => line
            .map(|line| file_request_at(path, line))
            .unwrap_or_else(|| file_request(path)),
        Request::Diff { root, rel, kind } => diff_request(root, rel, kind),
        Request::Show { root, spec, path } => show_request(root, spec, path.as_deref()),
    }
}

struct Doc {
    name: String,
    context: String,
    lines: Vec<Line<'static>>,
    /// File previews get a line-number gutter; diffs carry their own +/-.
    numbered: bool,
    /// Decoded raster media, rendered portably with true-color half blocks.
    /// Video files carry the poster frame extracted by ffmpeg.
    media: Option<MediaPreview>,
    /// Offset into [`Doc::rows`] — RENDERED rows, not source lines, so a
    /// wrapped line's continuations are scrolled to like anything else.
    scroll: usize,
    /// Long lines wrap unless the user toggles them off (`w`). Per document:
    /// a newly loaded one starts wrapped again.
    wrap: bool,
    /// Width glow formatted this markdown document for. A mismatch with the
    /// live body triggers one background reload instead of generic rewrap.
    glow_width: Option<u16>,
    /// `lines` laid out for the pane, rebuilt only when the width or the
    /// wrap toggle changes (see [`Doc::relayout`]).
    rows: Vec<Row>,
    /// The (width, height, wrap) `rows` was built for; `None` until first draw.
    rows_key: Option<(u16, u16, bool)>,
    /// Source line to scroll back to once `rows` is rebuilt — how a wrap
    /// toggle and a diff refresh keep the reader's place even though the
    /// row index underneath them changed.
    pending_src: Option<usize>,
    selection: PreviewSelection,
    /// A file preview's modified-time/size stamp, taken on the load worker
    /// BEFORE the content was read: the baseline later polls compare against.
    stamp: Option<FileStamp>,
}

struct MediaPreview {
    pixels: image::RgbaImage,
    source_width: u32,
    source_height: u32,
    video_poster: bool,
}

/// One rendered row of the body: the source line it came from (so scroll
/// position survives a relayout) and the styled row itself.
struct Row {
    src: usize,
    line: Line<'static>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct RenderPos {
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct PreviewSelection {
    anchor: Option<RenderPos>,
    cursor: Option<RenderPos>,
    mouse_anchor: Option<RenderPos>,
}

impl Doc {
    /// Rebuild [`Doc::rows`] for the viewport when the layout inputs changed,
    /// then honour any pending source-line scroll request.
    fn relayout(&mut self, width: u16, height: u16) {
        if self.rows_key != Some((width, height, self.wrap)) {
            self.rows = if let Some(media) = &self.media {
                render_media_rows(media, width, height)
            } else {
                build_rows(&self.lines, self.numbered, self.wrap, width)
            };
            self.rows_key = Some((width, height, self.wrap));
            self.selection = PreviewSelection::default();
        }
        if let Some(src) = self.pending_src.take() {
            self.scroll = self
                .rows
                .iter()
                .position(|r| r.src >= src)
                .unwrap_or(self.rows.len().saturating_sub(1));
        }
    }

    /// The source line at the top of the body — the anchor a relayout
    /// scrolls back to.
    fn top_src(&self) -> usize {
        self.rows.get(self.scroll).map_or(0, |r| r.src)
    }

    fn gutter(&self) -> usize {
        if self.numbered {
            self.lines.len().to_string().len() + 1
        } else {
            0
        }
    }

    /// True when a mouse release finishes a non-empty selection.
    fn on_mouse(&mut self, mouse: &MouseEvent, body: Rect) -> bool {
        if self.media.is_some() {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(position) = self.text_position_at(body, mouse.column, mouse.row) else {
                    return false;
                };
                let extending = mouse.modifiers.contains(KeyModifiers::SHIFT);
                let anchor = if extending {
                    self.selection.anchor.unwrap_or(position)
                } else {
                    position
                };
                self.selection.anchor = Some(anchor);
                self.selection.cursor = Some(position);
                self.selection.mouse_anchor = Some(anchor);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(anchor) = self.selection.mouse_anchor else {
                    return false;
                };
                let Some(position) = self.text_position_at(body, mouse.column, mouse.row) else {
                    return false;
                };
                self.selection.anchor = Some(anchor);
                self.selection.cursor = Some(position);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.selection.mouse_anchor.take().is_none() {
                    return false;
                }
                if let Some(position) = self.text_position_at(body, mouse.column, mouse.row) {
                    self.selection.cursor = Some(position);
                }
                return self.selection_range().is_some();
            }
            _ => {}
        }
        false
    }

    fn text_position_at(&self, body: Rect, column: u16, row: u16) -> Option<RenderPos> {
        if column < body.x || column >= body.right() || row < body.y || row >= body.bottom() {
            return None;
        }
        let row = self.scroll + usize::from(row - body.y);
        let rendered = self.rows.get(row)?;
        let text = row_text_without_gutter(rendered, self.gutter());
        let x = usize::from(column - body.x).saturating_sub(self.gutter());
        Some(RenderPos {
            row,
            col: col_at_display_x(&text, x),
        })
    }

    fn selection_range(&self) -> Option<(RenderPos, RenderPos)> {
        let anchor = self.selection.anchor?;
        let cursor = self.selection.cursor?;
        (anchor != cursor).then(|| {
            if anchor < cursor {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            }
        })
    }

    fn select_all(&mut self) {
        if self.media.is_some() {
            return;
        }
        let Some(last) = self.rows.last() else { return };
        let last_col = row_text_without_gutter(last, self.gutter()).chars().count();
        self.selection.anchor = Some(RenderPos::default());
        self.selection.cursor = Some(RenderPos {
            row: self.rows.len() - 1,
            col: last_col,
        });
        self.selection.mouse_anchor = None;
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        let mut selected = String::new();
        for row_index in start.row..=end.row {
            let row = self.rows.get(row_index)?;
            let text = row_text_without_gutter(row, self.gutter());
            let from = if row_index == start.row { start.col } else { 0 };
            let to = if row_index == end.row {
                end.col
            } else {
                text.chars().count()
            };
            selected.push_str(&char_slice(&text, from, to));
            if row_index < end.row
                && self
                    .rows
                    .get(row_index + 1)
                    .is_some_and(|next| next.src != row.src)
            {
                selected.push('\n');
            }
        }
        Some(selected)
    }
}

fn row_text_without_gutter(row: &Row, gutter: usize) -> String {
    row.line
        .spans
        .iter()
        .flat_map(|span| span.content.chars())
        .skip(gutter)
        .collect()
}

fn col_at_display_x(text: &str, wanted: usize) -> usize {
    let mut x = 0;
    for (col, ch) in text.chars().enumerate() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if x + width > wanted {
            return col;
        }
        x += width;
    }
    text.chars().count()
}

fn char_slice(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn selected_row(
    line: &Line<'static>,
    gutter: usize,
    row: usize,
    range: Option<(RenderPos, RenderPos)>,
) -> Line<'static> {
    let Some((start, end)) = range else {
        return line.clone();
    };
    if row < start.row || row > end.row {
        return line.clone();
    }
    let mut rendered = Line {
        style: line.style,
        ..Line::default()
    };
    let mut global_col: usize = 0;
    for span in &line.spans {
        for ch in span.content.chars() {
            let content_col = global_col.checked_sub(gutter);
            let selected = content_col.is_some_and(|col| {
                let after_start = row > start.row || col >= start.col;
                let before_end = row < end.row || col < end.col;
                after_start && before_end
            });
            let style = if selected {
                span.style.bg(palette().text_selection_bg)
            } else {
                span.style
            };
            rendered.spans.push(Span::styled(ch.to_string(), style));
            global_col += 1;
        }
    }
    rendered
}

fn media_pixel(pixel: image::Rgba<u8>) -> Option<(u8, u8, u8)> {
    let alpha = u16::from(pixel[3]);
    if alpha < 8 {
        return None;
    }
    let base = if crate::ui::is_light() { 255 } else { 0 };
    let blend = |channel: u8| ((u16::from(channel) * alpha + base * (255 - alpha)) / 255) as u8;
    Some((blend(pixel[0]), blend(pixel[1]), blend(pixel[2])))
}

/// Render raster media without relying on terminal-specific image protocols.
/// One `▀` cell carries an upper pixel in its foreground and a lower pixel in
/// its background, giving the pane two vertical pixels per terminal row.
fn render_media_rows(media: &MediaPreview, width: u16, height: u16) -> Vec<Row> {
    if width == 0 || height == 0 || media.source_width == 0 || media.source_height == 0 {
        return Vec::new();
    }
    let max_pixel_height = u32::from(height).saturating_mul(2);
    let scale = (f64::from(width) / f64::from(media.source_width))
        .min(f64::from(max_pixel_height) / f64::from(media.source_height));
    let target_width = (f64::from(media.source_width) * scale).round().max(1.0) as u32;
    let target_height = (f64::from(media.source_height) * scale).round().max(1.0) as u32;
    let pixels = image::imageops::resize(
        &media.pixels,
        target_width,
        target_height,
        image::imageops::FilterType::Triangle,
    );
    let rendered_height = target_height.div_ceil(2) as u16;
    let top_pad = height.saturating_sub(rendered_height) / 2;
    let left_pad = usize::from(width.saturating_sub(target_width as u16) / 2);
    let mut rows = Vec::with_capacity(usize::from(top_pad + rendered_height));
    for src in 0..usize::from(top_pad) {
        rows.push(Row {
            src,
            line: Line::default(),
        });
    }
    for pixel_y in (0..target_height).step_by(2) {
        let mut spans = Vec::with_capacity(target_width as usize + 1);
        if left_pad > 0 {
            spans.push(Span::raw(" ".repeat(left_pad)));
        }
        for pixel_x in 0..target_width {
            let upper = media_pixel(*pixels.get_pixel(pixel_x, pixel_y));
            let lower = (pixel_y + 1 < target_height)
                .then(|| media_pixel(*pixels.get_pixel(pixel_x, pixel_y + 1)))
                .flatten();
            let span = match (upper, lower) {
                (Some(top), Some(bottom)) => Span::styled(
                    "▀",
                    Style::default()
                        .fg(Color::Rgb(top.0, top.1, top.2))
                        .bg(Color::Rgb(bottom.0, bottom.1, bottom.2)),
                ),
                (Some(top), None) => {
                    Span::styled("▀", Style::default().fg(Color::Rgb(top.0, top.1, top.2)))
                }
                (None, Some(bottom)) => Span::styled(
                    "▄",
                    Style::default().fg(Color::Rgb(bottom.0, bottom.1, bottom.2)),
                ),
                (None, None) => Span::raw(" "),
            };
            spans.push(span);
        }
        rows.push(Row {
            src: rows.len(),
            line: Line::from(spans),
        });
    }
    rows
}

/// Lay `lines` out for a `width`-wide body: wrap each source line (when
/// wrapping is on), prefix the line-number gutter (blank on continuation
/// rows, like an editor), and pad tinted diff rows to the full width.
fn build_rows(lines: &[Line<'static>], numbered: bool, wrap: bool, width: u16) -> Vec<Row> {
    let number_width = lines.len().to_string().len();
    let gutter = if numbered { number_width + 1 } else { 0 };
    let content = usize::from(width).saturating_sub(gutter);
    let mut rows = Vec::with_capacity(lines.len());
    for (src, line) in lines.iter().enumerate() {
        let contains_tab = line.spans.iter().any(|span| span.content.contains('\t'));
        let pieces = if content > 0 && contains_tab {
            crate::wrap::wrap_line(line, if wrap { content } else { usize::MAX })
        } else if wrap && content > 0 && line.width() > content {
            crate::wrap::wrap_line(line, content)
        } else {
            vec![line.clone()]
        };
        for (i, piece) in pieces.into_iter().enumerate() {
            let mut row = piece;
            if numbered {
                // The number sits on the first row only; continuations
                // indent to the same column so the code stays aligned.
                let label = if i == 0 {
                    format!("{:>number_width$} ", src + 1)
                } else {
                    " ".repeat(gutter)
                };
                let style = row.style;
                let mut spans = vec![Span::styled(label, Style::default().dim())];
                spans.append(&mut row.spans);
                row = Line::from(spans);
                row.style = style;
            }
            // Tinted diff rows fill the full row, like an editor — every
            // row, so a wrapped change keeps its colour to the pane edge.
            if row.style.bg.is_some() {
                let pad = usize::from(width).saturating_sub(row.width());
                if pad > 0 {
                    row.spans.push(Span::raw(" ".repeat(pad)));
                }
            }
            rows.push(Row { src, line: row });
        }
    }
    rows
}

fn load(request: &Request) -> Doc {
    match request {
        Request::Close => Doc {
            name: "Preview".into(),
            context: String::new(),
            lines: vec![Line::raw("(closing)")],
            numbered: false,
            media: None,
            scroll: 0,
            wrap: true,
            glow_width: None,
            rows: Vec::new(),
            rows_key: None,
            pending_src: None,
            selection: PreviewSelection::default(),
            stamp: None,
        },
        Request::File { path, line } => {
            // Stat before reading: a write that lands during the read leaves
            // the stamp behind the content, so the next poll reloads again.
            let stamp = file_stamp(path);
            let mut doc = load_file(path, *line);
            doc.stamp = stamp;
            doc
        }
        Request::Diff { root, rel, kind } => load_diff(root, rel, kind),
        Request::Show { root, spec, path } => load_show(root, spec, path.as_deref()),
    }
}

/// Lightweight first frame while parsing/decoding happens off the terminal
/// event loop. Large media, an external markdown renderer, or a cold syntax
/// grammar must never make a preview swap look like the click was ignored.
fn loading_doc(request: &Request) -> Doc {
    let (name, context) = match request {
        Request::Close => ("Preview".into(), String::new()),
        Request::File { path, .. } => (
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            path.display().to_string(),
        ),
        Request::Diff { root, rel, .. } => (
            rel.rsplit('/').next().unwrap_or(rel).to_string(),
            root.display().to_string(),
        ),
        Request::Show { root, spec, .. } => (spec.clone(), root.display().to_string()),
    };
    Doc {
        name,
        context,
        lines: vec![Line::raw("(loading preview…)")],
        numbered: false,
        media: None,
        scroll: 0,
        wrap: true,
        glow_width: None,
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
        stamp: None,
    }
}

/// Bumped by every fresh document load. A background refresh carries the
/// generation it started under and is discarded once another load began, so
/// a stale result (A→B→A, leaving edit mode) can never be applied and a stuck
/// refresh of an old document never blocks refreshes of the new one.
static LOAD_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn load_generation() -> u64 {
    LOAD_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

fn start_preview_load(request: Request) -> (Request, std::sync::mpsc::Receiver<Doc>) {
    LOAD_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let worker_request = request.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(load(&worker_request));
    });
    (request, receiver)
}

type FileStamp = (std::time::SystemTime, u64);

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Whether a file preview must reload: its current stamp differs from the one
/// taken when the displayed content was read. Runs on the refresh worker —
/// a `stat` on a stalled network or cloud drive can block indefinitely.
fn file_refresh_needed(path: &Path, baseline: Option<FileStamp>) -> bool {
    file_stamp(path) != baseline
}

/// Refresh workers still running. A stall on a dead network drive can block
/// a worker forever and dropping its receiver cannot cancel it, so new
/// refreshes pause while this many are outstanding instead of piling up.
static REFRESH_IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_REFRESH_IN_FLIGHT: usize = 2;
/// A refresh still unanswered after this long is abandoned so the document
/// can retry (bounded by [`MAX_REFRESH_IN_FLIGHT`]).
const REFRESH_ABANDON_AFTER: Duration = Duration::from_secs(30);

struct InFlight;

impl InFlight {
    fn claim() -> Option<Self> {
        // Claim, then back out if over the cap: `fetch_update` is deprecated
        // on current stable and its `try_update` rename is newer than our MSRV.
        let order = std::sync::atomic::Ordering::SeqCst;
        if REFRESH_IN_FLIGHT.fetch_add(1, order) < MAX_REFRESH_IN_FLIGHT {
            Some(Self)
        } else {
            REFRESH_IN_FLIGHT.fetch_sub(1, order);
            None
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        REFRESH_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Whether a finished refresh still describes what is on screen.
fn refresh_is_current(
    started: u64,
    request: &Request,
    generation: u64,
    current: Option<&Request>,
) -> bool {
    started == generation && current == Some(request)
}

/// herdr's own `[ui] copy_on_select` (default on), as herdr's LIVE loader
/// sees it: a missing file means defaults, but an unreadable or half-edited
/// file keeps the last good value instead of silently re-enabling copying.
/// Mouse capture means herdr cannot copy a preview selection itself, so the
/// viewer mirrors the host's behavior — and its opt-out.
fn herdr_copy_on_select(last_good: &mut Option<bool>) -> bool {
    let read = herdr_config_path().map(std::fs::read_to_string);
    copy_on_select_now(read, last_good)
}

fn copy_on_select_now(read: Option<std::io::Result<String>>, last_good: &mut Option<bool>) -> bool {
    let value = match read {
        None => Some(true),
        Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Some(true),
        Some(Err(_)) => None,
        Some(Ok(config)) => copy_on_select_parsed(&config),
    };
    match value {
        Some(value) => {
            *last_good = Some(value);
            value
        }
        None => last_good.unwrap_or(true),
    }
}

/// Where herdr 0.9 reads its config (`src/config/io.rs`): `HERDR_CONFIG_PATH`,
/// then `$XDG_CONFIG_HOME/herdr` (honored on Windows too), then on Windows
/// `%APPDATA%\herdr`, `%USERPROFILE%\AppData\Roaming\herdr` or
/// `$HOME/.config/herdr`, elsewhere `~/.config/herdr`.
fn herdr_config_path() -> Option<PathBuf> {
    let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(path) = set("HERDR_CONFIG_PATH") {
        return Some(PathBuf::from(path));
    }
    let dir = if let Some(dir) = set("XDG_CONFIG_HOME") {
        PathBuf::from(dir).join("herdr")
    } else if cfg!(windows) {
        if let Some(dir) = set("APPDATA") {
            PathBuf::from(dir).join("herdr")
        } else if let Some(profile) = set("USERPROFILE") {
            PathBuf::from(profile)
                .join("AppData")
                .join("Roaming")
                .join("herdr")
        } else {
            PathBuf::from(set("HOME")?).join(".config").join("herdr")
        }
    } else {
        PathBuf::from(set("HOME")?).join(".config").join("herdr")
    };
    Some(dir.join("config.toml"))
}

/// `[ui] copy_on_select` parsed the way herdr does: BOMs stripped, then real
/// TOML (`[ ui ]`, `["ui"]`, dotted keys and inline tables all count). `None`
/// when the file is not valid TOML.
fn copy_on_select_parsed(config: &str) -> Option<bool> {
    let table = config.replace('\u{feff}', "").parse::<toml::Table>().ok()?;
    // A present but wrongly-typed value is a schema error: herdr's loader
    // rejects it and keeps its current config, so it must not read as "on".
    match table.get("ui") {
        None => Some(true),
        Some(toml::Value::Table(ui)) => match ui.get("copy_on_select") {
            None => Some(true),
            Some(value) => value.as_bool(),
        },
        Some(_) => None,
    }
}

/// The first-load view of [`copy_on_select_parsed`]: invalid TOML means
/// herdr starts on defaults.
#[cfg(test)]
fn copy_on_select_from(config: &str) -> bool {
    copy_on_select_parsed(config).unwrap_or(true)
}

/// What the copy worker reports back to the event loop.
#[derive(Debug, PartialEq, Eq)]
enum ReleaseCopy {
    /// herdr's `copy_on_select` is off: nothing was copied.
    Disabled,
    Copied,
    /// OSC 52 is wanted; the event loop writes it (it owns stdout).
    Osc52(String),
    Failed(String),
}

/// Decide and perform one copy. Pure apart from its arguments, so the
/// routing is testable without a clipboard.
fn release_copy(
    text: String,
    enabled: bool,
    prefer_osc52: bool,
    native: impl FnOnce(&str) -> std::io::Result<()>,
    fallback_osc52: bool,
) -> ReleaseCopy {
    if !enabled {
        return ReleaseCopy::Disabled;
    }
    if prefer_osc52 {
        return ReleaseCopy::Osc52(text);
    }
    match native(&text) {
        Ok(()) => ReleaseCopy::Copied,
        Err(_) if fallback_osc52 => ReleaseCopy::Osc52(text),
        Err(error) => ReleaseCopy::Failed(error.to_string()),
    }
}

/// A copy request. Automatic (mouse-release) copies honor `copy_on_select`;
/// explicit Ctrl/Cmd+C always copies.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CopyRequest {
    text: String,
    automatic: bool,
}

/// Queue a request in order. A newer automatic copy replaces any automatic
/// copy still waiting — only the latest drag matters — so a stalled
/// clipboard can never build an unbounded backlog; explicit copies are kept
/// and stay ordered against automatic ones, so an older release can never
/// overwrite a newer Ctrl+C.
fn enqueue_copy(queue: &mut std::collections::VecDeque<CopyRequest>, request: CopyRequest) {
    if request.automatic {
        queue.retain(|queued| !queued.automatic);
    }
    queue.push_back(request);
}

type CopyQueue = std::sync::Arc<(
    std::sync::Mutex<std::collections::VecDeque<CopyRequest>>,
    std::sync::Condvar,
)>;

/// One worker for every preview copy, so copies stay in order and a hung
/// clipboard tool (or a config on a stalled drive) can never freeze the
/// viewer's event loop. The helper itself is time-limited in `actions`.
struct ReleaseCopier {
    queue: CopyQueue,
    results: std::sync::mpsc::Receiver<ReleaseCopy>,
    /// Released by the event loop after it wrote an OSC 52 copy, so a later
    /// native copy cannot finish first and then be overwritten by it.
    written: std::sync::mpsc::Sender<()>,
}

impl ReleaseCopier {
    fn start() -> Option<Self> {
        let queue: CopyQueue = std::sync::Arc::default();
        let (outbox, results) = std::sync::mpsc::channel();
        let (written, osc52_done) = std::sync::mpsc::channel::<()>();
        let worker_queue = queue.clone();
        std::thread::Builder::new()
            .name("preview-copy".into())
            .spawn(move || {
                let mut last_good = None;
                loop {
                    let request = {
                        let (lock, ready) = &*worker_queue;
                        let Ok(mut pending) = lock.lock() else { return };
                        loop {
                            if let Some(request) = pending.pop_front() {
                                break request;
                            }
                            let Ok(next) = ready.wait(pending) else {
                                return;
                            };
                            pending = next;
                        }
                    };
                    let enabled = !request.automatic || herdr_copy_on_select(&mut last_good);
                    let result = release_copy(
                        request.text,
                        enabled,
                        crate::actions::osc52_preferred(),
                        crate::actions::copy_native,
                        crate::actions::osc52_fallback_available(),
                    );
                    let wait_for_terminal = matches!(result, ReleaseCopy::Osc52(_));
                    if outbox.send(result).is_err() {
                        return;
                    }
                    if wait_for_terminal && osc52_done.recv().is_err() {
                        return;
                    }
                }
            })
            .ok()?;
        Some(Self {
            queue,
            results,
            written,
        })
    }

    /// Forget drags still waiting: entering edit mode starts explicit,
    /// synchronous editor copies that a late read-only copy must not follow.
    fn drop_waiting_automatic(&self) {
        let (lock, _) = &*self.queue;
        if let Ok(mut pending) = lock.lock() {
            pending.retain(|queued| !queued.automatic);
        }
    }

    fn submit(&self, request: CopyRequest) {
        let (lock, ready) = &*self.queue;
        if let Ok(mut pending) = lock.lock() {
            enqueue_copy(&mut pending, request);
            ready.notify_one();
        }
    }
}

/// The footer notice for a finished copy, writing OSC 52 here on the drawing
/// thread when the worker asked for it.
fn release_copy_notice(result: ReleaseCopy) -> Option<String> {
    match result {
        ReleaseCopy::Disabled => None,
        ReleaseCopy::Copied => Some("copied selection".into()),
        ReleaseCopy::Osc52(text) => Some(match crate::actions::write_osc52_stdout(&text) {
            Ok(_) => "sent selection to terminal clipboard".into(),
            Err(error) => format!("clipboard unavailable: {error}"),
        }),
        ReleaseCopy::Failed(error) => Some(format!("clipboard unavailable: {error}")),
    }
}

fn copy_selection(doc: &Doc) -> String {
    let Some(text) = doc.selected_text() else {
        return "select text before copying".into();
    };
    match crate::actions::copy_to_clipboard(&text) {
        Ok(crate::actions::ClipboardWrite::Native) => "copied selection".into(),
        Ok(crate::actions::ClipboardWrite::Osc52Unacknowledged) => {
            "sent selection to terminal clipboard".into()
        }
        Err(error) => format!("clipboard unavailable: {error}"),
    }
}

fn apply_diff_refresh(doc: &mut Doc, mut refreshed: Doc) {
    if doc.media.is_none()
        && doc.name == refreshed.name
        && doc.context == refreshed.context
        && doc.numbered == refreshed.numbered
        && doc.lines == refreshed.lines
    {
        doc.stamp = refreshed.stamp;
        return;
    }
    refreshed.wrap = doc.wrap;
    refreshed.pending_src = Some(doc.top_src());
    *doc = refreshed;
}

enum ViewMode {
    Preview(Doc),
    Edit(Editor),
}

#[derive(Clone)]
enum Pending {
    Close,
    LeaveEdit,
    Switch(Request),
}

enum Prompt {
    Unsaved(Pending),
    Conflict(Option<Pending>),
}

impl Prompt {
    fn text(&self) -> &'static str {
        match self {
            Self::Unsaved(_) => " Unsaved changes: [s] save  [d] discard  [Esc] cancel",
            Self::Conflict(_) => {
                " File changed on disk: [o] overwrite  [r] reload disk version  [Esc] cancel"
            }
        }
    }
}

fn mode_pane_label(mode: &ViewMode) -> String {
    match mode {
        ViewMode::Preview(doc) => preview_pane_label(&doc.name),
        ViewMode::Edit(editor) => editor_pane_label(&editor.name()),
    }
}

fn apply_pending(
    pending: Pending,
    mode: &mut ViewMode,
    current: &mut Option<Request>,
    preview_load: &mut Option<(Request, std::sync::mpsc::Receiver<Doc>)>,
    identity_pending: &mut bool,
    control: &Path,
    notice: &mut Option<String>,
) -> bool {
    match pending {
        Pending::Close => close_own_pane(control, current, notice),
        Pending::LeaveEdit => {
            if let Some(request) = current.clone() {
                *mode = ViewMode::Preview(loading_doc(&request));
                *preview_load = Some(start_preview_load(request));
                *identity_pending = true;
            }
            false
        }
        Pending::Switch(request) => {
            if request == Request::Close {
                close_own_pane(control, current, notice)
            } else {
                *mode = ViewMode::Preview(loading_doc(&request));
                *current = Some(request.clone());
                *preview_load = Some(start_preview_load(request));
                *identity_pending = true;
                false
            }
        }
    }
}

fn restore_current_control(control: &Path, current: &Option<Request>) {
    if let Some(request) = current {
        let _ = write_scratch_file(control, &request_payload(request));
    }
}

/// `git show` with stat + patch, colored — what a click on a commit, stash,
/// tag, or branch line renders. Immutable content: no refresh loop needed.
fn load_show(root: &Path, spec: &str, path: Option<&str>) -> Doc {
    let display_spec = spec.strip_prefix("refs/tags/").unwrap_or(spec);
    let mut args: Vec<String> = vec![
        "-c".into(),
        "color.ui=always".into(),
        "show".into(),
        "--color=always".into(),
        "--stat".into(),
        "--patch".into(),
        "--no-ext-diff".into(),
        "--end-of-options".into(),
        spec.to_string(),
    ];
    if let Some(p) = path {
        args.push("--".into());
        args.push(p.replace('/', std::path::MAIN_SEPARATOR_STR));
    }
    let output = std::process::Command::new("git")
        .args(&args)
        .current_dir(root)
        .output();
    let lines = match output {
        Err(e) => vec![Line::raw(format!("(git failed: {e})"))],
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            if text.trim().is_empty() {
                let err = String::from_utf8_lossy(&out.stderr);
                if err.trim().is_empty() {
                    vec![Line::raw("(nothing to show)")]
                } else {
                    vec![Line::raw(format!("({})", err.trim()))]
                }
            } else {
                ansi::to_lines(&text)
            }
        }
    };
    Doc {
        name: display_spec.to_string(),
        context: format!("git show {display_spec} — {}", root.display()),
        lines,
        numbered: false,
        media: None,
        scroll: 0,
        wrap: true,
        glow_width: None,
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
        stamp: None,
    }
}

/// Render markdown text via `glow`. Returns `None` when glow is not installed
/// or exits non-zero (caller falls back to syntax highlight).
///
/// Receives the already-read `text` buffer so the MAX_BYTES guard in
/// `load_file` is honoured — glow would otherwise re-read the full file.
/// Pipes via stdin (`-`) to avoid treating filenames starting with `-` as
/// flags. The first render uses the most recently drawn body width; the event
/// loop schedules one background reload if the actual body width differs.
fn glow_markdown(text: &str, width: u16) -> Option<Vec<Line<'static>>> {
    use std::io::Write as _;
    let args = glow_args(width, crate::ui::is_light());
    let mut child = std::process::Command::new("glow")
        .args(args)
        .env("CLICOLOR_FORCE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        let _ = stdin.write_all(text.as_bytes());
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let rendered = String::from_utf8_lossy(&output.stdout);
    if rendered.trim().is_empty() {
        return None;
    }
    let mut lines = ansi::to_lines(&rendered);
    lines.truncate(MAX_LINES);
    if lines.is_empty() {
        return None;
    }
    Some(lines)
}

fn glow_args(width: u16, light: bool) -> Vec<String> {
    vec![
        "--style".into(),
        if light { "light" } else { "dark" }.into(),
        "--width".into(),
        width.to_string(),
        "-".into(),
    ]
}

fn choose_text_render<F>(
    glow_rendered: Option<Vec<Line<'static>>>,
    fallback: F,
    glow_width: u16,
) -> (Vec<Line<'static>>, bool, Option<u16>)
where
    F: FnOnce() -> Vec<Line<'static>>,
{
    match glow_rendered {
        Some(rendered) => (rendered, false, Some(glow_width)),
        None => (fallback(), true, None),
    }
}

const MAX_MEDIA_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MEDIA_PIXELS: u64 = 12_000_000;
const MAX_MEDIA_DIMENSION: u32 = 8192;
const MAX_MEDIA_ALLOC_BYTES: u64 = 64 * 1024 * 1024;
const MAX_VIDEO_FRAME_BYTES: u64 = 16 * 1024 * 1024;
const VIDEO_FRAME_TIMEOUT: Duration = Duration::from_secs(4);

fn has_extension(target: &Path, extensions: &[&str]) -> bool {
    target
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extensions
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

fn is_image_file(target: &Path) -> bool {
    has_extension(
        target,
        &[
            "bmp", "gif", "ico", "jpeg", "jpg", "png", "tif", "tiff", "webp",
        ],
    )
}

fn is_video_file(target: &Path) -> bool {
    has_extension(
        target,
        &[
            "avi", "flv", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "webm", "wmv",
        ],
    )
}

fn decode_image_file(target: &Path) -> Result<image::RgbaImage, String> {
    let file_bytes = std::fs::metadata(target)
        .map_err(|error| error.to_string())?
        .len();
    if file_bytes > MAX_MEDIA_FILE_BYTES {
        return Err(format!(
            "image file is too large ({} MiB limit)",
            MAX_MEDIA_FILE_BYTES / 1024 / 1024
        ));
    }
    let (width, height) = image::image_dimensions(target).map_err(|error| error.to_string())?;
    if width > MAX_MEDIA_DIMENSION
        || height > MAX_MEDIA_DIMENSION
        || u64::from(width).saturating_mul(u64::from(height)) > MAX_MEDIA_PIXELS
    {
        return Err(format!("image is too large ({width}×{height})"));
    }
    let mut reader = image::ImageReader::open(target)
        .map_err(|error| error.to_string())?
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_MEDIA_DIMENSION);
    limits.max_image_height = Some(MAX_MEDIA_DIMENSION);
    limits.max_alloc = Some(MAX_MEDIA_ALLOC_BYTES);
    reader.limits(limits);
    reader
        .decode()
        .map(|image| image.to_rgba8())
        .map_err(|error| error.to_string())
}

fn ffmpeg_in_path(path: &std::ffi::OsStr, cwd: Option<&Path>) -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .filter_map(|directory| directory.canonicalize().ok())
        .filter(|directory| !cwd.is_some_and(|cwd| directory.starts_with(cwd)))
        .map(|directory| directory.join(executable))
        .find(|candidate| {
            if !candidate.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                candidate
                    .metadata()
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            true
        })
}

fn ffmpeg_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    ffmpeg_in_path(&path, cwd.as_deref())
}

fn decode_video_poster(target: &Path) -> Result<image::RgbaImage, String> {
    let ffmpeg =
        ffmpeg_on_path().ok_or_else(|| "video preview needs ffmpeg on PATH".to_string())?;
    let mut command = std::process::Command::new(ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-ss",
        "0",
        "-i",
    ]);
    command.arg(target);
    command.args([
        "-frames:v",
        "1",
        "-vf",
        "scale=1280:720:force_original_aspect_ratio=decrease",
        "-f",
        "image2pipe",
        "-vcodec",
        "png",
        "pipe:1",
    ]);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("ffmpeg failed: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ffmpeg stdout unavailable".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "ffmpeg stderr unavailable".to_string())?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout)
            .take(MAX_VIDEO_FRAME_BYTES + 1)
            .read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stderr).take(16 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + VIDEO_FRAME_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err("video poster timed out after 4 seconds".into());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("ffmpeg status failed: {error}"));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "ffmpeg output reader failed".to_string())?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "ffmpeg error reader failed".to_string())?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(format!(
            "ffmpeg could not read video: {}",
            stderr.lines().next().unwrap_or("unknown error")
        ));
    }
    if stdout.len() as u64 > MAX_VIDEO_FRAME_BYTES {
        return Err("video poster frame exceeded 16 MiB".into());
    }
    image::load_from_memory_with_format(&stdout, image::ImageFormat::Png)
        .map(|image| image.to_rgba8())
        .map_err(|error| format!("could not decode video frame: {error}"))
}

fn load_media_file(target: &Path, name: String, video_poster: bool) -> Doc {
    let result = if video_poster {
        decode_video_poster(target)
    } else {
        decode_image_file(target)
    };
    let (lines, media, context) = match result {
        Ok(pixels) => {
            let (source_width, source_height) = pixels.dimensions();
            let kind = if video_poster {
                "video poster"
            } else {
                "image"
            };
            (
                Vec::new(),
                Some(MediaPreview {
                    pixels,
                    source_width,
                    source_height,
                    video_poster,
                }),
                format!(
                    "{} — {source_width}×{source_height} {kind}",
                    target.display()
                ),
            )
        }
        Err(error) => (
            vec![Line::raw(format!("({error})"))],
            None,
            target.display().to_string(),
        ),
    };
    Doc {
        name,
        context,
        lines,
        numbered: false,
        media,
        scroll: 0,
        wrap: false,
        glow_width: None,
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
        stamp: None,
    }
}

fn load_file(target: &Path, target_line: Option<usize>) -> Doc {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    let lower = name.to_lowercase();
    if is_image_file(target) {
        return load_media_file(target, name, false);
    }
    if is_video_file(target) {
        return load_media_file(target, name, true);
    }
    let is_markdown = lower.ends_with(".md") || lower.ends_with(".markdown");
    let (lines, numbered, glow_width) = match std::fs::read(target) {
        Err(e) => (vec![Line::raw(format!("(unreadable: {e})"))], true, None),
        Ok(bytes) => {
            if bytes.contains(&0) {
                (
                    vec![Line::raw(format!("(binary file — {} bytes)", bytes.len()))],
                    false,
                    None,
                )
            } else {
                let truncated = bytes.len() > MAX_BYTES;
                let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]);
                // Markdown: render via glow; fall back to syntax highlight on failure.
                let requested_glow_width = LAST_BODY_WIDTH
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .max(20);
                let glow_rendered = (is_markdown && target_line.is_none())
                    .then(|| glow_markdown(&text, requested_glow_width))
                    .flatten();
                let fallback = || {
                    crate::syntax::highlight(&name, &text, MAX_LINES).unwrap_or_else(|| {
                        text.lines()
                            .take(MAX_LINES)
                            .map(|l| Line::raw(l.to_string()))
                            .collect()
                    })
                };
                let (mut lines, numbered, glow_width) =
                    choose_text_render(glow_rendered, fallback, requested_glow_width);
                if truncated || text.lines().count() > MAX_LINES {
                    lines.push(Line::raw("… (truncated)"));
                }
                if lines.is_empty() {
                    lines.push(Line::raw("(empty file)"));
                }
                (lines, numbered, glow_width)
            }
        }
    };
    Doc {
        name,
        context: target.display().to_string(),
        lines,
        numbered,
        media: None,
        scroll: 0,
        // Glow already laid out tables and paragraphs for `glow_width`.
        wrap: glow_width.is_none(),
        glow_width,
        rows: Vec::new(),
        rows_key: None,
        pending_src: target_line.map(|line| line.saturating_sub(1)),
        selection: PreviewSelection::default(),
        stamp: None,
    }
}

fn glow_needs_reload(doc: &Doc, body_width: u16) -> bool {
    let body_width = body_width.max(20);
    doc.glow_width.is_some_and(|width| width != body_width)
}

fn load_diff(root: &Path, rel: &str, kind: &str) -> Doc {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
    // Plain (uncolored) diff: crate::diffview parses it and renders the
    // VS Code look — dual gutters, tinted rows, syntax-highlighted code.
    let mut args: Vec<String> = vec!["diff".into(), "--no-ext-diff".into()];
    match kind {
        "staged" => args.push("--cached".into()),
        // An untracked file has no diff; --no-index against the null device
        // renders it as one big addition, like VS Code does.
        "untracked" => {
            args.push("--no-index".into());
            args.push(if cfg!(windows) {
                "NUL".into()
            } else {
                "/dev/null".into()
            });
        }
        _ => {}
    }
    args.push("--".into());
    args.push(rel.replace('/', std::path::MAIN_SEPARATOR_STR));

    let output = std::process::Command::new("git")
        .args(&args)
        .current_dir(root)
        .output();
    let lines = match output {
        Err(e) => vec![Line::raw(format!("(git failed: {e})"))],
        Ok(out) => {
            // --no-index exits 1 when the files differ; that's not an error.
            let text = String::from_utf8_lossy(&out.stdout);
            if text.trim().is_empty() {
                let err = String::from_utf8_lossy(&out.stderr);
                if err.trim().is_empty() {
                    vec![Line::raw("(no changes)")]
                } else {
                    vec![Line::raw(format!("({})", err.trim()))]
                }
            } else {
                crate::diffview::render(rel, &text)
            }
        }
    };
    let what = match kind {
        "staged" => "staged",
        "untracked" => "untracked",
        _ => "working tree",
    };
    Doc {
        name: name.clone(),
        context: format!("{} — {what} diff", root.join(rel).display()),
        lines,
        numbered: false,
        media: None,
        scroll: 0,
        wrap: true,
        glow_width: None,
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
        stamp: None,
    }
}

fn read_control(control: &Path) -> Option<Request> {
    if !control_path_ok(control) {
        return None;
    }
    read_control_file(control)
}

#[cfg(all(test, unix))]
fn read_control_in(control: &Path, expected_dir: &Path) -> Option<Request> {
    if !control_path_ok_in(control, expected_dir) {
        return None;
    }
    read_control_file(control)
}

fn read_control_file(control: &Path) -> Option<Request> {
    let mut buf = String::new();
    std::fs::File::open(control)
        .ok()?
        .read_to_string(&mut buf)
        .ok()?;
    parse_request(&buf)
}

/// Tag our pane (heartbeat-stamped, see launch::HEARTBEAT_STALE_SECS), record
/// WHICH document we show so any sidebar can route clicks to us, and title the
/// pane with the document's name. A `None` key clears the token — showing
/// nothing must not look like a preview of the empty path.
fn report_identity(mode: &ViewMode, doc_key: Option<&str>, control: &Path) {
    let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else {
        return;
    };
    if pane_id.is_empty() {
        return;
    }
    let inline = runs_inline();
    let doc_token = doc_key.map(document_token);
    let control_token = control_token(control);
    let mut tokens = serde_json::json!({
        METADATA_SOURCE: crate::state::unix_now().to_string(),
        TOKEN_PATH: doc_token,
        TOKEN_CONTROL: control_token,
    });
    if inline {
        // We share the tab with the sidebar and whatever the user put there.
        // Claiming ownership of it would let a close take the whole tab down.
        tokens[TOKEN_INLINE] = serde_json::Value::String("1".into());
    } else {
        tokens[TOKEN_DEDICATED] = serde_json::Value::String("1".into());
    }
    let _ = ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": METADATA_SOURCE,
            "tokens": tokens,
        }),
    );
    let _ = ipc::call_text(
        "pane.rename",
        serde_json::json!({ "pane_id": pane_id, "label": mode_pane_label(mode) }),
    );
    let Some(doc_key) = doc_key else { return };
    // The tab is the user's, not the document's, when we share it.
    if inline {
        return;
    }
    if let Ok(list) = ipc::call_text("pane.list", serde_json::json!({}))
        && let Some(preview) = previews_in(&list)
            .into_iter()
            .find(|p| p.pane_id == pane_id)
    {
        let _ = ipc::call_text(
            "tab.rename",
            serde_json::json!({
                "tab_id": preview.tab_id,
                "label": tab_label(doc_key, preview.pinned),
            }),
        );
    }
}

/// This viewer shares the sidebar's tab. Read from the spawn env rather than
/// the settings file: the setting decides where the NEXT preview opens, while
/// a running viewer's placement is whatever its pane already is.
fn runs_inline() -> bool {
    std::env::var(crate::state::PREVIEW_INLINE_ENV).is_ok_and(|value| value == "1")
}

fn preview_pane_label(doc_name: &str) -> String {
    format!("{doc_name} · preview")
}

fn editor_pane_label(doc_name: &str) -> String {
    format!("{doc_name} · editor")
}

/// What the control file must hold after a close that did not happen: the
/// document still on screen (or nothing), never the `close` request itself.
fn control_after_failed_close(current: &Option<Request>) -> String {
    current.as_ref().map(request_payload).unwrap_or_default()
}

/// Set once a takeover restore failed while this viewer tried to close.
static RESTORE_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Close the whole preview tab. Closing only the viewer pane leaves its
/// auto-docked sidebar behind as a convincing but unusable preview husk.
/// An inline viewer owns nothing but its own pane, so it closes just that.
fn close_own_pane(control: &Path, current: &Option<Request>, notice: &mut Option<String>) -> bool {
    let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else {
        return false;
    };
    if pane_id.is_empty() {
        return false;
    }
    if runs_inline() {
        let list = ipc::call_text("pane.list", serde_json::json!({})).unwrap_or_default();
        let preview = previews_in(&list)
            .into_iter()
            .find(|preview| preview.pane_id == pane_id);
        let tab = preview
            .as_ref()
            .map(|preview| preview.origin_tab_id.as_str())
            .unwrap_or("");
        if !tab.is_empty()
            && let Err(error) = crate::takeover::restore(tab, &pane_id)
        {
            // Never trap the viewer: a second close gives up on the layout
            // and leaves the working panes in their (relabelled) tab.
            if !RESTORE_FAILED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                // A `close` request that asked for this must not stay in the
                // control file: the loop would re-read it and silently take
                // the failure's "close anyway" branch on the next iteration.
                let _ = write_scratch_file(control, &control_after_failed_close(current));
                *notice = Some(format!(
                    "Cannot restore: {error}. Press q again to close anyway; your panes stay in the temporary tab."
                ));
                return false;
            }
            let _ = crate::takeover::abandon_tab(tab);
        }
        let closed = pane_close_succeeded(ipc::call_text(
            "pane.close",
            serde_json::json!({ "pane_id": pane_id }),
        ));
        if closed {
            remove_control_file(control);
            remove_control_file(&control_path_for_pane(&pane_id));
        }
        return closed;
    }
    let list = ipc::call_text("pane.list", serde_json::json!({})).ok();
    let preview = list.as_deref().and_then(|json| {
        previews_in(json)
            .into_iter()
            .find(|preview| preview.pane_id == pane_id)
    });
    remove_control_file(control);
    remove_control_file(&control_path_for_pane(&pane_id));
    if let Some(preview) = preview.filter(|preview| {
        preview.dedicated
            && list
                .as_deref()
                .is_some_and(|json| tab_is_plugin_only(json, &preview.tab_id))
    }) {
        close_preview_tab(&preview);
    } else {
        let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id }));
    }
    true
}

/// Bring the VIEWING client to `tab_id`. Since herdr 0.9 each client views
/// its own tab: `tab.focus` (and `pane.move`'s `focus: true`) only update the
/// session-wide focus record, which a client no longer follows, so the tab
/// opened "in the back" and the origin tab was left with a stale split.
/// `pane.focus` is the one call that still moves the client — but only on a
/// TRANSITION: if the server already records the target as focused (it kept
/// that record from the last preview while the user clicked elsewhere), the
/// call changes nothing and emits nothing. Step through another pane first
/// in that case. Falls back to `tab.focus` for hosts older than 0.9 and for
/// tabs whose panes cannot be listed.
pub(crate) fn focus_tab_for_client(tab_id: &str, pane_id: Option<&str>) {
    let list = ipc::call_text("pane.list", serde_json::json!({})).unwrap_or_default();
    let pane = match pane_id {
        Some(pane) if !pane.is_empty() => pane.to_string(),
        _ => crate::launch::pane_in_tab(&list, tab_id),
    };
    if pane.is_empty() {
        let _ = ipc::call_text("tab.focus", serde_json::json!({ "tab_id": tab_id }));
        return;
    }
    if crate::launch::server_focused_pane_id(&list) == pane {
        let step = crate::launch::pane_outside_tab(&list, tab_id);
        if !step.is_empty() {
            let _ = ipc::call_text("pane.focus", serde_json::json!({ "pane_id": step }));
        }
    }
    if ipc::call_text("pane.focus", serde_json::json!({ "pane_id": pane })).is_err() {
        let _ = ipc::call_text("tab.focus", serde_json::json!({ "tab_id": tab_id }));
    }
}

fn close_preview_tab(preview: &PreviewPane) {
    // Focus first: closing our own tab kills this process, so code after a
    // successful tab.close is not guaranteed to run.
    if !preview.origin_tab_id.is_empty() {
        focus_tab_for_client(&preview.origin_tab_id, None);
    }
    let _ = ipc::call_text("tab.close", serde_json::json!({ "tab_id": preview.tab_id }));
}

/// The first edit that dirties the buffer pins this document's tab, so the
/// next file click cannot silently take the tab away from it. Inline has no
/// second tab to send that click to — the switch prompt guards the buffer
/// there instead — so pinning is skipped rather than faked.
fn pin_own_tab(doc_key: &str) {
    if runs_inline() {
        return;
    }
    let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else {
        return;
    };
    let Ok(list) = ipc::call_text("pane.list", serde_json::json!({})) else {
        return;
    };
    let tab_id = crate::launch::tab_of(&list, &pane_id);
    if pane_id.is_empty() || tab_id.is_empty() {
        return;
    }
    let _ = ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": METADATA_SOURCE,
            "tokens": { TOKEN_PINNED: "1" },
        }),
    );
    let _ = ipc::call_text(
        "tab.rename",
        serde_json::json!({ "tab_id": tab_id, "label": tab_label(doc_key, true) }),
    );
}

/// Delete control files whose pane is gone. `close_own_pane` handles the
/// clean exit; this catches previews killed from outside (pane closed by
/// herdr, redeploy, server restart), which never get to run their own
/// cleanup. Cheap: one readdir against a `pane.list` we already have.
fn sweep_orphan_controls(pane_list_json: &str) {
    let previews = previews_in(pane_list_json);
    sweep_orphan_controls_in(&scratch_dir(), &previews);
    sweep_orphan_controls_in(&legacy_scratch_dir(), &previews);
}

fn sweep_orphan_controls_in(dir: &Path, previews: &[PreviewPane]) {
    // Skip entirely rather than reading through a directory we have not
    // verified as private — an unswept orphan is harmless; touching files in
    // a directory another local user could have planted is not.
    if !crate::rundir::is_private(dir) {
        return;
    }
    // A present viewer keeps its control file even while its heartbeat is
    // stale (a suspended machine, a stalled loop): deleting it would leave a
    // live, possibly dirty, editor unable to receive its close request. Only
    // label-only panes from a server resume are known dead.
    let live: std::collections::BTreeSet<String> = previews
        .iter()
        .filter(|preview| !preview.resumed)
        .map(|preview| preview.pane_id.replace(':', "_"))
        .collect();
    let live_controls: std::collections::BTreeSet<PathBuf> = previews
        .iter()
        .filter(|preview| !preview.resumed)
        .map(|preview| preview.control.clone())
        .collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if live_controls.contains(&entry.path()) {
            continue;
        }
        let legacy_orphan = name
            .strip_prefix("herdr-sidebar-preview-")
            .and_then(|value| value.strip_suffix(".ctl"))
            .is_some_and(|id| !live.contains(id));
        let abandoned_spawn = (name.starts_with("herdr-sidebar-control-")
            || name.starts_with("p-"))
            && entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= Duration::from_secs(60));
        if legacy_orphan || abandoned_spawn {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Re-stamp every 5s of wall-clock time, and immediately after the clock
/// jumps backwards (a correction must not silence the heartbeat).
fn heartbeat_due(last: u64, now: u64) -> bool {
    now < last || now - last >= 5
}

/// The viewer's event loop; returns when the user closes it.
pub fn run(control: &Path) -> std::io::Result<()> {
    // A control path outside our confined scratch directory would otherwise
    // leave this process idling forever, silently never seeing a request —
    // fail fast instead. This can legitimately happen if the launcher and
    // this process disagree on `rundir::dir`'s no-`HOME` fallback (`TMPDIR`
    // can vary by process), so say that rather than just "denied".
    if !control_path_ok(control) {
        let mut err = control_path_err(control).to_string();
        err.push_str(
            " (the launcher and this process may have resolved a different \
             runtime directory — see rundir::dir's fallback)",
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            err,
        ));
    }
    let theme = IconTheme::resolve(
        std::env::var("HERDR_SIDEBAR_ICONS")
            .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
            .ok()
            .as_deref(),
        crate::state::load_state().icons,
    );
    let mut current = read_control(control);
    let mut preview_load = current.clone().map(start_preview_load);
    let doc = current.as_ref().map(loading_doc).unwrap_or_else(|| Doc {
        name: "(nothing to show)".into(),
        context: String::new(),
        lines: vec![Line::raw("(waiting for a click in the sidebar)")],
        numbered: false,
        media: None,
        scroll: 0,
        wrap: true,
        glow_width: None,
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
        stamp: None,
    });
    let mut mode = ViewMode::Preview(doc);
    report_identity(
        &mode,
        current.as_ref().map(Request::doc_key).as_deref(),
        control,
    );

    // Blank the primary screen so pane handoffs never flash the shell.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
        crossterm::cursor::MoveTo(0, 0),
    );
    crossterm::style::force_color_output(true); // TUI colors ≠ pipeable output
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let mut page: usize = 20;
    let mut preview_body = Rect::default();
    let mut edit_width: usize = 1;
    let mut edit_body = Rect::default();
    let mut prompt: Option<Prompt> = None;
    let mut notice: Option<String> = None;
    let mut last_heartbeat = crate::state::unix_now();
    let mut last_external_check = Instant::now();
    let mut last_diff_refresh = Instant::now();
    let mut diff_refresh: Option<(
        u64,
        Request,
        std::sync::mpsc::Receiver<Option<Doc>>,
        Instant,
    )> = None;
    let mut copier: Option<ReleaseCopier> = None;
    let mut identity_pending = false;
    let result = loop {
        let loaded =
            preview_load
                .as_ref()
                .and_then(|(request, receiver)| match receiver.try_recv() {
                    Ok(doc) => Some((request.clone(), Some(doc))),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Some((request.clone(), None))
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                });
        if let Some((request, loaded)) = loaded {
            preview_load = None;
            if current.as_ref() == Some(&request)
                && let (ViewMode::Preview(doc), Some(mut loaded)) = (&mut mode, loaded)
            {
                if doc.glow_width.is_some() && loaded.glow_width.is_some() {
                    loaded.scroll = doc.scroll;
                }
                *doc = loaded;
            }
        }
        let prompt_text = prompt.as_ref().map(Prompt::text);
        let draw = terminal.draw(|frame| match &mut mode {
            ViewMode::Preview(doc) => {
                (page, preview_body) = draw_doc(
                    frame,
                    doc,
                    theme,
                    matches!(current, Some(Request::File { .. })),
                    notice.as_deref(),
                );
            }
            ViewMode::Edit(editor) => {
                (page, edit_width, edit_body) = draw_editor(frame, editor, theme, prompt_text);
            }
        });
        if let Err(e) = draw {
            break Err(e);
        }
        if identity_pending {
            report_identity(
                &mode,
                current.as_ref().map(Request::doc_key).as_deref(),
                control,
            );
            identity_pending = false;
        }
        let body_width = preview_body.width.max(20);
        if preview_load.is_none()
            && let (ViewMode::Preview(doc), Some(request @ Request::File { line: None, .. })) =
                (&mode, current.as_ref())
            && glow_needs_reload(doc, body_width)
        {
            LAST_BODY_WIDTH.store(body_width, std::sync::atomic::Ordering::Relaxed);
            preview_load = Some(start_preview_load(request.clone()));
        }
        let mut should_close = false;
        let poll = if preview_load.is_some() {
            LOAD_POLL
        } else {
            POLL
        };
        if event::poll(poll)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(active_prompt) = prompt.take() {
                        match (active_prompt, key.code) {
                            (Prompt::Unsaved(pending), KeyCode::Char('s')) => {
                                if let ViewMode::Edit(editor) = &mut mode {
                                    match editor.save(false) {
                                        Ok(SaveOutcome::Saved) => {
                                            should_close = apply_pending(
                                                pending,
                                                &mut mode,
                                                &mut current,
                                                &mut preview_load,
                                                &mut identity_pending,
                                                control,
                                                &mut notice,
                                            );
                                        }
                                        Ok(SaveOutcome::Conflict) => {
                                            prompt = Some(Prompt::Conflict(Some(pending)));
                                        }
                                        Err(e) => {
                                            editor.set_status(format!("save failed: {e}"));
                                            prompt = Some(Prompt::Unsaved(pending));
                                        }
                                    }
                                }
                            }
                            (Prompt::Unsaved(pending), KeyCode::Char('d')) => {
                                should_close = apply_pending(
                                    pending,
                                    &mut mode,
                                    &mut current,
                                    &mut preview_load,
                                    &mut identity_pending,
                                    control,
                                    &mut notice,
                                );
                            }
                            (Prompt::Unsaved(_), KeyCode::Esc | KeyCode::Char('c')) => {
                                restore_current_control(control, &current);
                                report_identity(
                                    &mode,
                                    current.as_ref().map(Request::doc_key).as_deref(),
                                    control,
                                );
                            }
                            (Prompt::Conflict(after), KeyCode::Char('o')) => {
                                if let ViewMode::Edit(editor) = &mut mode {
                                    match editor.save(true) {
                                        Ok(SaveOutcome::Saved) => {
                                            if let Some(pending) = after {
                                                should_close = apply_pending(
                                                    pending,
                                                    &mut mode,
                                                    &mut current,
                                                    &mut preview_load,
                                                    &mut identity_pending,
                                                    control,
                                                    &mut notice,
                                                );
                                            }
                                        }
                                        Ok(SaveOutcome::Conflict) => unreachable!(),
                                        Err(e) => {
                                            editor.set_status(format!("save failed: {e}"));
                                            prompt = Some(Prompt::Conflict(after));
                                        }
                                    }
                                }
                            }
                            (Prompt::Conflict(after), KeyCode::Char('r')) => {
                                if let ViewMode::Edit(editor) = &mut mode {
                                    match editor.reload(MAX_BYTES, MAX_LINES) {
                                        Ok(()) => {
                                            if let Some(pending) = after {
                                                should_close = apply_pending(
                                                    pending,
                                                    &mut mode,
                                                    &mut current,
                                                    &mut preview_load,
                                                    &mut identity_pending,
                                                    control,
                                                    &mut notice,
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            editor.set_status(e.to_string());
                                            prompt = Some(Prompt::Conflict(after));
                                        }
                                    }
                                }
                            }
                            (Prompt::Conflict(_), KeyCode::Esc | KeyCode::Char('c')) => {
                                restore_current_control(control, &current);
                                report_identity(
                                    &mode,
                                    current.as_ref().map(Request::doc_key).as_deref(),
                                    control,
                                );
                            }
                            (active, _) => prompt = Some(active),
                        }
                    } else {
                        match &mut mode {
                            ViewMode::Preview(doc) => {
                                let max = doc.rows.len().saturating_sub(1);
                                let shortcut = (key.modifiers.contains(KeyModifiers::CONTROL)
                                    && !key.modifiers.contains(KeyModifiers::ALT))
                                    || key.modifiers.contains(KeyModifiers::SUPER);
                                match key.code {
                                    KeyCode::Char('a') if shortcut => doc.select_all(),
                                    KeyCode::Char('c') if shortcut => {
                                        // Same ordered queue as release copies, so an older
                                        // automatic copy cannot land after this one.
                                        match doc.selected_text() {
                                            None => {
                                                notice = Some("select text before copying".into())
                                            }
                                            Some(text) => {
                                                if copier.is_none() {
                                                    copier = ReleaseCopier::start();
                                                }
                                                match &copier {
                                                    Some(copier) => copier.submit(CopyRequest {
                                                        text,
                                                        automatic: false,
                                                    }),
                                                    None => notice = Some(copy_selection(doc)),
                                                }
                                            }
                                        }
                                    }
                                    KeyCode::Esc | KeyCode::Char('q') => {
                                        should_close =
                                            close_own_pane(control, &current, &mut notice);
                                    }
                                    KeyCode::Char('e') => {
                                        if let Some(copier) = &copier {
                                            copier.drop_waiting_automatic();
                                        }
                                        if doc.media.is_some() {
                                            notice = Some("media previews are read-only".into());
                                        } else if let Some(Request::File { path, .. }) =
                                            current.as_ref()
                                        {
                                            match Editor::open(path, MAX_BYTES, MAX_LINES) {
                                                Ok(editor) => {
                                                    mode = ViewMode::Edit(editor);
                                                    preview_load = None;
                                                    notice = None;
                                                    report_identity(
                                                        &mode,
                                                        current
                                                            .as_ref()
                                                            .map(Request::doc_key)
                                                            .as_deref(),
                                                        control,
                                                    );
                                                }
                                                Err(e) => notice = Some(e.to_string()),
                                            }
                                        } else {
                                            notice = Some(
                                                "diffs and history previews are read-only".into(),
                                            );
                                        }
                                    }
                                    KeyCode::Up | KeyCode::Char('k') => {
                                        doc.scroll = doc.scroll.saturating_sub(1)
                                    }
                                    KeyCode::Down | KeyCode::Char('j') => {
                                        doc.scroll = (doc.scroll + 1).min(max)
                                    }
                                    // Shift+Space is byte-identical to Space without the
                                    // kitty keyboard protocol, so page-back takes `b`.
                                    KeyCode::PageUp | KeyCode::Char('b') => {
                                        doc.scroll = doc.scroll.saturating_sub(page)
                                    }
                                    KeyCode::PageDown | KeyCode::Char(' ') => {
                                        doc.scroll = (doc.scroll + page).min(max)
                                    }
                                    KeyCode::Home | KeyCode::Char('g') => doc.scroll = 0,
                                    KeyCode::End | KeyCode::Char('G') => doc.scroll = max,
                                    KeyCode::Char('w') if doc.media.is_none() => {
                                        doc.pending_src = Some(doc.top_src());
                                        doc.wrap = !doc.wrap;
                                    }
                                    _ => {}
                                }
                            }
                            ViewMode::Edit(editor) => {
                                let was_dirty = editor.dirty;
                                let action = editor.on_key(key, edit_width, page);
                                if !was_dirty
                                    && editor.dirty
                                    && let Some(doc_key) = current.as_ref().map(Request::doc_key)
                                {
                                    pin_own_tab(&doc_key);
                                }
                                match action {
                                    EditAction::None => {}
                                    EditAction::Leave => {
                                        if editor.dirty {
                                            prompt = Some(Prompt::Unsaved(Pending::LeaveEdit));
                                        } else {
                                            should_close = apply_pending(
                                                Pending::LeaveEdit,
                                                &mut mode,
                                                &mut current,
                                                &mut preview_load,
                                                &mut identity_pending,
                                                control,
                                                &mut notice,
                                            );
                                        }
                                    }
                                    EditAction::Close => {
                                        if editor.dirty {
                                            prompt = Some(Prompt::Unsaved(Pending::Close));
                                        } else {
                                            should_close =
                                                close_own_pane(control, &current, &mut notice);
                                        }
                                    }
                                    EditAction::Save => match editor.save(false) {
                                        Ok(SaveOutcome::Saved) => notice = None,
                                        Ok(SaveOutcome::Conflict) => {
                                            prompt = Some(Prompt::Conflict(None))
                                        }
                                        Err(e) => editor.set_status(format!("save failed: {e}")),
                                    },
                                }
                            }
                        }
                    }
                }
                Event::Mouse(mouse) => match &mut mode {
                    ViewMode::Preview(doc) => {
                        let max = doc.rows.len().saturating_sub(1);
                        match mouse.kind {
                            MouseEventKind::ScrollUp => doc.scroll = doc.scroll.saturating_sub(3),
                            MouseEventKind::ScrollDown => doc.scroll = (doc.scroll + 3).min(max),
                            MouseEventKind::Down(MouseButton::Left)
                                if mouse.row == 0 && mouse.column < 3 =>
                            {
                                should_close = close_own_pane(control, &current, &mut notice);
                            }
                            _ => {
                                // Copy on release, like herdr's own panes — unless the
                                // user turned that off in herdr (`[ui] copy_on_select`).
                                // Off the event loop: a hung clipboard tool must not
                                // freeze the preview.
                                if doc.on_mouse(&mouse, preview_body)
                                    && let Some(text) = doc.selected_text()
                                {
                                    if copier.is_none() {
                                        copier = ReleaseCopier::start();
                                    }
                                    match &copier {
                                        Some(copier) => copier.submit(CopyRequest {
                                            text,
                                            automatic: true,
                                        }),
                                        None => notice = Some(copy_selection(doc)),
                                    }
                                }
                            }
                        }
                    }
                    ViewMode::Edit(editor) => match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            editor.scroll_by(-3, edit_width, page.saturating_add(1))
                        }
                        MouseEventKind::ScrollDown => {
                            editor.scroll_by(3, edit_width, page.saturating_add(1))
                        }
                        MouseEventKind::Down(MouseButton::Left)
                            if mouse.row == 0 && mouse.column < 3 =>
                        {
                            if editor.dirty {
                                prompt = Some(Prompt::Unsaved(Pending::Close));
                            } else {
                                should_close = close_own_pane(control, &current, &mut notice);
                            }
                        }
                        _ if prompt.is_none() => editor.on_mouse(&mouse, edit_body),
                        _ => {}
                    },
                },
                _ => {} // resize etc: redraw
            }
        }
        if should_close {
            break Ok(());
        }

        // These checks run after every iteration, including sustained input;
        // otherwise a held key can starve the liveness heartbeat indefinitely.
        // Wall-clock, not `Instant`: a monotonic clock stops during suspend, so
        // after a resume the viewer would wait out the rest of its interval
        // while recovery code already reads the old stamp as stale.
        if heartbeat_due(last_heartbeat, crate::state::unix_now()) {
            report_identity(
                &mode,
                current.as_ref().map(Request::doc_key).as_deref(),
                control,
            );
            // Settings live in a shared file another pane may rewrite; the
            // heartbeat is the sidebar's own re-read cadence. Chrome, diff
            // tints and selection follow immediately — an already-highlighted
            // file keeps its syntax colors until it is reloaded.
            crate::ui::set_color_theme(crate::state::load_state().color_theme);
            last_heartbeat = crate::state::unix_now();
        }
        if prompt.is_none() {
            let target = read_control(control);
            if target != current
                && let Some(request) = target
            {
                if let ViewMode::Edit(editor) = &mode
                    && editor.dirty
                {
                    prompt = Some(Prompt::Unsaved(Pending::Switch(request)));
                } else if request == Request::Close {
                    if close_own_pane(control, &current, &mut notice) {
                        break Ok(());
                    }
                } else {
                    mode = ViewMode::Preview(loading_doc(&request));
                    current = Some(request.clone());
                    preview_load = Some(start_preview_load(request));
                    identity_pending = true;
                    notice = None;
                }
            }
        }
        if last_external_check.elapsed() >= Duration::from_secs(2) {
            if let ViewMode::Edit(editor) = &mut mode {
                editor.poll_external(MAX_BYTES, MAX_LINES);
            }
            last_external_check = Instant::now();
        }
        if let Some(copier) = &copier {
            while let Ok(result) = copier.results.try_recv() {
                let osc52 = matches!(result, ReleaseCopy::Osc52(_));
                if let Some(text) = release_copy_notice(result) {
                    notice = Some(text);
                }
                if osc52 {
                    let _ = copier.written.send(());
                }
            }
        }
        // A refresh started for an earlier document is abandoned, not awaited:
        // its worker may be stuck on a stalled drive.
        if diff_refresh.as_ref().is_some_and(|(started, _, _, at)| {
            *started != load_generation() || at.elapsed() >= REFRESH_ABANDON_AFTER
        }) {
            diff_refresh = None;
        }
        let refreshed = diff_refresh
            .as_ref()
            .and_then(
                |(started, request, receiver, _)| match receiver.try_recv() {
                    Ok(doc) => Some((*started, request.clone(), doc)),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Some((*started, request.clone(), None))
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                },
            );
        if let Some((started, request, refreshed)) = refreshed {
            diff_refresh = None;
            if refresh_is_current(started, &request, load_generation(), current.as_ref())
                && let (ViewMode::Preview(doc), Some(refreshed)) = (&mut mode, refreshed)
            {
                apply_diff_refresh(doc, refreshed);
            }
        }
        if last_diff_refresh.elapsed() >= Duration::from_secs(2) {
            if preview_load.is_none()
                && diff_refresh.is_none()
                && let ViewMode::Preview(doc) = &mode
                && let Some(request @ (Request::Diff { .. } | Request::File { .. })) =
                    current.clone()
                && let Some(in_flight) = InFlight::claim()
            {
                let baseline = doc.stamp;
                let worker_request = request.clone();
                let (sender, receiver) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _in_flight = in_flight;
                    // Files stat first, here on the worker; only a changed
                    // stamp pays for a reload. Diffs always re-run.
                    let reload = match &worker_request {
                        Request::File { path, .. } => file_refresh_needed(path, baseline),
                        _ => true,
                    };
                    let _ = sender.send(reload.then(|| load(&worker_request)));
                });
                diff_refresh = Some((load_generation(), request, receiver, Instant::now()));
            }
            last_diff_refresh = Instant::now();
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Header (✕ close + name + context), body, hint footer. Returns the page
/// stride for PageUp/Down.
fn draw_doc(
    frame: &mut Frame,
    doc: &mut Doc,
    theme: IconTheme,
    editable: bool,
    notice: Option<&str>,
) -> (usize, Rect) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    // Lay the body out for THIS width first: everything below (the clamp,
    // the slice, the page stride) counts rendered rows.
    LAST_BODY_WIDTH.store(body.width, std::sync::atomic::Ordering::Relaxed);
    doc.relayout(body.width, body.height);
    doc.scroll = doc.scroll.min(
        doc.rows
            .len()
            .saturating_sub(usize::from(body.height).max(1)),
    );

    let file_icon = icon(theme, &doc.name, false, false);
    let icon_style = ui_icon_style(file_icon.rgb);
    let left = vec![
        Span::styled(" ✕ ", Style::default().bold().fg(palette().header_accent)),
        Span::styled(format!("{} ", file_icon.glyph), icon_style),
        Span::styled(doc.name.clone(), Style::default().bold()),
    ];
    let used: usize = left.iter().map(Span::width).sum();
    let avail = usize::from(area.width).saturating_sub(used + 2);
    let shown = if doc.context.chars().count() > avail {
        let tail: String = doc
            .context
            .chars()
            .skip(
                doc.context
                    .chars()
                    .count()
                    .saturating_sub(avail.saturating_sub(1)),
            )
            .collect();
        format!("…{tail}")
    } else {
        doc.context.clone()
    };
    let mut spans = left;
    spans.push(Span::styled(format!("  {shown}"), Style::default().dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), header);

    // Rows are pre-wrapped, so the Paragraph never wraps for us: its
    // continuations would render past the bottom of the pane, where no
    // amount of scrolling could reach them.
    let selection = doc.selection_range();
    let gutter = doc.gutter();
    let text: Vec<Line> = doc
        .rows
        .iter()
        .enumerate()
        .skip(doc.scroll)
        .take(usize::from(body.height))
        .map(|(row, rendered)| selected_row(&rendered.line, gutter, row, selection))
        .collect();
    frame.render_widget(Paragraph::new(text), body);

    let wrap_hint = if doc.wrap {
        "w: wrap on"
    } else {
        "w: wrap off"
    };
    let hint = if let Some(notice) = notice {
        format!(" {notice}")
    } else if let Some(media) = &doc.media {
        if media.video_poster {
            " video poster frame  q close".into()
        } else {
            " image preview  q close".into()
        }
    } else if editable {
        preview_hint(&["e edit", "space/b page", wrap_hint], footer.width)
    } else {
        preview_hint(&["space/b page", "↑↓ scroll", wrap_hint], footer.width)
    };
    frame.render_widget(Paragraph::new(Line::from(hint).dim()), footer);
    (usize::from(body.height).saturating_sub(1).max(1), body)
}

/// The read-only footer: `q close` first so a narrow inline or takeover pane
/// never clips the way out, then as many optional hints as fit, then the
/// selection/copy hint when there is room for everything.
fn preview_hint(optional: &[&str], width: u16) -> String {
    let width = usize::from(width);
    let mut hint = String::from(" q close");
    for part in optional
        .iter()
        .copied()
        .chain(["drag select  Ctrl/Cmd+C copy"])
    {
        if hint.chars().count() + 2 + part.chars().count() <= width {
            hint.push_str("  ");
            hint.push_str(part);
        }
    }
    hint
}

fn draw_editor(
    frame: &mut Frame,
    editor: &mut Editor,
    theme: IconTheme,
    prompt: Option<&str>,
) -> (usize, usize, Rect) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let name = editor.name();
    let file_icon = icon(theme, &name, false, false);
    let icon_style = ui_icon_style(file_icon.rgb);
    let dirty = if editor.dirty { " ●" } else { "" };
    let external = if editor.external_changed {
        "  EXTERNAL CHANGE"
    } else {
        ""
    };
    let left = vec![
        Span::styled(" ✕ ", Style::default().bold().fg(palette().header_accent)),
        Span::styled(format!("{} ", file_icon.glyph), icon_style),
        Span::styled(format!("{name}{dirty}"), Style::default().bold()),
        Span::styled(
            "  EDIT (experimental)",
            Style::default().fg(palette().warning),
        ),
        Span::styled(external, Style::default().fg(palette().conflict).bold()),
    ];
    let used: usize = left.iter().map(Span::width).sum();
    let context = editor.context();
    let avail = usize::from(area.width).saturating_sub(used + 2);
    let shown = if context.chars().count() > avail {
        let tail: String = context
            .chars()
            .skip(
                context
                    .chars()
                    .count()
                    .saturating_sub(avail.saturating_sub(1)),
            )
            .collect();
        format!("…{tail}")
    } else {
        context
    };
    let mut spans = left;
    spans.push(Span::styled(format!("  {shown}"), Style::default().dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), header);
    editor.draw(frame, body, footer, prompt);
    let gutter = editor.line_count().to_string().len() + 1;
    (
        usize::from(body.height).saturating_sub(1).max(1),
        usize::from(body.width).saturating_sub(gutter).max(1),
        body,
    )
}

// ---------------------------------------------------------------------------
// Client side: how the sidebar views open things in the viewer pane.
// ---------------------------------------------------------------------------

/// What a file click may do to a stale preview in its space.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StaleCleanup {
    CloseTab { tab_id: String, control: PathBuf },
    ClosePane { pane_id: String, control: PathBuf },
}

/// Only label-only panes left by a server resume are provably dead — their
/// process metadata is gone, so nothing unsaved can live there. A pane that
/// still carries a (stale) heartbeat may be a suspended or stalled viewer
/// holding an unsaved editor buffer, including a temporary takeover viewer;
/// a click never kills it. It is simply not routed to, and its own close
/// (q / Esc / ✕) still works once it responds.
pub(crate) fn stale_cleanup(previews: &[PreviewPane], pane_list_json: &str) -> Vec<StaleCleanup> {
    previews
        .iter()
        .filter(|preview| preview.stale && preview.resumed)
        .map(|preview| {
            if tab_is_plugin_only(pane_list_json, &preview.tab_id) {
                StaleCleanup::CloseTab {
                    tab_id: preview.tab_id.clone(),
                    control: preview.control.clone(),
                }
            } else {
                StaleCleanup::ClosePane {
                    pane_id: preview.pane_id.clone(),
                    control: preview.control.clone(),
                }
            }
        })
        .collect()
}

/// Can a click route its document into `preview`? Stale viewers are never
/// routed to (and never killed, see [`stale_cleanup`]); an inline viewer
/// belongs to ONE tab, and a tab-mode click never adopts one. Pure and
/// non-blocking: a stale viewer anywhere else in the workspace costs a click
/// nothing.
pub(crate) fn routable(preview: &PreviewPane, inline: bool, caller_tab: &str) -> bool {
    !preview.stale && preview.inline == inline && (!inline || preview.tab_id == caller_tab)
}

/// Inline placement only: a present-but-unresponsive viewer already in the
/// caller's own tab. Spawning another beside it would split the tab again on
/// every click, so the click hands it the request and reports instead.
pub(crate) fn unresponsive_inline_in<'a>(
    previews: &'a [PreviewPane],
    inline: bool,
    caller_tab: &str,
) -> Option<&'a PreviewPane> {
    if !inline {
        return None;
    }
    previews.iter().find(|preview| {
        preview.inline && preview.tab_id == caller_tab && preview.stale && !preview.resumed
    })
}

/// Open `payload` (identified by `doc_key`) where the `Preview placement`
/// setting says.
///
/// In `Tab` placement this follows VS Code tab rules: jump to the document's
/// existing tab, else overwrite the one ephemeral tab, else create a tab.
///
/// In `Pane` placement there is exactly ONE viewer pane per tab, split in
/// beside the sidebar and reused for every later click. Pinning is meaningless
/// without a tab of its own, so an inline viewer is always reusable — a dirty
/// editor is protected by the viewer's own unsaved-changes prompt on the
/// control-file switch, not by refusing to route to it (refusing would split
/// the tab again on every click).
pub fn open_in_pane(
    my_pane_id: &str,
    spawn_cwd: &Path,
    doc_key: &str,
    payload: &str,
) -> Result<PreviewTarget, String> {
    let state = crate::state::load_state();
    let inline = state.preview_placement.is_inline();
    let list = ipc::call_text("pane.list", serde_json::json!({}))
        .map_err(|e| format!("preview failed: {e}"))?;
    let caller_tab_id = crate::launch::tab_of(&list, my_pane_id);
    // An unrestorable journal must never block ordinary previews in this
    // tab. The takeover path below retries the restore and reports it.
    if let Err(error) = crate::takeover::recover(&caller_tab_id) {
        eprintln!("herdr-sidebar: temporary preview restore deferred: {error}");
    }
    let list =
        ipc::call_text("pane.list", serde_json::json!({})).map_err(|error| error.to_string())?;
    // Route only within OUR space. A session-wide search reused another
    // project's ephemeral tab and focus jumped there, which reads as the
    // tree refusing to change.
    let my_workspace = crate::launch::workspace_of(&list, my_pane_id);
    sweep_orphan_controls(&list);
    let mut previews: Vec<PreviewPane> = previews_in(&list)
        .into_iter()
        .filter(|p| p.workspace_id == my_workspace)
        .collect();
    for action in stale_cleanup(&previews, &list) {
        match action {
            StaleCleanup::CloseTab { tab_id, control } => {
                remove_control_file(&control);
                let _ = ipc::call_text("tab.close", serde_json::json!({ "tab_id": tab_id }));
            }
            StaleCleanup::ClosePane { pane_id, control } => {
                remove_control_file(&control);
                let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id }));
            }
        }
    }
    if let Some(frozen) = unresponsive_inline_in(&previews, inline, &caller_tab_id) {
        // Never wait here and never kill it: queue the document for when it
        // responds (its own unsaved-changes prompt still guards a dirty buffer).
        let _ = write_scratch_file(&frozen.control, payload);
        return Err(
            "the preview in this tab is not responding; it will show this file when it does \
             (or close its pane from herdr)"
                .into(),
        );
    }
    previews.retain(|preview| routable(preview, inline, &caller_tab_id));
    previews.sort_by(|a, b| a.tab_id.cmp(&b.tab_id).then(a.pane_id.cmp(&b.pane_id)));
    let origin_tab_id = preview_origin_tab(&previews, &caller_tab_id);

    // 1. Already open — jump to it, pinned or not.
    if let Some(p) = preview_for_doc(&previews, doc_key) {
        write_scratch_file(&p.control, payload).map_err(|e| format!("preview failed: {e}"))?;
        remember_origin(&p.pane_id, &origin_tab_id);
        if !inline {
            focus_tab_for_client(&p.tab_id, Some(&p.pane_id));
        }
        return Ok(PreviewTarget {
            pane_id: p.pane_id,
            tab_id: p.tab_id,
            origin_tab_id,
            inline,
        });
    }

    // 2. Overwrite the ephemeral tab (inline: the tab's one viewer pane).
    if let Some(p) = reusable_preview(&previews, inline) {
        write_scratch_file(&p.control, payload).map_err(|e| format!("preview failed: {e}"))?;
        remember_origin(&p.pane_id, &origin_tab_id);
        if !inline {
            focus_tab_for_client(&p.tab_id, Some(&p.pane_id));
        }
        return Ok(PreviewTarget {
            pane_id: p.pane_id,
            tab_id: p.tab_id,
            origin_tab_id,
            inline,
        });
    }

    // 3. Nothing reusable — a pane beside the sidebar, or a tab of its own.
    if state.preview_placement == crate::state::PreviewPlacement::Replace {
        crate::takeover::open(
            my_pane_id,
            spawn_cwd,
            doc_key,
            payload,
            &caller_tab_id,
            state.dock_right,
            state.sidebar_width,
        )
    } else if inline {
        spawn_inline_pane(
            my_pane_id,
            spawn_cwd,
            doc_key,
            payload,
            &caller_tab_id,
            state.dock_right,
            state.sidebar_width,
        )
    } else {
        spawn_preview_tab(my_pane_id, spawn_cwd, doc_key, payload, &origin_tab_id)
    }
}

/// Where a preview request landed. Handed back so a double click can pin
/// exactly the tab its first click used — searching by document key would
/// race the viewer, which only stamps `hs-preview-path` once it has started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewTarget {
    pub pane_id: String,
    pub tab_id: String,
    pub origin_tab_id: String,
    /// The viewer shares the caller's tab, so it has no tab of its own to pin
    /// or rename.
    pub inline: bool,
}

/// Mark a preview's tab pinned: wait briefly for a clean viewer to acknowledge
/// the first click, then stamp the token so it stops being reusable. Dirty
/// editors deliberately do not acknowledge a switch until the user resolves
/// their unsaved-change prompt. Idempotent.
///
/// Inline placement has no second tab for the next file to land in, so there
/// is nothing to pin — report success so a double click stays a plain
/// "show me this" instead of raising a bogus confirmation warning.
pub fn pin_target(target: &PreviewTarget, doc_key: &str) -> bool {
    if target.inline {
        return true;
    }
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        let Ok(list) = ipc::call_text("pane.list", serde_json::json!({})) else {
            return false;
        };
        if target_is_showing(&previews_in(&list), target, doc_key) {
            break;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": target.pane_id,
            "source": METADATA_SOURCE,
            "tokens": { TOKEN_PINNED: "1" },
        }),
    );
    let _ = ipc::call_text(
        "tab.rename",
        serde_json::json!({ "tab_id": target.tab_id, "label": tab_label(doc_key, true) }),
    );
    true
}

fn target_is_showing(previews: &[PreviewPane], target: &PreviewTarget, doc_key: &str) -> bool {
    let expected = document_token(doc_key);
    previews.iter().any(|preview| {
        !preview.stale && preview.pane_id == target.pane_id && preview.doc_token == expected
    })
}

/// Spawn a preview in a tab of its own. The viewer is the new tab's ROOT
/// pane (`tab.create` with the viewer's cwd/env), so the origin tab is never
/// split into and never has a pane moved out of it — on herdr 0.9 those two
/// layout changes reached the client as a visible flicker, and its re-fit of
/// the origin tab lagged. The `tab.created` hook docks a sidebar alongside,
/// so the tree stays reachable.
fn spawn_preview_tab(
    my_pane_id: &str,
    spawn_cwd: &Path,
    doc_key: &str,
    payload: &str,
    origin_tab_id: &str,
) -> Result<PreviewTarget, String> {
    let (new_pane, tab_id, control) = create_viewer_tab(my_pane_id, spawn_cwd, doc_key, payload)?;
    if !mark_dedicated_preview(&new_pane) {
        cleanup_moved_spawn(&new_pane, &tab_id, &control);
        return Err("preview tab could not record ownership".into());
    }
    remember_origin(&new_pane, origin_tab_id);
    if !start_viewer_pane(&new_pane) {
        cleanup_moved_spawn(&new_pane, &tab_id, &control);
        return Err("preview process failed to start".into());
    }
    // Bring the client along. `tab.create` ran with `focus: false` so this
    // is a real focus transition — the only kind a 0.9 client follows.
    focus_tab_for_client(&tab_id, Some(&new_pane));
    Ok(PreviewTarget {
        pane_id: new_pane,
        tab_id,
        origin_tab_id: origin_tab_id.to_string(),
        inline: false,
    })
}

/// Spawn the tab's one inline viewer pane, beside the sidebar on the side
/// away from its dock edge, and leave focus in the sidebar: the preview is
/// visible right there, so stealing focus would only stop the user from
/// walking the tree with the arrow keys.
pub(crate) fn spawn_inline_pane(
    my_pane_id: &str,
    spawn_cwd: &Path,
    doc_key: &str,
    payload: &str,
    caller_tab_id: &str,
    dock_right: bool,
    sidebar_cols: u16,
) -> Result<PreviewTarget, String> {
    let (new_pane, control) = spawn_viewer_pane(
        my_pane_id,
        spawn_cwd,
        doc_key,
        payload,
        Some(InlineSpawn {
            dock_right,
            sidebar_cols,
        }),
    )?;
    // A swap moves the FOCUSED SLOT's occupant, not the focus: when the plan
    // swapped us out of our own slot, focus is now sitting on the brand-new
    // pane. Put it back before the shell there starts consuming keystrokes.
    let _ = ipc::call_text("pane.focus", serde_json::json!({ "pane_id": my_pane_id }));
    remember_origin(&new_pane, caller_tab_id);
    if !start_viewer_pane(&new_pane) {
        cleanup_spawn(&new_pane, &control);
        return Err("preview process failed to start".into());
    }
    Ok(PreviewTarget {
        pane_id: new_pane,
        tab_id: caller_tab_id.to_string(),
        origin_tab_id: caller_tab_id.to_string(),
        inline: true,
    })
}

fn remember_origin(pane_id: &str, tab_id: &str) {
    if tab_id.is_empty() {
        return;
    }
    let _ = ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": METADATA_SOURCE,
            "tokens": { TOKEN_ORIGIN_TAB: tab_id },
        }),
    );
}

/// How the sidebar's Esc treats this tab's viewer.
#[derive(Debug, PartialEq, Eq)]
enum EscClose {
    /// Ask the viewer to close itself, so a dirty editor can confirm first.
    Request,
    /// A label-only pane from a server resume: provably no process left.
    Kill,
}

/// Only a resumed husk (no heartbeat token at all) is killed. A stale stamp
/// may be a suspended or stalled viewer with unsaved edits.
fn esc_close_action(stale: bool, resumed: bool) -> EscClose {
    if stale && resumed {
        EscClose::Kill
    } else {
        EscClose::Request
    }
}

/// Ask this tab's viewer to close (Esc from the sidebar). A viewer owns its
/// close so an editor with unsaved changes can confirm first. Returns a
/// notice for the sidebar when the viewer does not respond.
pub fn close_in_tab(my_pane_id: &str) -> Option<String> {
    let Ok(json) = ipc::call_text("pane.list", serde_json::json!({})) else {
        return None;
    };
    let tab = crate::launch::tab_of(&json, my_pane_id);
    let Some((id, stale)) = viewer_pane_in_tab(&json, my_pane_id) else {
        // Nothing to close, but an orphaned takeover journal (its viewer was
        // closed from herdr) can still bring the working panes back.
        return crate::takeover::recover(&tab)
            .err()
            .map(|error| format!("cannot restore the working panes: {error}"));
    };
    let preview = previews_in(&json)
        .into_iter()
        .find(|preview| preview.pane_id == id);
    let resumed = preview.as_ref().is_some_and(|preview| preview.resumed);
    match esc_close_action(stale, resumed) {
        EscClose::Kill => {
            match preview {
                Some(preview)
                    if (preview.dedicated || preview.resumed)
                        && tab_is_plugin_only(&json, &preview.tab_id) =>
                {
                    close_preview_tab(&preview);
                }
                _ => {
                    let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }));
                }
            }
            crate::takeover::recover(&tab)
                .err()
                .map(|error| format!("cannot restore the working panes: {error}"))
        }
        EscClose::Request => {
            let control = preview
                .map(|preview| preview.control)
                .unwrap_or_else(|| control_path_for_pane(&id));
            let _ = write_scratch_file(&control, "close");
            // No waiting: the request applies as soon as the viewer responds.
            stale.then(|| {
                "preview is not responding; it will close when it does (or close its pane \
                 from herdr)"
                    .to_string()
            })
        }
    }
}

/// The viewer pane in the same tab, by metadata token, plus whether its
/// heartbeat says it is DEAD (`(pane_id, stale)`).
fn viewer_pane_in_tab(pane_list_json: &str, my_pane_id: &str) -> Option<(String, bool)> {
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
        label: Option<String>,
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
    // Token match finds a live viewer; a preview-labelled pane WITHOUT the
    // token is a resumed corpse (labels survive server restarts, tokens
    // don't) — report it too, with a missing token, so the stale check
    // below flags it and the caller closes it instead of spawning a twin.
    let viewer = panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(my_tab.as_str()))
        .find(|p| {
            p.tokens.contains_key(METADATA_SOURCE)
                || p.label
                    .as_deref()
                    .is_some_and(crate::launch::is_preview_label)
        })?;
    let id = viewer.pane_id.clone()?;
    let now = crate::state::unix_now();
    let stale = viewer
        .tokens
        .get(METADATA_SOURCE)
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ts| now.saturating_sub(ts) > crate::launch::HEARTBEAT_STALE_SECS)
        .unwrap_or(true);
    Some((id, stale))
}

pub const TOKEN_PATH: &str = "hs-preview-path";
pub const TOKEN_PINNED: &str = "hs-preview-pinned";
pub const TOKEN_CONTROL: &str = "hs-preview-control";
pub const TOKEN_DEDICATED: &str = "hs-preview-dedicated";
pub const TOKEN_ORIGIN_TAB: &str = "hs-preview-origin-tab";
/// This viewer shares the sidebar's tab instead of owning one
/// (`PreviewPlacement::Pane`). Routing needs it on the PANE so flipping the
/// setting cannot hand a tab-mode click an inline pane, or the reverse.
pub const TOKEN_INLINE: &str = "hs-preview-inline";

/// A live preview pane and the document it is showing. State lives on the
/// pane, so it cannot outlive what it describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewPane {
    pub pane_id: String,
    pub tab_id: String,
    /// The space this preview belongs to. Routing is scoped to it: the
    /// ephemeral tab is per project, and reusing another workspace's would
    /// yank focus into a different project.
    pub workspace_id: String,
    pub doc_token: String,
    pub pinned: bool,
    pub control: PathBuf,
    pub stale: bool,
    pub dedicated: bool,
    /// Shares the sidebar's tab (see [`TOKEN_INLINE`]).
    pub inline: bool,
    pub origin_tab_id: String,
    /// Herdr restored the pane label but not its process metadata. This is a
    /// dead shell left behind by server resume, not a live unsaved editor.
    pub resumed: bool,
}

/// Every preview pane in the session, from one `pane.list` payload.
pub(crate) fn previews_in(pane_list_json: &str) -> Vec<PreviewPane> {
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
        workspace_id: Option<String>,
        label: Option<String>,
        #[serde(default)]
        tokens: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let Ok(msg) = serde_json::from_str::<Msg>(crate::launch::strip_bom(pane_list_json)) else {
        return Vec::new();
    };
    msg.result
        .panes
        .into_iter()
        .filter_map(|p| {
            let preview_label = p
                .label
                .as_deref()
                .is_some_and(crate::launch::is_preview_label);
            let raw_doc_token = p.tokens.get(TOKEN_PATH).and_then(|value| value.as_str());
            if raw_doc_token.is_none() && !preview_label {
                return None;
            }
            let doc_token = raw_doc_token
                .map(|raw| {
                    if raw.len() == 16 && raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        raw.to_string()
                    } else {
                        document_token(raw)
                    }
                })
                .unwrap_or_default();
            let pane_id = p.pane_id?;
            let control = p
                .tokens
                .get(TOKEN_CONTROL)
                .and_then(|value| value.as_str())
                .map(control_from_token)
                .unwrap_or_else(|| control_path_for_pane(&pane_id));
            let heartbeat = p
                .tokens
                .get(METADATA_SOURCE)
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<u64>().ok());
            let resumed = preview_label && heartbeat.is_none();
            let stale = heartbeat
                .map(|stamp| {
                    crate::state::unix_now().saturating_sub(stamp)
                        > crate::launch::HEARTBEAT_STALE_SECS
                })
                .unwrap_or(true);
            Some(PreviewPane {
                pane_id,
                tab_id: p.tab_id?,
                workspace_id: p.workspace_id.unwrap_or_default(),
                doc_token,
                pinned: p.tokens.contains_key(TOKEN_PINNED),
                control,
                stale,
                dedicated: p.tokens.contains_key(TOKEN_DEDICATED),
                inline: p.tokens.contains_key(TOKEN_INLINE),
                origin_tab_id: p
                    .tokens
                    .get(TOKEN_ORIGIN_TAB)
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                resumed,
            })
        })
        .collect()
}

fn tab_is_plugin_only(pane_list_json: &str, tab_id: &str) -> bool {
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
        tab_id: Option<String>,
        label: Option<String>,
        #[serde(default)]
        tokens: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let Ok(msg) = serde_json::from_str::<Msg>(crate::launch::strip_bom(pane_list_json)) else {
        return false;
    };
    let panes: Vec<_> = msg
        .result
        .panes
        .into_iter()
        .filter(|pane| pane.tab_id.as_deref() == Some(tab_id))
        .collect();
    let resumed_preview = panes.iter().any(|pane| {
        pane.label
            .as_deref()
            .is_some_and(crate::launch::is_preview_label)
            && !pane.tokens.contains_key(METADATA_SOURCE)
    });
    !panes.is_empty()
        && panes.iter().all(|pane| {
            pane.tokens.contains_key(METADATA_SOURCE)
                || pane.tokens.contains_key("herdr-sidebar-explorer")
                || pane.tokens.contains_key("herdr-sidebar-git")
                || pane
                    .label
                    .as_deref()
                    .is_some_and(crate::launch::is_preview_label)
                || (resumed_preview
                    && matches!(
                        pane.label.as_deref(),
                        Some("Sidebar" | "Explorer" | "Source Control")
                    ))
        })
}

/// The tab already showing `doc_key`, pinned or not — checked FIRST so
/// re-selecting an open file jumps instead of clobbering the ephemeral tab.
fn preview_for_doc(previews: &[PreviewPane], doc_key: &str) -> Option<PreviewPane> {
    let expected = document_token(doc_key);
    previews
        .iter()
        .find(|p| !p.stale && p.doc_token == expected)
        .cloned()
}

/// The ephemeral tab, if one exists. Pinned tabs are never overwritten —
/// except inline, where the pane IS the tab's single viewer and pinning never
/// happens (see [`open_in_pane`]).
fn reusable_preview(previews: &[PreviewPane], inline: bool) -> Option<PreviewPane> {
    previews
        .iter()
        .find(|p| !p.stale && (inline || !p.pinned))
        .cloned()
}

/// An ephemeral preview's sidebar is itself a valid launch surface. Preserve
/// the tab that originally opened it rather than recording the preview tab as
/// its own return destination. A pinned preview is a normal caller tab.
fn preview_origin_tab(previews: &[PreviewPane], caller_tab_id: &str) -> String {
    previews
        .iter()
        .find(|preview| {
            !preview.stale
                && !preview.pinned
                && preview.tab_id == caller_tab_id
                && !preview.origin_tab_id.is_empty()
        })
        .map(|preview| preview.origin_tab_id.clone())
        .unwrap_or_else(|| caller_tab_id.to_string())
}

/// Split a viewer pane directly to the caller's right: split the right
/// NEIGHBOR and swap the fresh pane into its left slot (split only goes
/// right/down), so the layout reads sidebar | preview | rest.
/// Geometry inputs for an inline spawn: which edge the sidebar is docked at
/// and how wide it wants to stay.
#[derive(Clone, Copy)]
struct InlineSpawn {
    dock_right: bool,
    sidebar_cols: u16,
}

/// One `pane.split` invocation: what to split, the ORIGINAL pane's share, and
/// whether the fresh pane has to be swapped into the split target's slot.
#[derive(Clone, Debug, PartialEq)]
struct SplitPlan {
    target: String,
    ratio: f64,
    swap: bool,
}

/// A legal self-split when `pane.layout` cannot describe the real geometry.
/// Keep the historical 30% sidebar share and mirror the split for a right
/// dock: after swapping occupants, the sidebar lands in the 30% right slot.
fn fallback_inline_split_plan(pane_id: &str, inline: InlineSpawn) -> SplitPlan {
    SplitPlan {
        target: pane_id.to_string(),
        ratio: if inline.dock_right { 0.7 } else { 0.3 },
        swap: inline.dock_right,
    }
}

fn spawn_viewer_pane(
    my_pane_id: &str,
    spawn_cwd: &Path,
    doc_key: &str,
    payload: &str,
    inline: Option<InlineSpawn>,
) -> Result<(String, PathBuf), String> {
    crate::rundir::ensure_private(&scratch_dir()).map_err(|e| format!("preview failed: {e}"))?;
    let control = fresh_control_path();
    write_scratch_file(&control, payload).map_err(|e| format!("preview failed: {e}"))?;
    let layout = ipc::call_text("pane.layout", serde_json::json!({ "pane_id": my_pane_id })).ok();
    let plan = match inline {
        Some(inline) => layout
            .as_deref()
            .and_then(|json| inline_split_plan(json, my_pane_id, inline))
            .unwrap_or_else(|| fallback_inline_split_plan(my_pane_id, inline)),
        // Tab placement: the pane is moved out immediately, so this geometry
        // only has to be legal, never pretty.
        None => {
            let neighbor = layout
                .as_deref()
                .and_then(|json| side_neighbor(json, my_pane_id, false));
            match neighbor {
                Some(target) => SplitPlan {
                    target,
                    ratio: 0.5,
                    swap: true,
                },
                None => SplitPlan {
                    target: my_pane_id.to_string(),
                    ratio: 0.3,
                    swap: false,
                },
            }
        }
    };
    let response = ipc::call_text(
        "pane.split",
        serde_json::json!({
            "target_pane_id": plan.target,
            "direction": "right",
            "ratio": plan.ratio,
            "focus": false,
            "cwd": spawn_cwd.display().to_string(),
            "env": preview_spawn_env(&control, inline.is_some()),
        }),
    );
    let new_pane = response
        .ok()
        .and_then(|r| crate::launch::split_pane_id(&r))
        .ok_or_else(|| {
            remove_control_file(&control);
            "preview pane failed to open".to_string()
        })?;
    if plan.swap
        && !ipc::call_text(
            "pane.swap",
            serde_json::json!({ "source_pane_id": new_pane, "target_pane_id": plan.target }),
        )
        .is_ok_and(|response| ipc_succeeded(&response))
    {
        cleanup_spawn(&new_pane, &control);
        return Err("preview pane could not be positioned".into());
    }
    register_viewer_pane(&new_pane, &control, doc_key, inline.is_some())?;
    Ok((new_pane, control))
}

/// Spawn the viewer's shell pane as the ROOT pane of a brand-new tab. This is
/// the tab-placement counterpart of `spawn_viewer_pane`: nothing is split into
/// the origin tab and nothing is moved out of it again, so the tab the user
/// clicked from never changes shape (herdr 0.9 clients redraw those two
/// layout changes as a visible flicker, and re-fit lazily). `tab.create`
/// takes the same cwd/env as `pane.split`, so the root pane is driven exactly
/// like a split one; `focus: false` keeps the later `pane.focus` a real
/// transition. Returns (pane_id, tab_id, control).
fn create_viewer_tab(
    my_pane_id: &str,
    spawn_cwd: &Path,
    doc_key: &str,
    payload: &str,
) -> Result<(String, String, PathBuf), String> {
    crate::rundir::ensure_private(&scratch_dir()).map_err(|e| format!("preview failed: {e}"))?;
    let control = fresh_control_path();
    write_scratch_file(&control, payload).map_err(|e| format!("preview failed: {e}"))?;
    let workspace_id = ipc::call_text("pane.list", serde_json::json!({}))
        .map(|list| crate::launch::workspace_of(&list, my_pane_id))
        .unwrap_or_default();
    let mut params = serde_json::json!({
        "label": tab_label(doc_key, false),
        "focus": false,
        "cwd": spawn_cwd.display().to_string(),
        "env": preview_spawn_env(&control, false),
    });
    if !workspace_id.is_empty() {
        params["workspace_id"] = serde_json::Value::String(workspace_id);
    }
    let response = ipc::call_text("tab.create", params).ok();
    let Some((tab_id, new_pane)) = response
        .as_deref()
        .and_then(crate::launch::created_tab_root_pane)
    else {
        remove_control_file(&control);
        return Err("preview tab failed to open".into());
    };
    if let Err(error) = register_viewer_pane(&new_pane, &control, doc_key, false) {
        let _ = ipc::call_text("tab.close", serde_json::json!({ "tab_id": tab_id }));
        return Err(error);
    }
    Ok((new_pane, tab_id, control))
}

/// Stamp a freshly spawned shell pane as ours: the document/control tokens
/// the sidebar routes clicks by, plus the "Preview" label. Closes the pane
/// (and drops the control file) when the stamp does not land, so an unowned
/// shell never lingers.
fn register_viewer_pane(
    new_pane: &str,
    control: &Path,
    doc_key: &str,
    inline: bool,
) -> Result<(), String> {
    let mut tokens = serde_json::json!({
        METADATA_SOURCE: crate::state::unix_now().to_string(),
        TOKEN_PATH: document_token(doc_key),
        TOKEN_CONTROL: control_token(control),
    });
    if inline {
        tokens[TOKEN_INLINE] = serde_json::Value::String("1".into());
    }
    if !ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": new_pane,
            "source": METADATA_SOURCE,
            "tokens": tokens,
        }),
    )
    .is_ok_and(|response| ipc_succeeded(&response))
    {
        cleanup_spawn(new_pane, control);
        return Err("preview pane could not be identified".into());
    }
    let _ = ipc::call_text(
        "pane.rename",
        serde_json::json!({ "pane_id": new_pane, "label": "Preview" }),
    );
    Ok(())
}

fn start_viewer_pane(pane_id: &str) -> bool {
    let command = format!("{} --preview", crate::state::EXECUTABLE_NAME);
    ipc::call_text(
        "pane.send_input",
        serde_json::json!({ "pane_id": pane_id, "text": command, "keys": ["Enter"] }),
    )
    .is_ok_and(|response| ipc_succeeded(&response))
}

fn mark_dedicated_preview(pane_id: &str) -> bool {
    ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": METADATA_SOURCE,
            "tokens": { TOKEN_DEDICATED: "1" },
        }),
    )
    .is_ok_and(|response| ipc_succeeded(&response))
}

fn cleanup_spawn(pane_id: &str, control: &Path) {
    remove_control_file(control);
    let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id }));
}

fn cleanup_moved_spawn(pane_id: &str, tab_id: &str, control: &Path) {
    remove_control_file(control);
    let plugin_only = ipc::call_text("pane.list", serde_json::json!({}))
        .ok()
        .is_some_and(|list| tab_is_plugin_only(&list, tab_id));
    if plugin_only {
        let _ = ipc::call_text("tab.close", serde_json::json!({ "tab_id": tab_id }));
    } else {
        let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id }));
    }
}

fn ipc_succeeded(response: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(crate::launch::strip_bom(response))
        .ok()
        .is_some_and(|value| value.get("result").is_some() && value.get("error").is_none())
}

fn pane_close_succeeded(response: std::io::Result<String>) -> bool {
    response.is_ok_and(|response| ipc_succeeded(&response))
}

fn preview_spawn_env(control: &Path, inline: bool) -> serde_json::Value {
    let mut env = crate::state::spawn_env();
    env[crate::state::PREVIEW_CONTROL_ENV] =
        serde_json::Value::String(control.display().to_string());
    if inline {
        env[crate::state::PREVIEW_INLINE_ENV] = serde_json::Value::String("1".into());
    }
    env
}

/// Where an inline viewer pane goes: on the side of the sidebar AWAY from its
/// dock edge, so the sidebar stays on its edge and the layout reads
/// `sidebar | preview | rest` (or the mirror image when docked right).
///
/// Splitting only goes right, so the two cases differ:
/// - a neighbour on that side is split in half and the fresh pane takes the
///   slot nearest us (a right dock needs no swap: splitting the LEFT
///   neighbour already lands the new pane between it and the sidebar);
/// - with no neighbour we split ourselves and give away everything past the
///   sidebar's column target, swapping only when the sidebar must end up on
///   the right.
///
/// `None` when the layout can't be read — the caller falls back to a plain
/// self-split.
fn inline_split_plan(layout_json: &str, pane_id: &str, inline: InlineSpawn) -> Option<SplitPlan> {
    if let Some(target) = side_neighbor(layout_json, pane_id, inline.dock_right) {
        return Some(SplitPlan {
            target,
            ratio: 0.5,
            swap: !inline.dock_right,
        });
    }
    let my_width = pane_width(layout_json, pane_id)?;
    let share = (f64::from(inline.sidebar_cols) / my_width).clamp(0.15, 0.5);
    Some(SplitPlan {
        target: pane_id.to_string(),
        // `ratio` is the ORIGINAL pane's share, and a swap moves us into the
        // other slot — so keeping the sidebar's share means asking for its
        // complement when we are about to swap.
        ratio: if inline.dock_right {
            1.0 - share
        } else {
            share
        },
        swap: inline.dock_right,
    })
}

#[derive(serde::Deserialize)]
struct LayoutMsg {
    result: LayoutRes,
}
#[derive(serde::Deserialize)]
struct LayoutRes {
    layout: LayoutTree,
}
#[derive(serde::Deserialize)]
struct LayoutTree {
    #[serde(default)]
    panes: Vec<LayoutPane>,
}
#[derive(serde::Deserialize)]
struct LayoutPane {
    pane_id: Option<String>,
    rect: Option<LayoutRect>,
}
#[derive(serde::Deserialize)]
struct LayoutRect {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

fn layout_panes(layout_json: &str) -> Option<Vec<LayoutPane>> {
    serde_json::from_str::<LayoutMsg>(layout_json.trim_start_matches('\u{feff}'))
        .ok()
        .map(|msg| msg.result.layout.panes)
}

/// Width of `pane_id`'s rect from a `pane.layout` response; `None` if it is
/// missing or degenerate.
fn pane_width(layout_json: &str, pane_id: &str) -> Option<f64> {
    let panes = layout_panes(layout_json)?;
    let rect = panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    (rect.width > 0).then_some(rect.width as f64)
}

/// The pane directly beside `pane_id` (sharing vertical overlap), from a
/// `pane.layout` response — to its left when `on_the_left`, else its right.
fn side_neighbor(layout_json: &str, pane_id: &str, on_the_left: bool) -> Option<String> {
    let panes = layout_panes(layout_json)?;
    let me = panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    let (my_top, my_bottom) = (me.y, me.y + me.height);
    panes
        .iter()
        .filter(|p| p.pane_id.as_deref() != Some(pane_id))
        .filter_map(|p| Some((p.pane_id.clone()?, p.rect.as_ref()?)))
        .find(|(_, r)| {
            let touches = if on_the_left {
                r.x + r.width == me.x
            } else {
                r.x == me.x + me.width
            };
            touches && r.y < my_bottom && r.y + r.height > my_top
        })
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_of(lines: Vec<Line<'static>>, numbered: bool) -> Doc {
        Doc {
            name: "t".into(),
            context: String::new(),
            lines,
            numbered,
            media: None,
            scroll: 0,
            wrap: true,
            glow_width: None,
            rows: Vec::new(),
            rows_key: None,
            pending_src: None,
            selection: PreviewSelection::default(),
            stamp: None,
        }
    }

    fn row_texts(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| r.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn raster_media_uses_two_truecolor_pixels_per_cell() {
        let mut pixels = image::RgbaImage::new(2, 2);
        for x in 0..2 {
            pixels.put_pixel(x, 0, image::Rgba([255, 0, 0, 255]));
            pixels.put_pixel(x, 1, image::Rgba([0, 0, 255, 255]));
        }
        let rows = render_media_rows(
            &MediaPreview {
                pixels,
                source_width: 2,
                source_height: 2,
                video_poster: false,
            },
            2,
            1,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(row_texts(&rows), vec!["▀▀"]);
        for span in &rows[0].line.spans {
            assert_eq!(span.style.fg, Some(Color::Rgb(255, 0, 0)));
            assert_eq!(span.style.bg, Some(Color::Rgb(0, 0, 255)));
        }
    }

    #[test]
    fn media_extensions_are_case_insensitive_and_disjoint() {
        assert!(is_image_file(Path::new("photo.JPEG")));
        assert!(is_video_file(Path::new("clip.MP4")));
        assert!(!is_video_file(Path::new("photo.png")));
        assert!(!is_image_file(Path::new("notes.txt")));
    }

    #[test]
    fn image_files_decode_into_media_and_oversized_inputs_stop_before_decode() {
        let root = std::env::temp_dir().join(format!("viewer-media-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let image_path = root.join("sample.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([12, 34, 56, 255]))
            .save_with_format(&image_path, image::ImageFormat::Png)
            .unwrap();
        let doc = load_file(&image_path, None);
        assert!(doc.media.is_some());
        assert!(doc.lines.is_empty());

        let oversized = root.join("oversized.png");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_MEDIA_FILE_BYTES + 1)
            .unwrap();
        assert!(
            decode_image_file(&oversized)
                .unwrap_err()
                .contains("MiB limit")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ffmpeg_lookup_skips_project_local_path_entries() {
        let root = std::env::temp_dir().join(format!("viewer-ffmpeg-{}", std::process::id()));
        let project = root.join("project");
        let local_bin = project.join("tools");
        let external_bin = root.join("external");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&local_bin).unwrap();
        std::fs::create_dir_all(&external_bin).unwrap();
        let executable = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        for directory in [&local_bin, &external_bin] {
            let file = directory.join(executable);
            std::fs::write(&file, b"placeholder").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let search_path = std::env::join_paths([&local_bin, &external_bin]).unwrap();
        let project = project.canonicalize().unwrap();
        assert_eq!(
            ffmpeg_in_path(&search_path, Some(&project)),
            Some(external_bin.canonicalize().unwrap().join(executable))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wrapping_makes_every_continuation_row_scrollable() {
        // One source line, four rows' worth of text in a 10-wide pane.
        let long = "alpha beta gamma delta epsilon zeta";
        let mut doc = doc_of(vec![Line::raw(long), Line::raw("tail")], false);
        doc.relayout(10, 20);
        assert!(doc.rows.len() > 2, "the long line must occupy several rows");
        // Every row belongs to a source line, in order, and the last row is
        // reachable by scrolling — which the pre-fix source-line scroll
        // (capped at lines.len() - 1 = 1) could never do.
        assert_eq!(doc.rows.last().map(|r| r.src), Some(1));
        assert_eq!(
            row_texts(&doc.rows).concat().replace(' ', ""),
            format!("{long}tail").replace(' ', "")
        );
        let max = doc.rows.len() - 1;
        assert!(max > 1);
    }

    #[test]
    fn wrap_off_keeps_one_row_per_source_line() {
        let mut doc = doc_of(vec![Line::raw("a".repeat(120)), Line::raw("b")], false);
        doc.wrap = false;
        doc.relayout(20, 20);
        assert_eq!(doc.rows.len(), 2);
        assert_eq!(
            doc.rows.iter().map(|r| r.src).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn the_gutter_is_blank_on_continuation_rows() {
        let mut doc = doc_of(
            vec![Line::raw("one two three four five six"), Line::raw("x")],
            true,
        );
        doc.relayout(14, 20);
        let texts = row_texts(&doc.rows);
        assert!(texts[0].starts_with("1 "), "{texts:?}");
        // A continuation indents to the number column instead of renumbering.
        assert!(texts[1].starts_with("  "), "{texts:?}");
        assert_eq!(doc.rows[1].src, 0);
        // The next source line gets its own number.
        let second = doc.rows.iter().position(|r| r.src == 1).unwrap();
        assert!(texts[second].starts_with("2 "), "{texts:?}");
    }

    #[test]
    fn preview_selection_copies_text_without_line_numbers() {
        let mut doc = doc_of(vec![Line::raw("alpha beta"), Line::raw("gamma")], true);
        doc.relayout(40, 20);
        doc.selection.anchor = Some(RenderPos { row: 0, col: 1 });
        doc.selection.cursor = Some(RenderPos { row: 1, col: 2 });
        assert_eq!(doc.selected_text().as_deref(), Some("lpha beta\nga"));
    }

    #[test]
    fn copy_on_release_follows_herdrs_copy_on_select_setting() {
        assert!(copy_on_select_from(""), "herdr's default is on");
        assert!(copy_on_select_from("[ui]\n# copy_on_select = false\n"));
        assert!(!copy_on_select_from("[ui]\ncopy_on_select = false # off\n"));
        assert!(copy_on_select_from("[ui]\ncopy_on_select = true\n"));
        assert!(
            copy_on_select_from("[other]\ncopy_on_select = false\n[ui]\nmouse_capture = true\n"),
            "only the [ui] table counts"
        );
        // Astra #5: every TOML spelling herdr accepts.
        assert!(!copy_on_select_from("[ ui ]\ncopy_on_select = false\n"));
        assert!(!copy_on_select_from("[\"ui\"]\ncopy_on_select = false\n"));
        assert!(!copy_on_select_from("ui.copy_on_select = false\n"));
        assert!(!copy_on_select_from("ui = { copy_on_select = false }\n"));
        assert!(!copy_on_select_from(
            "\u{feff}[ui]\ncopy_on_select = false\n"
        ));
        assert!(
            !copy_on_select_from(
                "[keys]\nlist = [\n  \"[ui]\",\n]\n[ui]\ncopy_on_select = false\n"
            ),
            "array lines that look like headers are not tables"
        );
        assert!(
            copy_on_select_from("[ui\ncopy_on_select = false\n"),
            "invalid TOML: herdr defaults"
        );
    }

    /// Astra round 2 #6: herdr's live loader keeps its current config when the
    /// file turns unreadable or invalid mid-edit; so does the viewer. A
    /// missing file means defaults, exactly as in herdr.
    #[test]
    fn a_broken_config_reload_keeps_the_last_good_setting() {
        let mut last_good = None;
        let off = Some(Ok("[ui]\ncopy_on_select = false\n".to_string()));
        assert!(!copy_on_select_now(off, &mut last_good));
        let wrong_type = Some(Ok("[ui]\ncopy_on_select = \"false\"\n".to_string()));
        assert!(
            !copy_on_select_now(wrong_type, &mut last_good),
            "schema error keeps last good"
        );
        let half_edited = Some(Ok("[ui\ncopy_on".to_string()));
        assert!(!copy_on_select_now(half_edited, &mut last_good));
        let locked = Some(Err(std::io::Error::other("sharing violation")));
        assert!(!copy_on_select_now(locked, &mut last_good));
        let missing = Some(Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
        assert!(
            copy_on_select_now(missing, &mut last_good),
            "no file: defaults"
        );
        assert!(
            copy_on_select_now(None, &mut None),
            "no config location: defaults"
        );
        let mut fresh = None;
        assert!(
            copy_on_select_now(Some(Ok("[ui".to_string())), &mut fresh),
            "an invalid file at first load: defaults"
        );
    }

    /// Astra round 2 #2/#4: copies stay ordered across kinds, and pending
    /// automatic copies coalesce so a stalled clipboard cannot grow a backlog.
    #[test]
    fn copy_queue_orders_explicit_copies_and_coalesces_automatic_ones() {
        let auto = |text: &str| CopyRequest {
            text: text.into(),
            automatic: true,
        };
        let explicit = |text: &str| CopyRequest {
            text: text.into(),
            automatic: false,
        };
        let mut queue = std::collections::VecDeque::new();
        for drag in 0..1000 {
            enqueue_copy(&mut queue, auto(&format!("drag {drag}")));
        }
        assert_eq!(queue, [auto("drag 999")], "only the latest drag waits");
        enqueue_copy(&mut queue, explicit("ctrl-c"));
        assert_eq!(queue, [auto("drag 999"), explicit("ctrl-c")]);
        enqueue_copy(&mut queue, auto("later drag"));
        assert_eq!(
            queue,
            [explicit("ctrl-c"), auto("later drag")],
            "the explicit copy stays ahead of the newer drag and is never dropped"
        );
    }

    #[test]
    fn refresh_workers_are_capped() {
        let held: Vec<_> = std::iter::from_fn(InFlight::claim).take(10).collect();
        assert_eq!(held.len(), MAX_REFRESH_IN_FLIGHT);
        assert!(InFlight::claim().is_none());
        drop(held);
        assert!(
            InFlight::claim().is_some(),
            "finished workers free their slot"
        );
    }

    #[test]
    fn release_copies_route_without_touching_the_clipboard_when_off() {
        let never = |_: &str| -> std::io::Result<()> { panic!("must not run the clipboard tool") };
        assert_eq!(
            release_copy("t".into(), false, false, never, true),
            ReleaseCopy::Disabled
        );
        assert_eq!(
            release_copy("t".into(), true, true, never, true),
            ReleaseCopy::Osc52("t".into())
        );
        assert_eq!(
            release_copy("t".into(), true, false, |_| Ok(()), false),
            ReleaseCopy::Copied
        );
        let broken = |_: &str| -> std::io::Result<()> { Err(std::io::Error::other("no tool")) };
        assert_eq!(
            release_copy("t".into(), true, false, broken, true),
            ReleaseCopy::Osc52("t".into())
        );
        assert!(matches!(
            release_copy("t".into(), true, false, broken, false),
            ReleaseCopy::Failed(_)
        ));
        assert_eq!(release_copy_notice(ReleaseCopy::Disabled), None);
    }

    #[test]
    fn only_a_mouse_release_that_selects_text_asks_for_a_copy() {
        let mut doc = doc_of(vec![Line::raw("alpha beta")], false);
        doc.relayout(40, 20);
        let body = Rect::new(0, 0, 40, 20);
        let at = |kind, column| MouseEvent {
            kind,
            column,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(!doc.on_mouse(&at(MouseEventKind::Down(MouseButton::Left), 2), body));
        assert!(!doc.on_mouse(&at(MouseEventKind::Up(MouseButton::Left), 2), body));
        assert!(!doc.on_mouse(&at(MouseEventKind::Down(MouseButton::Left), 0), body));
        assert!(!doc.on_mouse(&at(MouseEventKind::Drag(MouseButton::Left), 5), body));
        assert!(doc.on_mouse(&at(MouseEventKind::Up(MouseButton::Left), 5), body));
        assert_eq!(doc.selected_text().as_deref(), Some("alpha"));
        assert!(!doc.on_mouse(&at(MouseEventKind::Up(MouseButton::Left), 5), body));
    }

    #[test]
    fn preview_selection_does_not_copy_visual_wrap_breaks() {
        let mut doc = doc_of(vec![Line::raw("alpha beta")], false);
        doc.relayout(6, 20);
        assert_eq!(doc.rows.len(), 2);
        let last_col = row_text_without_gutter(doc.rows.last().unwrap(), 0)
            .chars()
            .count();
        doc.selection.anchor = Some(RenderPos::default());
        doc.selection.cursor = Some(RenderPos {
            row: 1,
            col: last_col,
        });
        assert_eq!(doc.selected_text().as_deref(), Some("alpha beta"));
    }

    #[test]
    fn unchanged_diff_refresh_preserves_selection_and_layout() {
        let mut doc = doc_of(vec![Line::raw("-old"), Line::raw("+new")], false);
        doc.relayout(40, 20);
        doc.scroll = 1;
        doc.selection.anchor = Some(RenderPos { row: 0, col: 1 });
        doc.selection.cursor = Some(RenderPos { row: 1, col: 3 });
        let selection = doc.selection;
        let rows_key = doc.rows_key;

        let refreshed = doc_of(vec![Line::raw("-old"), Line::raw("+new")], false);
        apply_diff_refresh(&mut doc, refreshed);

        assert_eq!(doc.selection.anchor, selection.anchor);
        assert_eq!(doc.selection.cursor, selection.cursor);
        assert_eq!(doc.rows_key, rows_key);
        assert_eq!(doc.scroll, 1);
    }

    /// Astra #3: a change saved before the first poll must still reload. The
    /// baseline is the stamp taken when the shown content was read, not
    /// whatever the first poll happens to see.
    #[test]
    fn a_change_before_the_first_poll_still_reloads() {
        let path = std::env::temp_dir().join(format!("viewer-watch-{}", std::process::id()));
        std::fs::write(&path, "version A").unwrap();
        let request = Request::File {
            path: path.clone(),
            line: None,
        };
        let shown = load(&request);
        assert!(shown.stamp.is_some(), "file loads carry their stamp");
        assert!(
            !file_refresh_needed(&path, shown.stamp),
            "nothing changed yet"
        );
        std::fs::write(&path, "version B, saved before any poll").unwrap();
        assert!(
            file_refresh_needed(&path, shown.stamp),
            "the very first poll after the save must reload"
        );
        let mut doc = shown;
        let reloaded = load(&request);
        let new_stamp = reloaded.stamp;
        apply_diff_refresh(&mut doc, reloaded);
        assert_eq!(doc.stamp, new_stamp);
        assert!(
            !file_refresh_needed(&path, doc.stamp),
            "settles after reloading"
        );
        let _ = std::fs::remove_file(path);
    }

    /// An unchanged reload (a `touch`) adopts the new stamp, or the viewer
    /// would reload the same content every two seconds forever.
    #[test]
    fn an_identical_reload_adopts_the_new_stamp() {
        let mut doc = doc_of(vec![Line::raw("same")], false);
        let mut refreshed = doc_of(vec![Line::raw("same")], false);
        let stamp = Some((std::time::SystemTime::UNIX_EPOCH, 4));
        refreshed.stamp = stamp;
        apply_diff_refresh(&mut doc, refreshed);
        assert_eq!(doc.stamp, stamp);
    }

    /// Astra #4: a refresh started under an earlier load never applies, even
    /// when the document key matches again (A→B→A).
    #[test]
    fn stale_refreshes_are_rejected_by_generation() {
        let a = Request::File {
            path: "a.md".into(),
            line: None,
        };
        let b = Request::File {
            path: "b.md".into(),
            line: None,
        };
        assert!(refresh_is_current(7, &a, 7, Some(&a)));
        assert!(!refresh_is_current(7, &a, 7, Some(&b)), "another document");
        assert!(
            !refresh_is_current(7, &a, 9, Some(&a)),
            "A, then B, then A again"
        );
        assert!(!refresh_is_current(7, &a, 7, None));
    }

    #[test]
    fn build_rows_expands_tabs_in_wrapped_and_unwrapped_modes() {
        let line = Line::raw("\t1234");
        let wrapped = build_rows(std::slice::from_ref(&line), false, true, 4);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].line.spans[0].content.as_ref(), "    ");
        let unwrapped = build_rows(&[line], false, false, 4);
        let text: String = unwrapped[0]
            .line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "    1234");
        assert!(!text.contains('\t'));
    }

    #[test]
    fn tinted_diff_rows_stay_tinted_across_the_wrap() {
        use ratatui::style::Stylize;
        let mut doc = doc_of(
            vec![Line::raw("+ a long added line of code here").on_green()],
            false,
        );
        doc.relayout(12, 20);
        assert!(doc.rows.len() > 1);
        for row in &doc.rows {
            assert_eq!(row.line.style.bg, Some(Color::Green));
            // Padded to the pane edge so the tint is a full-width band.
            assert_eq!(row.line.width(), 12);
        }
    }

    #[test]
    fn toggling_wrap_holds_the_readers_place_by_source_line() {
        let lines: Vec<Line<'static>> = (0..6)
            .map(|n| Line::raw(format!("line {n} with a good deal of text on it")))
            .collect();
        let mut doc = doc_of(lines, false);
        doc.relayout(12, 20);
        // Scroll to the first row of source line 3.
        doc.scroll = doc.rows.iter().position(|r| r.src == 3).unwrap();
        assert_eq!(doc.top_src(), 3);

        doc.pending_src = Some(doc.top_src());
        doc.wrap = false;
        doc.relayout(12, 20);
        assert_eq!(doc.scroll, 3, "unwrapped rows are 1:1 with source lines");
        assert_eq!(doc.top_src(), 3);

        doc.pending_src = Some(doc.top_src());
        doc.wrap = true;
        doc.relayout(12, 20);
        assert_eq!(doc.top_src(), 3, "and back, still on the same source line");
    }

    /// End-to-end through the real draw: the pane is 6 rows tall (1 header,
    /// 4 body, 1 footer) and the doc is a single line four rows long when
    /// wrapped. Scrolling must walk it row by row and reach the tail — the
    /// pre-fix source-line scroll had a max of 0 here, so everything past
    /// the first screen was unreachable.
    #[test]
    fn the_rendered_pane_scrolls_through_a_wrapped_line() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let words = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        let mut doc = doc_of(vec![Line::raw(words)], false);
        let mut terminal = Terminal::new(TestBackend::new(16, 6)).unwrap();

        let render = |terminal: &mut Terminal<TestBackend>, doc: &mut Doc| -> Vec<String> {
            terminal
                .draw(|f| {
                    draw_doc(f, doc, IconTheme::Emoji, false, None);
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .chunks(16)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect()
        };

        let first = render(&mut terminal, &mut doc);
        assert!(doc.rows.len() > 4, "16 cols must need more than one screen");
        // Body rows are 1..5; the header and footer are chrome.
        assert!(first[1].starts_with("alpha"), "{first:?}");

        // Walk to the bottom the way the key handler does.
        let max = doc.rows.len() - 1;
        doc.scroll = max;
        let last = render(&mut terminal, &mut doc);
        let body: String = last[1..5].concat();
        assert!(
            body.contains("juliet"),
            "the final row must be reachable: {last:?}"
        );

        // …and the clamp keeps a full screen of content in view rather than
        // scrolling off into blank rows.
        assert!(doc.scroll <= doc.rows.len().saturating_sub(4));
    }

    /// With wrapping off the same doc is one clipped row, and the toggle
    /// round-trips without losing the reader's place.
    #[test]
    fn the_rendered_pane_clips_when_wrapping_is_off() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let words = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        let mut doc = doc_of(vec![Line::raw(words)], false);
        doc.wrap = false;
        let mut terminal = Terminal::new(TestBackend::new(16, 6)).unwrap();
        terminal
            .draw(|f| {
                draw_doc(f, &mut doc, IconTheme::Emoji, false, None);
            })
            .unwrap();
        assert_eq!(doc.rows.len(), 1);
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .chunks(16)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()[1]
            .clone();
        assert!(rendered.starts_with("alpha bravo char"), "{rendered:?}");
        assert!(!rendered.contains("juliet"));
    }

    #[test]
    fn a_resize_relayouts_and_keeps_the_rows_cached_otherwise() {
        let mut doc = doc_of(
            vec![Line::raw("wrap me around a narrow pane please")],
            false,
        );
        doc.relayout(10, 20);
        let narrow = doc.rows.len();
        doc.relayout(10, 20);
        assert_eq!(doc.rows.len(), narrow, "same key: no rebuild, no change");
        doc.relayout(40, 20);
        assert!(doc.rows.len() < narrow, "a wider pane needs fewer rows");
    }

    /// One pinned preview, one ephemeral preview, and a sidebar pane that is
    /// neither — the three cases every routing decision must separate.
    const PREVIEWS: &str = r#"{"result":{"panes":[
        {"pane_id":"w4:p1","tab_id":"w4:t1","tokens":{"herdr-sidebar-explorer":"1"}},
        {"pane_id":"w4:p2","tab_id":"w4:t2",
          "tokens":{"herdr-sidebar-preview":"9999999999","hs-preview-path":"/r/a.rs","hs-preview-pinned":"1","hs-preview-dedicated":"1","hs-preview-origin-tab":"w4:t1","hs-preview-control":"C:/state/a preview.ctl"}},
        {"pane_id":"w4:p3","tab_id":"w4:t3",
          "tokens":{"herdr-sidebar-preview":"9999999999","hs-preview-path":"/r/b.rs"}}
    ]}}"#;

    #[test]
    fn a_pinned_tab_is_no_longer_reusable() {
        let before = r#"{"result":{"panes":[
            {"pane_id":"w4:p3","tab_id":"w4:t3","tokens":{"herdr-sidebar-preview":"9999999999","hs-preview-path":"/r/b.rs"}}
        ]}}"#;
        let after = r#"{"result":{"panes":[
            {"pane_id":"w4:p3","tab_id":"w4:t3",
             "tokens":{"herdr-sidebar-preview":"9999999999","hs-preview-path":"/r/b.rs","hs-preview-pinned":"1"}}
        ]}}"#;
        assert!(reusable_preview(&previews_in(before), false).is_some());
        assert!(
            reusable_preview(&previews_in(after), false).is_none(),
            "pinning must push the next file onto a new tab"
        );
        // ...and the pinned tab is still reachable by its document.
        assert!(preview_for_doc(&previews_in(after), "/r/b.rs").is_some());
    }

    #[test]
    fn legacy_control_files_are_addressed_by_the_preview_pane() {
        assert_eq!(
            control_path_for_pane("w4:p9"),
            control_path_for_pane("w4:p9")
        );
        assert_ne!(
            control_path_for_pane("w4:p9"),
            control_path_for_pane("w4:pA")
        );
        assert!(
            control_path_for_pane("w4:p9")
                .to_string_lossy()
                .contains("w4_p9"),
            "colons are not filename-safe"
        );
    }

    #[test]
    fn preview_control_path_travels_in_the_spawn_environment() {
        let control = Path::new("C:/plugin state/preview control.ctl");
        let env = preview_spawn_env(control, false);
        assert_eq!(
            env.get(crate::state::PREVIEW_CONTROL_ENV)
                .and_then(|value| value.as_str()),
            Some("C:/plugin state/preview control.ctl")
        );
        assert!(env.get("PATH").and_then(|value| value.as_str()).is_some());
    }

    #[test]
    fn metadata_uses_compact_tokens_that_survive_host_limits() {
        let control = fresh_control_path();
        let token = control_token(&control);
        assert!(token.len() < 64, "{token}");
        assert_eq!(control_from_token(&token), control);
        assert_eq!(document_token(&"x".repeat(500)).len(), 16);
    }

    #[test]
    fn whole_tab_close_requires_only_plugin_owned_panes() {
        let plugin_only = r#"{"result":{"panes":[
            {"tab_id":"w1:t2","label":"Sidebar","tokens":{"herdr-sidebar-explorer":"1"}},
            {"tab_id":"w1:t2","label":"a.rs · preview","tokens":{"herdr-sidebar-preview":"1"}}
        ]}}"#;
        let with_shell = r#"{"result":{"panes":[
            {"tab_id":"w1:t2","label":"Sidebar","tokens":{"herdr-sidebar-explorer":"1"}},
            {"tab_id":"w1:t2","label":"a.rs · preview","tokens":{"herdr-sidebar-preview":"1"}},
            {"tab_id":"w1:t2","label":"shell","tokens":{}}
        ]}}"#;
        let resumed = r#"{"result":{"panes":[
            {"tab_id":"w1:t2","label":"Sidebar","tokens":{}},
            {"tab_id":"w1:t2","label":"a.rs · editor","tokens":{}}
        ]}}"#;
        let resumed_with_shell = r#"{"result":{"panes":[
            {"tab_id":"w1:t2","label":"Sidebar","tokens":{}},
            {"tab_id":"w1:t2","label":"a.rs · preview","tokens":{}},
            {"tab_id":"w1:t2","label":"pwsh","tokens":{}}
        ]}}"#;
        assert!(tab_is_plugin_only(plugin_only, "w1:t2"));
        assert!(!tab_is_plugin_only(with_shell, "w1:t2"));
        assert!(tab_is_plugin_only(resumed, "w1:t2"));
        assert!(!tab_is_plugin_only(resumed_with_shell, "w1:t2"));
        assert!(!tab_is_plugin_only(plugin_only, "w1:t9"));
    }

    /// The ephemeral tab is per WORKSPACE. Session-wide, opening a file in
    /// tremor found learnings' unpinned preview, rewrote it, and focus jumped
    /// to the other project — the tree "stayed" on learnings because you were
    /// teleported there.
    #[test]
    fn the_ephemeral_tab_is_not_shared_between_workspaces() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"wB:pE","tab_id":"wB:t3","workspace_id":"wB",
             "tokens":{"herdr-sidebar-preview":"9999999999","hs-preview-path":"/learnings/a.md"}},
            {"pane_id":"wH:p9","tab_id":"wH:t4","workspace_id":"wH",
             "tokens":{"herdr-sidebar-preview":"9999999999","hs-preview-path":"/tremor/b.rs","hs-preview-pinned":"1"}}
        ]}}"#;
        let all = previews_in(json);
        assert_eq!(all.len(), 2);

        let tremor: Vec<_> = all
            .iter()
            .filter(|p| p.workspace_id == "wH")
            .cloned()
            .collect();
        assert!(
            reusable_preview(&tremor, false).is_none(),
            "learnings' ephemeral tab must not be reusable from tremor"
        );
        let learnings: Vec<_> = all
            .iter()
            .filter(|p| p.workspace_id == "wB")
            .cloned()
            .collect();
        assert_eq!(
            reusable_preview(&learnings, false).unwrap().pane_id,
            "wB:pE"
        );

        // Matching an already-open document is scoped too: jumping to another
        // workspace's tab is the same teleport by a different route.
        assert!(preview_for_doc(&tremor, "/learnings/a.md").is_none());
    }

    #[test]
    fn previews_carry_their_document_and_pin_state() {
        let ps = previews_in(PREVIEWS);
        assert_eq!(ps.len(), 2, "sidebar panes are not preview candidates");
        let a = ps
            .iter()
            .find(|p| p.doc_token == document_token("/r/a.rs"))
            .unwrap();
        assert!(a.pinned);
        assert!(a.dedicated);
        assert!(!a.resumed);
        assert_eq!(a.tab_id, "w4:t2");
        assert_eq!(a.origin_tab_id, "w4:t1");
        assert_eq!(a.control, PathBuf::from("C:/state/a preview.ctl"));
        assert!(
            !ps.iter()
                .find(|p| p.doc_token == document_token("/r/b.rs"))
                .unwrap()
                .pinned
        );
    }

    #[test]
    fn an_open_document_is_matched_before_anything_is_reused() {
        let ps = previews_in(PREVIEWS);
        assert_eq!(preview_for_doc(&ps, "/r/a.rs").unwrap().tab_id, "w4:t2");
        assert!(preview_for_doc(&ps, "/r/zz.rs").is_none());
    }

    #[test]
    fn only_unpinned_previews_are_reusable() {
        let ps = previews_in(PREVIEWS);
        assert_eq!(
            reusable_preview(&ps, false).unwrap().doc_token,
            document_token("/r/b.rs")
        );

        let all_pinned = r#"{"result":{"panes":[
            {"pane_id":"w4:p2","tab_id":"w4:t2",
             "tokens":{"herdr-sidebar-preview":"9999999999","hs-preview-path":"/r/a.rs","hs-preview-pinned":"1"}}
        ]}}"#;
        assert!(
            reusable_preview(&previews_in(all_pinned), false).is_none(),
            "every tab pinned must force a new tab"
        );
    }

    #[test]
    fn ephemeral_previews_preserve_the_original_return_tab() {
        let mut previews = previews_in(PREVIEWS);
        let ephemeral = previews.iter_mut().find(|preview| !preview.pinned).unwrap();
        ephemeral.origin_tab_id = "w4:t1".into();
        assert_eq!(preview_origin_tab(&previews, "w4:t3"), "w4:t1");
        assert_eq!(preview_origin_tab(&previews, "w4:t2"), "w4:t2");
        assert_eq!(preview_origin_tab(&previews, "w4:t9"), "w4:t9");
    }

    #[test]
    fn pinning_requires_the_viewer_to_acknowledge_that_document() {
        let previews = previews_in(PREVIEWS);
        let target = PreviewTarget {
            pane_id: "w4:p3".into(),
            tab_id: "w4:t3".into(),
            origin_tab_id: "w4:t1".into(),
            inline: false,
        };
        assert!(target_is_showing(&previews, &target, "/r/b.rs"));
        assert!(!target_is_showing(&previews, &target, "/r/other.rs"));
    }

    /// `sidebar | rest`, and the same tab mirrored for a right dock.
    fn layout(panes: &str) -> String {
        format!(r#"{{"result":{{"layout":{{"panes":[{panes}]}}}}}}"#)
    }

    const SIDEBAR_LEFT: &str = r#"
        {"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":32,"height":50}},
        {"pane_id":"w1:p2","rect":{"x":32,"y":0,"width":148,"height":50}}"#;
    const SIDEBAR_RIGHT: &str = r#"
        {"pane_id":"w1:p2","rect":{"x":0,"y":0,"width":148,"height":50}},
        {"pane_id":"w1:p1","rect":{"x":148,"y":0,"width":32,"height":50}}"#;

    /// The inline viewer goes between the sidebar and the user's panes, on
    /// whichever side is away from the dock edge — the sidebar must not be
    /// pushed off its own edge.
    #[test]
    fn an_inline_preview_splits_the_neighbour_away_from_the_dock_edge() {
        let left = inline_split_plan(
            &layout(SIDEBAR_LEFT),
            "w1:p1",
            InlineSpawn {
                dock_right: false,
                sidebar_cols: 32,
            },
        )
        .unwrap();
        // Split only goes right, so the neighbour is halved and the fresh
        // pane swapped into the half nearest us.
        assert_eq!(
            left,
            SplitPlan {
                target: "w1:p2".into(),
                ratio: 0.5,
                swap: true
            }
        );

        let right = inline_split_plan(
            &layout(SIDEBAR_RIGHT),
            "w1:p1",
            InlineSpawn {
                dock_right: true,
                sidebar_cols: 32,
            },
        )
        .unwrap();
        // Splitting the LEFT neighbour rightwards already lands the new pane
        // between it and the sidebar — no swap needed.
        assert_eq!(
            right,
            SplitPlan {
                target: "w1:p2".into(),
                ratio: 0.5,
                swap: false
            }
        );
    }

    /// A tab whose only pane is the sidebar: we give away everything past our
    /// own column target instead of an arbitrary half.
    #[test]
    fn an_inline_preview_alone_in_a_tab_keeps_the_sidebar_column_target() {
        let alone = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":160,"height":50}}"#);
        let left = inline_split_plan(
            &alone,
            "w1:p1",
            InlineSpawn {
                dock_right: false,
                sidebar_cols: 32,
            },
        )
        .unwrap();
        assert_eq!(left.target, "w1:p1");
        assert!(!left.swap);
        assert!((left.ratio - 0.2).abs() < 1e-9, "{}", left.ratio);

        // Docked right we end up in the far slot, so the ratio (the ORIGINAL
        // pane's share) is the complement of the share we want to keep.
        let right = inline_split_plan(
            &alone,
            "w1:p1",
            InlineSpawn {
                dock_right: true,
                sidebar_cols: 32,
            },
        )
        .unwrap();
        assert!(right.swap);
        assert!((right.ratio - 0.8).abs() < 1e-9, "{}", right.ratio);

        // herdr clamps split ratios anyway; never ask for something absurd.
        let narrow = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":40,"height":50}}"#);
        let clamped = inline_split_plan(
            &narrow,
            "w1:p1",
            InlineSpawn {
                dock_right: false,
                sidebar_cols: 80,
            },
        )
        .unwrap();
        assert!((clamped.ratio - 0.5).abs() < 1e-9, "{}", clamped.ratio);
    }

    #[test]
    fn fallback_plan_mirrors_a_right_docked_sidebar() {
        let left = fallback_inline_split_plan(
            "w1:p1",
            InlineSpawn {
                dock_right: false,
                sidebar_cols: 32,
            },
        );
        assert_eq!(
            left,
            SplitPlan {
                target: "w1:p1".into(),
                ratio: 0.3,
                swap: false
            }
        );

        let right = fallback_inline_split_plan(
            "w1:p1",
            InlineSpawn {
                dock_right: true,
                sidebar_cols: 32,
            },
        );
        assert_eq!(
            right,
            SplitPlan {
                target: "w1:p1".into(),
                ratio: 0.7,
                swap: true
            }
        );
    }

    #[test]
    fn an_unreadable_layout_leaves_the_caller_to_fall_back() {
        assert!(
            inline_split_plan(
                "not json",
                "w1:p1",
                InlineSpawn {
                    dock_right: false,
                    sidebar_cols: 32
                }
            )
            .is_none()
        );
        // Zero-width rects would divide by nothing.
        let degenerate =
            layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":0,"height":50}}"#);
        assert!(
            inline_split_plan(
                &degenerate,
                "w1:p1",
                InlineSpawn {
                    dock_right: false,
                    sidebar_cols: 32
                }
            )
            .is_none()
        );
    }

    /// Inline previews stay reusable even once a dirty editor would have
    /// pinned them: there is no second tab for the next click to land in, so
    /// refusing would split the tab again on every file.
    #[test]
    fn inline_previews_are_reusable_regardless_of_pin_state() {
        let pinned = r#"{"result":{"panes":[
            {"pane_id":"w4:p2","tab_id":"w4:t1","workspace_id":"w4",
             "tokens":{"herdr-sidebar-preview":"9999999999","hs-preview-path":"/r/a.rs",
                       "hs-preview-pinned":"1","hs-preview-inline":"1"}}
        ]}}"#;
        let previews = previews_in(pinned);
        assert!(previews[0].inline);
        assert!(
            !previews[0].dedicated,
            "an inline viewer never owns its tab"
        );
        assert!(reusable_preview(&previews, true).is_some());
        assert!(reusable_preview(&previews, false).is_none());
    }

    #[test]
    fn a_tab_mode_preview_is_not_inline() {
        assert!(!previews_in(PREVIEWS)[0].inline);
    }

    #[test]
    fn the_inline_flag_travels_in_the_spawn_environment() {
        let control = Path::new("C:/plugin state/preview control.ctl");
        assert!(
            preview_spawn_env(control, false)
                .get(crate::state::PREVIEW_INLINE_ENV)
                .is_none()
        );
        assert_eq!(
            preview_spawn_env(control, true)
                .get(crate::state::PREVIEW_INLINE_ENV)
                .and_then(|value| value.as_str()),
            Some("1")
        );
    }

    /// Pinning is a tab-bar concept. Inline placement has no tab of its own,
    /// so a double click must report success rather than warn about a
    /// confirmation that could never arrive.
    #[test]
    fn pinning_an_inline_target_is_a_silent_no_op() {
        assert!(pin_target(
            &PreviewTarget {
                pane_id: "w4:p3".into(),
                tab_id: "w4:t3".into(),
                origin_tab_id: "w4:t3".into(),
                inline: true,
            },
            "/r/b.rs"
        ));
    }

    #[test]
    fn a_nul_after_the_old_probe_window_is_still_binary() {
        let path = std::env::temp_dir().join(format!("viewer-late-nul-{}", std::process::id()));
        let mut bytes = vec![b'a'; 9000];
        bytes.push(0);
        std::fs::write(&path, bytes).unwrap();
        let doc = load_file(&path, None);
        let rendered: String = doc.lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.contains("binary file"), "{rendered}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stale_preview_heartbeats_are_neither_matched_nor_reused() {
        let stale = r#"{"result":{"panes":[{
            "pane_id":"w4:p3","tab_id":"w4:t3","workspace_id":"w4",
            "tokens":{"herdr-sidebar-preview":"1","hs-preview-path":"/r/b.rs"}
        }]}}"#;
        let previews = previews_in(stale);
        assert!(previews[0].stale);
        assert!(preview_for_doc(&previews, "/r/b.rs").is_none());
        assert!(reusable_preview(&previews, false).is_none());
    }

    #[test]
    fn label_only_resumed_previews_are_stale_cleanup_candidates() {
        let resumed = r#"{"result":{"panes":[{
            "pane_id":"w4:p3","tab_id":"w4:t3","workspace_id":"w4",
            "label":"b.rs · preview","tokens":{}
        }]}}"#;
        let previews = previews_in(resumed);
        assert_eq!(previews.len(), 1);
        assert!(previews[0].stale);
        assert!(previews[0].resumed);
        assert!(previews[0].doc_token.is_empty());
        assert!(preview_for_doc(&previews, "/r/b.rs").is_none());
        assert!(reusable_preview(&previews, false).is_none());
    }

    #[test]
    fn document_metadata_without_a_heartbeat_is_stale() {
        let missing_heartbeat = r#"{"result":{"panes":[{
            "pane_id":"w4:p3","tab_id":"w4:t3","workspace_id":"w4",
            "label":"b.rs · preview","tokens":{"hs-preview-path":"/r/b.rs"}
        }]}}"#;
        let previews = previews_in(missing_heartbeat);
        assert_eq!(previews.len(), 1);
        assert!(previews[0].stale);
        assert!(previews[0].resumed);
    }

    #[test]
    fn doc_keys_separate_a_file_from_its_diff_and_its_history() {
        let f = doc_key_for_file(Path::new("/repo/src/main.rs"));
        let d = doc_key_for_diff(Path::new("/repo"), "src/main.rs", "staged");
        let w = doc_key_for_diff(Path::new("/repo"), "src/main.rs", "worktree");
        let s = doc_key_for_show(Path::new("/repo"), "HEAD~1", Some("src/main.rs"));
        assert_eq!(f, "/repo/src/main.rs");
        assert_ne!(f, d, "a file and its diff need their own tabs");
        assert_ne!(d, w, "staged and worktree diffs are different documents");
        assert_ne!(d, s, "a diff and a git-show are different documents");
        assert!(d.starts_with("diff:"), "{d}");
        assert!(s.starts_with("show:"), "{s}");
    }

    #[test]
    fn tab_labels_mark_the_ephemeral_tab_not_the_pinned_one() {
        // `*` warns "this one is about to be overwritten".
        assert_eq!(tab_label("/repo/src/main.rs", false), "main.rs · preview");
        assert_eq!(tab_label("/repo/src/main.rs", true), "main.rs");
        assert_eq!(
            parse_tab_label("main.rs · preview"),
            ("main.rs".to_string(), false)
        );
        assert_eq!(parse_tab_label("main.rs"), ("main.rs".to_string(), true));
        assert_eq!(
            tab_label("diff:/repo/src/main.rs:staged", false),
            "main.rs · preview"
        );
        assert_eq!(tab_label("show:/repo:abc123", true), "abc123");
        assert_eq!(tab_label("show:/repo:abc123:src/lib.rs", true), "lib.rs");
        assert_eq!(preview_pane_label("main.rs"), "main.rs · preview");
        assert_eq!(editor_pane_label("main.rs"), "main.rs · editor");
    }

    #[test]
    fn pane_close_failure_keeps_the_inline_viewer_running() {
        assert!(!pane_close_succeeded(Err(std::io::Error::other(
            "socket closed",
        ))));
        assert!(!pane_close_succeeded(Ok(
            r#"{"error":{"message":"nope"}}"#.into()
        )));
        assert!(pane_close_succeeded(Ok(r#"{"result":{}}"#.into())));
    }

    #[test]
    fn tab_create_yields_tab_and_root_pane_or_nothing() {
        use crate::launch::created_tab_root_pane;
        let created = r#"{"result":{"type":"tab_created","tab":{"tab_id":"w9:tX","label":"x · preview"},"root_pane":{"pane_id":"w9:p1E","tab_id":"w9:tX"}}}"#;
        assert_eq!(
            created_tab_root_pane(created),
            Some(("w9:tX".into(), "w9:p1E".into()))
        );
        // A tab without a pane id is unusable as a viewer; so is an error.
        let no_pane = r#"{"result":{"type":"tab_created","tab":{"tab_id":"w9:tX"}}}"#;
        assert_eq!(created_tab_root_pane(no_pane), None);
        assert_eq!(
            created_tab_root_pane(r#"{"error":{"message":"nope"}}"#),
            None
        );
        // Ids that could be mistaken for CLI flags are rejected like everywhere else.
        let flaggy = r#"{"result":{"tab":{"tab_id":"--tab"},"root_pane":{"pane_id":"w9:p1"}}}"#;
        assert_eq!(created_tab_root_pane(flaggy), None);
    }

    fn test_scratch(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-viewer-scratch-{label}-{}",
            std::process::id()
        ))
    }

    #[cfg(unix)]
    #[test]
    fn scratch_dir_is_private_to_the_owning_user() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = test_scratch("private");
        let _ = std::fs::remove_dir_all(&dir);
        crate::rundir::ensure_private(&dir).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o700,
            "scratch dir must not be group/world readable or writable"
        );
        assert_eq!(meta.uid(), unsafe { libc::geteuid() });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_scratch_file_refuses_to_follow_a_preexisting_symlink() {
        use std::os::unix::fs::symlink;
        let dir = test_scratch("symlink");
        let _ = std::fs::remove_dir_all(&dir);
        crate::rundir::ensure_private(&dir).unwrap();
        let victim = dir.join(format!("aa-victim-{}.txt", std::process::id()));
        let link = dir.join(format!("aa-link-{}.ctl", std::process::id()));
        std::fs::write(&victim, "original victim contents").unwrap();
        let _ = std::fs::remove_file(&link);
        symlink(&victim, &link).unwrap();

        write_scratch_file_in(&link, "payload", &dir).unwrap();

        // The symlink must have been replaced by a real file, and the
        // victim it used to point at must be untouched.
        assert!(
            !std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "payload");
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "original victim contents"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Every control-file operation gates on the parent being a verified
    /// private directory, not just a path that looks like ours — a stale or
    /// hijacked parent must not be trusted just because it used to be ours.
    #[cfg(unix)]
    #[test]
    fn control_operations_refuse_a_path_whose_parent_is_not_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "herdr-viewer-untrusted-parent-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Group/world readable: not private by rundir's definition.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let control = dir.join("untrusted.ctl");

        assert!(write_scratch_file_in(&control, "payload", &dir).is_err());
        assert!(!control.exists(), "write must not have happened");

        std::fs::write(&control, "planted").unwrap();
        assert_eq!(read_control_in(&control, &dir), None);

        remove_control_file_in(&control, &dir);
        assert!(
            control.exists(),
            "delete must refuse to touch a file under an unverified parent"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A metadata-derived control path naming some OTHER directory we own
    /// privately (an attacker's own `~/.ssh`, say, if it happens to be 0700)
    /// must be refused just as firmly as a non-private one — `control_path_ok`
    /// requires confinement to `scratch_dir()` itself, not merely that
    /// *some* private directory is involved.
    #[cfg(unix)]
    #[test]
    fn control_operations_refuse_a_path_confined_to_a_different_private_directory() {
        let expected = test_scratch("expected-private-dir");
        let dir = test_scratch("other-private-dir");
        let _ = std::fs::remove_dir_all(&expected);
        let _ = std::fs::remove_dir_all(&dir);
        crate::rundir::ensure_private(&expected).unwrap();
        crate::rundir::ensure_private(&dir).unwrap();
        assert!(crate::rundir::is_private(&dir), "test setup sanity check");
        let control = dir.join("not-ours.ctl");

        assert!(write_scratch_file_in(&control, "payload", &expected).is_err());
        assert!(!control.exists(), "write must not have happened");

        std::fs::write(&control, "planted").unwrap();
        assert_eq!(read_control_in(&control, &expected), None);

        remove_control_file_in(&control, &expected);
        assert!(
            control.exists(),
            "delete must refuse to touch a file outside scratch_dir(), \
             even in a directory we privately own"
        );

        let _ = std::fs::remove_dir_all(expected);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The confinement checks in `control_path_ok` must not reject a
    /// legitimate path directly under `scratch_dir()` itself.
    #[cfg(unix)]
    #[test]
    fn control_operations_accept_a_path_directly_under_scratch_dir() {
        let dir = test_scratch("accepted");
        let _ = std::fs::remove_dir_all(&dir);
        crate::rundir::ensure_private(&dir).unwrap();
        let control = dir.join(format!("accepted-{}.ctl", std::process::id()));
        let _ = std::fs::remove_file(&control);

        write_scratch_file_in(&control, "close", &dir).unwrap();
        assert!(control.exists());
        assert_eq!(read_control_in(&control, &dir), Some(Request::Close));

        remove_control_file_in(&control, &dir);
        assert!(
            !control.exists(),
            "delete must succeed inside scratch_dir()"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn control_token_falls_back_to_a_verified_legacy_file() {
        let current = test_scratch("migration-current");
        let legacy = test_scratch("migration-legacy");
        let _ = std::fs::remove_dir_all(&current);
        let _ = std::fs::remove_dir_all(&legacy);
        crate::rundir::ensure_private(&current).unwrap();
        crate::rundir::ensure_private(&legacy).unwrap();
        std::fs::write(legacy.join("preview.ctl"), "close").unwrap();

        assert_eq!(
            control_from_token_in("preview.ctl", &current, &legacy),
            legacy.join("preview.ctl")
        );
        std::fs::write(current.join("preview.ctl"), "new").unwrap();
        assert_eq!(
            control_from_token_in("preview.ctl", &current, &legacy),
            current.join("preview.ctl")
        );

        let _ = std::fs::remove_dir_all(current);
        let _ = std::fs::remove_dir_all(legacy);
    }

    #[test]
    fn requests_roundtrip() {
        assert_eq!(parse_request("close"), Some(Request::Close));
        let f = file_request(Path::new("C:/x/y.rs"));
        assert_eq!(
            parse_request(&f),
            Some(Request::File {
                path: PathBuf::from("C:/x/y.rs"),
                line: None,
            })
        );
        let f = file_request_at(Path::new("C:/x/y.rs"), 42);
        assert_eq!(
            parse_request(&f),
            Some(Request::File {
                path: PathBuf::from("C:/x/y.rs"),
                line: Some(42),
            })
        );
        let s = show_request(Path::new("C:/repo"), "stash@{1}", None);
        assert_eq!(
            parse_request(&s),
            Some(Request::Show {
                root: PathBuf::from("C:/repo"),
                spec: "stash@{1}".into(),
                path: None,
            })
        );
        let s = show_request(Path::new("C:/repo"), "a1b2c3d", Some("src/a.rs"));
        assert_eq!(
            parse_request(&s),
            Some(Request::Show {
                root: PathBuf::from("C:/repo"),
                spec: "a1b2c3d".into(),
                path: Some("src/a.rs".into()),
            })
        );
        let d = diff_request(Path::new("C:/repo"), "src/a.rs", "staged");
        assert_eq!(
            parse_request(&d),
            Some(Request::Diff {
                root: PathBuf::from("C:/repo"),
                rel: "src/a.rs".into(),
                kind: "staged".into()
            })
        );
        // Legacy bare path still works.
        assert_eq!(
            parse_request("C:/plain.txt"),
            Some(Request::File {
                path: PathBuf::from("C:/plain.txt"),
                line: None,
            })
        );
        assert_eq!(parse_request("  "), None);
    }

    #[test]
    fn git_show_treats_a_dash_prefixed_ref_as_data_not_an_option() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-dash-ref-{}-{}",
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
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let head = String::from_utf8(head.stdout).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["update-ref", "refs/tags/--output=pwned.txt", head.trim()])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );

        let doc = load_show(&root, "--output=pwned.txt", None);
        assert!(!doc.lines.is_empty());
        assert!(
            !root.join("pwned.txt").exists(),
            "the ref name must never become a git-show option"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn line_target_anchors_plain_and_markdown_source_previews() {
        let root =
            std::env::temp_dir().join(format!("herdr-sidebar-line-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for name in ["sample.txt", "sample.md"] {
            let path = root.join(name);
            std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
            let doc = load_file(&path, Some(2));
            assert_eq!(doc.pending_src, Some(1));
            assert!(
                doc.numbered,
                "line-target markdown must keep source mapping"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires the optional glow executable"]
    fn glow_markdown_returns_styled_spans() {
        let md = "# Heading\n\n**bold** and `code`\n";
        let lines = glow_markdown(md, 80);
        assert!(lines.is_some(), "glow_markdown returned None");
        let lines = lines.unwrap();
        assert!(!lines.is_empty(), "glow_markdown returned empty lines");
        // At least one span must have a non-default style (proof that ANSI was parsed)
        let has_styled = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style != ratatui::style::Style::default())
        });
        assert!(
            has_styled,
            "glow_markdown returned no styled spans — ANSI not parsed"
        );
    }

    #[test]
    fn glow_rendered_markdown_uses_the_body_width_without_generic_rewrap() {
        let rendered = vec![Line::raw("preformatted table")];
        let fallback = vec![Line::raw("markdown source")];
        let (lines, numbered, glow_width) = choose_text_render(Some(rendered), || fallback, 80);
        let doc = Doc {
            name: "table.md".into(),
            context: String::new(),
            lines,
            numbered,
            media: None,
            scroll: 0,
            wrap: glow_width.is_none(),
            glow_width,
            rows: Vec::new(),
            rows_key: None,
            pending_src: None,
            selection: PreviewSelection::default(),
            stamp: None,
        };
        assert!(!doc.wrap, "glow output must not be generically re-wrapped");
        assert!(!doc.numbered, "glow formats its own layout, no gutter");
        assert_eq!(doc.glow_width, Some(80));
        assert!(!glow_needs_reload(&doc, 80));
        assert!(glow_needs_reload(&doc, 60));
    }

    #[test]
    fn glow_arguments_always_carry_theme_width_and_stdin_sentinel() {
        assert_eq!(
            glow_args(39, true),
            ["--style", "light", "--width", "39", "-"]
        );
        assert_eq!(
            glow_args(120, false),
            ["--style", "dark", "--width", "120", "-"]
        );
    }

    #[test]
    fn click_cleanup_only_kills_label_only_resumed_husks() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","label":"Sidebar"},
            {"pane_id":"w1:p2","tab_id":"w1:t1","workspace_id":"w1",
             "tokens":{"herdr-sidebar-preview":"100","hs-preview-path":"0123456789abcdef","hs-preview-inline":"1"}},
            {"pane_id":"w1:p3","tab_id":"w1:t2","workspace_id":"w1","label":"notes.md · preview"}
        ]}}"#;
        let previews = previews_in(json);
        let stamped = previews.iter().find(|p| p.pane_id == "w1:p2").unwrap();
        assert!(stamped.stale && !stamped.resumed);
        let actions = stale_cleanup(&previews, json);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(matches!(
            &actions[0],
            StaleCleanup::CloseTab { tab_id, .. } if tab_id == "w1:t2"
        ));
    }

    fn preview(pane: &str, tab: &str, inline: bool, stale: bool) -> PreviewPane {
        PreviewPane {
            pane_id: pane.into(),
            tab_id: tab.into(),
            workspace_id: "w1".into(),
            doc_token: String::new(),
            pinned: false,
            control: PathBuf::from(format!("{pane}.ctl")),
            stale,
            dedicated: !inline,
            inline,
            origin_tab_id: tab.into(),
            resumed: false,
        }
    }

    /// A stale (dead-but-stamped) viewer in ANOTHER tab must not affect a
    /// click at all: no wait, no refusal, just not a routing target.
    #[test]
    fn unrelated_stale_viewers_neither_block_nor_receive_clicks() {
        let previews = vec![
            preview("w1:p9", "w1:t9", false, true),
            preview("w1:p8", "w1:t8", true, true),
            preview("w1:p2", "w1:t1", true, false),
        ];
        for inline in [false, true] {
            assert!(unresponsive_inline_in(&previews, inline, "w1:t1").is_none());
        }
        let routed: Vec<_> = previews
            .iter()
            .filter(|p| routable(p, true, "w1:t1"))
            .map(|p| p.pane_id.as_str())
            .collect();
        assert_eq!(routed, vec!["w1:p2"]);
        assert!(!previews.iter().any(|p| routable(p, false, "w1:t1")));
    }

    #[test]
    fn a_stale_inline_viewer_in_the_callers_tab_is_reported_not_duplicated() {
        let previews = vec![preview("w1:p2", "w1:t1", true, true)];
        assert_eq!(
            unresponsive_inline_in(&previews, true, "w1:t1").map(|p| p.pane_id.as_str()),
            Some("w1:p2")
        );
        assert!(unresponsive_inline_in(&previews, false, "w1:t1").is_none());
        assert!(!routable(&previews[0], true, "w1:t1"));
    }

    /// A takeover close that failed must not leave `close` in the control
    /// file, or the next loop iteration would take the "close anyway" branch
    /// without the user's second deliberate q.
    #[test]
    fn a_failed_close_never_leaves_the_close_request_behind() {
        let file = Request::File {
            path: PathBuf::from("/repo/a.rs"),
            line: None,
        };
        let restored = control_after_failed_close(&Some(file.clone()));
        assert_eq!(parse_request(&restored), Some(file));
        assert_eq!(parse_request(&control_after_failed_close(&None)), None);
    }

    #[test]
    fn preview_footer_keeps_close_visible_when_narrow() {
        assert_eq!(preview_hint(&["space/b page"], 12), " q close");
        let wide = preview_hint(&["space/b page", "↑↓ scroll", "w: wrap on"], 200);
        assert!(wide.starts_with(" q close  space/b page"), "{wide}");
        assert!(wide.ends_with("Ctrl/Cmd+C copy"), "{wide}");
        assert!(
            preview_hint(&["space/b page", "↑↓ scroll"], 30)
                .chars()
                .count()
                <= 30
        );
    }

    #[test]
    fn sidebar_escape_never_kills_a_stamped_viewer() {
        assert_eq!(esc_close_action(false, false), EscClose::Request);
        assert_eq!(esc_close_action(true, false), EscClose::Request);
        assert_eq!(esc_close_action(true, true), EscClose::Kill);
    }

    #[test]
    fn heartbeat_follows_wall_clock_across_suspend_and_corrections() {
        assert!(!heartbeat_due(1_000, 1_004));
        assert!(heartbeat_due(1_000, 1_005));
        // Resumed after an hour asleep: due at once, not after 5 more seconds.
        assert!(heartbeat_due(1_000, 4_600));
        // Clock corrected backwards: stamp again rather than go silent.
        assert!(heartbeat_due(1_000, 900));
    }

    #[test]
    fn orphan_sweep_keeps_the_control_of_a_present_stale_viewer() {
        let dir = std::env::temp_dir().join(format!("herdr-viewer-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::rundir::ensure_private(&dir).unwrap();
        let kept = dir.join("p-1-a-0.ctl");
        let orphan = dir.join("p-1-b-0.ctl");
        let old = std::time::SystemTime::now() - Duration::from_secs(600);
        for path in [&kept, &orphan] {
            std::fs::write(path, "close").unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let present = PreviewPane {
            pane_id: "w1:p2".into(),
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            doc_token: String::new(),
            pinned: false,
            control: kept.clone(),
            stale: true,
            dedicated: false,
            inline: true,
            origin_tab_id: "w1:t1".into(),
            resumed: false,
        };
        sweep_orphan_controls_in(&dir, &[present]);
        assert!(kept.exists(), "a present (stale) viewer keeps its control");
        assert!(!orphan.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn viewer_lookup_reports_staleness() {
        let now = crate::state::unix_now();
        let json = format!(
            r#"{{"result":{{"panes":[
                {{"pane_id":"w1:p1","tab_id":"w1:t1"}},
                {{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-preview":"{}"}}}}
            ]}}}}"#,
            now - 2
        );
        assert_eq!(
            viewer_pane_in_tab(&json, "w1:p1"),
            Some(("w1:p2".into(), false))
        );
        let stale = format!(
            r#"{{"result":{{"panes":[
                {{"pane_id":"w1:p1","tab_id":"w1:t1"}},
                {{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-preview":"{}"}}}}
            ]}}}}"#,
            now - 999
        );
        assert_eq!(
            viewer_pane_in_tab(&stale, "w1:p1"),
            Some(("w1:p2".into(), true))
        );

        let resumed = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1"},
            {"pane_id":"w1:p2","tab_id":"w1:t1","label":"Preview · routes.rs"}
        ]}}"#;
        assert_eq!(
            viewer_pane_in_tab(resumed, "w1:p1"),
            Some(("w1:p2".into(), true)),
            "document-labelled resumed viewers remain reclaimable"
        );
    }
}
