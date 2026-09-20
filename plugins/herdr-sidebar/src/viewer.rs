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

/// Directory for the sidebar's private scratch files (viewer control files).
/// `std::env::temp_dir()` can be a shared, world-writable directory (unix
/// `/tmp`) where our filenames are predictable from the pane id; scope our
/// files into a private, mode-0700 subdirectory so another local user can't
/// plant a symlink at a path we're about to `fs::write` through. Windows'
/// per-user `%TEMP%` needs no extra scoping.
fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("herdr-sidebar-scratch");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// Write `contents` to `path`, refusing to follow a pre-existing symlink at
/// that location (defense in depth alongside `scratch_dir`'s 0700 perms).
fn write_scratch_file(path: &Path, contents: &str) -> std::io::Result<()> {
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
    let path = Path::new(token);
    if path.components().count() == 1 {
        scratch_dir().join(path)
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
    /// Decoded raster media: herdr's pane graphics layer paints it at full
    /// resolution when available, true-color half blocks otherwise. Video
    /// files carry the poster frame extracted by ffmpeg; PDF pages and SVGs
    /// are rasterized by pdftoppm / rsvg-convert.
    media: Option<MediaPreview>,
    /// Offset into [`Doc::rows`] — RENDERED rows, not source lines, so a
    /// wrapped line's continuations are scrolled to like anything else.
    scroll: usize,
    /// Long lines wrap unless the user toggles them off (`w`). Per document:
    /// a newly loaded one starts wrapped again.
    wrap: bool,
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
}

struct MediaPreview {
    /// Identifies these pixels: a new image, page, or rasterization gets a
    /// new id, and only then is the pane graphics layer re-sent.
    id: u64,
    pixels: image::RgbaImage,
    source_width: u32,
    source_height: u32,
    kind: MediaKind,
    /// The pixel box a vector source (PDF page, SVG) was rasterized for, so a
    /// pane that grows past it can be re-rasterized instead of upscaled.
    raster: Option<RasterBox>,
    /// The rasterizer that produced these pixels, resolved once per document
    /// so a page turn does not walk `PATH` again.
    tool: Option<PathBuf>,
    /// The file these pixels came from. Pages rasterized from a different
    /// revision must never be mixed into one document.
    stamp: FileStamp,
    /// Painted by herdr's pane graphics layer: the text body stays blank
    /// underneath instead of drawing half blocks.
    overlay: bool,
    /// Recently shown PDF pages, so paging back and forth is instant.
    page_cache: Vec<CachedPage>,
}

struct CachedPage {
    page: usize,
    raster: RasterBox,
    stamp: FileStamp,
    pixels: image::RgbaImage,
}

/// What a file looked like when it was rasterized. A rebuilt PDF changes its
/// length or its modification time, and every cached page of the old revision
/// is dropped rather than shown next to a new one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn file_stamp(target: &Path) -> FileStamp {
    std::fs::metadata(target).map_or_else(
        |_| FileStamp::default(),
        |metadata| FileStamp {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        },
    )
}

#[derive(Clone, Debug, PartialEq)]
enum MediaKind {
    Image,
    VideoPoster,
    Svg,
    /// One-based `page`; `pages` is empty when pdfinfo could not count them.
    Pdf {
        page: usize,
        pages: std::sync::Arc<[PageSize]>,
    },
}

/// Pixel box a vector document is rasterized to fit.
type RasterBox = (u32, u32);

/// A PDF page's displayed size in points (rotation applied).
#[derive(Clone, Copy, Debug, PartialEq)]
struct PageSize {
    width: f64,
    height: f64,
}

/// Decoded pages are megabytes each: cap the cache by bytes, not by count.
const PAGE_CACHE_MAX_BYTES: u64 = 48 * 1024 * 1024;

fn next_media_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl MediaPreview {
    fn new(
        pixels: image::RgbaImage,
        kind: MediaKind,
        raster: Option<RasterBox>,
        tool: Option<PathBuf>,
        stamp: FileStamp,
    ) -> Self {
        let (source_width, source_height) = pixels.dimensions();
        Self {
            id: next_media_id(),
            pixels,
            source_width,
            source_height,
            kind,
            raster,
            tool,
            stamp,
            overlay: false,
            page_cache: Vec::new(),
        }
    }

    fn page(&self) -> Option<usize> {
        match self.kind {
            MediaKind::Pdf { page, .. } => Some(page),
            _ => None,
        }
    }

    /// Swap in a newly rasterized page (or re-rasterized SVG), keeping the
    /// outgoing PDF page for a quick turn back. Pages of another revision or
    /// another raster box can never be shown again, so they are dropped.
    fn replace(
        &mut self,
        pixels: image::RgbaImage,
        kind: MediaKind,
        raster: RasterBox,
        stamp: FileStamp,
    ) {
        if let (Some(page), Some(old_raster)) = (self.page(), self.raster) {
            let old = std::mem::replace(&mut self.pixels, image::RgbaImage::new(0, 0));
            self.page_cache.push(CachedPage {
                page,
                raster: old_raster,
                stamp: self.stamp,
                pixels: old,
            });
        }
        self.page_cache
            .retain(|cached| cached.stamp == stamp && cached.raster == raster);
        let mut bytes: u64 = self
            .page_cache
            .iter()
            .map(|cached| u64::from(cached.pixels.width()) * u64::from(cached.pixels.height()) * 4)
            .sum();
        while bytes > PAGE_CACHE_MAX_BYTES && !self.page_cache.is_empty() {
            let dropped = self.page_cache.remove(0);
            bytes = bytes.saturating_sub(
                u64::from(dropped.pixels.width()) * u64::from(dropped.pixels.height()) * 4,
            );
        }
        (self.source_width, self.source_height) = pixels.dimensions();
        self.pixels = pixels;
        self.kind = kind;
        self.raster = Some(raster);
        self.stamp = stamp;
        self.id = next_media_id();
    }

    /// A cached page, but only for this revision of the file and this raster
    /// box — a rebuilt PDF must not show one page from each build.
    fn take_cached(
        &mut self,
        page: usize,
        raster: RasterBox,
        stamp: FileStamp,
    ) -> Option<image::RgbaImage> {
        self.page_cache
            .retain(|cached| cached.stamp == stamp && cached.raster == raster);
        let index = self
            .page_cache
            .iter()
            .position(|cached| cached.page == page)?;
        Some(self.page_cache.remove(index).pixels)
    }

    fn context(&self, target: &Path) -> String {
        let (width, height) = (self.source_width, self.source_height);
        match &self.kind {
            MediaKind::Image => format!("{} — {width}×{height} image", target.display()),
            MediaKind::VideoPoster => {
                format!("{} — {width}×{height} video poster", target.display())
            }
            MediaKind::Svg => format!("{} — SVG", target.display()),
            MediaKind::Pdf { page, pages } => {
                format!("{} — {}", target.display(), page_label(*page, pages.len()))
            }
        }
    }

    fn hint(&self) -> String {
        match &self.kind {
            MediaKind::Image => " image preview  o open  q close".into(),
            MediaKind::VideoPoster => " video poster frame  o open  q close".into(),
            MediaKind::Svg => " SVG preview  o open  q close".into(),
            MediaKind::Pdf { page, pages } => format!(
                " {}  n/p page  o open  q close",
                page_label(*page, pages.len())
            ),
        }
    }
}

fn page_label(page: usize, count: usize) -> String {
    if count > 0 {
        format!("page {page}/{count}")
    } else {
        format!("page {page}")
    }
}

/// What a paging key does to a PDF preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageMove {
    /// Render that page (or serve it from the cache).
    Show(usize),
    /// The key lands back on the page already on screen: the render in
    /// flight is no longer wanted.
    Cancel,
}

/// Where a paging key goes. `requested` is the page a render is already
/// working towards, so holding `n` counts up from there instead of asking for
/// the same page again — one rasterizer runs at a time. `None` for other keys
/// or a move past either end; an unknown page count (no pdfinfo) blocks only
/// the jump to the last page.
fn page_move(
    shown: usize,
    requested: Option<usize>,
    count: Option<usize>,
    key: KeyCode,
) -> Option<PageMove> {
    let from = requested.unwrap_or(shown);
    let target = match key {
        KeyCode::Char('n') | KeyCode::PageDown => from + 1,
        KeyCode::Char('p') | KeyCode::PageUp => from.saturating_sub(1),
        KeyCode::Home | KeyCode::Char('g') => 1,
        KeyCode::End | KeyCode::Char('G') => count?,
        _ => return None,
    };
    if target < 1 || count.is_some_and(|count| target > count) || target == from {
        return None;
    }
    Some(if target == shown {
        PageMove::Cancel
    } else {
        PageMove::Show(target)
    })
}

