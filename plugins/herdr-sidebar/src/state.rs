//! Unified-sidebar state: which layout the user chose (one combined Sidebar
//! pane vs separate Explorer / Source Control panes) and which view was
//! active last, persisted in a small JSON file so every pane and launcher
//! agrees across restarts.
//!
//! - `merged`: the unified sidebar is on (survives restarts).
//! - `active`: the view shown last, so a fresh sidebar opens where the user
//!   left off.
//! - `follow_cwd`: follow the live cwd of the neighbouring pane.
//! - `dock_right`: dock at the right edge instead of the default left edge.
//! - `sidebar_width`: preferred sidebar width in terminal columns.
//!
//! Both views live in ONE binary; switching is an in-process re-render, and
//! separated panes are the same binary pinned to a starting view with
//! `--view`.

use std::fs::File;
use std::path::{Path, PathBuf};

/// Pane label (and metadata identity) of the unified pane.
pub const SIDEBAR_LABEL: &str = "Sidebar";

pub const DEFAULT_SIDEBAR_WIDTH: u16 = 32;
pub const MIN_SIDEBAR_WIDTH: u16 = 24;
pub const MAX_SIDEBAR_WIDTH: u16 = 80;
pub const SIDEBAR_WIDTH_STEP: u16 = 4;

pub fn clamp_sidebar_width(width: u16) -> u16 {
    width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH)
}

pub fn step_sidebar_width(width: u16, wider: bool) -> u16 {
    let width = clamp_sidebar_width(width);
    if wider {
        (width + SIDEBAR_WIDTH_STEP).min(MAX_SIDEBAR_WIDTH)
    } else {
        width
            .saturating_sub(SIDEBAR_WIDTH_STEP)
            .max(MIN_SIDEBAR_WIDTH)
    }
}

/// Shell-agnostic command name typed into panes we create. [`spawn_env`]
/// prepends this binary's directory to PATH, so PowerShell, cmd, sh, bash,
/// nushell, and pwsh all resolve the same bare executable name.
pub const EXECUTABLE_NAME: &str = "herdr-sidebar";

/// Desired working directory for a directly spawned plugin pane. Herdr 0.8.2
/// resolves relative pane commands against the requested cwd, so launchers
/// keep the process cwd at the plugin root and let the TUI move here itself.
pub const SPAWN_CWD_ENV: &str = "HERDR_SIDEBAR_SPAWN_CWD";

/// The viewer's control path travels in the pane environment rather than in
/// a shell-quoted argv. Paths can contain spaces and every supported shell
/// has different quoting/call syntax.
pub const PREVIEW_CONTROL_ENV: &str = "HERDR_SIDEBAR_PREVIEW_CONTROL";

/// Set on viewer panes spawned into the sidebar's own tab
/// ([`PreviewPlacement::Pane`]). A viewer's placement is fixed by where its
/// pane physically sits, so it travels with the process rather than being
/// re-read from the settings file, which the user can flip mid-life.
pub const PREVIEW_INLINE_ENV: &str = "HERDR_SIDEBAR_PREVIEW_INLINE";

/// One-shot activity requested by a host-level action while opening a fresh
/// sidebar pane.
pub const INITIAL_ACTIVITY_ENV: &str = "HERDR_SIDEBAR_INITIAL_ACTIVITY";

/// Opt-in (`1`) for a Source Control pane that keeps polling Git status while
/// unfocused. Meant for panes used as a passive "what changed" monitor next to
/// an agent; by default an unfocused pane backs off to save `git status` runs.
pub const BACKGROUND_REFRESH_ENV: &str = "HERDR_SIDEBAR_BACKGROUND_REFRESH";

/// Whether a [`BACKGROUND_REFRESH_ENV`] value enables background refresh.
pub fn background_refresh_enabled(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1"))
}

/// Unix seconds now — the heartbeat clock for pane identity tokens.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Why a view's event loop ended; main.rs acts on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exit {
    Quit,
    /// The user picked the other view — main re-renders in process.
    Switch,
    /// Switch to the Search view. `focus_query` puts the caret in the search
    /// box (the Ctrl+F "find" gesture); a plain view switch passes false so the
    /// box isn't focused and 1/2/3 keep switching.
    Search {
        focus_query: bool,
    },
    /// Switch to Explorer and open its Quick Open picker.
    QuickOpen,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Explorer,
    SourceControl,
}

impl View {
    pub fn other(self) -> View {
        match self {
            View::Explorer => View::SourceControl,
            View::SourceControl => View::Explorer,
        }
    }

