//! The preview pane: file contents AND git diffs, opened beside the sidebar
//! (the tree stays visible, like VS Code's editor area). The sidebar keeps
//! ONE viewer pane per tab and steers it through a small CONTROL FILE: each
//! click writes a request there; the running viewer polls it and reloads in
//! place, so repeated clicks never churn panes. Diff requests re-run git
//! every couple of seconds, so the diff live-updates while you edit.
//! `q`/Esc (or clicking the ✕ header) closes the pane itself.
//!
//! The tail of this module is the CLIENT side — the request format plus the
//! ensure-a-viewer-pane logic both sidebar views share.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::ansi;
use crate::icons::{IconTheme, icon};
use crate::ipc;

/// Metadata source/token that marks the viewer pane, so the sidebar can find
/// and reuse it (distinct from the sidebar's own identity tokens).
pub const METADATA_SOURCE: &str = "herdr-sidebar-preview";

/// How often the control file is re-checked while idle.
const POLL: Duration = Duration::from_millis(250);

/// Preview size guards: don't slurp huge files into a pane.
const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 5000;

/// Directory for the sidebar's private scratch files (control/park files).
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
    if std::fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        std::fs::remove_file(path)?;
    }
    std::fs::write(path, contents)
}

/// The control file a viewer watches, named after the VIEWER's own pane.
///
/// The path is argv for the viewer process, fixed for its life, so it must
/// key on something that outlives a tab move — the preview pane itself. The
/// sidebar's pane id would be wrong (it churns on every redeploy and
/// ensure-hook heal) and so would the tab (previews get moved between tabs).
pub fn control_path_for_pane(preview_pane_id: &str) -> PathBuf {
    scratch_dir().join(format!(
        "herdr-sidebar-preview-{}.ctl",
        preview_pane_id.replace(':', "_")
    ))
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

/// The tab name for a document. `*` marks the tab as EPHEMERAL — the next
/// file will overwrite it — so a pinned tab reads as a plain name, the way
/// a settled document should. `tab.rename` is the only display lever herdr
/// gives a plugin.
pub fn tab_label(doc_key: &str, pinned: bool) -> String {
    let name = doc_key
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(doc_key);
    if pinned { name.to_string() } else { format!("*{name}") }
}

/// Inverse of [`tab_label`]: the displayed name and whether it is pinned.
pub fn parse_tab_label(label: &str) -> (String, bool) {
    match label.strip_prefix('*') {
        Some(rest) => (rest.to_string(), false),
        None => (label.to_string(), true),
    }
}

impl Request {
    /// The document identity this request renders.
    fn doc_key(&self) -> String {
        match self {
            Self::File(p) => doc_key_for_file(p),
            Self::Diff { root, rel, kind } => doc_key_for_diff(root, rel, kind),
            Self::Show { root, spec, path } => doc_key_for_show(root, spec, path.as_deref()),
        }
    }
}

/// What the sidebar asked the viewer to show.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Request {
    File(PathBuf),
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
        Some("file") => Some(Request::File(PathBuf::from(parts.next()?))),
        // Legacy: a bare path.
        _ => Some(Request::File(PathBuf::from(raw))),
    }
}

struct Doc {
    name: String,
    context: String,
    lines: Vec<Line<'static>>,
    /// File previews get a line-number gutter; diffs carry their own +/-.
    numbered: bool,
    scroll: usize,
}