/// The page a PDF media kind shows, and how many pages it has (when pdfinfo
/// could count them).
fn pdf_position(kind: &MediaKind) -> Option<(usize, Option<usize>)> {
    match kind {
        MediaKind::Pdf { page, pages } => Some((*page, (!pages.is_empty()).then_some(pages.len()))),
        _ => None,
    }
}

/// Paging keys, unmodified (Shift only for `G`): Ctrl+N must not turn a page.
fn is_page_key(key: KeyCode, modifiers: KeyModifiers) -> bool {
    if !modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return false;
    }
    matches!(
        key,
        KeyCode::Char('n' | 'p' | 'g' | 'G')
            | KeyCode::PageDown
            | KeyCode::PageUp
            | KeyCode::Home
            | KeyCode::End
    )
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
                if media.overlay {
                    Vec::new()
                } else {
                    render_media_rows(media, width, height)
                }
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

    fn on_mouse(&mut self, mouse: &MouseEvent, body: Rect) {
        if self.media.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(position) = self.text_position_at(body, mouse.column, mouse.row) else {
                    return;
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
                    return;
                };
                let Some(position) = self.text_position_at(body, mouse.column, mouse.row) else {
                    return;
                };
                self.selection.anchor = Some(anchor);
                self.selection.cursor = Some(position);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(position) = self.text_position_at(body, mouse.column, mouse.row) {
                    self.selection.cursor = Some(position);
                }
                self.selection.mouse_anchor = None;
            }
            _ => {}
        }
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

/// What a preview worker needs from the pane: the pixel box to rasterize
/// for, and the token that stops its helpers once the user moves on.
#[derive(Clone, Default)]
struct LoadContext {
    raster: RasterBox,
    cancel: Cancel,
}

fn load(request: &Request, ctx: &LoadContext) -> Doc {
    match request {
        Request::Close => Doc {
            name: "Preview".into(),
            context: String::new(),
            lines: vec![Line::raw("(closing)")],
            numbered: false,
            media: None,
            scroll: 0,
            wrap: true,
            rows: Vec::new(),
            rows_key: None,
            pending_src: None,
            selection: PreviewSelection::default(),
        },
        Request::File { path, line } => load_file(path, *line, ctx),
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
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
    }
}

/// A preview being loaded off the event loop. Dropping it tells its helpers
/// to stop: clicking through a folder of PDFs must not leave a pdfinfo and a
/// pdftoppm running per discarded click.
struct PreviewLoad {
    request: Request,
    cancel: Cancel,
    receiver: std::sync::mpsc::Receiver<Doc>,
}

