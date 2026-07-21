//! First-run helper: when no Nerd Font is installed, a fullscreen prompt
//! offers to download + install one (JetBrainsMono Nerd Font — the family
//! winget carries) so the material icon theme has glyphs to draw. Shown at
//! most once; the answer is persisted either way. The install runs on a
//! background thread while the UI polls, so nothing blocks for long.
//!
//! Every screen re-wraps its copy to the pane width and drops the least
//! important blocks first when the pane is too short — the actionable
//! keycap options are NEVER allowed to fall out of view (the original
//! fixed-width layout clipped them in narrow panes, leaving "Download and
//! install" looking like static text with no visible way to answer).

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::state::{self, View};
use crate::ui::{KEYCAP_BG, KEYCAP_FG, wrap_footer_message};
use crate::{actions, icons, ipc};

const FONT_NAME: &str = "JetBrainsMono Nerd Font";
// Keep the URL inside the MANUAL_CMD literals below in sync with this one.
const ZIP_URL: &str =
    "https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip";

/// The exact command a user can run by hand when the built-in install
/// fails (winget/curl missing, network policy, …). Shown — and copyable
/// with `c` — on the failure screen, so nobody has to rediscover it.
#[cfg(windows)]
pub const MANUAL_CMD: &str = "winget install DEVCOM.JetBrainsMonoNerdFont";
#[cfg(target_os = "macos")]
pub const MANUAL_CMD: &str = "curl -fsSL https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip -o /tmp/jbm.zip && unzip -o /tmp/jbm.zip -d ~/Library/Fonts";
#[cfg(all(not(windows), not(target_os = "macos")))]
pub const MANUAL_CMD: &str = "curl -fsSL https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip -o /tmp/jbm.zip && unzip -o /tmp/jbm.zip -d ~/.local/share/fonts && fc-cache -f";

/// Testing/ops hook: `force` shows the prompt regardless of probe and flag;
/// `off` suppresses it entirely.
fn env_mode() -> Option<String> {
    std::env::var("HERDR_SIDEBAR_FONT_PROMPT").ok().map(|v| v.trim().to_lowercase())
}

/// Show the prompt if this looks like a first run on a machine without a
/// Nerd Font (and the user hasn't answered before or picked a theme).
///
/// `view`/`merged` feed the liveness heartbeat: the prompt runs BEFORE the
/// app loop ever stamps the pane's identity token, so without its own
/// stamping a user who ponders the question (or a slow winget) for more
/// than `launch::HEARTBEAT_STALE_SECS` gets their pane REPLACE-killed by
/// the corpse rule mid-prompt.
pub fn maybe_prompt(
    terminal: &mut ratatui::DefaultTerminal,
    view: View,
    merged: bool,
) -> std::io::Result<()> {
    let mode = env_mode();
    if mode.as_deref() == Some("off") {
        return Ok(());
    }
    let mut st = state::load_state();
    if mode.as_deref() != Some("force")
        && (st.font_prompt_done || st.icons.is_some() || icons::nerd_font_installed())
    {
        return Ok(());
    }
    let mut heartbeat = Heartbeat::new(view, merged);
    run(terminal, &mut st, &mut heartbeat)
}

/// Identity-token stamping for the prompt's lifetime (see [`maybe_prompt`]).
/// Self-throttled, so calling it every loop iteration is free — per the
/// heartbeat rule, never only in the poll-timeout branch.
struct Heartbeat {
    pane_id: Option<String>,
    view: View,
    merged: bool,
    last: Option<Instant>,
}

impl Heartbeat {
    fn new(view: View, merged: bool) -> Self {
        let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|id| !id.is_empty());
        Self { pane_id, view, merged, last: None }
    }

    fn beat(&mut self) {
        let Some(pane_id) = &self.pane_id else { return };
        if self.last.is_some_and(|at| at.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.last = Some(Instant::now());
        ipc::report_identity(pane_id, self.view, self.merged);
    }
}