fn load(request: &Request) -> Doc {
    match request {
        Request::File(path) => load_file(path),
        Request::Diff { root, rel, kind } => load_diff(root, rel, kind),
        Request::Show { root, spec, path } => load_show(root, spec, path.as_deref()),
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
    let output = std::process::Command::new("git").args(&args).current_dir(root).output();
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
        scroll: 0,
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

fn load_file(target: &Path) -> Doc {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    let lower = name.to_lowercase();
    let is_markdown = lower.ends_with(".md") || lower.ends_with(".markdown");
    let (lines, numbered) = match std::fs::read(target) {
        Err(e) => (vec![Line::raw(format!("(unreadable: {e})"))], true),
        Ok(bytes) => {
            let head = &bytes[..bytes.len().min(8192)];
            if head.contains(&0) {
                (vec![Line::raw(format!("(binary file — {} bytes)", bytes.len()))], false)
            } else {
                let truncated = bytes.len() > MAX_BYTES;
                let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]);
                // Markdown: render via glow; fall back to syntax highlight on failure.
                // Width is approximated by subtracting 6 for the sidebar share and
                // line-number gutter; ideal fix is to pass body.width from draw_doc.
                let glow_width =
                    crossterm::terminal::size().map(|(w, _)| w.saturating_sub(6)).unwrap_or(74);
                let glow_rendered =
                    is_markdown.then(|| glow_markdown(&text, glow_width)).flatten();
                // Glow-rendered markdown gets no line numbers (it formats its own layout).
                let numbered = glow_rendered.is_none();
                let mut lines: Vec<Line<'static>> = if let Some(rendered) = glow_rendered {
                    rendered
                } else {
                    crate::syntax::highlight(&name, &text, MAX_LINES).unwrap_or_else(|| {
                        text.lines().take(MAX_LINES).map(|l| Line::raw(l.to_string())).collect()
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
        scroll: 0,
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
            args.push(if cfg!(windows) { "NUL".into() } else { "/dev/null".into() });
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
        scroll: 0,
    }
}

fn read_control(control: &Path) -> Option<Request> {
    let mut buf = String::new();
    std::fs::File::open(control).ok()?.read_to_string(&mut buf).ok()?;
    parse_request(&buf)
}

/// Tag our pane (heartbeat-stamped, see launch::HEARTBEAT_STALE_SECS), record
/// WHICH document we show so any sidebar can route clicks to us, and title the
/// pane with the document's name. A `None` key clears the token — showing
/// nothing must not look like a preview of the empty path.
fn report_identity(doc_name: &str, doc_key: Option<&str>) {
    let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else { return };
    if pane_id.is_empty() {
        return;
    }
    let _ = ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": METADATA_SOURCE,
            "tokens": {
                METADATA_SOURCE: crate::state::unix_now().to_string(),
                TOKEN_PATH: doc_key,
            },
        }),
    );
    let _ = ipc::call_text(
        "pane.rename",
        serde_json::json!({ "pane_id": pane_id, "label": doc_name }),
    );
}

/// Close our own pane (ends this process with it), handing focus back to
/// the sidebar in our tab so the user lands where they were clicking.
fn close_own_pane() {
    let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else { return };
    if pane_id.is_empty() {
        return;
    }
    if let Ok(list) = ipc::call_text("pane.list", serde_json::json!({})) {
        let tab = crate::launch::tab_of(&list, &pane_id);
        if let Some(sidebar) = sidebar_in_tab(&list, &tab) {
            let _ = ipc::call_text("pane.focus", serde_json::json!({ "pane_id": sidebar }));
        }
    }
    let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id }));
}

/// The sidebar pane in `tab`, so a closing preview can hand focus back.
fn sidebar_in_tab(pane_list_json: &str, tab: &str) -> Option<String> {
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
        tokens: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let msg = serde_json::from_str::<Msg>(crate::launch::strip_bom(pane_list_json)).ok()?;
    msg.result
        .panes
        .into_iter()
        .filter(|p| p.tab_id.as_deref() == Some(tab))
        .find(|p| {
            p.tokens.contains_key("herdr-sidebar-explorer")
                || p.tokens.contains_key("herdr-sidebar-git")
        })?
        .pane_id
}