impl Drop for PreviewLoad {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn start_preview_load(request: Request, raster: RasterBox) -> PreviewLoad {
    let worker_request = request.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let cancel = Cancel::default();
    let ctx = LoadContext {
        raster,
        cancel: cancel.clone(),
    };
    std::thread::spawn(move || {
        let _ = sender.send(load(&worker_request, &ctx));
    });
    PreviewLoad {
        request,
        cancel,
        receiver,
    }
}

fn apply_diff_refresh(doc: &mut Doc, mut refreshed: Doc) {
    if doc.name == refreshed.name
        && doc.context == refreshed.context
        && doc.numbered == refreshed.numbered
        && doc.lines == refreshed.lines
    {
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
    preview_load: &mut Option<PreviewLoad>,
    identity_pending: &mut bool,
    control: &Path,
    raster: RasterBox,
) -> bool {
    match pending {
        Pending::Close => close_own_pane(control),
        Pending::LeaveEdit => {
            if let Some(request) = current.clone() {
                *mode = ViewMode::Preview(loading_doc(&request));
                *preview_load = Some(start_preview_load(request, raster));
                *identity_pending = true;
            }
            false
        }
        Pending::Switch(request) => {
            if request == Request::Close {
                close_own_pane(control)
            } else {
                *mode = ViewMode::Preview(loading_doc(&request));
                *current = Some(request.clone());
                *preview_load = Some(start_preview_load(request, raster));
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
    let mut args: Vec<String> = vec![
        "-c".into(),
        "color.ui=always".into(),
        "show".into(),
        "--color=always".into(),
        "--stat".into(),
        "--patch".into(),
        "--no-ext-diff".into(),
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
        name: spec.to_string(),
        context: format!("git show {spec} — {}", root.display()),
        lines,
        numbered: false,
        media: None,
        scroll: 0,
        wrap: true,
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
    }
}

/// Render markdown text via `glow`. Returns `None` when glow is not installed
/// or exits non-zero (caller falls back to syntax highlight).
///
/// Receives the already-read `text` buffer so the MAX_BYTES guard in
/// `load_file` is honoured — glow would otherwise re-read the full file.
/// Pipes via stdin (`-`) to avoid treating filenames starting with `-` as
/// flags. Width is a best-effort approximation; the ideal fix would pass
/// `body.width` from `draw_doc` once that is available at load time.
fn glow_markdown(text: &str, width: u16) -> Option<Vec<Line<'static>>> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("glow")
        .args(["--style", "dark", "--width", &width.to_string(), "-"])
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

const MAX_MEDIA_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MEDIA_PIXELS: u64 = 12_000_000;
const MAX_MEDIA_DIMENSION: u32 = 8192;
const MAX_MEDIA_ALLOC_BYTES: u64 = 64 * 1024 * 1024;
const MAX_VIDEO_FRAME_BYTES: u64 = 16 * 1024 * 1024;
const VIDEO_FRAME_TIMEOUT: Duration = Duration::from_secs(4);
/// Longest side a PDF page or SVG is rasterized to.
const MAX_RASTER_SIDE: u32 = 4096;
const RASTER_TIMEOUT: Duration = Duration::from_secs(10);
const PDFINFO_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_PDFINFO_BYTES: u64 = 1024 * 1024;

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

fn is_pdf_file(target: &Path) -> bool {
    has_extension(target, &["pdf"])
}

fn is_svg_file(target: &Path) -> bool {
    has_extension(target, &["svg"])
}

fn media_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_MEDIA_DIMENSION);
    limits.max_image_height = Some(MAX_MEDIA_DIMENSION);
    limits.max_alloc = Some(MAX_MEDIA_ALLOC_BYTES);
    limits
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
    reader.limits(media_limits());
    reader
        .decode()
        .map(|image| image.to_rgba8())
        .map_err(|error| error.to_string())
}

/// Decode a helper's PNG output under the same limits as image files.
fn decode_png(bytes: &[u8]) -> Result<image::RgbaImage, String> {
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    reader.limits(media_limits());
    reader
        .decode()
        .map(|image| image.to_rgba8())
        .map_err(|error| error.to_string())
}

/// Resolve a helper executable (ffmpeg, pdftoppm, …) from `path`, skipping
/// relative and project-local entries: Windows process lookup would otherwise
/// search the previewed project's directory first.
fn executable_in_path(name: &str, path: &std::ffi::OsStr, cwd: Option<&Path>) -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .filter_map(|directory| directory.canonicalize().ok())
        .filter(|directory| !cwd.is_some_and(|cwd| directory.starts_with(cwd)))
        .map(|directory| directory.join(&executable))
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

fn executable_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    executable_in_path(name, &path, cwd.as_deref())
}

/// A file argument for a helper: a relative name starting with `-` would be
/// parsed as an option.
fn file_arg(target: &Path) -> PathBuf {
    if target.is_relative() && target.as_os_str().to_string_lossy().starts_with('-') {
        Path::new(".").join(target)
    } else {
        target.to_path_buf()
    }
}

/// Cancellation for helper subprocesses: a preview the user already moved
/// away from must not keep a `pdfinfo` and a `pdftoppm` running (nor start
/// the next one), and a superseded page turn is killed rather than raced.
#[derive(Clone, Default)]
struct Cancel(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Cancel {
    fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

struct Captured {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run a helper with a wall-clock limit, keeping at most `max_stdout + 1`
/// bytes of its output so callers can reject oversized results.
fn run_bounded(
    mut command: std::process::Command,
    tool: &str,
    max_stdout: u64,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<Captured, String> {
    if cancel.is_cancelled() {
        return Err(format!("{tool} was not needed anymore"));
    }
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
        .map_err(|error| format!("{tool} failed: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{tool} stdout unavailable"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{tool} stderr unavailable"))?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(max_stdout + 1).read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stderr).take(16 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if cancel.is_cancelled() => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("{tool} was not needed anymore"));
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!(
                    "{tool} timed out after {} seconds",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("{tool} status failed: {error}"));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| format!("{tool} output reader failed"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| format!("{tool} error reader failed"))?;
    Ok(Captured {
        status,
        stdout,
        stderr,
    })
}

fn first_stderr_line(captured: &Captured) -> String {
    String::from_utf8_lossy(&captured.stderr)
        .lines()
        .next()
        .unwrap_or("unknown error")
        .to_string()
}

fn decode_video_poster(target: &Path, cancel: &Cancel) -> Result<image::RgbaImage, String> {
    let ffmpeg = executable_on_path("ffmpeg")
        .ok_or_else(|| "video preview needs ffmpeg on PATH".to_string())?;
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
    command.arg(file_arg(target));
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
    let captured = run_bounded(
        command,
        "ffmpeg",
        MAX_VIDEO_FRAME_BYTES,
        VIDEO_FRAME_TIMEOUT,
        cancel,
    )?;
    if !captured.status.success() {
        return Err(format!(
            "ffmpeg could not read video: {}",
            first_stderr_line(&captured)
        ));
    }
    if captured.stdout.len() as u64 > MAX_VIDEO_FRAME_BYTES {
        return Err("video poster frame exceeded 16 MiB".into());
    }
    decode_png(&captured.stdout).map_err(|error| format!("could not decode video frame: {error}"))
}

/// The pane's size in cells, as the viewer's own terminal reports it.
fn terminal_cells() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

/// Pixel box of the preview body (the pane minus header and footer rows):
/// real pixels when herdr reports the cell size, else the half-block grid of
/// two pixels per cell. Bounded so a rasterized page stays decodable.
fn raster_box(terminal: (u16, u16), cell: Option<crate::pane_graphics::CellSize>) -> RasterBox {
    let cols = u32::from(terminal.0).max(1);
    let rows = u32::from(terminal.1.saturating_sub(2)).max(1);
    let (width, height) = match cell {
        Some(cell) => (cols * cell.width_px, rows * cell.height_px),
        None => (cols, rows * 2),
    };
    let mut width = f64::from(width.clamp(1, MAX_RASTER_SIDE));
    let mut height = f64::from(height.clamp(1, MAX_RASTER_SIDE));
    let pixels = width * height;
    if pixels > MAX_MEDIA_PIXELS as f64 {
        let shrink = (MAX_MEDIA_PIXELS as f64 / pixels).sqrt();
        width *= shrink;
        height *= shrink;
    }
    ((width as u32).max(1), (height as u32).max(1))
}

/// A vector source rasterized for `have` looks soft once the pane outgrows it.
fn needs_rerender(have: RasterBox, want: RasterBox) -> bool {
    u64::from(want.0) * 10 > u64::from(have.0) * 11
        || u64::from(want.1) * 10 > u64::from(have.1) * 11
}

/// Page sizes out of `pdfinfo -f 1 -l <n>`; empty when the count is missing.
fn parse_pdfinfo(text: &str) -> Vec<PageSize> {
    let count = text.lines().find_map(|line| {
        line.strip_prefix("Pages:")
            .and_then(|count| count.trim().parse::<usize>().ok())
    });
    let count = count.unwrap_or(0).min(100_000);
    let mut sizes = vec![(0.0, 0.0); count];
    let mut quarter_turns = vec![false; count];
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("Page") {
            continue;
        }
        let Some(index) = words
            .next()
            .and_then(|page| page.parse::<usize>().ok())
            .and_then(|page| page.checked_sub(1))
            .filter(|index| *index < count)
        else {
            continue;
        };
        match (words.next(), words.next(), words.next(), words.next()) {
            (Some("size:"), Some(width), Some("x"), Some(height)) => {
                if let (Ok(width), Ok(height)) = (width.parse(), height.parse()) {
                    sizes[index] = (width, height);
                }
            }
            (Some("rot:"), Some(rotation), _, _) => {
                quarter_turns[index] = rotation
                    .parse::<i64>()
                    .is_ok_and(|rotation| rotation.rem_euclid(180) == 90);
            }
            _ => {}
        }
    }
    sizes
        .into_iter()
        .zip(quarter_turns)
        .map(|((width, height), quarter_turn)| {
            if quarter_turn {
                PageSize {
                    width: height,
                    height: width,
                }
            } else {
                PageSize { width, height }
            }
        })
        .collect()
}

fn pdf_pages(target: &Path, cancel: &Cancel) -> std::sync::Arc<[PageSize]> {
    let Some(pdfinfo) = executable_on_path("pdfinfo") else {
        return std::sync::Arc::from(Vec::new());
    };
    let mut command = std::process::Command::new(pdfinfo);
    command
        .args(["-f", "1", "-l", "100000"])
        .arg(file_arg(target));
    let pages = match run_bounded(
        command,
        "pdfinfo",
        MAX_PDFINFO_BYTES,
        PDFINFO_TIMEOUT,
        cancel,
    ) {
        Ok(captured) if captured.status.success() => {
            parse_pdfinfo(&String::from_utf8_lossy(&captured.stdout))
        }
        _ => Vec::new(),
    };
    std::sync::Arc::from(pages)
}

/// pdftoppm `-scale-to` (long side, px) that fits `page` inside `raster`.
fn pdf_scale_to(page: Option<PageSize>, raster: RasterBox) -> u32 {
    let (box_w, box_h) = (f64::from(raster.0), f64::from(raster.1));
    let long_side = match page {
        Some(page) if page.width > 0.0 && page.height > 0.0 => {
            let scale = (box_w / page.width).min(box_h / page.height);
            page.width.max(page.height) * scale
        }
        _ => box_w.min(box_h),
    };
    (long_side.round() as u32).clamp(16, MAX_RASTER_SIDE)
}

/// Rasterize a vector document (one PDF page, or an SVG) to fit `raster`,
/// with the stamp of the file it was read from.
fn rasterize(
    tool_path: &Path,
    target: &Path,
    kind: &MediaKind,
    raster: RasterBox,
    cancel: &Cancel,
) -> Result<(image::RgbaImage, FileStamp), String> {
    let stamp = file_stamp(target);
    let (tool, command) = match kind {
        MediaKind::Pdf { page, pages } => {
            let page_size = pages.get(page.saturating_sub(1)).copied();
            let mut command = std::process::Command::new(tool_path);
            command
                .args(["-png", "-singlefile", "-f"])
                .arg(page.to_string())
                .arg("-l")
                .arg(page.to_string())
                .arg("-scale-to")
                .arg(pdf_scale_to(page_size, raster).to_string())
                .arg(file_arg(target));
            ("pdftoppm", command)
        }
        MediaKind::Svg => {
            let mut command = std::process::Command::new(tool_path);
            command
                .args(["--keep-aspect-ratio", "--format", "png", "--width"])
                .arg(raster.0.to_string())
                .arg("--height")
                .arg(raster.1.to_string())
                .arg(file_arg(target));
            ("rsvg-convert", command)
        }
        MediaKind::Image | MediaKind::VideoPoster => {
            return Err("not a vector document".into());
        }
    };
    let captured = run_bounded(command, tool, MAX_MEDIA_FILE_BYTES, RASTER_TIMEOUT, cancel)?;
    if !captured.status.success() {
        return Err(format!("{tool}: {}", first_stderr_line(&captured)));
    }
    if captured.stdout.len() as u64 > MAX_MEDIA_FILE_BYTES {
        return Err(format!("{tool} output exceeded 32 MiB"));
    }
    let pixels = decode_png(&captured.stdout)
        .map_err(|error| format!("could not decode {tool} output: {error}"))?;
    Ok((pixels, stamp))
}

/// Rasterize off the event loop (page turns and pane growth). One job runs
/// at a time; a newer goal waits for it (see `RasterGoal`) instead of piling
/// up subprocesses, and dropping the job kills the one in flight.
struct RasterLoad {
    request: Request,
    kind: MediaKind,
    raster: RasterBox,
    cancel: Cancel,
    receiver: std::sync::mpsc::Receiver<Result<(image::RgbaImage, FileStamp), String>>,
}

impl Drop for RasterLoad {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// The page (or re-rasterization) the user is asking for, which may arrive
/// while an earlier one is still rendering.
#[derive(Clone)]
struct RasterGoal {
    kind: MediaKind,
    raster: RasterBox,
}

fn start_raster(request: Request, target: PathBuf, tool: PathBuf, goal: RasterGoal) -> RasterLoad {
    let (sender, receiver) = std::sync::mpsc::channel();
    let cancel = Cancel::default();
    let worker_kind = goal.kind.clone();
    let worker_cancel = cancel.clone();
    let raster = goal.raster;
    std::thread::spawn(move || {
        let _ = sender.send(rasterize(
            &tool,
            &target,
            &worker_kind,
            raster,
            &worker_cancel,
        ));
    });
    RasterLoad {
        request,
        kind: goal.kind,
        raster,
        cancel,
        receiver,
    }
}

fn media_doc(target: &Path, name: String, result: Result<MediaPreview, String>) -> Doc {
    let (lines, context, media) = match result {
        Ok(media) => (Vec::new(), media.context(target), Some(media)),
        Err(error) => (
            vec![Line::raw(format!("({error})"))],
            target.display().to_string(),
            None,
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
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
    }
}

fn load_media_file(target: &Path, name: String, video_poster: bool, cancel: &Cancel) -> Doc {
    let stamp = file_stamp(target);
    let result = if video_poster {
        decode_video_poster(target, cancel)
            .map(|pixels| MediaPreview::new(pixels, MediaKind::VideoPoster, None, None, stamp))
    } else {
        decode_image_file(target)
            .map(|pixels| MediaPreview::new(pixels, MediaKind::Image, None, None, stamp))
    };
    media_doc(target, name, result)
}

/// A PDF's first page, or an SVG, rasterized by an already-resolved `tool`.
/// `None` when the SVG cannot be rendered, so its source still previews as
/// text.
fn load_vector_file(
    target: &Path,
    name: String,
    kind: MediaKind,
    tool: &Path,
    ctx: &LoadContext,
) -> Option<Doc> {
    let kind = match kind {
        MediaKind::Pdf { .. } => MediaKind::Pdf {
            page: 1,
            pages: pdf_pages(target, &ctx.cancel),
        },
        kind => kind,
    };
    let result = rasterize(tool, target, &kind, ctx.raster, &ctx.cancel).map(|(pixels, stamp)| {
        MediaPreview::new(
            pixels,
            kind.clone(),
            Some(ctx.raster),
            Some(tool.to_path_buf()),
            stamp,
        )
    });
    if result.is_err() && kind == MediaKind::Svg {
        return None;
    }
    Some(media_doc(
        target,
        name,
        result.map_err(|error| format!("could not render PDF: {error}")),
    ))
}

fn load_file(target: &Path, target_line: Option<usize>, ctx: &LoadContext) -> Doc {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    let lower = name.to_lowercase();
    if is_image_file(target) {
        return load_media_file(target, name, false, &ctx.cancel);
    }
    if is_video_file(target) {
        return load_media_file(target, name, true, &ctx.cancel);
    }
    // Vector documents need their rasterizer; without it they keep the plain
    // preview (binary notice for PDF, source text for SVG).
    if is_pdf_file(target)
        && let Some(pdftoppm) = executable_on_path("pdftoppm")
        && let Some(doc) = load_vector_file(
            target,
            name.clone(),
            MediaKind::Pdf {
                page: 1,
                pages: std::sync::Arc::from(Vec::new()),
            },
            &pdftoppm,
            ctx,
        )
    {
        return doc;
    }
    if is_svg_file(target)
        && let Some(rsvg) = executable_on_path("rsvg-convert")
        && let Some(doc) = load_vector_file(target, name.clone(), MediaKind::Svg, &rsvg, ctx)
    {
        return doc;
    }
    let is_markdown = lower.ends_with(".md") || lower.ends_with(".markdown");
    let (lines, numbered) = match std::fs::read(target) {
        Err(e) => (vec![Line::raw(format!("(unreadable: {e})"))], true),
        Ok(bytes) => {
            if bytes.contains(&0) {
                (
                    vec![Line::raw(format!("(binary file — {} bytes)", bytes.len()))],
                    false,
                )
            } else {
                let truncated = bytes.len() > MAX_BYTES;
                let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]);
                // Markdown: render via glow; fall back to syntax highlight on failure.
                // Width is approximated by subtracting 6 for the sidebar share and
                // line-number gutter; ideal fix is to pass body.width from draw_doc.
                let glow_width = crossterm::terminal::size()
                    .map(|(w, _)| w.saturating_sub(6))
                    .unwrap_or(74);
                let glow_rendered = (is_markdown && target_line.is_none())
                    .then(|| glow_markdown(&text, glow_width))
                    .flatten();
                // Glow-rendered markdown gets no line numbers (it formats its own layout).
                let numbered = glow_rendered.is_none();
                let mut lines: Vec<Line<'static>> = if let Some(rendered) = glow_rendered {
                    rendered
                } else {
                    crate::syntax::highlight(&name, &text, MAX_LINES).unwrap_or_else(|| {
                        text.lines()
                            .take(MAX_LINES)
                            .map(|l| Line::raw(l.to_string()))
                            .collect()
                    })
                };
                if truncated || text.lines().count() > MAX_LINES {
                    lines.push(Line::raw("… (truncated)"));
                }
                if lines.is_empty() {
                    lines.push(Line::raw("(empty file)"));
                }
                (lines, numbered)
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
        wrap: true,
        rows: Vec::new(),
        rows_key: None,
        pending_src: target_line.map(|line| line.saturating_sub(1)),
        selection: PreviewSelection::default(),
    }
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
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
    }
}

fn read_control(control: &Path) -> Option<Request> {
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

/// Close the whole preview tab. Closing only the viewer pane leaves its
/// auto-docked sidebar behind as a convincing but unusable preview husk.
/// An inline viewer owns nothing but its own pane, so it closes just that.
fn close_own_pane(control: &Path) -> bool {
    let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else {
        return false;
    };
    if pane_id.is_empty() {
        return false;
    }
    if runs_inline() {
        let closed = pane_close_succeeded(ipc::call_text(
            "pane.close",
            serde_json::json!({ "pane_id": pane_id }),
        ));
        if closed {
            let _ = std::fs::remove_file(control);
            let _ = std::fs::remove_file(control_path_for_pane(&pane_id));
        }
        return closed;
    }
    let list = ipc::call_text("pane.list", serde_json::json!({})).ok();
    let preview = list.as_deref().and_then(|json| {
        previews_in(json)
            .into_iter()
            .find(|preview| preview.pane_id == pane_id)
    });
    let _ = std::fs::remove_file(control);
    let _ = std::fs::remove_file(control_path_for_pane(&pane_id));
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
    let live: std::collections::BTreeSet<String> = previews
        .iter()
        .filter(|preview| !preview.stale)
        .map(|preview| preview.pane_id.replace(':', "_"))
        .collect();
    let live_controls: std::collections::BTreeSet<PathBuf> = previews
        .into_iter()
        .filter(|preview| !preview.stale)
        .map(|preview| preview.control)
        .collect();
    let Ok(entries) = std::fs::read_dir(scratch_dir()) else {
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

/// The viewer's event loop; returns when the user closes it.
/// Full-resolution media through herdr's pane graphics layer (see
/// `pane_graphics`). Without it — kitty graphics disabled, Windows, or a
/// rejected frame — `cell` is `None` and media renders as half blocks.
/// Full-resolution media through herdr's pane graphics layer (see
/// `pane_graphics`). Without it — kitty graphics disabled, Windows, or a
/// rejected frame — media renders as half blocks.
///
/// Every herdr roundtrip here happens on a worker: the API call has a five
/// second timeout, and the draw loop must never wait that long. The cell size
/// and the stream therefore arrive asynchronously, and the frame goes out on
/// the first draw after they do.
struct Graphics {
    pane_id: String,
    cell: Option<crate::pane_graphics::CellSize>,
    probe: Option<std::sync::mpsc::Receiver<Option<crate::pane_graphics::CellSize>>>,
    /// Debounces the re-probe: a resize drag emits a burst of events.
    probed: Option<Instant>,
    /// What the cell size was last probed for, so a new document re-probes
    /// (the font or the display may have changed without a resize).
    probed_media: Option<u64>,
    opening: Option<std::sync::mpsc::Receiver<Option<crate::pane_graphics::Stream>>>,
    stream: Option<crate::pane_graphics::Stream>,
    /// The media id and body rect currently on screen: a frame is sent only
    /// when one of them changes, never per tick.
    shown: Option<(u64, Rect)>,
    /// The media whose frame herdr refused. Only that one falls back; the
    /// next document tries again, so one transient failure (a stream still
    /// owned by a closing predecessor) does not mosaic the whole session.
    refused: Option<u64>,
}

/// What `sync` changed, so the caller can repaint immediately instead of
/// leaving a blank body (or a mosaic) on screen for a poll interval.
#[derive(PartialEq, Eq)]
enum Painted {
    Unchanged,
    Repaint,
}

/// How long a burst of resize events is coalesced before re-probing.
const PROBE_DEBOUNCE: Duration = Duration::from_millis(150);

/// How many immediate redraws a layer change may chain before the loop goes
/// back to waiting for input.
const MAX_CHAINED_REPAINTS: u8 = 3;

impl Graphics {
    fn new() -> Self {
        let mut graphics = Self {
            pane_id: std::env::var("HERDR_PANE_ID").unwrap_or_default(),
            cell: None,
            probe: None,
            probed: None,
            probed_media: None,
            opening: None,
            stream: None,
            shown: None,
            refused: None,
        };
        graphics.start_probe();
        graphics
    }

    fn start_probe(&mut self) {
        if self.pane_id.is_empty() || self.probe.is_some() {
            return;
        }
        self.probed = Some(Instant::now());
        let pane_id = self.pane_id.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(crate::pane_graphics::probe(&pane_id));
        });
        self.probe = Some(receiver);
    }

    /// A resize can follow a font or display change: re-probe, debounced.
    fn resized(&mut self) {
        self.shown = None;
        if self
            .probed
            .is_none_or(|probed| probed.elapsed() >= PROBE_DEBOUNCE)
        {
            self.start_probe();
        }
    }

    fn start_open(&mut self) {
        if self.pane_id.is_empty() || self.opening.is_some() || self.stream.is_some() {
            return;
        }
        let pane_id = self.pane_id.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(crate::pane_graphics::Stream::open(&pane_id));
        });
        self.opening = Some(receiver);
    }

    /// Collect whatever the workers finished, without blocking.
    fn collect(&mut self) {
        if let Some(probe) = &self.probe
            && let Ok(cell) = probe.try_recv()
        {
            self.probe = None;
            self.cell = cell;
        }
        if let Some(opening) = &self.opening
            && let Ok(stream) = opening.try_recv()
        {
            self.opening = None;
            self.stream = stream;
        }
    }

    /// Whether `media` will be painted by the layer rather than half blocks.
    /// A stream that is still being opened counts: this pane has already said
    /// it does pane graphics, so the body stays blank for the frame or two
    /// that takes instead of flashing a mosaic that is about to be replaced.
    fn paints(&self, media: &MediaPreview) -> bool {
        self.cell.is_some()
            && (self.stream.is_some() || self.opening.is_some())
            && self.refused != Some(media.id)
    }

    /// Before drawing: collect finished workers, start the stream the media
    /// on screen is about to need, and leave the body blank where the layer
    /// will paint.
    fn prepare(&mut self, doc: &mut Doc) {
        self.collect();
        if doc.media.is_some() && self.cell.is_some() {
            self.start_open();
        }
        if let Some(media) = &mut doc.media {
            let overlay = self.paints(media);
            if media.overlay != overlay {
                media.overlay = overlay;
                doc.rows_key = None;
            }
        }
    }

    /// After drawing: put `media` on the layer, or take the layer down.
    fn sync(&mut self, media: Option<&MediaPreview>, body: Rect) -> Painted {
        self.collect();
        let Some(media) = media else {
            self.clear();
            return Painted::Unchanged;
        };
        if media.id != self.probed_media.unwrap_or(0) {
            self.probed_media = Some(media.id);
            self.start_probe();
        }
        if self
            .stream
            .as_ref()
            .is_some_and(crate::pane_graphics::Stream::failed)
        {
            // herdr closed the stream: drop it (which releases the layer) and
            // open a fresh one for the next frame.
            self.stream = None;
            self.shown = None;
        }
        let Some(cell) = self.cell else {
            return Painted::Unchanged;
        };
        if self.refused == Some(media.id) {
            return Painted::Unchanged;
        }
        if self.stream.is_none() {
            self.start_open();
            // Still opening: the body is already blank for it (see `paints`).
            return Painted::Unchanged;
        }
        let repaint = if media.overlay {
            Painted::Unchanged
        } else {
            // The body was drawn as half blocks before the layer was ready.
            Painted::Repaint
        };
        if self.shown == Some((media.id, body)) {
            return repaint;
        }
        let area = crate::pane_graphics::CellRect {
            col: body.x,
            row: body.y,
            cols: body.width,
            rows: body.height,
        };
        let Some(plan) = crate::pane_graphics::plan(
            area,
            cell,
            media.pixels.dimensions(),
            crate::pane_graphics::FRAME_MAX_BYTES,
        ) else {
            self.clear();
            return Painted::Unchanged;
        };
        let frame = crate::pane_graphics::compose(&plan, &media.pixels);
        if self
            .stream
            .as_mut()
            .is_some_and(|stream| stream.send(&plan, &frame))
        {
            self.shown = Some((media.id, body));
            repaint
        } else {
            self.stream = None;
            self.shown = None;
            self.refused = Some(media.id);
            // Fall back for this document only, and repaint it as half
            // blocks right away instead of leaving a blank body.
            Painted::Repaint
        }
    }

    /// Close the stream: herdr removes the layer with it.
    fn clear(&mut self) {
        self.stream = None;
        self.shown = None;
        self.refused = None;
        self.probed_media = None;
    }
}

pub fn run(control: &Path) -> std::io::Result<()> {
    let theme = IconTheme::resolve(
        std::env::var("HERDR_SIDEBAR_ICONS")
            .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
            .ok()
            .as_deref(),
        crate::state::load_state().icons,
    );
    let mut current = read_control(control);
    // The cell size is not known yet (it is probed on a worker), so the first
    // rasterization uses the half-block grid and is refreshed by the "pane
    // outgrew its raster" check as soon as the probe lands.
    let mut preview_load = current
        .clone()
        .map(|request| start_preview_load(request, raster_box(terminal_cells(), None)));
    let doc = current.as_ref().map(loading_doc).unwrap_or_else(|| Doc {
        name: "(nothing to show)".into(),
        context: String::new(),
        lines: vec![Line::raw("(waiting for a click in the sidebar)")],
        numbered: false,
        media: None,
        scroll: 0,
        wrap: true,
        rows: Vec::new(),
        rows_key: None,
        pending_src: None,
        selection: PreviewSelection::default(),
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
    let mut last_heartbeat = Instant::now();
    let mut last_external_check = Instant::now();
    let mut last_diff_refresh = Instant::now();
    let mut diff_refresh: Option<(Request, std::sync::mpsc::Receiver<Doc>)> = None;
    let mut identity_pending = false;
    let mut graphics = Graphics::new();
    let mut raster_load: Option<RasterLoad> = None;
    let mut raster_goal: Option<RasterGoal> = None;
    let mut resized = false;
    let mut repaints = 0_u8;
    let result = loop {
        let loaded = preview_load
            .as_ref()
            .and_then(|load| match load.receiver.try_recv() {
                Ok(doc) => Some((load.request.clone(), Some(doc))),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some((load.request.clone(), None))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            });
        if let Some((request, loaded)) = loaded {
            preview_load = None;
            if current.as_ref() == Some(&request)
                && let (ViewMode::Preview(doc), Some(loaded)) = (&mut mode, loaded)
            {
                *doc = loaded;
            }
        }
        let rastered = raster_load
            .as_ref()
            .and_then(|load| match load.receiver.try_recv() {
                Ok(result) => Some(Some(result)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(None),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            });
        if let Some(result) = rastered
            && let Some(load) = raster_load.take()
            && current.as_ref() == Some(&load.request)
            && let (ViewMode::Preview(doc), Some(Request::File { path, .. })) =
                (&mut mode, current.as_ref())
            && let Some(media) = &mut doc.media
        {
            match result {
                Some(Ok((pixels, stamp))) => {
                    media.replace(pixels, load.kind.clone(), load.raster, stamp);
                    doc.context = media.context(path);
                    doc.rows_key = None;
                }
                Some(Err(error)) => notice = Some(error),
                None => {}
            }
        }
        // One rasterizer at a time: a held-down `n` sets the goal repeatedly
        // and only the page the user landed on is rendered.
        if raster_load.is_none()
            && let Some(goal) = raster_goal.clone()
            && let (ViewMode::Preview(doc), Some(request @ Request::File { path, .. })) =
                (&mode, current.as_ref())
            && let Some(media) = &doc.media
        {
            raster_goal = None;
            let shown = media.kind == goal.kind && media.raster == Some(goal.raster);
            if !shown && let Some(tool) = media.tool.clone() {
                raster_load = Some(start_raster(request.clone(), path.clone(), tool, goal));
            }
        }
        if let ViewMode::Preview(doc) = &mut mode {
            graphics.prepare(doc);
        }
        let prompt_text = prompt.as_ref().map(Prompt::text);
        // Progress belongs to the in-flight job, not to `notice`: a render
        // finishing must not swallow the message from an `o` press.
        let rendering = raster_load.as_ref().and_then(|load| match load.kind {
            MediaKind::Pdf { page, .. } => Some(format!("rendering page {page}…")),
            _ => None,
        });
        let status = notice.as_deref().or(rendering.as_deref());
        let draw = terminal.draw(|frame| match &mut mode {
            ViewMode::Preview(doc) => {
                (page, preview_body) = draw_doc(
                    frame,
                    doc,
                    theme,
                    matches!(current, Some(Request::File { .. })),
                    status,
                );
            }
            ViewMode::Edit(editor) => {
                (page, edit_width, edit_body) = draw_editor(frame, editor, theme, prompt_text);
            }
        });
        if let Err(e) = draw {
            break Err(e);
        }
        let media = match &mode {
            ViewMode::Preview(doc) => doc.media.as_ref(),
            ViewMode::Edit(_) => None,
        };
        let painted = graphics.sync(media, preview_body);
        // A grown pane re-rasterizes vector media instead of upscaling it.
        // The cell size arrives asynchronously, so this also picks up the
        // first probe after startup.
        let geometry_changed = std::mem::take(&mut resized) || painted == Painted::Repaint;
        if geometry_changed
            && let ViewMode::Preview(doc) = &mode
            && let Some(media) = &doc.media
            && let Some(have) = media.raster
        {
            let want = raster_box(terminal_cells(), graphics.cell);
            if needs_rerender(have, want) {
                raster_goal = Some(RasterGoal {
                    kind: media.kind.clone(),
                    raster: want,
                });
            }
        }
        if painted == Painted::Repaint && repaints < MAX_CHAINED_REPAINTS {
            // The layer came up (or fell back): redraw before waiting for
            // input, so the body never sits blank or mosaicked for a tick.
            // Bounded, so no state can turn this into a spin.
            repaints += 1;
            continue;
        }
        repaints = 0;
        if identity_pending {
            report_identity(
                &mode,
                current.as_ref().map(Request::doc_key).as_deref(),
                control,
            );
            identity_pending = false;
        }
        let mut should_close = false;
        let poll = if preview_load.is_some() || raster_load.is_some() {
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
                                                raster_box(terminal_cells(), graphics.cell),
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
                                    raster_box(terminal_cells(), graphics.cell),
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
                                                    raster_box(terminal_cells(), graphics.cell),
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
                                                    raster_box(terminal_cells(), graphics.cell),
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
                                        notice = Some(match doc.selected_text() {
                                            Some(text) => {
                                                match crate::actions::copy_to_clipboard(&text) {
                                                    Ok(()) => "copied selection".into(),
                                                    Err(error) => {
                                                        format!("clipboard unavailable: {error}")
                                                    }
                                                }
                                            }
                                            None => "select text before copying".into(),
                                        });
                                    }
                                    KeyCode::Esc | KeyCode::Char('q') => {
                                        should_close = close_own_pane(control);
                                    }
                                    code if is_page_key(code, key.modifiers)
                                        && doc.media.as_ref().is_some_and(|media| {
                                            matches!(media.kind, MediaKind::Pdf { .. })
                                        }) =>
                                    {
                                        // Count from the page already asked for, so repeated
                                        // presses keep advancing while one render is in flight.
                                        let requested = raster_goal
                                            .as_ref()
                                            .map(|goal| &goal.kind)
                                            .or(raster_load.as_ref().map(|load| &load.kind))
                                            .and_then(pdf_position)
                                            .map(|(page, _)| page);
                                        if let (Some(media), Some(Request::File { path, .. })) =
                                            (&mut doc.media, current.as_ref())
                                            && let Some((shown, count)) = pdf_position(&media.kind)
                                            && let Some(moved) =
                                                page_move(shown, requested, count, code)
                                            && let MediaKind::Pdf { pages, .. } = &media.kind
                                        {
                                            let pages = pages.clone();
                                            let raster = media.raster.unwrap_or_else(|| {
                                                raster_box(terminal_cells(), graphics.cell)
                                            });
                                            let stamp = file_stamp(path);
                                            raster_goal = None;
                                            raster_load = None;
                                            if let PageMove::Show(page) = moved {
                                                let kind = MediaKind::Pdf { page, pages };
                                                if let Some(pixels) =
                                                    media.take_cached(page, raster, stamp)
                                                {
                                                    media.replace(pixels, kind, raster, stamp);
                                                    doc.context = media.context(path);
                                                    doc.rows_key = None;
                                                } else {
                                                    raster_goal = Some(RasterGoal { kind, raster });
                                                }
                                            }
                                        }
                                    }
                                    KeyCode::Char('o') if doc.media.is_some() => {
                                        if let Some(Request::File { path, .. }) = current.as_ref() {
                                            notice =
                                                Some(match crate::actions::open_external(path) {
                                                    Ok(()) => "opened with the default app".into(),
                                                    Err(error) => {
                                                        format!("could not open: {error}")
                                                    }
                                                });
                                        }
                                    }
                                    KeyCode::Char('e') => {
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
                                    KeyCode::PageUp => doc.scroll = doc.scroll.saturating_sub(page),
                                    KeyCode::PageDown => doc.scroll = (doc.scroll + page).min(max),
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
                                                raster_box(terminal_cells(), graphics.cell),
                                            );
                                        }
                                    }
                                    EditAction::Close => {
                                        if editor.dirty {
                                            prompt = Some(Prompt::Unsaved(Pending::Close));
                                        } else {
                                            should_close = close_own_pane(control);
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
                                should_close = close_own_pane(control);
                            }
                            _ => doc.on_mouse(&mouse, preview_body),
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
                                should_close = close_own_pane(control);
                            }
                        }
                        _ if prompt.is_none() => editor.on_mouse(&mouse, edit_body),
                        _ => {}
                    },
                },
                Event::Resize(..) => {
                    graphics.resized();
                    resized = true;
                }
                _ => {} // focus etc: redraw
            }
        }
        if should_close {
            break Ok(());
        }

        // These checks run after every iteration, including sustained input;
        // otherwise a held key can starve the liveness heartbeat indefinitely.
        if last_heartbeat.elapsed() >= Duration::from_secs(5) {
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
            last_heartbeat = Instant::now();
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
                    if close_own_pane(control) {
                        break Ok(());
                    }
                } else {
                    mode = ViewMode::Preview(loading_doc(&request));
                    current = Some(request.clone());
                    preview_load = Some(start_preview_load(
                        request,
                        raster_box(terminal_cells(), graphics.cell),
                    ));
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
        let refreshed =
            diff_refresh
                .as_ref()
                .and_then(|(request, receiver)| match receiver.try_recv() {
                    Ok(doc) => Some((request.clone(), Some(doc))),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Some((request.clone(), None))
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                });
        if let Some((request, refreshed)) = refreshed {
            diff_refresh = None;
            if current.as_ref() == Some(&request)
                && let (ViewMode::Preview(doc), Some(refreshed)) = (&mut mode, refreshed)
            {
                apply_diff_refresh(doc, refreshed);
            }
        }
        if last_diff_refresh.elapsed() >= Duration::from_secs(2) {
            if preview_load.is_none()
                && diff_refresh.is_none()
                && matches!(mode, ViewMode::Preview(_))
                && let Some(request @ Request::Diff { .. }) = current.clone()
            {
                let worker_request = request.clone();
                let (sender, receiver) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    // Diffs never rasterize, so the context is irrelevant here.
                    let _ = sender.send(load(&worker_request, &LoadContext::default()));
                });
                diff_refresh = Some((request, receiver));
            }
            last_diff_refresh = Instant::now();
        }
    };
    graphics.clear();
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
        media.hint()
    } else if editable {
        format!(" drag select  Ctrl/Cmd+C copy  e edit  {wrap_hint}  q close")
    } else {
        format!(" drag select  Ctrl/Cmd+C copy  ↑↓ scroll  {wrap_hint}  q close")
    };
    frame.render_widget(Paragraph::new(Line::from(hint).dim()), footer);
    (usize::from(body.height).saturating_sub(1).max(1), body)
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
    sweep_orphan_controls(&list);
    // Route only within OUR space. A session-wide search reused another
    // project's ephemeral tab and focus jumped there, which reads as the
    // tree refusing to change.
    let my_workspace = crate::launch::workspace_of(&list, my_pane_id);
    let mut previews: Vec<PreviewPane> = previews_in(&list)
        .into_iter()
        .filter(|p| p.workspace_id == my_workspace)
        .collect();
    for stale in previews.iter().filter(|preview| preview.stale) {
        let _ = std::fs::remove_file(&stale.control);
        if (stale.dedicated || stale.resumed) && tab_is_plugin_only(&list, &stale.tab_id) {
            let _ = ipc::call_text("tab.close", serde_json::json!({ "tab_id": stale.tab_id }));
        } else {
            let _ = ipc::call_text(
                "pane.close",
                serde_json::json!({ "pane_id": stale.pane_id }),
            );
        }
    }
    previews.retain(|preview| !preview.stale);
    previews.sort_by(|a, b| a.tab_id.cmp(&b.tab_id).then(a.pane_id.cmp(&b.pane_id)));
    // An inline viewer belongs to ONE tab: never route this tab's clicks into
    // another tab's pane, and never let a tab-mode click adopt one.
    previews
        .retain(|preview| preview.inline == inline && (!inline || preview.tab_id == caller_tab_id));
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
    if inline {
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
fn spawn_inline_pane(
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

/// Ask this tab's viewer to close (Esc from the sidebar). A live viewer owns
/// the close so an editor with unsaved changes can confirm first; only a stale
/// resumed pane is killed directly.
pub fn close_in_tab(my_pane_id: &str) {
    let Ok(json) = ipc::call_text("pane.list", serde_json::json!({})) else {
        return;
    };
    if let Some((id, stale)) = viewer_pane_in_tab(&json, my_pane_id) {
        if stale {
            if let Some(preview) = previews_in(&json)
                .into_iter()
                .find(|preview| preview.pane_id == id)
                && (preview.dedicated || preview.resumed)
                && tab_is_plugin_only(&json, &preview.tab_id)
            {
                close_preview_tab(&preview);
            } else {
                let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }));
            }
        } else {
            let control = previews_in(&json)
                .into_iter()
                .find(|preview| preview.pane_id == id)
                .map(|preview| preview.control)
                .unwrap_or_else(|| control_path_for_pane(&id));
            let _ = write_scratch_file(&control, "close");
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
fn previews_in(pane_list_json: &str) -> Vec<PreviewPane> {
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
            let _ = std::fs::remove_file(&control);
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
        let _ = std::fs::remove_file(&control);
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
    let _ = std::fs::remove_file(control);
    let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id }));
}

