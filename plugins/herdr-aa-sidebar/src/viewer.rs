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
pub const METADATA_SOURCE: &str = "herdr-aa-sidebar-preview";

/// How often the control file is re-checked while idle.
const POLL: Duration = Duration::from_millis(250);

/// Preview size guards: don't slurp huge files into a pane.
const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 5000;

/// The control file the sidebar writes requests into, unique per sidebar
/// pane (tab) so tabs don't steer each other's viewers.
pub fn control_path(sidebar_pane_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "herdr-aa-sidebar-preview-{}.ctl",
        sidebar_pane_id.replace(':', "_")
    ))
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

fn load_file(target: &Path) -> Doc {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    let lines: Vec<Line<'static>> = match std::fs::read(target) {
        Err(e) => vec![Line::raw(format!("(unreadable: {e})"))],
        Ok(bytes) => {
            let head = &bytes[..bytes.len().min(8192)];
            if head.contains(&0) {
                vec![Line::raw(format!("(binary file — {} bytes)", bytes.len()))]
            } else {
                let truncated = bytes.len() > MAX_BYTES;
                let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]);
                // Syntax highlighting when a grammar matches; plain otherwise.
                let mut lines: Vec<Line<'static>> =
                    crate::syntax::highlight(&name, &text, MAX_LINES).unwrap_or_else(|| {
                        text.lines().take(MAX_LINES).map(|l| Line::raw(l.to_string())).collect()
                    });
                if truncated || text.lines().count() > MAX_LINES {
                    lines.push(Line::raw("… (truncated)"));
                }
                if lines.is_empty() {
                    lines.push(Line::raw("(empty file)"));
                }
                lines
            }
        }
    };
    Doc {
        name,
        context: target.display().to_string(),
        lines,
        numbered: true,
        scroll: 0,
    }
}

fn load_diff(root: &Path, rel: &str, kind: &str) -> Doc {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
    let mut args: Vec<String> = vec![
        "-c".into(),
        "color.ui=always".into(),
        "diff".into(),
        "--color=always".into(),
        "--no-ext-diff".into(),
    ];
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
                ansi::to_lines(&text)
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

/// Tag our pane (heartbeat-stamped, see launch::HEARTBEAT_STALE_SECS) and
/// title it with the shown document's name.
fn report_identity(doc_name: &str) {
    let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else { return };
    if pane_id.is_empty() {
        return;
    }
    let _ = ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": METADATA_SOURCE,
            "tokens": { METADATA_SOURCE: crate::state::unix_now().to_string() },
        }),
    );
    let _ = ipc::call_text(
        "pane.rename",
        serde_json::json!({ "pane_id": pane_id, "label": doc_name }),
    );
}

/// Close our own pane (ends this process with it).
fn close_own_pane() {
    if let Ok(pane_id) = std::env::var("HERDR_PANE_ID")
        && !pane_id.is_empty()
    {
        let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id }));
    }
}

/// The viewer's event loop; returns when the user closes it.
pub fn run(control: &Path) -> std::io::Result<()> {
    let theme = IconTheme::from_env(std::env::var("HERDR_AA_FILETREE_ICONS").ok().as_deref());
    let mut current = read_control(control);
    let mut doc = current.as_ref().map(load).unwrap_or_else(|| Doc {
        name: "(nothing to show)".into(),
        context: String::new(),
        lines: vec![Line::raw("(waiting for a click in the sidebar)")],
        numbered: false,
        scroll: 0,
    });
    report_identity(&doc.name);

    // Blank the primary screen so pane handoffs never flash the shell.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
        crossterm::cursor::MoveTo(0, 0),
    );
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
                report_identity(&doc.name);
            }
            let target = read_control(control);
            if target != current {
                current = target;
                if let Some(request) = &current {
                    doc = load(request);
                    report_identity(&doc.name);
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
                line.clone()
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

/// Write `payload` to the caller's control file and make sure a live viewer
/// pane exists beside it (spawning one to our right when needed). Errors are
/// human-readable notices.
pub fn open_in_pane(my_pane_id: &str, spawn_cwd: &Path, payload: &str) -> Result<(), String> {
    let control = control_path(my_pane_id);
    std::fs::write(&control, payload).map_err(|e| format!("preview failed: {e}"))?;

    // A live viewer in this tab follows the control file by itself; a DEAD
    // one (stale heartbeat) is closed and replaced.
    if let Ok(json) = ipc::call_text("pane.list", serde_json::json!({})) {
        match viewer_pane_in_tab(&json, my_pane_id) {
            Some((_, false)) => return Ok(()),
            Some((id, true)) => {
                let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }));
            }
            None => {}
        }
    }
    spawn_viewer_pane(my_pane_id, spawn_cwd, &control)
}

/// Close this tab's viewer pane if one is open (Esc from the sidebar).
pub fn close_in_tab(my_pane_id: &str) {
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
    let viewer = panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(my_tab.as_str()))
        .find(|p| p.tokens.contains_key(METADATA_SOURCE))?;
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

/// Split a viewer pane directly to the caller's right: split the right
/// NEIGHBOR and swap the fresh pane into its left slot (split only goes
/// right/down), so the layout reads sidebar | preview | rest.
fn spawn_viewer_pane(my_pane_id: &str, spawn_cwd: &Path, control: &Path) -> Result<(), String> {
    let layout = ipc::call_text("pane.layout", serde_json::json!({ "pane_id": my_pane_id })).ok();
    let neighbor = layout.as_deref().and_then(|json| right_neighbor(json, my_pane_id));
    let (target, ratio, needs_swap) = match &neighbor {
        Some(id) => (id.clone(), 0.5, true),
        None => (my_pane_id.to_string(), 0.3, false),
    };
    let response = ipc::call_text(
        "pane.split",
        serde_json::json!({
            "target_pane_id": target,
            "direction": "right",
            "ratio": ratio,
            "focus": false,
            "cwd": spawn_cwd.display().to_string(),
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
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "herdr-aa-sidebar".to_string());
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
    Ok(())
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
    fn viewer_lookup_reports_staleness() {
        let now = crate::state::unix_now();
        let json = format!(
            r#"{{"result":{{"panes":[
                {{"pane_id":"w1:p1","tab_id":"w1:t1"}},
                {{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-aa-sidebar-preview":"{}"}}}}
            ]}}}}"#,
            now - 2
        );
        assert_eq!(viewer_pane_in_tab(&json, "w1:p1"), Some(("w1:p2".into(), false)));
        let stale = format!(
            r#"{{"result":{{"panes":[
                {{"pane_id":"w1:p1","tab_id":"w1:t1"}},
                {{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-aa-sidebar-preview":"{}"}}}}
            ]}}}}"#,
            now - 999
        );
        assert_eq!(viewer_pane_in_tab(&stale, "w1:p1"), Some(("w1:p2".into(), true)));
    }
}