/// What the install thread reports back while the UI keeps polling.
enum Progress {
    Step(&'static str),
    Done(Result<(), String>),
}

enum Screen {
    Ask,
    Installing {
        rx: Receiver<Progress>,
        step: &'static str,
        started: Instant,
    },
    Done {
        result: Result<(), String>,
        /// A FRESH (uncached) probe re-run after a successful install —
        /// `false` means the registration isn't visible yet.
        probe_ok: bool,
        /// `c` copied [`MANUAL_CMD`] to the clipboard.
        copied: bool,
    },
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    st: &mut state::State,
    heartbeat: &mut Heartbeat,
) -> std::io::Result<()> {
    let mut screen = Screen::Ask;
    loop {
        heartbeat.beat();
        terminal.draw(|frame| draw(frame, &screen))?;
        if let Screen::Installing { rx, step, .. } = &mut screen {
            match rx.try_recv() {
                Ok(Progress::Step(s)) => *step = s,
                Ok(Progress::Done(result)) => {
                    let probe_ok = result.is_ok() && icons::probe_nerd_font();
                    if result.is_ok() {
                        // The (re-run) probe now finds it; commit to material
                        // like any machine that already had a Nerd Font.
                        st.icons = Some(icons::IconTheme::Material);
                    }
                    screen = Screen::Done { result, probe_ok, copied: false };
                }
                Err(_) => {}
            }
        }
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match &mut screen {
            Screen::Ask => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    let (tx, rx) = channel();
                    std::thread::spawn(move || {
                        let result = install(&tx);
                        let _ = tx.send(Progress::Done(result));
                    });
                    screen = Screen::Installing { rx, step: "Starting…", started: Instant::now() };
                }
                // Only an explicit decline answers "no" — a stray arrow key
                // must not silently commit the user to emoji icons.
                KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => {
                    st.font_prompt_done = true;
                    state::save_state(*st);
                    return Ok(());
                }
                _ => {}
            },
            Screen::Installing { .. } => {
                // Esc stops waiting (the thread finishes detached; `icons`
                // stays None, so the next start re-probes and still picks
                // material if the install landed). Never wedge the pane.
                if key.code == KeyCode::Esc {
                    st.font_prompt_done = true;
                    state::save_state(*st);
                    return Ok(());
                }
            }
            Screen::Done { result, copied, .. } => match key.code {
                KeyCode::Char('c' | 'C') if result.is_err() => {
                    if actions::copy_to_clipboard(MANUAL_CMD).is_ok() {
                        *copied = true;
                    }
                }
                _ => {
                    st.font_prompt_done = true;
                    state::save_state(*st);
                    return Ok(());
                }
            },
        }
    }
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn draw(frame: &mut Frame, screen: &Screen) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let width = area.width.min(66);
    let inner_w = width.saturating_sub(2);
    let max_h = usize::from(area.height.saturating_sub(2));
    let mut lines = match screen {
        Screen::Ask => ask_lines(inner_w, max_h),
        Screen::Installing { step, started, .. } => {
            installing_lines(step, started.elapsed(), inner_w, max_h)
        }
        Screen::Done { result: Ok(()), probe_ok, .. } => done_ok_lines(*probe_ok, inner_w, max_h),
        Screen::Done { result: Err(e), copied, .. } => done_err_lines(e, *copied, inner_w, max_h),
    };
    // A breathing row under the border when there's room for one.
    if lines.len() < max_h {
        lines.insert(0, Line::default());
    }
    let height = (lines.len() as u16).saturating_add(2).min(area.height);
    let card = Rect::new(
        (area.width.saturating_sub(width)) / 2,
        (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" herdr-sidebar ")
                .border_style(Style::default().dim()),
        ),
        card,
    );
}

/// Word-wrap uniform-style copy to the card's inner width (one leading
/// space per line, one column of right margin).
fn wrapped(text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    wrap_footer_message(text, width, 1)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, style)))
        .collect()
}

