# herdr plugins monorepo

Two herdr plugins, VS Code-style panels for the terminal, each a **self-contained Rust crate**:

- `plugins/herdr-aa-filetree` — file explorer (VS Code Explorer, but a ratatui TUI in a herdr pane)
- `plugins/herdr-aa-git` — source control panel (VS Code Source Control, same idea)

There is deliberately **no root cargo workspace**: `herdr plugin install <owner>/<repo>/<subdir>`
treats the subdirectory as the plugin root, and each plugin's `herdr-plugin.toml` points at
`./target/release/<bin>` — a shared workspace would hoist `target/` to the repo root and break
that path. Keep every crate buildable standalone from its own directory.

## Build / test / lint

Run from inside the plugin directory, not the repo root:

```
cd plugins/herdr-aa-filetree   # or plugins/herdr-aa-git
cargo build --release
cargo test
cargo clippy -- -D warnings
```

## Plugin dev workflow

- `herdr plugin link .` (from the plugin dir) registers the local checkout with the running
  herdr; `herdr plugin list --json` shows what's registered.
- `herdr plugin action list` / `herdr plugin action invoke <plugin>.<action>` run manifest actions.
- `herdr plugin log list --plugin <id>` shows plugin logs.
- Manifest format: `herdr-plugin.toml` (`[[build]]`, `[[panes]]`, `[[actions]]`).

### Reference implementations (installed locally, read these before designing)

- `%APPDATA%\herdr\plugins\github\herdr-file-viewer-c993314e2614\` — a mature git-aware file
  viewer plugin (ratatui). Its `herdr-plugin.toml` header documents hard-won **Windows
  findings** — read it before touching manifests.
- `%APPDATA%\herdr\plugins\github\herdr-spreader-f248c87aa2e2\` — minimal manifest + layout tool.
- herdr source: https://github.com/ogulcancelik/herdr

### Windows caveats (verified by herdr-file-viewer against herdr 0.7.1)

- herdr **cannot spawn a relative `[[panes]]` command on Windows** — it resolves the program
  against herdr's own directory and fails with ERROR_PATH_NOT_FOUND. Windows launches must go
  through an action script that spawns the binary **by absolute path** (`pane split` +
  `pane run`), locating the plugin root via `herdr plugin list --json` (strip the `\\?\` prefix).
- Action ids must be **globally unique** across platforms — use `-windows`-suffixed ids for
  the Windows variants and gate both with the item-level `platforms` key.
- herdr panes on this machine run **Windows PowerShell 5.1**: chain with `;` / `if ($?)`,
  never `&&`.
- **PS 5.1 prepends a UTF-8 BOM when piping into a native process's stdin** (`$json | my.exe`
  delivers `EF BB BF{...}`; verified live by herdr-aa-filetree). serde_json rejects a BOM before
  `{`, so anything parsing herdr JSON from stdin must strip a leading `\u{feff}` first.
- `cargo build --release` fails with **os error 5 (Access is denied)** while the plugin's TUI is
  running in a pane — Windows locks running exes. Close/quit the pane first, rebuild, relaunch.

### herdr behavior findings (verified live by herdr-aa-filetree against herdr 0.7.1)

Pane geometry & CLI semantics:

- `pane split` only goes `right|down`. **Left-docking a pane** = split the tab's leftmost pane
  right, then `pane swap --source-pane <new> --target-pane <leftmost>` to move the new pane into
  the left slot.
- `pane split --ratio` is the **original pane's share** (the new pane gets `1 - ratio`).
- After `pane swap`, **focus follows the SLOT, not the pane**: whichever pane now occupies the
  previously-focused slot is focused. Auto-open scripts that split the focused pane must hand
  focus back afterwards (`pane focus --direction right --pane <new>`).
- `pane resize --amount` is a **split-RATIO delta**, not columns (herdr `layout.rs`
  `resize_focused`: `current_ratio ± delta` on the nearest split). Convert columns to ratio via
  the split's rect from `pane layout`. Ratios clamp at **0.1 minimum**, which bounds how narrow
  a pane can get.
- There is no focus-by-id; focusing a pane is a `pane zoom <id> --on` / `--off` cycle.
- Panes are **tab-scoped only**. Plugin pane placements are exactly
  `overlay|popup|split|tab|zoomed` — plugins cannot add workspace-level chrome (e.g. a real
  sidebar next to herdr's own); the closest approximation is a per-tab dock via event hooks.
  There is also no way to insert a pane at a tab's layout root: a full-height left column is
  only achievable by docking while the tab still has a single pane.

Manifest `[[events]]` hooks (undocumented in CLI help; see herdr `src/api/schema/events.rs`):

- `[[events]]` entries (`on`, optional `platforms`, `command`) run a command on
  `workspace.*` / `worktree.*` / `tab.*` / `pane.*` events (`plugin_hook_event_names()` is the
  allowed list); the event payload arrives in the `HERDR_PLUGIN_EVENT_JSON` env var.
- **Focus events fire in bursts** (one tab switch emits `tab.focused` AND `workspace.focused`,
  sometimes more) and hook invocations run concurrently: an unguarded ensure-pane hook opened
  FOUR duplicate panes on one switch. Serialize hook bodies with an atomic `mkdir` lock (with a
  stale-lock timeout) and snapshot `pane list` only after acquiring it.
- Never hook `pane.*` events from a script that itself creates panes — feedback loop.

Pane environment: `HERDR_PANE_ID` is set inside every pane's shell; `HERDR_BIN_PATH` is injected
for **actions/hooks but not panes** — fall back to `herdr` on PATH. A binary started via
`pane run` gets no `HERDR_PLUGIN_CONTEXT_JSON`; root it from its cwd (pass `--cwd` at split).

## Herdr workspace

`herdr-layout.yaml` at the repo root describes the workspace (Coordinator tab running claude,
shell tab, git tab with lazygit). The Coordinator session delegates feature work to sibling
panes — see `.claude/skills/feature-worktree/` (one feature = one git worktree = one pane).
