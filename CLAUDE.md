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
- PowerShell 5.1 **prepends a UTF-8 BOM when piping into a native process's stdin** — strip
  `\u{feff}` before JSON-parsing anything piped from a `.ps1` launcher (see `strip_bom` in
  both plugins' `launch.rs`).

### Herdr behavior (verified live against herdr 0.7.1, both platforms)

- `pane split --ratio` is the **original** pane's share, not the new pane's. `pane split`
  only goes right/down; a left dock = split the leftmost pane + `pane swap` the new pane
  into the left slot. After a swap, **focus stays with the slot, not the pane**.
- There is no focus-by-id command; a `pane zoom <id> --on` / `--off` cycle focuses a pane
  deterministically.
- `pane send-keys` accepts only a limited key-name set: `Up`/`Down`/`Enter`/`Escape`/`Tab`
  and plain characters work, but `Home` is rejected with `invalid_key`. Give TUIs
  single-char fallbacks (`g`/`G` for Home/End) so they stay drivable via send-keys.

### Verifying a plugin TUI end-to-end

Drive the real binary in a throwaway herdr pane instead of unit-testing rendering:
`pane split --current --no-focus --cwd <scratch repo>`, then `pane run <id> "& '<abs path to exe>'"`
(PS call operator — a bare path splits on spaces), then `pane send-keys <id> Down Enter …`,
capture with `pane read <id> --source visible`, and confirm side effects with plain `git`
commands in the scratch repo. Close the pane when done. Cheap, and it catches layout
truncation bugs unit tests can't.

## Herdr workspace

`herdr-layout.yaml` at the repo root describes the workspace (Coordinator tab running claude,
shell tab, git tab with lazygit). The Coordinator session delegates feature work to sibling
panes — see `.claude/skills/feature-worktree/` (one feature = one git worktree = one pane).
