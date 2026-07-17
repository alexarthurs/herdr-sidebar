# herdr-aa-filetree

VS Code Explorer for [herdr](https://github.com/ogulcancelik/herdr): a file-tree
TUI pane docked on the left edge of the tab, beside the spaces/agents sidebar.

```
┌ EXPLORER ─────────────────┐
│ 🗂  HERDR                  │
│▸ 📁 .claude               │
│▾ 📂 plugins               │
│  ▸ 📁 herdr-aa-filetree   │
│  ▸ 📁 herdr-aa-git        │
│  🙈 .gitignore            │
│  📝 CLAUDE.md             │
│  🔧 herdr-layout.yaml     │
│  📖 README.md             │
└───────────────────────────┘
```

## Usage

**Sidebar mode (automatic):** `[[events]]` hooks on `tab.focused` /
`workspace.focused` quietly ensure every tab you visit has a left-docked
Explorer — open, unfocused, rooted at that tab's working directory. Because a
tab gets its explorer at first focus (while it still has a single full-height
pane), the explorer becomes a full-height left column that later splits nest
to the right of. Concurrent focus events are serialized through an atomic
mkdir lock, so a burst of events can't open duplicates (verified live: without
it, one tab switch spawned four).

**Manual toggle:** the `open-explorer` action (`open-explorer-windows` on
Windows) toggles: open left-docked → focus if open → close if focused.

**Collapse:** click the `«` button (or press `b`) to shrink the explorer to a
sliver with EXPLORER written sideways — click it (or press any key) to expand
back. The TUI resizes its own pane through the herdr CLI; `pane resize
--amount` is a split-ratio delta, so the exact amount is computed from the
live `pane layout`. herdr's 0.1 minimum split ratio decides how thin the
sliver can actually get.

| Key | Action |
| --- | --- |
| `↑`/`k`, `↓`/`j` | move selection |
| `→`/`l` | expand directory / step into |
| `←`/`h` | collapse directory / jump to parent |
| `Enter`/`Space` | toggle directory |
| `g`/`G`, `Home`/`End`, `PgUp`/`PgDn` | jump / page |
| `r` | refresh from disk |
| `.` | show/hide dotfiles (`.git` is always hidden) |
| `i` | switch icon theme (emoji ↔ material) |
| `b` / click `«` | collapse to the sliver (any key or click expands) |
| `q`/`Esc` | quit |

## Icon themes

- **emoji** (default): colored emoji per file type, works in any font.
- **material**: Nerd Font glyphs tinted like VS Code's *Atom Material Icons*
  theme. Requires herdr's terminal font to be Nerd-Font-patched — if you see
  blanks or boxes, press `i` to go back to emoji.

Set `HERDR_AA_FILETREE_ICONS=material` to start in material mode.

## Why per-tab panes, not one real sidebar?

herdr's plugin API (0.7.1) offers exactly five pane placements — `overlay`,
`popup`, `split`, `tab`, `zoomed` — all scoped to a tab's layout (verified in
herdr's source: `compute_view_internal` splits the screen into the built-in
sidebar plus the tab surface, and panes are tab-owned throughout). Plugins
cannot add workspace-level chrome, so sidebar mode approximates it: every tab
gets its own left-docked explorer via the focus hooks. The launcher splits the
tab's leftmost pane and `pane swap`s the new pane into the left slot
(`pane split` itself only goes right/down).

One caveat: a tab whose left column was already split vertically *before* the
explorer arrives only gets a partial-height dock — herdr has no way to insert
a pane at the layout root. Tabs hooked at creation always get the full-height
column.

## Development

```
cargo build --release
cargo test
cargo clippy -- -D warnings
```

`herdr plugin link .` registers the checkout with a running herdr. On Windows,
launch goes through `scripts/open-explorer.ps1` because herdr cannot spawn a
relative `[[panes]]` command there — see the manifest header for the details
(absolute-path spawning, `\\?\` path stripping, and the UTF-8 BOM that Windows
PowerShell 5.1 prepends when piping into a native process's stdin).