/// The viewer's event loop; returns when the user closes it.
pub fn run(control: &Path) -> std::io::Result<()> {
    let theme = IconTheme::resolve(
        std::env::var("HERDR_SIDEBAR_ICONS")
            .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
            .ok()
            .as_deref(),
        crate::state::load_state().icons,
    );
    let mut current = read_control(control);
    let mut doc = current.as_ref().map(load).unwrap_or_else(|| Doc {
        name: "(nothing to show)".into(),
        context: String::new(),
        lines: vec![Line::raw("(waiting for a click in the sidebar)")],
        numbered: false,
        scroll: 0,
    });
    report_identity(&doc.name, current.as_ref().map(Request::doc_key).as_deref());

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
    let mut beat: u64 = 0;
    let result = loop {
        let draw = terminal.draw(|frame| page = draw_doc(frame, &mut doc, theme));
        if let Err(e) = draw {
            break Err(e);
        }
        let max = doc.lines.len().saturating_sub(1);
        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        close_own_pane();
                        break Ok(());
                    }
                    KeyCode::Up | KeyCode::Char('k') => doc.scroll = doc.scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => doc.scroll = (doc.scroll + 1).min(max),
                    KeyCode::PageUp => doc.scroll = doc.scroll.saturating_sub(page),
                    KeyCode::PageDown => doc.scroll = (doc.scroll + page).min(max),
                    KeyCode::Home | KeyCode::Char('g') => doc.scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => doc.scroll = max,
                    _ => {}
                },
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => doc.scroll = doc.scroll.saturating_sub(3),
                    MouseEventKind::ScrollDown => doc.scroll = (doc.scroll + 3).min(max),
                    MouseEventKind::Down(MouseButton::Left) if mouse.row == 0 => {
                        close_own_pane();
                        break Ok(());
                    }
                    _ => {}
                },
                _ => {} // resize etc: redraw
            }
        } else {
            // Idle: heartbeat, follow the control file, and live-refresh diffs.
            beat += 1;
            if beat.is_multiple_of(20) {
                report_identity(&doc.name, current.as_ref().map(Request::doc_key).as_deref());
            }
            let target = read_control(control);
            if target != current {
                current = target;
                if let Some(request) = &current {
                    doc = load(request);
                    report_identity(&doc.name, Some(&request.doc_key()));
                }
            } else if beat.is_multiple_of(8)
                && let Some(request @ Request::Diff { .. }) = &current
            {
                let keep = doc.scroll;
                doc = load(request);
                doc.scroll = keep.min(doc.lines.len().saturating_sub(1));
            }
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Header (✕ close + name + context), body, hint footer. Returns the page
/// stride for PageUp/Down.
fn draw_doc(frame: &mut Frame, doc: &mut Doc, theme: IconTheme) -> usize {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    doc.scroll = doc
        .scroll
        .min(doc.lines.len().saturating_sub(usize::from(body.height).max(1)));

    let file_icon = icon(theme, &doc.name, false, false);
    let icon_style = match file_icon.rgb {
        Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)),
        None => Style::default(),
    };
    let left = vec![
        Span::styled(" ✕ ", Style::default().bold().fg(Color::LightBlue)),
        Span::styled(format!("{} ", file_icon.glyph), icon_style),
        Span::styled(doc.name.clone(), Style::default().bold()),
    ];
    let used: usize = left.iter().map(Span::width).sum();
    let avail = usize::from(area.width).saturating_sub(used + 2);
    let shown = if doc.context.chars().count() > avail {
        let tail: String = doc
            .context
            .chars()
            .skip(doc.context.chars().count().saturating_sub(avail.saturating_sub(1)))
            .collect();
        format!("…{tail}")
    } else {
        doc.context.clone()
    };
    let mut spans = left;
    spans.push(Span::styled(format!("  {shown}"), Style::default().dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), header);

    let number_width = doc.lines.len().to_string().len();
    let text: Vec<Line> = doc
        .lines
        .iter()
        .enumerate()
        .skip(doc.scroll)
        .take(usize::from(body.height))
        .map(|(n, line)| {
            if doc.numbered {
                let mut spans =
                    vec![Span::styled(format!("{:>number_width$} ", n + 1), Style::default().dim())];
                spans.extend(line.spans.iter().cloned());
                Line::from(spans)
            } else {
                let mut line = line.clone();
                // Tinted diff rows fill the full row, like an editor.
                if line.style.bg.is_some() {
                    let pad = usize::from(body.width).saturating_sub(line.width());
                    if pad > 0 {
                        line.spans.push(Span::raw(" ".repeat(pad)));
                    }
                }
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(text), body);

    frame.render_widget(
        Paragraph::new(Line::from(" ↑↓ scroll  ⇞⇟ page  g G ends  q close".dim())),
        footer,
    );
    usize::from(body.height).saturating_sub(1).max(1)
}

// ---------------------------------------------------------------------------
// Client side: how the sidebar views open things in the viewer pane.
// ---------------------------------------------------------------------------

/// Open `payload` (identified by `doc_key`) following VS Code tab rules:
/// jump to the document's existing tab, else overwrite the one ephemeral
/// tab, else create a tab for it.
pub fn open_in_pane(
    my_pane_id: &str,
    spawn_cwd: &Path,
    doc_key: &str,
    payload: &str,
) -> Result<PreviewTarget, String> {
    let list = ipc::call_text("pane.list", serde_json::json!({}))
        .map_err(|e| format!("preview failed: {e}"))?;
    let previews = previews_in(&list);

    // 1. Already open — jump to it, pinned or not.
    if let Some(p) = preview_for_doc(&previews, doc_key) {
        let _ = ipc::call_text("tab.focus", serde_json::json!({ "tab_id": p.tab_id }));
        return Ok(PreviewTarget { pane_id: p.pane_id, tab_id: p.tab_id });
    }

    // 2. Overwrite the ephemeral tab.
    if let Some(p) = reusable_preview(&previews) {
        write_scratch_file(&control_path_for_pane(&p.pane_id), payload)
            .map_err(|e| format!("preview failed: {e}"))?;
        let _ = ipc::call_text(
            "tab.rename",
            serde_json::json!({ "tab_id": p.tab_id, "label": tab_label(doc_key, false) }),
        );
        let _ = ipc::call_text("tab.focus", serde_json::json!({ "tab_id": p.tab_id }));
        return Ok(PreviewTarget { pane_id: p.pane_id, tab_id: p.tab_id });
    }

    // 3. Nothing reusable — a tab of its own.
    spawn_preview_tab(my_pane_id, spawn_cwd, doc_key, payload)
}

/// Where a preview request landed. Handed back so a double click can pin
/// exactly the tab its first click used — searching by document key would
/// race the viewer, which only stamps `hs-preview-path` once it has started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewTarget {
    pub pane_id: String,
    pub tab_id: String,
}

/// Mark a preview's tab pinned: stamp the token so it stops being reusable,
/// and prefix its tab name with `*`. Idempotent.
pub fn pin_target(target: &PreviewTarget, doc_key: &str) {
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
}

