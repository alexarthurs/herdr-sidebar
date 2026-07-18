//! herdr-aa-sidebar — the VS Code sidebar for herdr: file explorer and source
//! control in ONE binary. In unified mode both views share a pane and the
//! activity bar switches between them IN PROCESS (instant, no flash); in
//! separated mode the same binary runs one pane per view, pinned with
//! `--view explorer|git`. `--preview <ctl>` runs the file-preview pane.
//!
//! The `--*` stdin→stdout helper modes serve the launcher scripts — see
//! launch.rs.

mod explorer_app;
mod scm_app;

use std::io::Read;
use std::time::Duration;

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use herdr_aa_sidebar::{launch, state, viewer};
use state::{Exit, View};

/// How often the source-control view re-reads `git status` while idle.
const REFRESH_EVERY: Duration = Duration::from_millis(1500);

fn main() -> std::io::Result<()> {
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("--launch-decision") => {
            // Optional second arg picks the source-control decision (the
            // open-git launcher); default is the explorer/sidebar decision.
            let now = state::unix_now();
            let out = if std::env::args().nth(2).as_deref() == Some("git") {
                launch::launch_decision_git(&read_stdin()?, now)
            } else {
                launch::launch_decision(&read_stdin()?, now)
            };
            println!("{out}");
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
        Some("--focused-tab") => {
            println!("{}", launch::focused_tab(&read_stdin()?));
            return Ok(());
        }
        Some("--preview") => {
            let Some(control) = std::env::args().nth(2) else {
                eprintln!("herdr-aa-sidebar: --preview needs a control-file path");
                std::process::exit(2);
            };
            return viewer::run(std::path::Path::new(&control));
        }
        Some("--view") => {}
        Some(other) => {
            eprintln!("herdr-aa-sidebar: unknown argument `{other}`");
            eprintln!(
                "usage: herdr-aa-sidebar [--view explorer|git|--preview <ctl>|--launch-decision [git]|--focused-pane|--open-plan|--focused-tab]"
            );
            std::process::exit(2);
        }
        None => {}
    }

    // Starting view: an explicit `--view` pin (separated panes), else the
    // last-active view when the unified sidebar is on.
    let pinned = if mode.as_deref() == Some("--view") {
        std::env::args().nth(2).as_deref().and_then(View::from_view_flag)
    } else {
        None
    };
    let persisted = state::load_state();
    let mut view = pinned.unwrap_or(if persisted.merged {
        persisted.active
    } else {
        View::Explorer
    });

    // ONE terminal session for every view: switching drops the old view's
    // state and draws the other in the same alternate screen — instant, and
    // the shell prompt underneath never flashes through.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
        crossterm::cursor::MoveTo(0, 0),
    );
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let result = loop {
        let exit = match view {
            View::Explorer => run_explorer(&mut terminal),
            View::SourceControl => run_scm(&mut terminal),
        };
        match exit {
            Ok(Exit::Quit) => break Ok(()),
            Ok(Exit::Switch) => {
                view = view.other();
            }
            Err(e) => break Err(e),
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn read_stdin() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

/// The explorer's event loop: short poll so the liveness heartbeat keeps
/// stamping even while idle.
fn run_explorer(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<Exit> {
    let root = std::env::current_dir()?;
    let mut app = explorer_app::App::new(root);
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        // 500ms: quick enough that a finished folder pick lands promptly,
        // still cheap for the heartbeat.
        if event::poll(Duration::from_millis(500))? {
            let exit = match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                _ => None, // resize, focus, … simply fall through to a redraw
            };
            if let Some(exit) = exit {
                return Ok(exit);
            }
        } else {
            app.heartbeat();
            app.poll_picker();
        }
    }
}

/// The source-control view's event loop: poll + tick so external changes and
/// finished background work (✧ suggestions, syncs) show up on their own.
fn run_scm(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<Exit> {
    let cwd = std::env::current_dir()?;
    let mut app = scm_app::App::new(cwd);
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(REFRESH_EVERY)? {
            let exit = match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                _ => None,
            };
            if let Some(exit) = exit {
                return Ok(exit);
            }
        } else {
            app.heartbeat();
            app.poll_picker();
            app.tick();
        }
    }
}