/// One actionable option: a keycap chip plus its label, label wrapped with
/// a hanging indent so the chip stays put at any width.
fn option_lines(key: &'static str, label: &str, width: u16) -> Vec<Line<'static>> {
    let cap = format!(" {key} ");
    let cap_cols = Span::raw(cap.as_str()).width() as u16 + 1; // + leading space
    let label_w = width.saturating_sub(cap_cols + 1);
    wrap_footer_message(label, label_w, 0)
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let lead: Span<'static> = if i == 0 {
                Span::styled(cap.clone(), Style::default().bg(KEYCAP_BG).fg(KEYCAP_FG))
            } else {
                Span::raw(" ".repeat(usize::from(cap_cols) - 1))
            };
            Line::from(vec![Span::raw(" "), lead, Span::raw(l)])
        })
        .collect()
}

/// Assemble copy blocks (document order, each with a keep-priority) into at
/// most `height` lines, one blank line between blocks. When it doesn't fit,
/// the LOWEST-priority block drops first — give the actionable options the
/// highest priority and they are the last thing standing.
fn fit_blocks(blocks: Vec<(u8, Vec<Line<'static>>)>, height: usize) -> Vec<Line<'static>> {
    let mut kept = vec![true; blocks.len()];
    let total = |kept: &[bool]| {
        let (sum, n) = blocks
            .iter()
            .zip(kept)
            .filter(|(_, k)| **k)
            .fold((0usize, 0usize), |(s, n), ((_, lines), _)| (s + lines.len(), n + 1));
        sum + n.saturating_sub(1)
    };
    while total(&kept) > height && kept.iter().filter(|&&k| k).count() > 1 {
        let drop = (0..blocks.len())
            .filter(|&i| kept[i])
            .min_by_key(|&i| blocks[i].0)
            .unwrap();
        kept[drop] = false;
    }
    let mut out = Vec::new();
    for ((_, lines), keep) in blocks.into_iter().zip(kept) {
        if !keep {
            continue;
        }
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.extend(lines);
    }
    // Degenerate panes (shorter than the surviving block alone): clip the
    // tail honestly instead of overflowing the card.
    out.truncate(height.max(1));
    out
}

fn ask_lines(width: u16, height: usize) -> Vec<Line<'static>> {
    // Below ~40 cols the full label wraps; a compact one keeps each option
    // on a single line far longer, which is what buys the options room in
    // exactly the panes that clip.
    let yes = if width >= 40 { "Download and install (Recommended)" } else { "Install (Recommended)" };
    let mut options = option_lines("Y", yes, width);
    options.extend(option_lines("N", "Use emoji icons", width));
    fit_blocks(
        vec![
            (3, wrapped("No Nerd Font detected", width, Style::default().bold())),
            (
                0,
                wrapped(
                    "The sidebar's material icon theme needs a Nerd-Font-patched terminal \
                     font. Without one, emoji icons are used instead (they render in any font).",
                    width,
                    Style::default(),
                ),
            ),
            (1, wrapped(&format!("Download and install {FONT_NAME} now?"), width, Style::default())),
            (4, options),
            (2, wrapped("Enter also installs", width, Style::default().dim())),
        ],
        height,
    )
}

fn installing_lines(step: &str, elapsed: Duration, width: u16, height: usize) -> Vec<Line<'static>> {
    let spin = SPINNER[(elapsed.as_millis() / 120) as usize % SPINNER.len()];
    fit_blocks(
        vec![
            (
                4,
                wrapped(&format!("{spin} Installing {FONT_NAME}…"), width, Style::default().bold()),
            ),
            (3, wrapped(&format!("{step} ({}s)", elapsed.as_secs()), width, Style::default())),
            (
                0,
                wrapped(
                    "The install runs in the background; the sidebar opens when it finishes.",
                    width,
                    Style::default().dim(),
                ),
            ),
            (5, option_lines("Esc", "stop waiting — use emoji icons for now", width)),
        ],
        height,
    )
}