/// Spawn a preview and give it its own tab. The pane is split beside the
/// sidebar first and then MOVED out: `tab.create` would leave a stray shell
/// pane, and the move reuses the proven `pane.move` path. The `tab.created`
/// hook docks a sidebar alongside it, so the tree stays reachable.
fn spawn_preview_tab(
    my_pane_id: &str,
    spawn_cwd: &Path,
    doc_key: &str,
    payload: &str,
) -> Result<PreviewTarget, String> {
    let new_pane = spawn_viewer_pane(my_pane_id, spawn_cwd, payload)?;
    let _ = ipc::call_text(
        "pane.move",
        serde_json::json!({
            "pane_id": new_pane,
            "destination": { "type": "new_tab", "label": tab_label(doc_key, false) },
            "focus": true,
        }),
    );
    let tab_id = ipc::call_text("pane.list", serde_json::json!({}))
        .map(|list| crate::launch::tab_of(&list, &new_pane))
        .unwrap_or_default();
    Ok(PreviewTarget { pane_id: new_pane, tab_id })
}

/// The owner pane's share of its tab width right now — `None` when the
/// geometry carries no signal (a full-width sidebar in a stacked or
/// single-pane layout would read as ~1.0 and, clamped, once produced a
/// half-tab sidebar — user-reported).
fn owner_frac(owner: &str) -> Option<f64> {
    let layout = ipc::call_text("pane.layout", serde_json::json!({ "pane_id": owner })).ok()?;
    let msg = serde_json::from_str::<LayoutMsg2>(&layout).ok()?;
    let body = msg.result.layout;
    body.panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(owner))
        .and_then(|p| p.rect)
        .map(|r| r.width as f64 / body.area.width.max(1) as f64)
        .filter(|f| (0.02..0.55).contains(f))
        .map(|f| f.max(0.1))
}

/// Pin the sidebar's width after the preview opens: in full mode the tab is
/// sidebar | viewer, so the root split ratio IS the sidebar's share. Makes
/// the width deterministic no matter which spawn path ran.
fn enforce_owner_width(owner: &str, frac: f64) {
    let _ = ipc::call_text(
        "layout.set_split_ratio",
        serde_json::json!({ "pane_id": owner, "path": [], "ratio": frac }),
    );
}

/// Close this tab's viewer pane if one is open (Esc from the sidebar),
/// bringing any parked panes home first.
pub fn close_in_tab(my_pane_id: &str) {
    restore_parked(my_pane_id);
    let Ok(json) = ipc::call_text("pane.list", serde_json::json!({})) else {
        return;
    };
    if let Some((id, _)) = viewer_pane_in_tab(&json, my_pane_id) {
        let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }));
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
    // Token match finds a live viewer; a "Preview"-labeled pane WITHOUT the
    // token is a resumed corpse (labels survive server restarts, tokens
    // don't) — report it too, with a missing token, so the stale check
    // below flags it and the caller closes it instead of spawning a twin.
    let viewer = panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(my_tab.as_str()))
        .find(|p| {
            p.tokens.contains_key(METADATA_SOURCE) || p.label.as_deref() == Some("Preview")
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

/// A live preview pane and the document it is showing. State lives on the
/// pane, so it cannot outlive what it describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewPane {
    pub pane_id: String,
    pub tab_id: String,
    pub doc_key: String,
    pub pinned: bool,
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
            let doc_key = p.tokens.get(TOKEN_PATH)?.as_str()?.to_string();
            Some(PreviewPane {
                pane_id: p.pane_id?,
                tab_id: p.tab_id?,
                doc_key,
                pinned: p.tokens.contains_key(TOKEN_PINNED),
            })
        })
        .collect()
}

/// The tab already showing `doc_key`, pinned or not — checked FIRST so
/// re-selecting an open file jumps instead of clobbering the ephemeral tab.
fn preview_for_doc(previews: &[PreviewPane], doc_key: &str) -> Option<PreviewPane> {
    previews.iter().find(|p| p.doc_key == doc_key).cloned()
}

/// The ephemeral tab, if one exists. Pinned tabs are never overwritten.
fn reusable_preview(previews: &[PreviewPane]) -> Option<PreviewPane> {
    previews.iter().find(|p| !p.pinned).cloned()
}