    /// The standalone pane label for this view.
    pub fn label(self) -> &'static str {
        match self {
            View::Explorer => "Explorer",
            View::SourceControl => "Source Control",
        }
    }

    /// The plugin that renders this view.
    pub fn plugin_id(self) -> &'static str {
        match self {
            View::Explorer => "herdr-sidebar-explorer",
            View::SourceControl => "herdr-sidebar-git",
        }
    }

    /// The `--view` flag value that pins a separated pane to this view.
    pub fn view_flag(self) -> &'static str {
        match self {
            View::Explorer => "explorer",
            View::SourceControl => "git",
        }
    }

    /// Manifest pane entrypoint that starts this view without an intermediary
    /// shell, avoiding a prompt flash while the TUI boots.
    pub fn entrypoint(self) -> &'static str {
        match self {
            View::Explorer => "sidebar",
            View::SourceControl => "source-control",
        }
    }

    pub fn from_view_flag(flag: &str) -> Option<View> {
        match flag {
            "explorer" => Some(View::Explorer),
            "git" => Some(View::SourceControl),
            _ => None,
        }
    }

    /// The metadata token value this view reports on its pane.
    pub fn token(self) -> &'static str {
        match self {
            View::Explorer => "explorer",
            View::SourceControl => "source-control",
        }
    }

    fn state_name(self) -> &'static str {
        match self {
            View::Explorer => "explorer",
            View::SourceControl => "source-control",
        }
    }

    fn from_state_name(name: &str) -> Option<View> {
        match name {
            "explorer" => Some(View::Explorer),
            "source-control" => Some(View::SourceControl),
            _ => None,
        }
    }
}

/// Accent palette for the sidebar. `VsCode` preserves the historical RGB
/// styling, which assumes a DARK terminal background; `Light` is its
/// light-background counterpart; `Terminal` uses ANSI colors so the terminal
/// profile remaps them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorTheme {
    VsCode,
    Light,
    Terminal,
}

impl ColorTheme {
    pub fn label(self) -> &'static str {
        match self {
            Self::VsCode => "vscode",
            Self::Light => "light",
            Self::Terminal => "terminal",
        }
    }

    /// The Settings row cycles through every theme — a rotation, not a
    /// two-way toggle.
    pub fn next(self) -> Self {
        match self {
            Self::VsCode => Self::Light,
            Self::Light => Self::Terminal,
            Self::Terminal => Self::VsCode,
        }
    }

    /// This palette is drawn for a LIGHT terminal background: the preview's
    /// syntax theme, diff tints and icon colors follow it.
    pub fn is_light(self) -> bool {
        self == Self::Light
    }

    fn from_state_name(name: &str) -> Option<Self> {
        match name {
            "vscode" => Some(Self::VsCode),
            "light" => Some(Self::Light),
            "terminal" => Some(Self::Terminal),
            _ => None,
        }
    }
}

/// Where a preview, diff or `git show` opens. `Tab` gives every document its
/// own herdr tab (VS Code editor-tab semantics, the historical default);
/// `Pane` splits ONE viewer pane into the tab the sidebar already lives in
/// and reuses it for every later click.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PreviewPlacement {
    Tab,
    Pane,
    Replace,
}

impl PreviewPlacement {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tab => "tab",
            Self::Pane => "pane",
            Self::Replace => "replace",
        }
    }

    pub fn other(self) -> Self {
        match self {
            Self::Tab => Self::Pane,
            Self::Pane => Self::Replace,
            Self::Replace => Self::Tab,
        }
    }

    /// True when previews share the caller's tab instead of getting one.
    pub fn is_inline(self) -> bool {
        matches!(self, Self::Pane | Self::Replace)
    }

    fn from_state_name(name: &str) -> Option<Self> {
        match name {
            "tab" => Some(Self::Tab),
            "pane" => Some(Self::Pane),
            "replace" => Some(Self::Replace),
            _ => None,
        }
    }
}

/// The sticky sidebar setting, shared by both plugins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct State {
    pub merged: bool,
    pub active: View,
    /// Restore the Search activity rather than the Explorer tree when the
    /// unified sidebar is reopened. Search shares the Explorer process.
    pub search_active: bool,
    /// Show the hotkey chips at the bottom of the sidebar (they always
    /// live in the ⚙ Settings modal; the footer copy is opt-in).
    pub show_hotkeys: bool,
    /// Show the compact branch + sync strip at the bottom while the current
    /// folder belongs to a Git repository.
    pub show_git_footer: bool,
    /// The user's explicit icon-theme choice; `None` = auto (Nerd Font
    /// probe). Set the moment they toggle `i` or the Settings row, so a
    /// wrong auto-guess is corrected once and stays corrected.
    pub icons: Option<crate::icons::IconTheme>,
    /// Accent palette. Terminal mode uses ANSI colors that inherit the
    /// terminal profile; VS Code preserves the original fixed RGB palette.
    pub color_theme: ColorTheme,
    /// The first-run "install a Nerd Font?" prompt was answered (either
    /// way) — never show it again.
    pub font_prompt_done: bool,
    /// The focus/created event hooks auto-dock a sidebar into tabs that lack
    /// one. Off = the sidebar stays closed until the user invokes the
    /// open-sidebar toggle themselves (issue #8); the explicit toggle always
    /// works regardless.
    pub auto_open: bool,
    /// The open-sidebar toggle treats an open-but-unfocused sidebar as CLOSE
    /// instead of FOCUS: one press opens, the next press closes, wherever
    /// focus is. Off keeps the historical open / focus / close cycle.
    pub strict_toggle: bool,
    /// Focus the sidebar once the toggle opens it. Off docks it in the
    /// background: focus stays in the pane the toggle was invoked from.
    pub focus_on_open: bool,
    /// Follow the live foreground cwd of a non-sidebar pane in this tab.
    /// Manual folder choices win until an already-observed pane changes cwd.
    pub follow_cwd: bool,
    /// Decorate the Explorer tree with git status (issue #19). Off stops the
    /// background `git status` polling entirely — the escape hatch for a repo
    /// where status is slow.
    pub git_deco: bool,
    /// Dock the sidebar at the right edge of each tab. False preserves the
    /// historical left dock.
    pub dock_right: bool,
    /// Preferred pane width in terminal columns. Layout code keeps this
    /// column target in the normal range and yields proportionally when the
    /// tab becomes unusually narrow.
    pub sidebar_width: u16,
    /// Whether a clicked file opens in its own tab or in a viewer pane beside
    /// the sidebar, inside the tab the click came from.
    pub preview_placement: PreviewPlacement,
    /// Replace mouse-click previews with the configured terminal editor.
    /// Keyboard Enter always retains the built-in preview path.
    pub custom_editor_on_click: bool,
    /// View mode in Source Control: true for hierarchical tree view, false for flat list view.
    pub scm_tree_view: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            merged: true,
            active: View::Explorer,
            search_active: false,
            show_hotkeys: false,
            show_git_footer: true,
            icons: None,
            color_theme: ColorTheme::VsCode,
            font_prompt_done: false,
            auto_open: true,
            strict_toggle: false,
            focus_on_open: true,
            follow_cwd: true,
            git_deco: true,
            dock_right: false,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            preview_placement: PreviewPlacement::Tab,
            custom_editor_on_click: false,
            scm_tree_view: false,
        }
    }
}

