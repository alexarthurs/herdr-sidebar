# herdr-aa-sidebar

**VS Code's sidebar, living in your terminal** — file explorer + source control in one
dockable herdr pane.

<img src="docs/media/hero.png" alt="The sidebar: explorer view with a live file preview beside it" width="860">

**The full tour lives in the [repo README](../../README.md)** — features, screenshots,
keys, and settings.

## Install

```
herdr plugin install alexarthurs/herdr-sidebar/plugins/herdr-aa-sidebar
```

or from a local checkout:

```
cargo build --release
herdr plugin link .
```

Open it (or just focus a tab — the hook docks it):

```
herdr plugin action invoke herdr-aa-sidebar.open-sidebar-windows   # windows
herdr plugin action invoke herdr-aa-sidebar.open-sidebar           # linux / macos
```
