//! The standalone file-preview pane. The explorer opens ONE viewer pane
//! beside itself (the sidebar stays visible, like VS Code's editor area) and
//! steers it through a small CONTROL FILE: each file click writes the target
//! path there; the running viewer polls it and reloads in place, so repeated
//! clicks never churn panes. `q`/Esc (or clicking the ✕ header) closes the
//! pane itself over the socket API.

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

use crate::icons::{IconTheme, icon};
use crate::ipc;

/// Metadata source/token that marks the viewer pane, so the explorer can find
/// and reuse it (distinct from the Explorer's own identity token).
pub const METADATA_SOURCE: &str = "herdr-aa-filetree-preview";

/// How often the control file is re-checked while idle.
const POLL: Duration = Duration::from_millis(250);

/// Preview size guards: don't slurp huge files into a pane.
const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 5000;

/// The control file the explorer writes target paths into, unique per
/// explorer pane (tab) so tabs don't steer each other's viewers.
pub fn control_path(explorer_pane_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "herdr-aa-filetree-preview-{}.ctl",
        explorer_pane_id.replace(':', "_")
    ))
}

struct Doc {
    name: String,
    path: String,
    lines: Vec<String>,
    scroll: usize,
}

fn load(target: &Path) -> Doc {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    let lines = match std::fs::read(target) {
        Err(e) => vec![format!("(unreadable: {e})")],
        Ok(bytes) => {
            let head = &bytes[..bytes.len().min(8192)];
            if head.contains(&0) {
                vec![format!("(binary file — {} bytes)", bytes.len())]
            } else {
                let truncated = bytes.len() > MAX_BYTES;
                let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]);
                let mut lines: Vec<String> =
                    text.lines().take(MAX_LINES).map(str::to_string).collect();
                if truncated || text.lines().count() > MAX_LINES {
                    lines.push("… (truncated)".to_string());
                }
                if lines.is_empty() {
                    lines.push("(empty file)".to_string());
                }
                lines
            }
        }
    };
    Doc { name, path: target.display().to_string(), lines, scroll: 0 }
}

fn read_control(control: &Path) -> Option<PathBuf> {
    let mut buf = String::new();
    std::fs::File::open(control).ok()?.read_to_string(&mut buf).ok()?;
    let target = buf.trim();
    (!target.is_empty()).then(|| PathBuf::from(target))
}

/// Tag our pane and title it with the previewed file's name.
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
            "tokens": { METADATA_SOURCE: "viewer" },
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
    let mut doc = current
        .as_deref()
        .map(load)
        .unwrap_or_else(|| Doc {
            name: "(no file)".into(),
            path: String::new(),
            lines: vec!["(waiting for a file click in the Explorer)".into()],
            scroll: 0,
        });
    report_identity(&doc.name);

    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let mut page: usize = 20;
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
            // Idle: follow the control file so file clicks reload in place.
            let target = read_control(control);
            if target != current {
                current = target;
                if let Some(path) = current.as_deref() {
                    doc = load(path);
                    report_identity(&doc.name);
                }
            }
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Header (✕ close + name + path), numbered body, hint footer. Returns the
/// page stride for PageUp/Down.
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
    let shown = if doc.path.chars().count() > avail {
        let tail: String = doc
            .path
            .chars()
            .skip(doc.path.chars().count().saturating_sub(avail.saturating_sub(1)))
            .collect();
        format!("…{tail}")
    } else {
        doc.path.clone()
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
            Line::from(vec![
                Span::styled(format!("{:>number_width$} ", n + 1), Style::default().dim()),
                Span::raw(line.clone()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(text), body);

    frame.render_widget(
        Paragraph::new(Line::from(" ↑↓ scroll  ⇞⇟ page  g G ends  q close".dim())),
        footer,
    );
    usize::from(body.height).saturating_sub(1).max(1)
}