pub fn follow_cwd_setting_value(enabled: bool) -> String {
    let value = if enabled { "on" } else { "off" };
    if cfg!(windows) {
        format!("{value} (host n/a)")
    } else {
        value.to_string()
    }
}

/// Durable state belongs in herdr's per-plugin state dir (docs: "store
/// runtime state in HERDR_PLUGIN_STATE_DIR"). herdr injects that env for
/// hooks/actions but NOT panes, so our launchers pass it into every pane
/// they split (see [`spawn_env`]); when it didn't reach us, fall back to
/// the conventional location herdr resolves it to.
pub fn state_path() -> Option<PathBuf> {
    Some(state_dir()?.join("state.json"))
}

fn editor_command_path() -> Option<PathBuf> {
    Some(state_dir()?.join("editor-command.txt"))
}

pub fn load_editor_command() -> Option<String> {
    let command = std::fs::read_to_string(editor_command_path()?).ok()?;
    let command = command.trim().to_string();
    (!command.is_empty()).then_some(command)
}

pub fn save_editor_command(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() || command.len() > 1024 || command.contains(['\r', '\n']) {
        return false;
    }
    let Some(path) = editor_command_path() else {
        return false;
    };
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return false;
    }
    let Some(_lock) = StateWriteLock::acquire(&path) else {
        return false;
    };
    std::fs::write(path, command).is_ok()
}

pub(crate) fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")));
    Some(base?.join("herdr").join("plugins").join("herdr-sidebar"))
}

/// Env for panes WE spawn. Panes don't inherit the hook/action env herdr
/// injects, so forward the state dir and prepend the directory containing
/// our executable to PATH. Launchers can then type the same bare command in
/// every configured shell without quoting an absolute path.
pub fn spawn_env() -> serde_json::Value {
    let mut env = serde_json::Map::new();
    if let Some(dir) = state_dir() {
        env.insert(
            "HERDR_PLUGIN_STATE_DIR".into(),
            serde_json::Value::String(dir.display().to_string()),
        );
    }
    if let Some(path) = launch_path() {
        env.insert("PATH".into(), serde_json::Value::String(path));
    }
    serde_json::Value::Object(env)
}

fn launch_path() -> Option<String> {
    let bin_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let mut paths = vec![bin_dir.clone()];
    paths.extend(
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .filter(|path| path != &bin_dir),
    );
    std::env::join_paths(paths)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

/// The pre-rename location (`%APPDATA%\herdr\aa-sidebar.json` / the XDG
/// config dir), read once so existing settings survive the migration.
fn legacy_state_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    Some(base?.join("herdr").join("aa-sidebar.json"))
}

pub fn load_state() -> State {
    if let Some(json) = state_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        return parse_state(&json);
    }
    // One-time migration from the legacy config-dir file.
    if let Some(json) = legacy_state_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        let state = parse_state(&json);
        save_state(state);
        return state;
    }
    State::default()
}

/// Best-effort persist; the sidebar still works for this session if it fails.
pub fn save_state(state: State) {
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Some(_lock) = StateWriteLock::acquire(&path) else {
        return;
    };
    write_state(&path, state);
}

