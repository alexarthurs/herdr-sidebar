//! herdr-aa-filetree — VS Code Explorer for herdr: a left-docked file-tree pane.
//!
//! With no arguments this runs the TUI, rooted at the process cwd (the launcher
//! scripts pass the user's focused-pane cwd via `pane split --cwd`). The three
//! `--*` stdin→stdout modes serve `scripts/open-explorer.{sh,ps1}` — see launch.rs.

mod app;
mod icons;
mod launch;
mod tree;

use std::io::Read;

use crossterm::event::{self, Event};

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
        Some(other) => {
            eprintln!("herdr-aa-filetree: unknown argument `{other}`");
            eprintln!("usage: herdr-aa-filetree [--launch-decision|--focused-pane|--open-plan]");
            std::process::exit(2);
        }
        None => {}
    }

    let root = std::env::current_dir()?;
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, app::App::new(root));
    ratatui::restore();
    result
}

fn read_stdin() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

fn run(terminal: &mut ratatui::DefaultTerminal, mut app: app::App) -> std::io::Result<()> {
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if let Event::Key(key) = event::read()?
            && !app.on_key(key)
        {
            return Ok(());
        }
        // Any other event (resize, focus, …) simply falls through to a redraw.
    }
}
