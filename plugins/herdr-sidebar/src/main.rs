//! herdr-sidebar — the VS Code sidebar for herdr: file explorer and source
//! control in ONE binary. In unified mode both views share a pane and the
//! activity bar switches between them IN PROCESS (instant, no flash); in
//! separated mode the same binary runs one pane per view, pinned with
//! `--view explorer|git`. `--preview <ctl>` runs the file-preview pane.
//!
//! The native `--ensure` / `--toggle*` modes drive pane lifecycle; the other
//! `--*` stdin→stdout helpers expose the unit-tested launch calculations.

mod explorer_app;
mod scm_app;

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;
use std::time::Duration;

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use herdr_sidebar::{ensure, launch, state, viewer};
use state::{Exit, View};

/// How often the source-control view re-reads `git status` while idle.
const REFRESH_EVERY: Duration = Duration::from_millis(1500);

fn main() -> std::io::Result<()> {
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("--ensure") => return ensure::run(ensure::Mode::Ensure),
        Some("--toggle") => {
            return ensure::run(ensure::Mode::Toggle(View::Explorer));
        }
        Some("--toggle-git") => {
            return ensure::run(ensure::Mode::Toggle(View::SourceControl));
        }
        Some("--launch-decision") => {
            // Optional second arg picks the source-control decision; default
            // is the explorer/sidebar decision.
            // Optional THIRD arg scopes the decision to a tab or workspace —
            // it must match the scope the hook docks into, or the decision
            // answers for one tab while the dock lands in another.
            let now = state::unix_now();
            let scope = std::env::args().nth(3).unwrap_or_default();
            let mut out = if std::env::args().nth(2).as_deref() == Some("git") {
                launch::launch_decision_git(&read_stdin()?, now)
            } else {
                launch::launch_decision_in(&read_stdin()?, now, &scope)
            };
            // Strict toggle (⚙ Settings): report an open-but-unfocused pane
            // as CLOSE so the toggle launchers close it instead of focusing
            // it first. Safe for the ensure hook, which ignores FOCUS and
            // CLOSE alike (it only acts on OPEN and REPLACE).
            if state::load_state().strict_toggle {
                out = launch::focus_as_close(&out);
            }
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
        Some("--pane-has-token") => {
            let pane_id = std::env::args().nth(2).unwrap_or_default();
            let present = launch::pane_has_token(&read_stdin()?, &pane_id);
            println!("{}", if present { "yes" } else { "no" });
            return Ok(());
        }
        Some("--event-scope") => {
            let payload = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
            println!("{}", launch::event_scope_in(&payload, &read_stdin()?));
            return Ok(());
        }
        Some("--open-plan") => {
            let state = state::load_state();
            println!("{}", launch::open_plan(&read_stdin()?, state.dock_right, state.sidebar_width));
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
        Some("--focus-on-open") => {
            // For the unix toggle launchers: skip the open-then-focus zoom
            // cycle when the user turned "Focus on open" off in ⚙ Settings,
            // so the sidebar docks in the background.
            println!("{}", if state::load_state().focus_on_open { "on" } else { "off" });
            return Ok(());
        }
        Some("--dock-right") => {
            println!("{}", if state::load_state().dock_right { "right" } else { "left" });
            return Ok(());
        }
        Some("--preview") => {
            let Some(control) = std::env::args()
                .nth(2)
                .or_else(|| std::env::var(state::PREVIEW_CONTROL_ENV).ok())
            else {
                eprintln!(
                    "herdr-sidebar: --preview needs {}",
                    state::PREVIEW_CONTROL_ENV
                );
                std::process::exit(2);
            };
            // The viewer is its own process: it must apply the persisted
            // color theme itself, or a preview pane keeps the default palette
            // (and a dark syntax theme) whatever the user chose.
            herdr_sidebar::ui::set_color_theme(state::load_state().color_theme);
            return viewer::run(std::path::Path::new(&control));
        }
        Some("--view") => {}
        Some(other) => {
            eprintln!("herdr-sidebar: unknown argument `{other}`");
            eprintln!(
                "usage: herdr-sidebar [--view explorer|git|--preview [ctl]|--ensure|--toggle|--toggle-git|--launch-decision [git]|--focused-pane|--pane-has-token <id>|--open-plan|--focused-tab|--auto-open|--focus-on-open|--dock-right]"
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
    herdr_sidebar::ui::set_color_theme(persisted.color_theme);
    let mut view = pinned.unwrap_or(if persisted.merged {
        persisted.active
    } else {
        View::Explorer
    });

    // Unix launchers use plugin.pane.open so Herdr starts this argv directly,
    // with no shell prompt between the split and the TUI. Keep the host's cwd
    // at the plugin root for relative-command resolution, then adopt the
    // requested project cwd inside the process.
    if let Some(cwd) = std::env::var_os(state::SPAWN_CWD_ENV).filter(|cwd| !cwd.is_empty()) {
        std::env::set_current_dir(cwd)?;
    }
    // Mark the short interval before App::new applies its live identity. The
    // launcher also writes a live stamp after plugin.pane.open returns, so any
    // ordering between the two ends with App::new clearing this marker before
    // the TUI can accept edits.
    if let Some(pane_id) = std::env::var_os("HERDR_PANE_ID").filter(|id| !id.is_empty()) {
        let _ = herdr_sidebar::ipc::report_starting_identity(
            &pane_id.to_string_lossy(),
            view,
            view == View::Explorer && persisted.merged,
        );
    }

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
    let cwd_follower = Rc::new(RefCell::new(launch::CwdFollower::default()));
    let workspace_label = workspace_label();
    let spawn_cwd = std::env::current_dir()?;
    let root_key = remembered_root_key(&workspace_label, &spawn_cwd);
    let result = loop {
        let exit = match view {
            View::Explorer => {
                run_explorer(
                    &mut terminal,
                    Rc::clone(&cwd_follower),
                    &root_key,
                    &workspace_label,
                    &spawn_cwd,
                )
            }
            View::SourceControl => {
                run_scm(
                    &mut terminal,
                    Rc::clone(&cwd_follower),
                    &root_key,
                    &workspace_label,
                    &spawn_cwd,
                )
            }
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

/// Roots are project state, not workspace state: one Herdr workspace may host
/// unrelated project tabs, while tab ids change across server restarts.
fn remembered_root_key(workspace_label: &str, spawn_cwd: &std::path::Path) -> String {
    let mut cwd = spawn_cwd.display().to_string().replace('\\', "/");
    if cfg!(windows) {
        cwd.make_ascii_lowercase();
    }
    format!("{workspace_label}::{cwd}")
}

/// The directory the tree is built from: the root this tab remembers,
/// else the cwd the pane was spawned with.
///
/// A remembered root that has since been deleted is ignored rather than
/// yielding an empty tree. The guarded legacy lookup migrates v0.10's
/// workspace-keyed entry only when it contains this tab's spawn cwd.
fn resolve_root(
    root_key: &str,
    legacy_workspace_label: &str,
    spawn_cwd: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    let root = if let Some(root) = herdr_sidebar::state::load_root(root_key)
        && root.is_dir()
    {
        root
    } else if let Some(root) = herdr_sidebar::state::load_root(legacy_workspace_label)
        && root.is_dir()
        && spawn_cwd.starts_with(&root)
    {
        // v0.10 keyed roots by workspace label. Migrate that choice only
        // when it contains this tab's live spawn cwd; sibling project tabs
        // must not all inherit the same old entry.
        root
    } else {
        spawn_cwd.to_path_buf()
    };
    herdr_sidebar::state::save_root(root_key, &root);
    Ok(root)
}

/// The explorer's event loop: short poll so the liveness heartbeat keeps
/// stamping even while idle.
fn run_explorer(
    terminal: &mut ratatui::DefaultTerminal,
    cwd_follower: Rc<RefCell<launch::CwdFollower>>,
    root_key: &str,
    legacy_workspace_label: &str,
    spawn_cwd: &std::path::Path,
) -> std::io::Result<Exit> {
    let root = resolve_root(root_key, legacy_workspace_label, spawn_cwd)?;
    let mut remembered_root = root.clone();
    let mut app = explorer_app::App::new(root, cwd_follower);
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        // 500ms: quick enough that a finished folder pick lands promptly,
        // still cheap for the heartbeat.
        if event::poll(Duration::from_millis(500))? {
            let exit = match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                Event::Resize(width, _) => {
                    app.on_resize(width);
                    None
                }
                _ => None, // resize, focus, … simply fall through to a redraw
            };
            if let Some(exit) = exit {
                if exit == Exit::Quit {
                    app.clear_identity();
                }
                return Ok(exit);
            }
        }
        app.heartbeat();
        app.poll_picker();
        app.tick();
        let root = app.root_path();
        if root != remembered_root {
            herdr_sidebar::state::save_root(root_key, &root);
            remembered_root = root;
        }
    }
}

/// The source-control view's event loop: poll + tick so external changes and
/// finished background work (✧ suggestions, syncs) show up on their own.
fn run_scm(
    terminal: &mut ratatui::DefaultTerminal,
    cwd_follower: Rc<RefCell<launch::CwdFollower>>,
    root_key: &str,
    legacy_workspace_label: &str,
    spawn_cwd: &std::path::Path,
) -> std::io::Result<Exit> {
    let cwd = resolve_root(root_key, legacy_workspace_label, spawn_cwd)?;
    let mut remembered_root = cwd.clone();
    let mut app = scm_app::App::new(cwd, cwd_follower);
    let mut last_tick = std::time::Instant::now();
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        let timeout = REFRESH_EVERY.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            let exit = match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                Event::Resize(width, _) => {
                    app.on_resize(width);
                    None
                }
                _ => None,
            };
            if let Some(exit) = exit {
                // Switching drops this App and rebuilds it later, so it needs
                // the same persistence gate as quitting or a draft would be
                // lost despite the process staying alive.
                if !app.persist_scm() {
                    continue;
                }
                if exit == Exit::Quit {
                    app.clear_identity();
                }
                return Ok(exit);
            }
        }
        app.heartbeat();
        app.poll_picker();
        if last_tick.elapsed() >= REFRESH_EVERY {
            app.tick();
            last_tick = std::time::Instant::now();
        }
        let root = app.root_path().to_path_buf();
        if root != remembered_root {
            herdr_sidebar::state::save_root(root_key, &root);
            remembered_root = root;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_root_keys_are_project_stable_not_tab_scoped() {
        let key = remembered_root_key("acme", std::path::Path::new(r"C:\Repo\Web"));
        assert!(key.starts_with("acme::"));
        assert!(!key.contains('\\'));
        assert!(!key.contains("w1:t"));
        if cfg!(windows) {
            assert_eq!(key, "acme::c:/repo/web");
        }
    }
}