/// Atomically-with-respect-to-other-sidebar-processes update one or more
/// settings. Every caller reloads after taking the lock, so a tab that has
/// been open for hours cannot overwrite newer fields from another tab.
pub fn update_state(update: impl FnOnce(&mut State)) -> State {
    let mut state = load_state();
    let Some(path) = state_path() else {
        update(&mut state);
        return state;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Some(_lock) = StateWriteLock::acquire(&path) else {
        update(&mut state);
        return state;
    };
    if let Ok(json) = std::fs::read_to_string(&path) {
        state = parse_state(&json);
    }
    update(&mut state);
    write_state(&path, state);
    state
}

fn write_state(path: &Path, state: State) {
    let icons = match state.icons {
        Some(theme) => format!(",\"icons\":\"{}\"", theme.state_name()),
        None => String::new(),
    };
    let json = format!(
        "{{\"merged\":{},\"active\":\"{}\",\"search_active\":{},\"hotkeys\":{},\"git_footer\":{},\"font_prompt\":{},\"auto_open\":{},\"strict_toggle\":{},\"focus_on_open\":{},\"follow_cwd\":{},\"git_deco\":{},\"dock_right\":{},\"sidebar_width\":{},\"colors\":\"{}\",\"preview_placement\":\"{}\",\"custom_editor_on_click\":{},\"scm_tree_view\":{}{icons}}}",
        state.merged,
        state.active.state_name(),
        state.search_active,
        state.show_hotkeys,
        state.show_git_footer,
        state.font_prompt_done,
        state.auto_open,
        state.strict_toggle,
        state.focus_on_open,
        state.follow_cwd,
        state.git_deco,
        state.dock_right,
        clamp_sidebar_width(state.sidebar_width),
        state.color_theme.label(),
        state.preview_placement.label(),
        state.custom_editor_on_click,
        state.scm_tree_view
    );
    let _ = std::fs::write(path, json);
}

struct StateWriteLock {
    _file: File,
}

impl StateWriteLock {
    fn acquire(state_path: &Path) -> Option<Self> {
        let path = state_path.with_extension("lock");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .ok()?;
        file.lock().ok()?;
        Some(Self { _file: file })
    }
}

/// The shape of the tree a freshly opened sidebar should start with, so a
/// new tab mirrors what the user was already looking at. Kept beside
/// `state.json` rather than in [`State`], which stays `Copy` because it is
/// passed by value everywhere.
///
/// Running sidebars periodically adopt the latest state for their root, so
/// tabs looking at the same project stay aligned.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct TreeState {
    pub expanded: Vec<PathBuf>,
    pub selected: Option<PathBuf>,
}

fn tree_path() -> Option<PathBuf> {
    Some(state_dir()?.join("tree.json"))
}

/// The whole file: tree state per workspace ROOT. One file serves every
/// sidebar in the session, and each agent's tab is rooted somewhere
/// different, so a single unkeyed entry let one project's expansion and
/// selection load under another project's root.
type TreeFile = serde_json::Map<String, serde_json::Value>;