fn cleanup_moved_spawn(pane_id: &str, tab_id: &str, control: &Path) {
    let _ = std::fs::remove_file(control);
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
            rows: Vec::new(),
            rows_key: None,
            pending_src: None,
            selection: PreviewSelection::default(),
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
            &MediaPreview::new(pixels, MediaKind::Image, None, None, FileStamp::default()),
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
        assert!(is_pdf_file(Path::new("paper.PDF")));
        assert!(is_svg_file(Path::new("logo.svg")));
        assert!(!is_image_file(Path::new("logo.svg")));
    }

    fn pdf(page: usize, count: usize) -> MediaKind {
        MediaKind::Pdf {
            page,
            pages: std::sync::Arc::from(vec![
                PageSize {
                    width: 595.0,
                    height: 842.0,
                };
                count
            ]),
        }
    }

    fn stamp(len: u64) -> FileStamp {
        FileStamp {
            len,
            modified: None,
        }
    }

    fn media(kind: MediaKind, raster: Option<RasterBox>, len: u64) -> MediaPreview {
        MediaPreview::new(image::RgbaImage::new(1, 1), kind, raster, None, stamp(len))
    }

    #[test]
    fn pdf_paging_keys_stay_within_the_document() {
        use PageMove::Show;
        assert_eq!(
            page_move(1, None, Some(3), KeyCode::Char('n')),
            Some(Show(2))
        );
        assert_eq!(page_move(2, None, Some(3), KeyCode::PageUp), Some(Show(1)));
        assert_eq!(
            page_move(1, None, Some(3), KeyCode::Char('G')),
            Some(Show(3))
        );
        assert_eq!(page_move(3, None, Some(3), KeyCode::Home), Some(Show(1)));
        assert_eq!(page_move(3, None, Some(3), KeyCode::Char('n')), None);
        assert_eq!(page_move(1, None, Some(3), KeyCode::Char('p')), None);
        assert_eq!(page_move(1, None, Some(3), KeyCode::Char('x')), None);
        // Without pdfinfo the count is unknown: forward paging is still
        // allowed (pdftoppm reports the end), jumping to the end is not.
        assert_eq!(page_move(4, None, None, KeyCode::Char('n')), Some(Show(5)));
        assert_eq!(page_move(4, None, None, KeyCode::End), None);
        assert_eq!(pdf_position(&MediaKind::Image), None);
        assert_eq!(pdf_position(&pdf(2, 3)), Some((2, Some(3))));
        assert_eq!(pdf_position(&pdf(2, 0)), Some((2, None)));
    }

    #[test]
    fn held_paging_counts_from_the_page_already_requested() {
        use PageMove::Show;
        // Page 1 on screen, page 2 rendering: the next press asks for 3, so
        // a held key advances instead of re-requesting the same render.
        assert_eq!(
            page_move(1, Some(2), Some(9), KeyCode::Char('n')),
            Some(Show(3))
        );
        assert_eq!(
            page_move(1, Some(8), Some(9), KeyCode::Char('n')),
            Some(Show(9))
        );
        assert_eq!(page_move(1, Some(9), Some(9), KeyCode::Char('n')), None);
        // Paging back onto the page already displayed cancels that render
        // rather than queueing a second one.
        assert_eq!(
            page_move(1, Some(2), Some(9), KeyCode::Char('p')),
            Some(PageMove::Cancel)
        );
        assert_eq!(
            page_move(1, Some(4), Some(9), KeyCode::Home),
            Some(PageMove::Cancel)
        );
    }

    #[test]
    fn pdfinfo_page_sizes_honour_rotation() {
        let info = "Title:           Report\nPages:           2\n\
                    Page    1 size:  595.28 x 841.89 pts (A4)\nPage    1 rot:   0\n\
                    Page    2 size:  595.28 x 841.89 pts (A4)\nPage    2 rot:   270\n";
        let pages = parse_pdfinfo(info);
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].width, pages[0].height), (595.28, 841.89));
        assert_eq!((pages[1].width, pages[1].height), (841.89, 595.28));
        assert!(parse_pdfinfo("Syntax Error: not a PDF").is_empty());
    }

    #[test]
    fn raster_box_uses_reported_cell_pixels_or_the_half_block_grid() {
        let retina = crate::pane_graphics::CellSize {
            width_px: 17,
            height_px: 35,
        };
        // Header and footer rows are not part of the body.
        assert_eq!(raster_box((119, 53), Some(retina)), (119 * 17, 51 * 35));
        assert_eq!(raster_box((119, 53), None), (119, 102));
        let (width, height) = raster_box((400, 200), Some(retina));
        assert!(u64::from(width) * u64::from(height) <= MAX_MEDIA_PIXELS);
        assert!(width <= MAX_RASTER_SIDE && height <= MAX_RASTER_SIDE);
    }

    #[test]
    fn pdf_pages_rasterize_to_fit_the_body_box() {
        let a4 = PageSize {
            width: 595.0,
            height: 842.0,
        };
        // Height-bound in a wide pane: the long side is the box height.
        assert_eq!(pdf_scale_to(Some(a4), (2023, 1785)), 1785);
        // Width-bound in a tall pane: the long side follows the aspect.
        assert_eq!(pdf_scale_to(Some(a4), (595, 2000)), 842);
        // Unknown page size: never overflow either side.
        assert_eq!(pdf_scale_to(None, (2023, 1785)), 1785);
        assert_eq!(pdf_scale_to(Some(a4), (100_000, 100_000)), MAX_RASTER_SIDE);
    }

    #[test]
    fn vector_media_rerenders_only_when_the_pane_outgrows_it() {
        assert!(!needs_rerender((2000, 1800), (2000, 1800)));
        assert!(!needs_rerender((2000, 1800), (1000, 900)));
        assert!(!needs_rerender((2000, 1800), (2150, 1800)));
        assert!(needs_rerender((2000, 1800), (3000, 1800)));
        assert!(needs_rerender((2000, 1800), (2000, 2600)));
    }

    #[test]
    fn media_footers_offer_open_and_pdf_paging_but_not_editing() {
        let image = media(MediaKind::Image, None, 1);
        assert!(image.hint().contains("o open"));
        assert!(!image.hint().contains("e edit"));
        assert!(!image.hint().contains("wrap"));
        let page = media(pdf(2, 3), Some((10, 10)), 1);
        assert!(page.hint().contains("page 2/3"));
        assert!(page.hint().contains("n/p page"));
        assert!(page.context(Path::new("/tmp/a.pdf")).ends_with("page 2/3"));
    }

    #[test]
    fn media_painted_by_the_graphics_layer_leaves_the_body_blank() {
        let mut doc = media_doc(
            Path::new("/tmp/red.png"),
            "red.png".into(),
            Ok(MediaPreview::new(
                image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255])),
                MediaKind::Image,
                None,
                None,
                FileStamp::default(),
            )),
        );
        doc.relayout(8, 4);
        assert!(!doc.rows.is_empty(), "half blocks without the layer");
        doc.media.as_mut().unwrap().overlay = true;
        doc.rows_key = None;
        doc.relayout(8, 4);
        assert!(doc.rows.is_empty(), "no half blocks under the layer");
    }

    #[test]
    fn turning_pages_keeps_recent_ones_cached() {
        let mut media = MediaPreview::new(
            image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 0, 0, 255])),
            pdf(1, 3),
            Some((10, 10)),
            None,
            stamp(7),
        );
        let first_id = media.id;
        media.replace(image::RgbaImage::new(2, 2), pdf(2, 3), (10, 10), stamp(7));
        assert_ne!(media.id, first_id);
        assert_eq!(media.source_width, 2);
        let cached = media
            .take_cached(1, (10, 10), stamp(7))
            .expect("page 1 cached");
        assert_eq!(*cached.get_pixel(0, 0), image::Rgba([1, 0, 0, 255]));
        assert!(media.take_cached(1, (20, 20), stamp(7)).is_none());
    }

    #[test]
    fn a_rebuilt_file_drops_every_page_of_the_old_revision() {
        let mut media = MediaPreview::new(
            image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 0, 0, 255])),
            pdf(1, 3),
            Some((10, 10)),
            None,
            stamp(7),
        );
        media.replace(image::RgbaImage::new(2, 2), pdf(2, 3), (10, 10), stamp(7));
        // The file was rebuilt while page 3 rendered: the cached page 2 and
        // the cached page 1 belong to the previous revision.
        media.replace(image::RgbaImage::new(3, 3), pdf(3, 3), (10, 10), stamp(9));
        assert!(media.page_cache.is_empty(), "old revision kept");
        assert!(media.take_cached(1, (10, 10), stamp(9)).is_none());
        // A page cached under the new revision is served again.
        media.replace(image::RgbaImage::new(4, 4), pdf(1, 3), (10, 10), stamp(9));
        assert!(media.take_cached(3, (10, 10), stamp(9)).is_some());
        assert!(media.take_cached(3, (10, 10), stamp(11)).is_none());
    }

    #[test]
    fn the_page_cache_is_bounded_by_bytes() {
        let big = || {
            // 2 MP: four of these blow well past the cache budget.
            image::RgbaImage::new(2000, 1000)
        };
        let mut media = MediaPreview::new(big(), pdf(1, 20), Some((10, 10)), None, stamp(7));
        for page in 2..=12 {
            media.replace(big(), pdf(page, 20), (10, 10), stamp(7));
        }
        let cached: u64 = media
            .page_cache
            .iter()
            .map(|page| u64::from(page.pixels.width()) * u64::from(page.pixels.height()) * 4)
            .sum();
        assert!(cached <= PAGE_CACHE_MAX_BYTES, "cached {cached} bytes");
        assert!(!media.page_cache.is_empty(), "nothing cached at all");
        // A raster box the pane outgrew can never be shown again.
        media.replace(big(), pdf(13, 20), (40, 40), stamp(7));
        assert!(
            media.page_cache.iter().all(|page| page.raster == (40, 40)),
            "stale raster boxes kept"
        );
    }

    #[test]
    fn page_keys_ignore_control_and_alt() {
        assert!(is_page_key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(is_page_key(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert!(!is_page_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(!is_page_key(KeyCode::PageDown, KeyModifiers::ALT));
        assert!(!is_page_key(KeyCode::Char('x'), KeyModifiers::NONE));
    }

    /// The shared bounded runner still drives ffmpeg (skipped without it).
    #[test]
    fn installed_ffmpeg_extracts_a_poster_frame() {
        let Some(ffmpeg) = executable_on_path("ffmpeg") else {
            eprintln!("skipped: ffmpeg not on PATH");
            return;
        };
        let root = std::env::temp_dir().join(format!("viewer-video-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let clip = root.join("clip.mp4");
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=1",
                "-frames:v",
                "1",
            ])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(status.success());
        let doc = load_file(&clip, None, &test_context());
        let media = doc.media.as_ref().expect("poster frame");
        assert_eq!(media.kind, MediaKind::VideoPoster);
        // The poster is fitted into 1280×720, keeping the 4:3 aspect.
        assert_eq!((media.source_width, media.source_height), (960, 720));
        std::fs::remove_dir_all(root).unwrap();
    }

    fn test_context() -> LoadContext {
        LoadContext {
            raster: (600, 400),
            cancel: Cancel::default(),
        }
    }

    /// An SVG the renderer rejects always falls back to its source text —
    /// with rsvg-convert installed it fails at render time, without it the
    /// extension never reaches the renderer.
    #[test]
    fn an_unrenderable_svg_previews_as_its_source_text() {
        let root = std::env::temp_dir().join(format!("viewer-broken-svg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let broken = root.join("broken.svg");
        std::fs::write(&broken, "<svg this is not xml").unwrap();
        let doc = load_file(&broken, None, &test_context());
        assert!(doc.media.is_none());
        assert!(row_texts(&build_rows(&doc.lines, false, false, 80))[0].contains("<svg"));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Exercises the real rasterizers where they are installed (skipped
    /// otherwise): an SVG, and a PDF made from it, render as media.
    #[test]
    fn installed_rasterizers_render_svg_and_pdf() {
        let Some(rsvg) = executable_on_path("rsvg-convert") else {
            eprintln!("skipped: rsvg-convert not on PATH");
            return;
        };
        let root = std::env::temp_dir().join(format!("viewer-vector-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let svg = root.join("shape.svg");
        std::fs::write(
            &svg,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="#f00"/></svg>"##,
        )
        .unwrap();
        let doc = load_file(&svg, None, &test_context());
        let media = doc.media.as_ref().expect("svg renders as media");
        assert_eq!(media.kind, MediaKind::Svg);
        assert!(media.source_width > media.source_height);
        assert_eq!(media.stamp, file_stamp(&svg));

        if executable_on_path("pdftoppm").is_some() {
            let pdf_path = root.join("shape.pdf");
            let status = std::process::Command::new(rsvg)
                .args(["--format", "pdf", "--output"])
                .arg(&pdf_path)
                .arg(&svg)
                .status()
                .unwrap();
            assert!(status.success());
            let doc = load_file(&pdf_path, None, &test_context());
            let media = doc.media.as_ref().expect("pdf renders as media");
            assert!(matches!(media.kind, MediaKind::Pdf { page: 1, .. }));
            assert_eq!(
                media.tool.as_deref(),
                executable_on_path("pdftoppm").as_deref()
            );

            // A cancelled load stops before it spawns anything.
            let cancelled = LoadContext {
                raster: (600, 400),
                cancel: Cancel::default(),
            };
            cancelled.cancel.cancel();
            let doc = load_file(&pdf_path, None, &cancelled);
            assert!(doc.media.is_none(), "cancelled load still rasterized");
        } else {
            eprintln!("skipped PDF: pdftoppm not on PATH");
        }
        std::fs::remove_dir_all(root).unwrap();
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
        let doc = load_file(&image_path, None, &test_context());
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
    fn helper_lookup_skips_project_local_path_entries() {
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
            executable_in_path("ffmpeg", &search_path, Some(&project)),
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
        let doc = load_file(&path, None, &test_context());
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

    #[cfg(unix)]
    #[test]
    fn scratch_dir_is_private_to_the_owning_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "scratch dir must not be group/world readable or writable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_scratch_file_refuses_to_follow_a_preexisting_symlink() {
        use std::os::unix::fs::symlink;
        let dir = scratch_dir();
        let victim = dir.join(format!("aa-victim-{}.txt", std::process::id()));
        let link = dir.join(format!("aa-link-{}.ctl", std::process::id()));
        std::fs::write(&victim, "original victim contents").unwrap();
        let _ = std::fs::remove_file(&link);
        symlink(&victim, &link).unwrap();

        write_scratch_file(&link, "payload").unwrap();

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

        let _ = std::fs::remove_file(&victim);
        let _ = std::fs::remove_file(&link);
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
    fn line_target_anchors_plain_and_markdown_source_previews() {
        let root =
            std::env::temp_dir().join(format!("herdr-sidebar-line-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for name in ["sample.txt", "sample.md"] {
            let path = root.join(name);
            std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
            let doc = load_file(&path, Some(2), &test_context());
            assert_eq!(doc.pending_src, Some(1));
            assert!(
                doc.numbered,
                "line-target markdown must keep source mapping"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn glow_markdown_returns_styled_spans() {
        // Skip if glow is not installed
        if std::process::Command::new("glow")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
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
