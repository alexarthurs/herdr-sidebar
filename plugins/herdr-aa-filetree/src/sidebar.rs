//! Merged-sidebar coordination, copy-mirrored between herdr-aa-filetree and
//! herdr-aa-git (keep the two files identical — the module is deliberately
//! pure: no ipc, no crate-specific imports).
//!
//! When both plugins are installed, either panel can merge the two into one
//! "Sidebar" pane with a VS Code-style activity bar: the two views share the
//! single pane and switch by swapping which binary runs in it. The merge is a
//! sticky user setting in a small JSON state file both plugins read:
//!
//! - `merged`: the user turned the merged sidebar on (survives restarts).
//! - `active`: the view shown last, so a fresh sidebar opens where the user
//!   left off.
//!
//! Process-swap protocol: the first binary launched in the pane is the HOST.
//! Switching views spawns the other plugin's binary in the same terminal with
//! `--sidebar-guest` and waits; the guest exits with [`EXIT_SWITCH`] to hand
//! the pane back (host re-runs its own TUI) or a normal code to quit
//! everything. The host↔guest pair never nests deeper than one level.

use std::path::PathBuf;

/// Guest exit code meaning "the user switched back to your view".
pub const EXIT_SWITCH: i32 = 42;

/// Spawn the guest with this flag; a guest exits [`EXIT_SWITCH`] on switch
/// instead of spawning a nested host.
pub const GUEST_FLAG: &str = "--sidebar-guest";

/// Pane label (and metadata identity) of the merged pane.
pub const SIDEBAR_LABEL: &str = "Sidebar";

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
            View::Explorer => "herdr-aa-filetree",
            View::SourceControl => "herdr-aa-git",
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

/// The sticky sidebar setting, shared by both plugins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct State {
    pub merged: bool,
    pub active: View,
}

impl Default for State {
    fn default() -> Self {
        Self { merged: false, active: View::Explorer }
    }
}

/// State file location: `%APPDATA%\herdr\aa-sidebar.json` on Windows,
/// `$XDG_CONFIG_HOME/herdr/aa-sidebar.json` (or `~/.config/…`) elsewhere —
/// beside herdr's own config so it is easy to find and survives temp cleaning.
pub fn state_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    Some(base?.join("herdr").join("aa-sidebar.json"))
}

pub fn load_state() -> State {
    state_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|json| parse_state(&json))
        .unwrap_or_default()
}

/// Best-effort persist; the sidebar still works for this session if it fails.
pub fn save_state(state: State) {
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = format!(
        "{{\"merged\":{},\"active\":\"{}\"}}",
        state.merged,
        state.active.state_name()
    );
    let _ = std::fs::write(path, json);
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
        merged: value.get("merged").and_then(|v| v.as_bool()).unwrap_or(default.merged),
        active: value
            .get("active")
            .and_then(|v| v.as_str())
            .and_then(View::from_state_name)
            .unwrap_or(default.active),
    }
}

/// The other plugin's TUI binary, from a `plugin.list` response: its
/// `plugin_root` (verbatim `\\?\` prefix stripped) joined with the
/// conventional `target/release/<plugin_id>[.exe]`. `None` when the plugin is
/// not registered or its binary is not on disk — the merge affordance hides.
pub fn other_binary(plugin_list_json: &str, other: View) -> Option<PathBuf> {
    #[derive(serde::Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(serde::Deserialize)]
    struct Res {
        #[serde(default)]
        plugins: Vec<Plugin>,
    }
    #[derive(serde::Deserialize)]
    struct Plugin {
        plugin_id: Option<String>,
        plugin_root: Option<String>,
        #[serde(default)]
        enabled: bool,
    }
    let msg: Msg =
        serde_json::from_str(plugin_list_json.trim_start_matches('\u{feff}')).ok()?;
    let plugin = msg
        .result
        .plugins
        .iter()
        .find(|p| p.plugin_id.as_deref() == Some(other.plugin_id()) && p.enabled)?;
    let root = plugin.plugin_root.as_deref()?;
    let root = root.strip_prefix(r"\\?\").unwrap_or(root);
    let exe_name = if cfg!(windows) {
        format!("{}.exe", other.plugin_id())
    } else {
        other.plugin_id().to_string()
    };
    let exe = PathBuf::from(root).join("target").join("release").join(exe_name);
    exe.is_file().then_some(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip_and_forgiving_parse() {
        let state = State { merged: true, active: View::SourceControl };
        let json = "{\"merged\":true,\"active\":\"source-control\"}";
        assert_eq!(parse_state(json), state);
        assert!(parse_state("\u{feff}{\"merged\":true}").merged);
        assert_eq!(parse_state("garbage"), State::default());
        assert_eq!(parse_state("{\"active\":\"bogus\"}"), State::default());
    }

    #[test]
    fn views_pair_up() {
        assert_eq!(View::Explorer.other(), View::SourceControl);
        assert_eq!(View::SourceControl.other(), View::Explorer);
        assert_eq!(View::Explorer.label(), "Explorer");
        assert_eq!(View::SourceControl.plugin_id(), "herdr-aa-git");
    }

    #[test]
    fn other_binary_requires_registered_enabled_plugin() {
        let json = r#"{"result":{"plugins":[
            {"plugin_id":"herdr-aa-git","plugin_root":"\\\\?\\C:\\nowhere","enabled":true}
        ]}}"#;
        // Registered but binary missing on disk -> None.
        assert_eq!(other_binary(json, View::SourceControl), None);
        assert_eq!(other_binary(json, View::Explorer), None, "not registered");
        assert_eq!(other_binary("not json", View::SourceControl), None);
        let disabled = r#"{"result":{"plugins":[
            {"plugin_id":"herdr-aa-git","plugin_root":"C:\\x","enabled":false}
        ]}}"#;
        assert_eq!(other_binary(disabled, View::SourceControl), None);
    }
}
