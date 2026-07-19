//! First-run helper: when no Nerd Font is installed, a fullscreen prompt
//! offers to download + install one (JetBrainsMono Nerd Font — the family
//! winget carries) so the material icon theme has glyphs to draw. Shown at
//! most once; the answer is persisted either way. The install runs on a
//! background thread while the UI polls, so nothing blocks for long.

use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::icons;
use crate::state;
use crate::ui::{KEYCAP_BG, KEYCAP_FG};

const FONT_NAME: &str = "JetBrainsMono Nerd Font";
const ZIP_URL: &str =
    "https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip";

/// Testing/ops hook: `force` shows the prompt regardless of probe and flag;
/// `off` suppresses it entirely.
fn env_mode() -> Option<String> {
    std::env::var("HERDR_SIDEBAR_FONT_PROMPT").ok().map(|v| v.trim().to_lowercase())
}

/// Show the prompt if this looks like a first run on a machine without a
/// Nerd Font (and the user hasn't answered before or picked a theme).
pub fn maybe_prompt(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
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
    run(terminal, &mut st)
}

enum Screen {
    Ask,
    Installing(Receiver<Result<(), String>>),
    Done(Result<(), String>),
}

fn run(terminal: &mut ratatui::DefaultTerminal, st: &mut state::State) -> std::io::Result<()> {
    let mut screen = Screen::Ask;
    loop {
        terminal.draw(|frame| draw(frame, &screen))?;
        if let Screen::Installing(rx) = &screen
            && let Ok(result) = rx.try_recv()
        {
            if result.is_ok() {
                // The probe now finds it; commit to material like any
                // machine that already had a Nerd Font.
                st.icons = Some(icons::IconTheme::Material);
            }
            screen = Screen::Done(result);
        }
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match &screen {
            Screen::Ask => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let (tx, rx) = channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(install());
                    });
                    screen = Screen::Installing(rx);
                }
                _ => {
                    st.font_prompt_done = true;
                    state::save_state(*st);
                    return Ok(());
                }
            },
            // Let a running install finish; keys are ignored meanwhile.
            Screen::Installing(_) => {}
            Screen::Done(_) => {
                st.font_prompt_done = true;
                state::save_state(*st);
                return Ok(());
            }
        }
    }
}

fn draw(frame: &mut Frame, screen: &Screen) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let width = area.width.min(66);
    let height = area.height.min(15);
    let card = Rect::new(
        (area.width.saturating_sub(width)) / 2,
        (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    let key = |k: &'static str| Span::styled(format!(" {k} "), Style::default().bg(KEYCAP_BG).fg(KEYCAP_FG));
    let mut lines: Vec<Line> = vec![Line::default()];
    match screen {
        Screen::Ask => {
            lines.push(Line::from(Span::styled(
                "  No Nerd Font detected",
                Style::default().bold(),
            )));
            lines.push(Line::default());
            lines.push(Line::from("  The sidebar's recommended material icon theme needs a"));
            lines.push(Line::from("  Nerd-Font-patched terminal font. Without one, the emoji"));
            lines.push(Line::from("  icon theme is used instead (renders in any font)."));
            lines.push(Line::default());
            lines.push(Line::from(format!("  Download and install {FONT_NAME} now?")));
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::raw("  "),
                key("Y"),
                Span::raw(" install (Recommended)    "),
                key("N"),
                Span::raw(" not now — use emoji icons"),
            ]));
        }
        Screen::Installing(_) => {
            lines.push(Line::from(Span::styled(
                "  Installing…",
                Style::default().bold(),
            )));
            lines.push(Line::default());
            lines.push(Line::from(format!("  Downloading {FONT_NAME} and installing it")));
            lines.push(Line::from("  for your user. This takes a moment."));
        }
        Screen::Done(Ok(())) => {
            lines.push(Line::from(Span::styled(
                "  Installed — one step left",
                Style::default().bold().fg(Color::Green),
            )));
            lines.push(Line::default());
            lines.push(Line::from(format!("  Set your terminal's font to \"{FONT_NAME}\".")));
            lines.push(Line::from("  Windows Terminal: Settings → your profile → Appearance."));
            lines.push(Line::from("  macOS Terminal/iTerm2: Preferences → Profiles → Font."));
            lines.push(Line::default());
            lines.push(Line::from("  Material icons are now the default; press i in the"));
            lines.push(Line::from("  sidebar anytime to switch themes."));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  press any key to continue",
                Style::default().dim(),
            )));
        }
        Screen::Done(Err(e)) => {
            lines.push(Line::from(Span::styled(
                "  Install failed",
                Style::default().bold().fg(Color::Red),
            )));
            lines.push(Line::default());
            lines.push(Line::from(format!("  {e}")));
            lines.push(Line::default());
            lines.push(Line::from("  You can grab a font manually at nerdfonts.com —"));
            lines.push(Line::from("  until then the sidebar uses emoji icons."));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  press any key to continue",
                Style::default().dim(),
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .title(" herdr-sidebar ")
                .border_style(Style::default().dim()),
        ),
        card,
    );
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

/// Windows: winget when available (it downloads AND registers per-user);
/// otherwise curl + bsdtar (both ship with Windows 10+) plus per-user font
/// registration under HKCU.
#[cfg(windows)]
fn install() -> Result<(), String> {
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

    let tmp = std::env::temp_dir().join("herdr-sidebar-font");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let zip = tmp.join("font.zip");
    run_step("curl", &["-fsSL", ZIP_URL, "-o", &zip.display().to_string()])?;
    run_step("tar", &["-xf", &zip.display().to_string(), "-C", &tmp.display().to_string()])?;

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
fn install() -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    let fonts_dir = std::path::PathBuf::from(&home).join("Library/Fonts");
    #[cfg(not(target_os = "macos"))]
    let fonts_dir = std::path::PathBuf::from(&home).join(".local/share/fonts");
    std::fs::create_dir_all(&fonts_dir).map_err(|e| e.to_string())?;
    let zip = std::env::temp_dir().join("herdr-sidebar-font.zip");
    run_step("curl", &["-fsSL", ZIP_URL, "-o", &zip.display().to_string()])?;
    run_step(
        "unzip",
        &["-o", &zip.display().to_string(), "-d", &fonts_dir.display().to_string()],
    )?;
    let _ = std::fs::remove_file(&zip);
    #[cfg(not(target_os = "macos"))]
    let _ = std::process::Command::new("fc-cache").arg("-f").output();
    Ok(())
}