/// Split a viewer pane directly to the caller's right: split the right
/// NEIGHBOR and swap the fresh pane into its left slot (split only goes
/// right/down), so the layout reads sidebar | preview | rest.
fn spawn_viewer_pane(
    my_pane_id: &str,
    spawn_cwd: &Path,
    payload: &str,
) -> Result<String, String> {
    let layout = ipc::call_text("pane.layout", serde_json::json!({ "pane_id": my_pane_id })).ok();
    let neighbor = layout.as_deref().and_then(|json| right_neighbor(json, my_pane_id));
    // Splitting ourselves (no neighbor — e.g. everything else just parked,
    // leaving us momentarily full-width): keep the width the sidebar had
    // BEFORE the park, not a ballooned 30-50%.
    let own_frac = 0.3;
    let (target, ratio, needs_swap) = match &neighbor {
        Some(id) => (id.clone(), 0.5, true),
        None => (my_pane_id.to_string(), own_frac, false),
    };
    let response = ipc::call_text(
        "pane.split",
        serde_json::json!({
            "target_pane_id": target,
            "direction": "right",
            "ratio": ratio,
            "focus": false,
            "cwd": spawn_cwd.display().to_string(),
            "env": crate::state::spawn_env(),
        }),
    );
    let new_pane = response
        .ok()
        .and_then(|r| crate::launch::split_pane_id(&r))
        .ok_or_else(|| "preview pane failed to open".to_string())?;
    if needs_swap {
        let _ = ipc::call_text(
            "pane.swap",
            serde_json::json!({ "source_pane_id": new_pane, "target_pane_id": target }),
        );
    }
    // Seed the control file BEFORE launching: the path is argv, so it is
    // fixed for the process's life and must name the pane, not its tab.
    let control = control_path_for_pane(&new_pane);
    write_scratch_file(&control, payload).map_err(|e| format!("preview failed: {e}"))?;
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "herdr-sidebar".to_string());
    #[cfg(windows)]
    let command = format!("& \"{exe}\" --preview \"{}\"", control.display());
    #[cfg(not(windows))]
    let command = format!("exec \"{exe}\" --preview \"{}\"", control.display());
    let _ = ipc::call_text(
        "pane.send_input",
        serde_json::json!({ "pane_id": new_pane, "text": command, "keys": ["Enter"] }),
    );
    let _ = ipc::call_text(
        "pane.rename",
        serde_json::json!({ "pane_id": new_pane, "label": "Preview" }),
    );
    // The split/swap can move focus with the slot; stay in the sidebar so
    // the user keeps clicking.
    let _ = ipc::call_text("pane.focus", serde_json::json!({ "pane_id": my_pane_id }));
    Ok(new_pane)
}


// ---------------------------------------------------------------------------
// Full-size mode: park the tab's other panes while a preview is open.
// ---------------------------------------------------------------------------

/// Park plan for a TAB, recorded beside the control file so either process
/// (sidebar or viewer) can restore. Keyed by tab id and never by the
/// sidebar's pane id: the sidebar respawns with a fresh pane id on redeploy
/// and on ensure-hook healing, while the tab the parked panes must return to
/// does not change. A pane-id key made the "already parked?" guard miss, so
/// every preview re-parked into another new tab and orphaned the old plan.
fn park_path_for_tab(tab: &str) -> PathBuf {
    scratch_dir().join(format!("herdr-sidebar-park-{}.json", tab.replace(':', "_")))
}

/// `owner`'s plan path resolved from a `pane list` payload; `None` when that
/// pane sits in no tab (it has already gone away).
fn park_path_in(pane_list_json: &str, owner: &str) -> Option<PathBuf> {
    let tab = crate::launch::tab_of(pane_list_json, owner);
    (!tab.is_empty()).then(|| park_path_for_tab(&tab))
}

/// Live lookup of `owner`'s plan path, falling back to OUR own pane's tab so
/// the viewer can still restore after the sidebar that spawned it is gone.
fn park_path(owner: &str) -> Option<PathBuf> {
    let list = ipc::call_text("pane.list", serde_json::json!({})).ok()?;
    park_path_in(&list, owner).or_else(|| {
        let me = std::env::var("HERDR_PANE_ID").ok()?;
        park_path_in(&list, &me)
    })
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug)]
struct RectJ {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ParkPlan {
    /// The tab the panes came from (and go back to).
    tab: String,
    /// Sidebar's share of the tab width at park time, for the re-split.
    owner_ratio: f64,
    /// Parked panes with their ORIGINAL rects, reading order.
    panes: Vec<(String, RectJ)>,
}

#[derive(serde::Deserialize)]
struct LayoutMsg2 {
    result: LayoutRes2,
}
#[derive(serde::Deserialize)]
struct LayoutRes2 {
    layout: LayoutBody2,
}
#[derive(serde::Deserialize)]
struct LayoutBody2 {
    area: RectJ,
    panes: Vec<LayoutPane2>,
}
#[derive(serde::Deserialize)]
struct LayoutPane2 {
    pane_id: Option<String>,
    rect: Option<RectJ>,
}

/// Pane ids in `owner`'s tab that are OURS (sidebar views / preview) and so
/// never get parked, from a `pane.list` response.
fn our_panes_in_tab(pane_list_json: &str, owner: &str) -> Vec<String> {
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
    let Ok(msg) = serde_json::from_str::<Msg>(pane_list_json.trim_start_matches('\u{feff}'))
    else {
        return vec![owner.to_string()];
    };
    let tab = msg
        .result
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(owner))
        .and_then(|p| p.tab_id.clone());
    msg.result
        .panes
        .iter()
        .filter(|p| p.tab_id == tab)
        .filter(|p| {
            p.pane_id.as_deref() == Some(owner)
                || matches!(
                    p.label.as_deref(),
                    Some("Sidebar" | "Explorer" | "Source Control" | "Preview")
                )
                || p.tokens.keys().any(|k| k.starts_with("herdr-sidebar"))
        })
        .filter_map(|p| p.pane_id.clone())
        .collect()
}