/// Forgiving decode: anything missing, truncated, or written before this file
/// was root-keyed yields an empty map rather than wedging the tree.
fn decode_tree_file(json: &str) -> TreeFile {
    serde_json::from_str::<serde_json::Value>(json.trim_start_matches('\u{feff}'))
        .ok()
        .and_then(|v| match v {
            // Pre-root-keyed shapes (a bare array, then a flat
            // {expanded, selected}) cannot be attributed to a root, so they
            // are dropped instead of applied to whoever opens first.
            serde_json::Value::Object(m) if !m.contains_key("expanded") => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// One root's entry.
fn tree_state_for(file: &TreeFile, root: &Path) -> TreeState {
    let paths = |v: Option<&serde_json::Value>| -> Vec<PathBuf> {
        v.and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let Some(entry) = file.get(&root.display().to_string()) else {
        return TreeState::default();
    };
    TreeState {
        expanded: paths(entry.get("expanded")),
        selected: entry
            .get("selected")
            .and_then(|v| v.as_str())
            .map(PathBuf::from),
    }
}

/// The latest shared tree state saved for `root`.
pub fn load_tree_state(root: &Path) -> TreeState {
    let Some(path) = tree_path() else {
        return TreeState::default();
    };
    let Some(_lock) = StateWriteLock::acquire(&path) else {
        return TreeState::default();
    };
    let Some(json) = std::fs::read_to_string(path).ok() else {
        return TreeState::default();
    };
    tree_state_for(&decode_tree_file(&json), root)
}

/// Best-effort persist of `root`'s entry, leaving every other root's alone.
/// Losing it only costs the next sidebar its starting shape.
pub fn save_tree_state(root: &Path, state: &TreeState) {
    let Some(path) = tree_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Some(_lock) = StateWriteLock::acquire(&path) else {
        return;
    };
    // Read-modify-write: concurrent sidebars in DIFFERENT roots must not
    // erase each other's entries. Same-root sidebars converge on the last
    // complete write during their next idle tick.
    let mut file = std::fs::read_to_string(&path)
        .map(|json| decode_tree_file(&json))
        .unwrap_or_default();
    file.insert(
        root.display().to_string(),
        serde_json::json!({
            "expanded": state.expanded.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "selected": state.selected.as_ref().map(|p| p.display().to_string()),
        }),
    );
    if let Ok(json) = serde_json::to_string(&file) {
        let _ = std::fs::write(path, json);
    }
}

// ---------------------------------------------------------------------------
// The source-control view's shape, mirrored into new tabs (parallel to
// the explorer's tree state). Keyed by the sidebar's cwd: a preview tab's
// sidebar is spawned with the clicked repo as its cwd, so the two share a
// key when the originating sidebar already lived in that repo.
// ---------------------------------------------------------------------------

/// The SCM view state worth mirroring into a fresh sidebar: which drawers
/// are expanded (by title), the active repo's root, a stable id for the
/// selected row, the FILE HISTORY target, the scroll offset, and commit
/// drafts keyed by repository root.
#[derive(Default)]
pub struct ScmState {
    pub drawers: Vec<String>,
    pub active_root: Option<String>,
    pub selected: Option<String>,
    pub history_target: Option<String>,
    pub scroll: usize,
    pub drafts: std::collections::BTreeMap<String, String>,
    /// Draft roots this pane previously observed and has since emptied.
    /// Kept out of the JSON shape; it only scopes merge-on-write removals.
    pub cleared_drafts: std::collections::BTreeSet<String>,
    /// Tree-view folders the user collapsed, as JSON `(repo root, staged,
    /// path)` triples. The tree/list MODE itself is global (`scm_tree_view`).
    pub collapsed_dirs: Vec<String>,
}

fn scm_path() -> Option<PathBuf> {
    Some(state_dir()?.join("scm.json"))
}

/// Stable JSON key for a filesystem path. Git commonly reports `/` on
/// Windows while pane cwd values use `\`; treating those spellings as
/// different loses mirrored SCM state in preview tabs.
pub fn scm_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Forgiving decode: anything missing, truncated, or shaped before this file
/// was cwd-keyed yields an empty map rather than wedging the view.
type ScmFile = serde_json::Map<String, serde_json::Value>;

fn decode_scm_file(json: &str) -> ScmFile {
    serde_json::from_str::<serde_json::Value>(json.trim_start_matches('\u{feff}'))
        .ok()
        .and_then(|v| match v {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// One cwd's entry, decoded into [`ScmState`]. Unknown/missing fields default.
fn scm_state_for(file: &ScmFile, cwd: &Path) -> ScmState {
    let key = scm_path_key(cwd);
    let Some(entry) = file.get(&key).or_else(|| {
        file.iter()
            .find(|(stored, _)| scm_path_key(Path::new(stored)) == key)
            .map(|(_, entry)| entry)
    }) else {
        return ScmState::default();
    };
    let drawers = entry
        .get("drawers")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let collapsed_dirs = entry
        .get("collapsed_dirs")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ScmState {
        drawers,
        active_root: entry
            .get("active_root")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        selected: entry
            .get("selected")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        history_target: entry
            .get("history_target")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        scroll: entry.get("scroll").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        drafts: entry
            .get("drafts")
            .and_then(|v| v.as_object())
            .map(|drafts| {
                drafts
                    .iter()
                    .filter_map(|(root, message)| {
                        message
                            .as_str()
                            .map(|message| (scm_path_key(Path::new(root)), message.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        cleared_drafts: std::collections::BTreeSet::new(),
        collapsed_dirs,
    }
}

/// The SCM view saved for `cwd`, for a sidebar starting up in it.
pub fn load_scm_state(cwd: &Path) -> ScmState {
    let Some(json) = scm_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return ScmState::default();
    };
    scm_state_for(&decode_scm_file(&json), cwd)
}

/// Persist `cwd`'s entry, leaving every other cwd's alone. Returns false when
/// the write cannot be completed, so a graceful-close caller can keep the TUI
/// alive instead of acknowledging data it did not save.
pub fn save_scm_state(cwd: &Path, state: &ScmState) -> bool {
    let Some(path) = scm_path() else { return false };
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return false;
    }
    let Some(_lock) = StateWriteLock::acquire(&path) else {
        return false;
    };
    let mut file = std::fs::read_to_string(&path)
        .map(|json| decode_scm_file(&json))
        .unwrap_or_default();
    let key = scm_path_key(cwd);
    let mut drafts = scm_state_for(&file, cwd).drafts;
    merge_scm_drafts(&mut drafts, state);
    file.retain(|stored, _| scm_path_key(Path::new(stored)) != key);
    file.insert(
        key,
        serde_json::json!({
            "drawers": state.drawers,
            "active_root": state.active_root,
            "selected": state.selected,
            "history_target": state.history_target,
            "scroll": state.scroll,
            "drafts": drafts,
            "collapsed_dirs": state.collapsed_dirs,
        }),
    );
    serde_json::to_string(&file)
        .ok()
        .is_some_and(|json| std::fs::write(path, json).is_ok())
}

fn merge_scm_drafts(stored: &mut std::collections::BTreeMap<String, String>, state: &ScmState) {
    for root in &state.cleared_drafts {
        stored.remove(root);
    }
    stored.extend(state.drafts.clone());
}

// ---------------------------------------------------------------------------
// The root each space's tree is built from.
// ---------------------------------------------------------------------------

/// Remembered tree roots, keyed by workspace LABEL.
///
/// Not by workspace id: ids identify a space *instance*, not a project —
/// closing and recreating `tremor` moved it from `wG` to `wH` within one
/// session — so an id key would hand a new space a root chosen for an
/// unrelated one. The label is intrinsic to the project and survives a server
/// restart; renaming a space forgets its root, which is the accepted cost.
type RootsFile = serde_json::Map<String, serde_json::Value>;

fn roots_path() -> Option<PathBuf> {
    Some(state_dir()?.join("roots.json"))
}

/// Forgiving decode: anything missing or garbled yields an empty map, so a
/// hand-edited file forgets a choice rather than wedging the tree.
fn decode_roots_file(json: &str) -> RootsFile {
    serde_json::from_str::<serde_json::Value>(json.trim_start_matches('\u{feff}'))
        .ok()
        .and_then(|v| match v {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// The root remembered for `key`, if any. An empty key never matches — it
/// would collide across every pane that failed to report its project identity.
fn root_for_key(file: &RootsFile, key: &str) -> Option<PathBuf> {
    if key.is_empty() {
        return None;
    }
    file.get(key).and_then(|v| v.as_str()).map(PathBuf::from)
}

/// The root this project's tree should use, or `None` to fall back to the
/// pane's cwd.
pub fn load_root(key: &str) -> Option<PathBuf> {
    let json = roots_path().and_then(|p| std::fs::read_to_string(p).ok())?;
    root_for_key(&decode_roots_file(&json), key)
}

/// Remember `root` for `key`, leaving other projects' choices alone.
pub fn save_root(key: &str, root: &Path) {
    if key.is_empty() {
        return;
    }
    let Some(path) = roots_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Some(_lock) = StateWriteLock::acquire(&path) else {
        return;
    };
    let mut file = std::fs::read_to_string(&path)
        .map(|json| decode_roots_file(&json))
        .unwrap_or_default();
    file.insert(
        key.to_string(),
        serde_json::json!(root.display().to_string()),
    );
    if let Ok(json) = serde_json::to_string(&file) {
        let _ = std::fs::write(path, json);
    }
}

/// Forgiving parse: any missing/garbled field falls back to the default, so a
/// hand-edited or truncated file can never wedge the panels.
pub fn parse_state(json: &str) -> State {
    let value: serde_json::Value = match serde_json::from_str(json.trim_start_matches('\u{feff}')) {
        Ok(v) => v,
        Err(_) => return State::default(),
    };
    let default = State::default();
    State {
        merged: value
            .get("merged")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.merged),
        active: value
            .get("active")
            .and_then(|v| v.as_str())
            .and_then(View::from_state_name)
            .unwrap_or(default.active),
        search_active: value
            .get("search_active")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.search_active),
        show_hotkeys: value
            .get("hotkeys")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.show_hotkeys),
        show_git_footer: value
            .get("git_footer")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.show_git_footer),
        icons: value
            .get("icons")
            .and_then(|v| v.as_str())
            .and_then(crate::icons::IconTheme::from_state_name),
        color_theme: value
            .get("colors")
            .and_then(|v| v.as_str())
            .and_then(ColorTheme::from_state_name)
            .unwrap_or(default.color_theme),
        font_prompt_done: value
            .get("font_prompt")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.font_prompt_done),
        auto_open: value
            .get("auto_open")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.auto_open),
        strict_toggle: value
            .get("strict_toggle")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.strict_toggle),
        focus_on_open: value
            .get("focus_on_open")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.focus_on_open),
        follow_cwd: value
            .get("follow_cwd")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.follow_cwd),
        git_deco: value
            .get("git_deco")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.git_deco),
        dock_right: value
            .get("dock_right")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.dock_right),
        sidebar_width: value
            .get("sidebar_width")
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
            .map(clamp_sidebar_width)
            .unwrap_or(default.sidebar_width),
        preview_placement: value
            .get("preview_placement")
            .and_then(|v| v.as_str())
            .and_then(PreviewPlacement::from_state_name)
            .unwrap_or(default.preview_placement),
        custom_editor_on_click: value
            .get("custom_editor_on_click")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.custom_editor_on_click),
        scm_tree_view: value
            .get("scm_tree_view")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.scm_tree_view),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace can hold several unrelated project tabs. The caller
    /// combines its workspace label and normalized spawn cwd so a volatile tab
    /// id cannot leak state or grow the file on every server restart.
    #[test]
    fn remembered_roots_are_keyed_by_workspace_and_project() {
        let file = decode_roots_file(
            r#"{"acme::/repo/web":"/repo/web","acme::/repo/admin":"/repo/admin"}"#,
        );
        assert_eq!(
            root_for_key(&file, "acme::/repo/web"),
            Some(PathBuf::from("/repo/web"))
        );
        assert_eq!(
            root_for_key(&file, "acme::/repo/admin"),
            Some(PathBuf::from("/repo/admin"))
        );
        // An unknown space has made no choice yet — the caller falls back to cwd.
        assert_eq!(root_for_key(&file, "acme::/repo/jobs"), None);
        // An empty label must never match; it would collide across spaces.
        assert_eq!(root_for_key(&file, ""), None);
    }

    #[test]
    fn a_garbled_roots_file_forgets_rather_than_wedges() {
        for junk in ["garbage", "[]", r#"{"tremor":42}"#, ""] {
            assert_eq!(
                root_for_key(&decode_roots_file(junk), "acme::/repo/web"),
                None,
                "{junk}"
            );
        }
    }

    /// One state file serves every sidebar in the session, so it has to be
    /// keyed by tree ROOT. Keyed globally, a sidebar spawned in another
    /// agent's tab loaded whatever project the user last touched — their
    /// expansion and selection appeared under a different agent's root.
    #[test]
    fn tree_state_is_per_root_so_agents_do_not_bleed_into_each_other() {
        let a = PathBuf::from("/repo/faultline");
        let b = PathBuf::from("/repo/tremor");
        let file = decode_tree_file(
            r#"{"/repo/faultline":{"expanded":["/repo/faultline/src"],
                                   "selected":"/repo/faultline/src/main.rs"},
                "/repo/tremor":{"expanded":["/repo/tremor/lib"],"selected":null}}"#,
        );
        assert_eq!(tree_state_for(&file, &a).expanded, vec![a.join("src")]);
        assert_eq!(
            tree_state_for(&file, &a).selected,
            Some(a.join("src/main.rs"))
        );
        assert_eq!(tree_state_for(&file, &b).expanded, vec![b.join("lib")]);
        assert_eq!(
            tree_state_for(&file, &b).selected,
            None,
            "tremor has no selection"
        );
        // An unknown root starts fresh instead of inheriting someone else's.
        assert_eq!(
            tree_state_for(&file, Path::new("/repo/other")),
            TreeState::default()
        );
    }

    #[test]
    fn scm_keys_ignore_windows_separator_spelling() {
        let file = decode_scm_file(
            r#"{"C:/repo":{"drawers":["CHANGES"],"active_root":"C:/repo","scroll":4,"drafts":{"C:/repo":"keep me"}}}"#,
        );
        let state = scm_state_for(&file, Path::new(r"C:\repo"));
        assert_eq!(state.drawers, vec!["CHANGES"]);
        assert_eq!(state.active_root.as_deref(), Some("C:/repo"));
        assert_eq!(state.scroll, 4);
        assert_eq!(
            state.drafts.get("C:/repo").map(String::as_str),
            Some("keep me")
        );
        assert_eq!(scm_path_key(Path::new(r"C:\repo\src")), "C:/repo/src");
    }

    #[test]
    fn scm_draft_merge_preserves_unobserved_sibling_writes() {
        let mut stored = std::collections::BTreeMap::from([
            ("/repo/a".to_string(), "newer sibling draft".to_string()),
            ("/repo/b".to_string(), "draft to clear".to_string()),
        ]);
        let mut state = ScmState::default();
        state.cleared_drafts.insert("/repo/b".to_string());

        merge_scm_drafts(&mut stored, &state);

        assert_eq!(
            stored.get("/repo/a").map(String::as_str),
            Some("newer sibling draft")
        );
        assert!(!stored.contains_key("/repo/b"));
    }

    /// Older files were a bare array, then a flat object. Both predate
    /// root-keying and cannot be attributed to a root, so they are dropped
    /// rather than applied to whichever project opens first.
    #[test]
    fn pre_root_keyed_tree_files_are_discarded_not_misapplied() {
        for legacy in [
            r#"["/r/src"]"#,
            r#"{"expanded":["/r/src"],"selected":"/r/src/main.rs"}"#,
            "garbage",
        ] {
            let file = decode_tree_file(legacy);
            assert_eq!(
                tree_state_for(&file, Path::new("/r")),
                TreeState::default(),
                "{legacy}"
            );
        }
    }

    #[test]
    fn state_roundtrip_and_forgiving_parse() {
        let state = State {
            merged: true,
            active: View::SourceControl,
            search_active: true,
            show_hotkeys: true,
            show_git_footer: false,
            icons: Some(crate::icons::IconTheme::Emoji),
            color_theme: ColorTheme::Terminal,
            font_prompt_done: true,
            auto_open: false,
            strict_toggle: true,
            focus_on_open: false,
            follow_cwd: false,
            git_deco: false,
            dock_right: true,
            sidebar_width: 44,
            preview_placement: PreviewPlacement::Pane,
            custom_editor_on_click: true,
            scm_tree_view: true,
        };
        let json = "{\"merged\":true,\"active\":\"source-control\",\"search_active\":true,\"hotkeys\":true,\"git_footer\":false,\"font_prompt\":true,\"auto_open\":false,\"strict_toggle\":true,\"focus_on_open\":false,\"follow_cwd\":false,\"git_deco\":false,\"dock_right\":true,\"sidebar_width\":44,\"colors\":\"terminal\",\"preview_placement\":\"pane\",\"custom_editor_on_click\":true,\"scm_tree_view\":true,\"icons\":\"emoji\"}";
        assert_eq!(parse_state(json), state);
        assert!(parse_state("\u{feff}{\"merged\":true}").merged);
        // Files written before the flag existed keep auto-open AND the git
        // decorations on.
        assert!(parse_state("{\"merged\":true}").auto_open);
        // Files written before the toggle settings existed keep the
        // historical toggle behavior: focus an open sidebar, focus on open.
        assert!(!parse_state("{\"merged\":true}").strict_toggle);
        assert!(parse_state("{\"merged\":true}").focus_on_open);
        assert_eq!(
            parse_state("{\"merged\":true}").color_theme,
            ColorTheme::VsCode
        );
        // The Settings row cycles all three themes and comes back around, and
        // every label round-trips through the state file.
        let mut theme = ColorTheme::VsCode;
        for expected in [ColorTheme::Light, ColorTheme::Terminal, ColorTheme::VsCode] {
            theme = theme.next();
            assert_eq!(theme, expected);
            assert_eq!(ColorTheme::from_state_name(theme.label()), Some(theme));
        }
        assert_eq!(
            parse_state("{\"colors\":\"light\"}").color_theme,
            ColorTheme::Light
        );
        // An unknown name is not a light theme — it falls back to the default.
        assert!(
            !parse_state("{\"colors\":\"solarized\"}")
                .color_theme
                .is_light()
        );
        // Existing installs get neighbour following by default.
        assert!(parse_state("{\"merged\":true}").follow_cwd);
        assert!(parse_state("{\"merged\":true}").git_deco);
        assert!(parse_state("{\"merged\":true}").show_git_footer);
        // Files written before the dock setting existed stay left-docked.
        assert!(!parse_state("{\"merged\":true}").dock_right);
        assert_eq!(parse_state("{\"merged\":true}").sidebar_width, 32);
        assert_eq!(parse_state("{\"sidebar_width\":1}").sidebar_width, 24);
        assert_eq!(parse_state("{\"sidebar_width\":999}").sidebar_width, 80);
        // Files written before the preview setting existed keep the
        // historical behavior: a tab per document.
        assert_eq!(
            parse_state("{\"merged\":true}").preview_placement,
            PreviewPlacement::Tab
        );
        // Undocumented placement names fall back to the stable tab default.
        assert_eq!(
            parse_state("{\"preview_placement\":\"split\"}").preview_placement,
            PreviewPlacement::Tab
        );
        assert_eq!(
            parse_state("{\"preview_placement\":\"nonsense\"}").preview_placement,
            PreviewPlacement::Tab
        );
        assert_eq!(parse_state("garbage"), State::default());
        assert_eq!(parse_state("{\"active\":\"bogus\"}"), State::default());
        assert!(parse_state("{\"scm_tree_view\":true}").scm_tree_view);
    }

    #[test]
    fn scm_state_roundtrips_collapsed_dirs_and_ignores_legacy_tree_view() {
        let json = r#"{"/repo/project":{"tree_view":true,"collapsed_dirs":["src","tests"]}}"#;
        let file = decode_scm_file(json);
        let st = scm_state_for(&file, Path::new("/repo/project"));
        assert_eq!(st.collapsed_dirs, vec!["src", "tests"]);
    }

    #[test]
    fn preview_cycle_keeps_tab_default_and_persists_takeover() {
        assert_eq!(State::default().preview_placement, PreviewPlacement::Tab);
        assert_eq!(PreviewPlacement::Tab.other(), PreviewPlacement::Pane);
        assert_eq!(PreviewPlacement::Pane.other(), PreviewPlacement::Replace);
        assert_eq!(PreviewPlacement::Replace.other(), PreviewPlacement::Tab);
        assert_eq!(
            parse_state(r#"{"preview_placement":"replace"}"#).preview_placement,
            PreviewPlacement::Replace
        );
        assert!(PreviewPlacement::Replace.is_inline());
    }

    #[test]
    fn sidebar_width_steps_and_saturates_within_supported_bounds() {
        assert_eq!(step_sidebar_width(32, true), 36);
        assert_eq!(step_sidebar_width(32, false), 28);
        assert_eq!(
            step_sidebar_width(MAX_SIDEBAR_WIDTH, true),
            MAX_SIDEBAR_WIDTH
        );
        assert_eq!(
            step_sidebar_width(MIN_SIDEBAR_WIDTH, false),
            MIN_SIDEBAR_WIDTH
        );
        assert_eq!(step_sidebar_width(1, true), 28);
        assert_eq!(step_sidebar_width(u16::MAX, false), 76);
    }

    #[test]
    fn follow_cwd_status_stays_compact() {
        if cfg!(windows) {
            assert_eq!(follow_cwd_setting_value(true), "on (host n/a)");
            assert_eq!(follow_cwd_setting_value(false), "off (host n/a)");
        } else {
            assert_eq!(follow_cwd_setting_value(true), "on");
            assert_eq!(follow_cwd_setting_value(false), "off");
        }
    }

    #[test]
    fn views_pair_up() {
        assert_eq!(View::Explorer.other(), View::SourceControl);
        assert_eq!(View::SourceControl.other(), View::Explorer);
        assert_eq!(View::Explorer.label(), "Explorer");
        assert_eq!(View::SourceControl.plugin_id(), "herdr-sidebar-git");
        assert_eq!(View::Explorer.entrypoint(), "sidebar");
        assert_eq!(View::SourceControl.entrypoint(), "source-control");
    }

    #[test]
    fn spawn_env_prepends_the_binary_directory_to_path() {
        let env = spawn_env();
        let path = env.get("PATH").and_then(|v| v.as_str()).unwrap();
        let first = std::env::split_paths(std::ffi::OsStr::new(path))
            .next()
            .unwrap();
        assert_eq!(first, std::env::current_exe().unwrap().parent().unwrap());
    }
}