fn done_ok_lines(probe_ok: bool, width: u16, height: usize) -> Vec<Line<'static>> {
    #[cfg(windows)]
    let where_to = "Windows Terminal: Settings → your profile → Appearance → Font face.";
    #[cfg(not(windows))]
    let where_to = "macOS Terminal/iTerm2: Preferences → Profiles → Font.";
    let mut blocks = vec![
        (
            4,
            wrapped("Installed — one step left", width, Style::default().bold().fg(Color::Green)),
        ),
        (
            3,
            wrapped(
                &format!(
                    "Set your terminal's ACTIVE font to \"{FONT_NAME}\" — installing a font \
                     does not change the terminal's profile. {where_to}"
                ),
                width,
                Style::default(),
            ),
        ),
        (
            0,
            wrapped(
                "Material icons are now the default; press i in the sidebar anytime to \
                 switch themes.",
                width,
                Style::default().dim(),
            ),
        ),
        (5, option_lines("⏎", "continue", width)),
    ];
    if !probe_ok {
        blocks.insert(
            2,
            (
                1,
                wrapped(
                    "(the font probe can't see it yet — if icons render as boxes, restart \
                     the terminal)",
                    width,
                    Style::default().dim(),
                ),
            ),
        );
    }
    fit_blocks(blocks, height)
}

fn done_err_lines(err: &str, copied: bool, width: u16, height: usize) -> Vec<Line<'static>> {
    let mut options = option_lines(
        "C",
        if copied { "copy the command — copied ✓" } else { "copy the command" },
        width,
    );
    options.extend(option_lines("⏎", "continue with emoji icons", width));
    fit_blocks(
        vec![
            (5, wrapped("Install failed", width, Style::default().bold().fg(Color::Red))),
            (1, wrapped(err, width, Style::default())),
            (
                3,
                [
                    wrapped("Install it manually by running:", width, Style::default()),
                    wrapped(MANUAL_CMD, width, Style::default().fg(Color::Yellow)),
                ]
                .concat(),
            ),
            (6, options),
        ],
        height,
    )
}

fn run_step(prog: &str, args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("{prog} failed: {}", err.lines().next().unwrap_or("(no output)").trim()))
    }
}

/// Testing hook for the live failure/success screens without real installs:
/// `HERDR_SIDEBAR_FONT_INSTALL=fail|ok` short-circuits the installer.
fn simulated() -> Option<Result<(), String>> {
    match std::env::var("HERDR_SIDEBAR_FONT_INSTALL").ok()?.as_str() {
        "fail" => Some(Err("simulated failure (HERDR_SIDEBAR_FONT_INSTALL=fail)".into())),
        "ok" => Some(Ok(())),
        _ => None,
    }
}

/// Windows: winget when available (it downloads AND registers per-user);
/// otherwise curl + bsdtar (both ship with Windows 10+) plus per-user font
/// registration under HKCU.
#[cfg(windows)]
fn install(tx: &Sender<Progress>) -> Result<(), String> {
    let step = |s: &'static str| {
        let _ = tx.send(Progress::Step(s));
    };
    if let Some(result) = simulated() {
        step("Simulating (HERDR_SIDEBAR_FONT_INSTALL)…");
        std::thread::sleep(Duration::from_secs(2));
        return result;
    }
    step("Trying winget…");
    if let Ok(out) = std::process::Command::new("winget")
        .args([
            "install",
            "--id",
            "DEVCOM.JetBrainsMonoNerdFont",
            "--silent",
            "--accept-source-agreements",
            "--accept-package-agreements",
        ])
        .output()
        && out.status.success()
    {
        return Ok(());
    }

    step("winget unavailable — downloading the font archive…");
    let tmp = std::env::temp_dir().join("herdr-sidebar-font");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let zip = tmp.join("font.zip");
    run_step("curl", &["-fsSL", ZIP_URL, "-o", &zip.display().to_string()])?;
    step("Unpacking…");
    run_step("tar", &["-xf", &zip.display().to_string(), "-C", &tmp.display().to_string()])?;

    step("Registering fonts for your user…");
    let fonts_dir = std::path::PathBuf::from(
        std::env::var("LOCALAPPDATA").map_err(|e| e.to_string())?,
    )
    .join(r"Microsoft\Windows\Fonts");
    std::fs::create_dir_all(&fonts_dir).map_err(|e| e.to_string())?;
    let mut installed = 0usize;
    for entry in std::fs::read_dir(&tmp).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ttf") {
            continue;
        }
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let dest = fonts_dir.join(path.file_name().unwrap_or_default());
        std::fs::copy(&path, &dest).map_err(|e| e.to_string())?;
        run_step(
            "reg",
            &[
                "add",
                r"HKCU\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts",
                "/v",
                &format!("{stem} (TrueType)"),
                "/t",
                "REG_SZ",
                "/d",
                &dest.display().to_string(),
                "/f",
            ],
        )?;
        installed += 1;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    if installed == 0 {
        return Err("the download contained no ttf files".into());
    }
    Ok(())
}

