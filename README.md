<div align="center">

# Herdr Sidebar

### The sidebar your terminal was missing — inspired by VS Code.

A file explorer and a full source-control panel in one dockable
[herdr](https://github.com/ogulcancelik/herdr) pane — activity-bar switching,
mouse-driven controls, AI-drafted commit messages, and file previews that open as editor
tabs — ephemeral until you double-click to pin one.

<img alt="Rust" src="https://img.shields.io/badge/Rust-self--contained_crate-orange?logo=rust&logoColor=white">
<img alt="herdr" src="https://img.shields.io/badge/herdr-%E2%89%A5%200.8-5865a3">
<img alt="Platforms" src="https://img.shields.io/badge/Windows%20%C2%B7%20macOS%20%C2%B7%20Linux-supported-2ea44f">
<img alt="CI" src="https://github.com/alexarthurs/herdr-sidebar/actions/workflows/ci.yml/badge.svg">
<img alt="License" src="https://img.shields.io/badge/license-MIT-blue">

<br><br>

<img src="plugins/herdr-sidebar/docs/media/hero.png" alt="The sidebar docked beside a 2x2 fleet of Claude Code and Codex agents" width="920">

</div>

If you've ever alt-tabbed out of your terminal just to *look* at the tree, the diff, or
what's staged, this closes that loop.

```sh
herdr plugin install alexarthurs/herdr-sidebar/plugins/herdr-sidebar
```

Tagged releases use SHA-256-verified binaries on supported platforms and fall back to a
source build when needed.

## Three Views

The activity bar switches Explorer, Search, and Source Control instantly in one process.
Use the mouse or press `1`, `2`, and `3`.

### Explorer & Preview

<div align="center">
<img src="plugins/herdr-sidebar/docs/media/preview.png" alt="Explorer and file preview" width="920">
</div>

- Navigate a real expandable tree with file icons, hover actions, Git decorations, and
  `m` / Ctrl+right-click context menus.
- Click a file to reuse an ephemeral preview tab; double-click to pin it. Preview in the
  same tab instead by setting **Preview opens in** to `pane`.
- Preview text, Markdown, images, and—when `ffmpeg` is available—video poster frames.
  Read-only previews support mouse selection and clipboard copy.
- Find files with `Ctrl+P`; search project contents with `Ctrl+F` or `Ctrl+Shift+F`.
  Search supports case, whole-word, regex, and include/exclude filters.
- Stage files or folders from the tree without crossing nested-repository boundaries.
- Opt into a terminal editor for mouse clicks, while Enter keeps the built-in preview.
- Press `e` in a text preview for the experimental editor with selection, find,
  clipboard actions, explicit save, and external-change protection.

### Source Control

<div align="center">
<img src="plugins/herdr-sidebar/docs/media/source-control.png" alt="Source Control view" width="920">
</div>

- Stage, unstage, discard, commit, inspect diffs, and sync with the upstream.
- Click the branch name—in the panel header, a repository row, or the Git footer—to
  switch local branches or create a local tracking branch from a remote.
- Use one commit box per repository in multi-repo folders.
- Draft a commit message with the ✧ button through the local `claude` CLI, with a
  filename-based fallback when Claude is unavailable.
- Browse commits, file history, branches, worktrees, remotes, stashes, and tags.
- Keep branch and sync controls visible in every sidebar view with the compact Git footer;
  hide it from Settings if you prefer the extra row.

## Settings

<div align="center">
<img src="plugins/herdr-sidebar/docs/media/settings.png" alt="Sidebar settings" width="920">
</div>

Settings persist across tabs and restarts. Configure:

- Unified or separate Explorer and Source Control panes
- Left/right docking and preferred width
- Material/emoji icons and VS Code/light/terminal colors
- Tab/pane preview placement and optional custom editor
- Hidden files, Git decorations, Git footer, and footer hotkeys
- Auto-open, strict open/close toggle, focus-on-open, and live folder following

The sidebar follows a neighbouring pane's working directory by default. A manually chosen
folder stays put until that pane changes directory again.

## Keys

| Explorer / Search | Action | Source Control | Action |
|---|---|---|---|
| `↑↓` / `jk` | move | `Enter` | stage / unstage |
| `←→` / `hl` | fold / unfold | `a` / `u` | stage all / none |
| `Enter` | toggle / preview | `c` | commit message |
| `Ctrl+P` | quick open | `A` | draft message |
| `Ctrl+F` | content search | `S` | sync |
| `.` | hidden files | `o` | open diff |
| `r` | refresh | `r` | refresh |
| `m` | context menu | `m` | context menu |
| `s` | settings | `s` | settings |
| `b` | hide | `b` | hide |
| `1` / `2` / `3` | change view | `1` / `2` / `3` | change view |

Preview: drag to select, `Ctrl/Cmd+C` to copy, arrows/PageUp/PageDown or
Space/`b` to scroll, `w` to toggle wrapping, and `q` or Esc to close.

Host keybindings can invoke the direct `show-explorer`, `show-search`, `show-git`, and
`quick-open` actions. For example, bind `cmd+p` to:

```toml
[[keys.command]]
key = "cmd+p"
type = "shell"
command = "herdr plugin action invoke quick-open --plugin herdr-sidebar"
```

## Install & Develop

**Requirements:** herdr 0.8+. Source builds require Rust 1.89+.
A Nerd Font is recommended for material icons; the emoji theme works everywhere.

```sh
herdr plugin install alexarthurs/herdr-sidebar/plugins/herdr-sidebar
```

Local checkout:

```sh
cd plugins/herdr-sidebar
cargo build --release
herdr plugin link .
```

Open or toggle it:

```sh
herdr plugin action invoke herdr-sidebar.open-sidebar-windows   # Windows
herdr plugin action invoke herdr-sidebar.open-sidebar           # Linux / macOS
```

Useful development actions:

| Action | Purpose |
|---|---|
| `open-sidebar` / `open-sidebar-windows` | open, focus, or hide the sidebar |
| `open-git` / `open-git-windows` | toggle separate Source Control |
| `show-explorer`, `show-search`, `show-git` | open/focus one activity without toggling |
| `quick-open` | open/focus the sidebar and show the file picker |
| `redeploy` / `redeploy-windows` | refresh running sidebars after a rebuild |

Use the `-windows` suffix for each direct action on Windows.

All docking, metadata, pane creation, and preview control use herdr's socket API directly.
The plugin is one Rust crate; optional external tools only enhance Markdown (`glow`), video
posters (`ffmpeg`), and AI commit drafts (`claude`).

<div align="center">
<sub>Screenshots: herdr on Windows Terminal with a Nerd Font.</sub>
</div>