/// Move every non-ours pane of `owner`'s tab into a background tab and
/// record how to put them back. No-op when nothing to park or a plan
/// already exists.
fn park_others(owner: &str) {
    let Ok(list) = ipc::call_text("pane.list", serde_json::json!({})) else { return };
    let tab = crate::launch::tab_of(&list, owner);
    if tab.is_empty() {
        return;
    }
    let plan_path = park_path_for_tab(&tab);
    if plan_path.exists() {
        return;
    }
    let ours = our_panes_in_tab(&list, owner);
    let Ok(layout) = ipc::call_text("pane.layout", serde_json::json!({ "pane_id": owner }))
    else {
        return;
    };
    let Ok(msg) = serde_json::from_str::<LayoutMsg2>(&layout) else { return };
    let body = msg.result.layout;
    let owner_ratio = body
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(owner))
        .and_then(|p| p.rect)
        .map(|r| r.width as f64 / body.area.width.max(1) as f64)
        .filter(|f| (0.02..0.55).contains(f))
        .map(|f| f.max(0.1))
        .unwrap_or(0.3);
    let mut others: Vec<(String, RectJ)> = body
        .panes
        .into_iter()
        .filter_map(|p| Some((p.pane_id?, p.rect?)))
        .filter(|(id, _)| !ours.contains(id))
        .collect();
    if others.is_empty() {
        return;
    }
    others.sort_by_key(|(_, r)| (r.y, r.x));

    // First pane opens the park tab; the rest pile in (their layout there
    // is irrelevant — the tab is never focused).
    let mut park_tab = String::new();
    for (i, (id, _)) in others.iter().enumerate() {
        let dest = if i == 0 {
            serde_json::json!({ "type": "new_tab" })
        } else {
            serde_json::json!({ "type": "tab", "tab_id": park_tab, "split": "right" })
        };
        let _ = ipc::call_text(
            "pane.move",
            serde_json::json!({ "pane_id": id, "destination": dest, "focus": false }),
        );
        if i == 0 {
            if let Ok(list2) = ipc::call_text("pane.list", serde_json::json!({})) {
                park_tab = crate::launch::tab_of(&list2, id);
            }
            if park_tab.is_empty() || park_tab == tab {
                return; // move didn't take; don't strand a half-plan
            }
        }
    }
    let plan = ParkPlan { tab, owner_ratio, panes: others };
    if let Ok(json) = serde_json::to_string(&plan) {
        let _ = write_scratch_file(&plan_path, &json);
    }
}

/// Bring parked panes home, rebuilding their grid from the recorded rects
/// (each pane re-splits the recorded left/top neighbor at the recorded
/// proportions). Returns whether a plan existed.
pub fn restore_parked(owner: &str) -> bool {
    let Some(path) = park_path(owner) else { return false };
    let Ok(json) = std::fs::read_to_string(&path) else { return false };
    let _ = std::fs::remove_file(&path);
    let Ok(plan) = serde_json::from_str::<ParkPlan>(&json) else { return false };

    let viewer = ipc::call_text("pane.list", serde_json::json!({}))
        .ok()
        .and_then(|l| viewer_pane_in_tab(&l, owner).map(|(id, _)| id));

    let tree = build_tree(&plan.panes);
    // The tree's representative (top-left-most) pane comes home first,
    // splitting whatever holds the region: the preview (which closes right
    // after, handing everything over) or the sidebar itself.
    let (anchor, ratio) = match &viewer {
        Some(v) => (v.clone(), 0.1),
        None => (owner.to_string(), plan.owner_ratio),
    };
    move_into(&plan.tab, rep(&tree), &anchor, "right", ratio);
    replay(&plan.tab, &tree);
    // And the sidebar's own width, which the park inflated.
    let _ = ipc::call_text(
        "layout.set_split_ratio",
        serde_json::json!({ "pane_id": owner, "path": [], "ratio": plan.owner_ratio }),
    );
    true
}

