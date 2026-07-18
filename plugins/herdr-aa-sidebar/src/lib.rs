//! Shared library behind the sidebar binaries: the sidebar TUI
//! (`herdr-aa-sidebar`, hosting BOTH views — file explorer and source
//! control — plus the `--preview` file viewer) and the windowless ensure
//! sidecar (`herdr-aa-sidebar-ensure`).

pub mod actions;
pub mod ansi;
pub mod ensure;
pub mod git;
pub mod icons;
pub mod ipc;
pub mod launch;
pub mod state;
pub mod suggest;
pub mod tree;
pub mod ui;
pub mod viewer;
