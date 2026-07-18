//! Unified-sidebar state: which layout the user chose (one combined Sidebar
//! pane vs separate Explorer / Source Control panes) and which view was
//! active last, persisted in a small JSON file so every pane and launcher
//! agrees across restarts.
//!
//! - `merged`: the unified sidebar is on (survives restarts).
//! - `active`: the view shown last, so a fresh sidebar opens where the user
//!   left off.
//!
//! Both views live in ONE binary; switching is an in-process re-render, and
//! separated panes are the same binary pinned to a starting view with
//! `--view`.

use std::path::PathBuf;

/// Pane label (and metadata identity) of the unified pane.
pub const SIDEBAR_LABEL: &str = "Sidebar";

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
            View::Explorer => "herdr-aa-sidebar-explorer",
            View::SourceControl => "herdr-aa-sidebar-git",
        }
    }

    /// The `--view` flag value that pins a separated pane to this view.
    pub fn view_flag(self) -> &'static str {
        match self {
            View::Explorer => "explorer",
            View::SourceControl => "git",
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
        assert_eq!(View::SourceControl.plugin_id(), "herdr-aa-sidebar-git");
    }

}
