//! Shared library behind the sidebar binaries: the sidebar TUI
//! (`herdr-sidebar`, hosting BOTH views — file explorer and source
//! control — plus the `--preview` file viewer) and the windowless ensure
//! sidecar (`herdr-sidebar-ensure`).

pub mod actions;
pub mod ansi;
pub mod diffview;
pub mod ensure;
pub mod git;
pub mod icons;
pub mod ipc;
pub mod launch;
pub mod snooze;
pub mod state;
pub mod suggest;
pub mod syntax;
pub mod tree;
pub mod ui;
pub mod viewer;
