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

Invoke the `open-explorer` action (`open-explorer-windows` on Windows) — it
toggles: opens the explorer docked on the left of the current tab, focuses it if
it is already open, closes it if it is already focused. The tree roots at the
focused pane's working directory at the moment you open it.

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
| `q`/`Esc` | quit |

## Icon themes

- **emoji** (default): colored emoji per file type, works in any font.
- **material**: Nerd Font glyphs tinted like VS Code's *Atom Material Icons*
  theme. Requires herdr's terminal font to be Nerd-Font-patched — if you see
  blanks or boxes, press `i` to go back to emoji.

Set `HERDR_AA_FILETREE_ICONS=material` to start in material mode.

## Why a pane inside the tab, not a real sidebar?

herdr's plugin API (0.7.1) offers exactly five pane placements — `overlay`,
`popup`, `split`, `tab`, `zoomed` — all scoped to a tab's layout. Plugins cannot
add workspace-level chrome like the built-in spaces/agents sidebar, so the
explorer docks into the left slot of the current tab instead: the launcher
splits the tab's leftmost pane and `pane swap`s the new pane into the left slot
(`pane split` itself only goes right/down).

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