/// A recovered split tree: exactly the rects the panes had at park time.
enum Node {
    Leaf(String),
    Split {
        dir: &'static str,
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// The subtree's representative: its top-left-most pane, which stands in
/// for the whole region until the subtree's own splits are replayed.
fn rep(node: &Node) -> &str {
    match node {
        Node::Leaf(id) => id,
        Node::Split { first, .. } => rep(first),
    }
}

/// Rebuild the split tree from pane rects by guillotine recovery: find a
/// full-height (or full-width) cut line that cleanly partitions the panes,
/// recurse on both sides. Binary-split layouts always admit one; if none is
/// found (foreign layout), fall back to a degenerate right-stack.
fn build_tree(panes: &[(String, RectJ)]) -> Node {
    if panes.len() == 1 {
        return Node::Leaf(panes[0].0.clone());
    }
    let min_x = panes.iter().map(|(_, r)| r.x).min().unwrap_or(0);
    let max_x = panes.iter().map(|(_, r)| r.x + r.width).max().unwrap_or(0);
    let min_y = panes.iter().map(|(_, r)| r.y).min().unwrap_or(0);
    let max_y = panes.iter().map(|(_, r)| r.y + r.height).max().unwrap_or(0);

    // Vertical cut candidates: every pane's left edge strictly inside.
    for (_, r) in panes {
        let cut = r.x;
        if cut <= min_x || cut >= max_x {
            continue;
        }
        if panes.iter().all(|(_, q)| q.x + q.width <= cut || q.x >= cut) {
            let (a, b): (Vec<_>, Vec<_>) =
                panes.iter().cloned().partition(|(_, q)| q.x + q.width <= cut);
            if !a.is_empty() && !b.is_empty() {
                return Node::Split {
                    dir: "right",
                    ratio: (cut - min_x) as f64 / (max_x - min_x).max(1) as f64,
                    first: Box::new(build_tree(&a)),
                    second: Box::new(build_tree(&b)),
                };
            }
        }
    }
    for (_, r) in panes {
        let cut = r.y;
        if cut <= min_y || cut >= max_y {
            continue;
        }
        if panes.iter().all(|(_, q)| q.y + q.height <= cut || q.y >= cut) {
            let (a, b): (Vec<_>, Vec<_>) =
                panes.iter().cloned().partition(|(_, q)| q.y + q.height <= cut);
            if !a.is_empty() && !b.is_empty() {
                return Node::Split {
                    dir: "down",
                    ratio: (cut - min_y) as f64 / (max_y - min_y).max(1) as f64,
                    first: Box::new(build_tree(&a)),
                    second: Box::new(build_tree(&b)),
                };
            }
        }
    }
    // No clean cut (shouldn't happen for herdr layouts): stack them.
    let rest = build_tree(&panes[1..]);
    Node::Split {
        dir: "right",
        ratio: 0.5,
        first: Box::new(Node::Leaf(panes[0].0.clone())),
        second: Box::new(rest),
    }
}

/// Pre-order replay: at each split, the region is currently held entirely
/// by rep(first); moving rep(second) in with the recorded direction/ratio
/// carves the region correctly before either side's inner splits run.
fn replay(tab: &str, node: &Node) {
    let Node::Split { dir, ratio, first, second } = node else { return };
    move_into(tab, rep(second), rep(first), dir, *ratio);
    replay(tab, first);
    replay(tab, second);
}

fn move_into(tab: &str, pane: &str, target: &str, split: &str, ratio: f64) {
    let _ = ipc::call_text(
        "pane.move",
        serde_json::json!({
            "pane_id": pane,
            "destination": {
                "type": "tab",
                "tab_id": tab,
                "split": split,
                "target_pane_id": target,
                "ratio": ratio.clamp(0.1, 0.9),
            },
            "focus": false,
        }),
    );
}

/// The pane directly to the right of `pane_id` (sharing vertical overlap),
/// from a `pane.layout` response.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The sidebar respawns with a FRESH pane id on every redeploy and on
    /// ensure-hook healing, but the parked panes' way home is the TAB. Keying
    /// the plan by pane id meant the "already parked?" guard never fired for
    /// the new id: each preview parked the same terminals into yet another
    /// new tab and orphaned the previous plan, so nothing ever restored.
    const PREVIEWS: &str = r#"{"result":{"panes":[
        {"pane_id":"w4:p1","tab_id":"w4:t1","tokens":{"herdr-sidebar-explorer":"1"}},
        {"pane_id":"w4:p2","tab_id":"w4:t2",
         "tokens":{"herdr-sidebar-preview":"9","hs-preview-path":"/r/a.rs","hs-preview-pinned":"1"}},
        {"pane_id":"w4:p3","tab_id":"w4:t3",
         "tokens":{"herdr-sidebar-preview":"9","hs-preview-path":"/r/b.rs"}}
    ]}}"#;

    #[test]
    fn a_pinned_tab_is_no_longer_reusable() {
        let before = r#"{"result":{"panes":[
            {"pane_id":"w4:p3","tab_id":"w4:t3","tokens":{"hs-preview-path":"/r/b.rs"}}
        ]}}"#;
        let after = r#"{"result":{"panes":[
            {"pane_id":"w4:p3","tab_id":"w4:t3",
             "tokens":{"hs-preview-path":"/r/b.rs","hs-preview-pinned":"1"}}
        ]}}"#;
        assert!(reusable_preview(&previews_in(before)).is_some());
        assert!(
            reusable_preview(&previews_in(after)).is_none(),
            "pinning must push the next file onto a new tab"
        );
        // ...and the pinned tab is still reachable by its document.
        assert!(preview_for_doc(&previews_in(after), "/r/b.rs").is_some());
    }

    #[test]
    fn control_files_are_addressed_by_the_preview_pane() {
        // The path is argv for the viewer, so it must key on something that
        // survives a tab move: the preview pane itself. Any sidebar can then
        // steer any preview by naming its pane.
        assert_eq!(control_path_for_pane("w4:p9"), control_path_for_pane("w4:p9"));
        assert_ne!(control_path_for_pane("w4:p9"), control_path_for_pane("w4:pA"));
        assert!(
            control_path_for_pane("w4:p9").to_string_lossy().contains("w4_p9"),
            "colons are not filename-safe"
        );
    }

    #[test]
    fn previews_carry_their_document_and_pin_state() {
        let ps = previews_in(PREVIEWS);
        assert_eq!(ps.len(), 2, "only panes with hs-preview-path are previews");
        let a = ps.iter().find(|p| p.doc_key == "/r/a.rs").unwrap();
        assert!(a.pinned);
        assert_eq!(a.tab_id, "w4:t2");
        assert!(!ps.iter().find(|p| p.doc_key == "/r/b.rs").unwrap().pinned);
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
        assert_eq!(reusable_preview(&ps).unwrap().doc_key, "/r/b.rs");

        let all_pinned = r#"{"result":{"panes":[
            {"pane_id":"w4:p2","tab_id":"w4:t2",
             "tokens":{"hs-preview-path":"/r/a.rs","hs-preview-pinned":"1"}}
        ]}}"#;
        assert!(
            reusable_preview(&previews_in(all_pinned)).is_none(),
            "every tab pinned must force a new tab"
        );
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
        assert_eq!(tab_label("/repo/src/main.rs", false), "*main.rs");
        assert_eq!(tab_label("/repo/src/main.rs", true), "main.rs");
        assert_eq!(parse_tab_label("*main.rs"), ("main.rs".to_string(), false));
        assert_eq!(parse_tab_label("main.rs"), ("main.rs".to_string(), true));
    }

    #[test]
    fn park_plan_is_keyed_by_tab_not_by_the_sidebars_pane_id() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"w4:p1E","tab_id":"w4:tH"},
            {"pane_id":"w4:p1R","tab_id":"w4:tH"},
            {"pane_id":"w4:pM","tab_id":"w4:tP"}
        ]}}"#;
        let before_respawn = park_path_in(json, "w4:p1E").expect("sidebar is in a tab");
        let after_respawn = park_path_in(json, "w4:p1R").expect("sidebar is in a tab");
        assert_eq!(
            before_respawn, after_respawn,
            "a respawned sidebar must find the SAME park plan"
        );
        // A different tab keeps its own plan, and an unknown pane has none.
        assert_ne!(park_path_in(json, "w4:pM").expect("in a tab"), before_respawn);
        assert!(park_path_in(json, "w4:pZZ").is_none());
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
        assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "payload");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original victim contents");

        let _ = std::fs::remove_file(&victim);
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn requests_roundtrip() {
        let f = file_request(Path::new("C:/x/y.rs"));
        assert_eq!(parse_request(&f), Some(Request::File(PathBuf::from("C:/x/y.rs"))));
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
            Some(Request::File(PathBuf::from("C:/plain.txt")))
        );
        assert_eq!(parse_request("  "), None);
    }

    #[test]
    fn glow_markdown_returns_styled_spans() {
        // Skip if glow is not installed
        if std::process::Command::new("glow").arg("--version").output().is_err() {
            return;
        }
        let md = "# Heading\n\n**bold** and `code`\n";
        let lines = glow_markdown(md, 80);
        assert!(lines.is_some(), "glow_markdown returned None");
        let lines = lines.unwrap();
        assert!(!lines.is_empty(), "glow_markdown returned empty lines");
        // At least one span must have a non-default style (proof that ANSI was parsed)
        let has_styled = lines.iter().any(|l| {
            l.spans.iter().any(|s| s.style != ratatui::style::Style::default())
        });
        assert!(has_styled, "glow_markdown returned no styled spans — ANSI not parsed");
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
        assert_eq!(viewer_pane_in_tab(&json, "w1:p1"), Some(("w1:p2".into(), false)));
        let stale = format!(
            r#"{{"result":{{"panes":[
                {{"pane_id":"w1:p1","tab_id":"w1:t1"}},
                {{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-preview":"{}"}}}}
            ]}}}}"#,
            now - 999
        );
        assert_eq!(viewer_pane_in_tab(&stale, "w1:p1"), Some(("w1:p2".into(), true)));
    }
}
