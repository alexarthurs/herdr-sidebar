//! herdr-sidebar — the VS Code sidebar for herdr: file explorer and source
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
use herdr_sidebar::{launch, state, viewer};
use state::{Exit, View};

/// How often the source-control view re-reads `git status` while idle.
const REFRESH_EVERY: Duration = Duration::from_millis(1500);

fn main() -> std::io::Result<()> {
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("--launch-decision") => {
            // Optional second arg picks the source-control decision (the
            // open-git launcher); default is the explorer/sidebar decision.
            // Optional THIRD arg scopes the decision to a tab or workspace —
            // it must match the scope the hook docks into, or the decision
            // answers for one tab while the dock lands in another.
            let now = state::unix_now();
            let scope = std::env::args().nth(3).unwrap_or_default();
            let out = if std::env::args().nth(2).as_deref() == Some("git") {
                launch::launch_decision_git(&read_stdin()?, now)
            } else {
                launch::launch_decision_in(&read_stdin()?, now, &scope)
            };
            println!("{out}");
            return Ok(());
        }
        Some("--focused-pane") => {
            // Optional scope (tab or workspace id) confines the lookup to the
            // tab being docked; without it the globally focused pane wins and
            // a new tab gets rooted in whatever project was last focused.
            let scope = std::env::args().nth(2).unwrap_or_default();
            println!("{}", launch::focused_pane_in(&read_stdin()?, &scope));
            return Ok(());
        }
        Some("--event-scope") => {
            let payload = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
            println!("{}", launch::event_scope(&payload));
            return Ok(());
        }
        Some("--open-plan") => {
            println!("{}", launch::open_plan(&read_stdin()?));
            return Ok(());
        }
        Some("--event-kind") => {
            // Which event ran the ensure hook, so it can treat a brand-new
            // space differently from an ordinary focus. Empty when herdr
            // supplies no payload (e.g. a manual invocation).
            let payload = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
            println!("{}", launch::event_kind(&payload));
            return Ok(());
        }
        Some("--focused-tab") => {
            println!("{}", launch::focused_tab(&read_stdin()?));
            return Ok(());
        }
        Some("--auto-open") => {
            // For the unix ensure hook: skip auto-docking when the user
            // turned "Auto-open sidebar" off in ⚙ Settings (issue #8).
            println!("{}", if state::load_state().auto_open { "on" } else { "off" });
            return Ok(());
        }
        Some("--preview") => {
            let Some(control) = std::env::args().nth(2) else {
                eprintln!("herdr-sidebar: --preview needs a control-file path");
                std::process::exit(2);
            };
            return viewer::run(std::path::Path::new(&control));
        }
        Some("--view") => {}
        Some(other) => {
            eprintln!("herdr-sidebar: unknown argument `{other}`");
            eprintln!(
                "usage: herdr-sidebar [--view explorer|git|--preview <ctl>|--launch-decision [git]|--focused-pane|--open-plan|--focused-tab|--auto-open]"
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
    // A TUI's colors are interface, not pipeable output: ignore NO_COLOR,
    // which otherwise leaks in whenever the herdr server was (re)started
    // from an agent shell (Claude Code's tool env sets it) and silently
    // turns every pane we draw monochrome.
    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    // First run on a machine without a Nerd Font: offer to install one
    // before any icons render. The prompt stamps the pane's identity token
    // itself (the app loops haven't started yet, and a token-less pane gets
    // REPLACE-killed by the corpse rule while the user reads the prompt).
    herdr_sidebar::fontsetup::maybe_prompt(&mut terminal, view, persisted.merged)?;
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

/// The label of the space this pane lives in, or "" when it can't be
/// resolved — the caller then falls back to the pane's cwd.
fn workspace_label() -> String {
    let Ok(ws_id) = std::env::var("HERDR_WORKSPACE_ID") else { return String::new() };
    herdr_sidebar::ipc::call_text("workspace.list", serde_json::json!({}))
        .map(|json| herdr_sidebar::launch::workspace_label(&json, &ws_id))
        .unwrap_or_default()
}

/// The directory the tree is built from: the root this space remembers,
/// else the cwd the pane was spawned with.
///
/// The spawn cwd is only a guess — the ensure hook takes it from whichever
/// pane happened to be focused — so a remembered choice always wins. A
/// remembered root that has since been deleted is ignored rather than
/// yielding an empty tree.
fn resolve_root() -> std::io::Result<std::path::PathBuf> {
    if let Some(root) = herdr_sidebar::state::load_root(&workspace_label())
        && root.is_dir()
    {
        return Ok(root);
    }
    std::env::current_dir()
}

/// The explorer's event loop: short poll so the liveness heartbeat keeps
/// stamping even while idle.
fn run_explorer(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<Exit> {
    let root = resolve_root()?;
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
