//! herdr-aa-git — VS Code Source Control for herdr: a left-docked git panel pane.
//!
//! With no arguments this runs the TUI, operating on the repository containing
//! the process cwd (the launcher scripts pass the user's focused-pane cwd via
//! `pane split --cwd`). The `--*` stdin→stdout modes serve
//! `scripts/open-git-panel.{sh,ps1}` — see launch.rs.
//!
//! Merged sidebar (see sidebar.rs): the first binary in the pane is the HOST.
//! Switching views spawns the other plugin's binary with `--sidebar-guest` in
//! the same terminal and waits; EXIT_SWITCH from the guest hands the pane back.

mod app;
mod git;
mod icons;
mod ipc;
mod launch;
mod sidebar;
mod suggest;

use std::io::Read;
use std::time::Duration;

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};

use app::Exit;
use sidebar::View;

const MY_VIEW: View = View::SourceControl;

/// How often the panel re-reads `git status` while idle, so edits and commits
/// made in other panes show up without a manual refresh.
const REFRESH_EVERY: Duration = Duration::from_millis(1500);

fn main() -> std::io::Result<()> {
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("--launch-decision") => {
            println!("{}", launch::launch_decision(&read_stdin()?));
            return Ok(());
        }
        Some("--focused-pane") => {
            println!("{}", launch::focused_pane(&read_stdin()?));
            return Ok(());
        }
        Some("--open-plan") => {
            println!("{}", launch::open_plan(&read_stdin()?));
            return Ok(());
        }
        Some(flag) if flag == sidebar::GUEST_FLAG => {}
        Some(other) => {
            eprintln!("herdr-aa-git: unknown argument `{other}`");
            eprintln!(
                "usage: herdr-aa-git [--launch-decision|--focused-pane|--open-plan|{}]",
                sidebar::GUEST_FLAG
            );
            std::process::exit(2);
        }
        None => {}
    }
    let guest = mode.as_deref() == Some(sidebar::GUEST_FLAG);

    // A merged sidebar opens on the view the user used last: when that is the
    // other plugin's, hand over immediately instead of flashing our own TUI.
    let state = sidebar::load_state();
    let mut show_other_first = !guest && state.merged && state.active != MY_VIEW;

    loop {
        if show_other_first {
            show_other_first = false;
            match run_guest() {
                Some(code) if code == sidebar::EXIT_SWITCH => {} // handed back to us
                Some(_) => break,                                // guest quit for real
                None => {} // other binary unavailable: show our own TUI instead
            }
            continue;
        }
        match run_tui()? {
            Exit::Quit => break,
            Exit::Switch => {
                if guest {
                    // The host is waiting on us; hand the pane back.
                    std::process::exit(sidebar::EXIT_SWITCH);
                }
                match run_guest() {
                    Some(code) if code == sidebar::EXIT_SWITCH => continue,
                    _ => break,
                }
            }
        }
    }
    Ok(())
}

/// The other plugin's binary, freshly resolved (it may have been rebuilt or
/// unlinked since startup).
fn other_exe() -> Option<std::path::PathBuf> {
    let json = ipc::call_text("plugin.list", serde_json::json!({})).ok()?;
    sidebar::other_binary(&json, MY_VIEW.other())
}

/// Run the other view in this terminal until it quits or hands back.
fn run_guest() -> Option<i32> {
    let exe = other_exe()?;
    std::process::Command::new(exe)
        .arg(sidebar::GUEST_FLAG)
        .status()
        .ok()?
        .code()
}

/// One full TUI session: init terminal, run the event loop, restore. Restoring
/// before returning matters — a spawned guest re-initializes the same terminal.
fn run_tui() -> std::io::Result<Exit> {
    let cwd = std::env::current_dir()?;
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let result = run(&mut terminal, app::App::new(cwd));
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn read_stdin() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

fn run(terminal: &mut ratatui::DefaultTerminal, mut app: app::App) -> std::io::Result<Exit> {
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(REFRESH_EVERY)? {
            let exit = match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                _ => None, // resize, focus, … simply fall through to a redraw
            };
            if let Some(exit) = exit {
                return Ok(exit);
            }
        } else {
            app.tick();
        }
    }
}