/// macOS / Linux: curl the zip and unpack straight into the user's font
/// directory (`~/Library/Fonts` / `~/.local/share/fonts`); Linux refreshes
/// the fontconfig cache.
#[cfg(not(windows))]
fn install(tx: &Sender<Progress>) -> Result<(), String> {
    let step = |s: &'static str| {
        let _ = tx.send(Progress::Step(s));
    };
    if let Some(result) = simulated() {
        step("Simulating (HERDR_SIDEBAR_FONT_INSTALL)…");
        std::thread::sleep(Duration::from_secs(2));
        return result;
    }
    let home = std::env::var("HOME").map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    let fonts_dir = std::path::PathBuf::from(&home).join("Library/Fonts");
    #[cfg(not(target_os = "macos"))]
    let fonts_dir = std::path::PathBuf::from(&home).join(".local/share/fonts");
    std::fs::create_dir_all(&fonts_dir).map_err(|e| e.to_string())?;
    step("Downloading the font archive…");
    let zip = std::env::temp_dir().join("herdr-sidebar-font.zip");
    run_step("curl", &["-fsSL", ZIP_URL, "-o", &zip.display().to_string()])?;
    step("Unpacking into your font directory…");
    run_step(
        "unzip",
        &["-o", &zip.display().to_string(), "-d", &fonts_dir.display().to_string()],
    )?;
    let _ = std::fs::remove_file(&zip);
    #[cfg(not(target_os = "macos"))]
    {
        step("Refreshing the font cache…");
        let _ = std::process::Command::new("fc-cache").arg("-f").output();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn max_width(lines: &[Line<'_>]) -> usize {
        lines.iter().map(Line::width).max().unwrap_or(0)
    }

    /// Words survive wrapping: joining on whitespace recovers the copy.
    fn squashed(lines: &[Line<'_>]) -> String {
        text_of(lines).split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn ask_fits_the_reported_narrow_pane() {
        // The bug report: a ~30-col pane clipped the Y/N options entirely.
        // 30x12 pane → 28 inner cols, 10 inner rows.
        let lines = ask_lines(28, 10);
        assert!(lines.len() <= 10, "must fit the pane height: {}", lines.len());
        assert!(max_width(&lines) <= 28, "no line may exceed the pane: {:?}", text_of(&lines));
        let text = squashed(&lines);
        assert!(text.contains("Download and install"), "{text}");
        assert!(text.contains("Use emoji icons"), "{text}");
        // The keycaps themselves render as chips.
        assert!(text_of(&lines).contains(" Y "), "Y keycap visible");
        assert!(text_of(&lines).contains(" N "), "N keycap visible");
    }

    #[test]
    fn options_survive_a_tiny_pane() {
        // Degenerate 20x7 pane (18x5 inner): everything else may drop, the
        // options may not.
        let lines = ask_lines(18, 5);
        assert!(lines.len() <= 5, "{}", text_of(&lines));
        assert!(max_width(&lines) <= 18, "{:?}", text_of(&lines));
        let text = squashed(&lines);
        assert!(text.contains("Install"), "{text}");
        assert!(text.contains("emoji"), "{text}");
    }

    #[test]
    fn wide_panes_show_the_full_copy() {
        let lines = ask_lines(64, 30);
        let text = squashed(&lines);
        assert!(text.contains("No Nerd Font detected"), "{text}");
        assert!(text.contains("material icon theme"), "explanation shown: {text}");
        assert!(text.contains(&format!("Download and install {FONT_NAME} now?")), "{text}");
        assert!(text.contains("Enter also installs"), "{text}");
    }

    #[test]
    fn fit_blocks_drops_lowest_priority_first() {
        let block = |p: u8, n: usize, tag: &str| {
            (p, (0..n).map(|i| Line::from(format!("{tag}{i}"))).collect::<Vec<_>>())
        };
        // 3+1+3+1+3 = 11 lines total; at height 7 exactly one block must go.
        let fitted = fit_blocks(vec![block(2, 3, "a"), block(0, 3, "b"), block(1, 3, "c")], 7);
        let text = text_of(&fitted);
        assert!(text.contains("a0") && text.contains("c0"), "{text}");
        assert!(!text.contains("b0"), "lowest priority dropped: {text}");
        // Document order is preserved among survivors.
        assert!(text.find("a0").unwrap() < text.find("c0").unwrap());
        // Even an impossible height keeps the top-priority block.
        let last = fit_blocks(vec![block(0, 2, "x"), block(9, 2, "keep")], 1);
        assert!(text_of(&last).contains("keep0"));
    }

    #[test]
    fn failure_screen_shows_error_and_manual_command() {
        let lines = done_err_lines("winget failed: 0x8a150044", false, 38, 20);
        let text = squashed(&lines);
        assert!(text.contains("Install failed"), "{text}");
        assert!(text.contains("winget failed: 0x8a150044"), "{text}");
        // The manual command survives wrapping word-for-word (long tokens may
        // hard-break, so compare with separators squashed entirely).
        let no_ws: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let cmd_no_ws: String = MANUAL_CMD.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(no_ws.contains(&cmd_no_ws), "manual command shown: {text}");
        assert!(text.contains("copy the command"), "{text}");
        assert!(max_width(&lines) <= 38);
        // Copy feedback.
        let copied = done_err_lines("boom", true, 38, 20);
        assert!(squashed(&copied).contains("copied ✓"));
    }

    #[test]
    fn failure_screen_keeps_actions_when_narrow() {
        let lines = done_err_lines("some very long error message from the installer", false, 26, 8);
        assert!(lines.len() <= 8, "{}", text_of(&lines));
        assert!(max_width(&lines) <= 26, "{:?}", text_of(&lines));
        let text = squashed(&lines);
        assert!(text.contains("copy the command"), "actions never drop: {text}");
    }

    #[test]
    fn success_screen_mentions_the_terminal_profile_step() {
        let lines = done_ok_lines(true, 60, 24);
        let text = squashed(&lines);
        assert!(text.contains("ACTIVE font"), "{text}");
        assert!(text.contains(FONT_NAME), "{text}");
        assert!(text.contains("does not change the terminal's profile"), "{text}");
        assert!(!text.contains("probe can't see it"), "no probe note when probe passed");
        let unprobed = squashed(&done_ok_lines(false, 60, 24));
        assert!(unprobed.contains("probe can't see it yet"), "{unprobed}");
    }

    #[test]
    fn option_labels_hang_indent_under_the_keycap() {
        let lines = option_lines("Y", "a label long enough to wrap onto several lines", 20);
        assert!(lines.len() > 1);
        assert!(max_width(&lines) <= 20, "{:?}", text_of(&lines));
        // Continuation lines are indented past the chip, not flush left.
        let text = text_of(&lines);
        let cont = text.lines().nth(1).unwrap();
        assert!(cont.starts_with("    "), "hanging indent: {cont:?}");
    }
}
