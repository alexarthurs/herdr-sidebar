# herdr plugins monorepo

**This file is a living doc — always capture findings.** Whenever you discover something
non-obvious the hard way (a herdr behavior, a Windows quirk, a manifest gotcha, a build issue),
record it here in the relevant section before finishing the task, the way the Windows caveats
below were captured. If you're working in a feature worktree, commit the CLAUDE.md update on
your branch so it lands on main with the merge.

Two herdr plugins, VS Code-style panels for the terminal, each a **self-contained Rust crate**:

- `plugins/herdr-aa-filetree` — file explorer (VS Code Explorer, but a ratatui TUI in a herdr pane)
- `plugins/herdr-aa-git` — source control panel (VS Code Source Control, same idea)

There is deliberately **no root cargo workspace**: `herdr plugin install <owner>/<repo>/<subdir>`
treats the subdirectory as the plugin root, and each plugin's `herdr-plugin.toml` points at
`./target/release/<bin>` — a shared workspace would hoist `target/` to the repo root and break
that path. Keep every crate buildable standalone from its own directory.

Consequence: the crates **cannot share code**, so common modules are copy-mirrored and must
be kept in sync by hand — `icons.rs` (same emoji map in both) and the `launch.rs` pattern
(stdin-mode launcher helpers). When you change one plugin's copy, check the sibling's.

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
- herdr source: https://github.com/ogulcancelik/herdr — **if you run into issues integrating a
  plugin** (manifest not loading, pane spawn failures, action/IPC behavior that doesn't match the
  docs), read the open-source herdr code there to see what the host actually does, rather than
  guessing from error messages.

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
  delivers `EF BB BF{...}`; verified live by both plugins). serde_json rejects a BOM before
  `{`, so anything parsing herdr JSON from stdin must strip a leading `\u{feff}` first (see
  `strip_bom` in both plugins' `launch.rs`).
- `cargo build --release` fails with **os error 5 (Access is denied)** while the plugin's TUI is
  running in a pane — Windows locks running exes. Close/quit the pane first, rebuild, relaunch.
- **Propagating a rebuild to every workspace**: plugin registration is global (one `plugin link`
  serves all workspaces), but stale panes keep old binaries AND a dead-but-open Explorer/Sidebar
  pane blocks the ensure hook's re-dock (it matches by label/token, not liveness). Run
  `herdr plugin action invoke herdr-aa-filetree.redeploy-windows` after rebuilding: it closes
  every herdr-aa pane in every workspace, kills stragglers, and re-docks the focused workspace;
  the others re-dock via the focus hook the moment they're next visited.

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
- `pane send-keys` accepts only a limited key-name set: `Up`/`Down`/`Enter`/`Escape`/`Tab`
  and plain characters work, but `Home` is rejected with `invalid_key`. Give TUIs
  single-char fallbacks (`g`/`G` for Home/End) so they stay drivable via send-keys.
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

Console flashes from hooks (Windows 11, verified live):

- Any **console process in a hook/action chain briefly flashes a Windows Terminal window**
  when WT is the default terminal — even though herdr spawns plugin commands with
  CREATE_NO_WINDOW. Two hooks per tab switch made every pane-focus flash multiple windows.
  Fix: keep the whole chain GUI-subsystem — `wscript //B scripts/x.vbs` (PATH-resolvable, no
  console) launching a Rust sidecar built with `#![cfg_attr(windows, windows_subsystem =
  "windows")]` that talks to the **socket API directly** and spawns nothing.
- Hook/action commands run with **cwd = plugin root** (`runtime.rs` sets `current_dir`), so a
  relative script **argument** (`scripts/x.vbs`) resolves — the *program* itself still cannot
  be a relative path (resolved against herdr's own dir).
- Rebuilds fail while any plugin exe is running; stray TUI processes can outlive their closed
  panes — `Get-Process herdr-aa-filetree | Stop-Process` before `cargo build --release`.

Socket API (what the CLI wraps; usable directly from plugins, no subprocess needed):

- Windows: open `\\.\pipe\<HERDR_SOCKET_PATH>` as a plain read+write file; unix: connect to
  `$HERDR_SOCKET_PATH` as a unix socket. One request per connection: write
  `{"id":"…","method":"pane.split","params":{…}}\n`, read one JSON line back. Responses have
  the same shape the CLI prints, so CLI-output parsers work unchanged.
- The API is richer than the CLI: `pane.focus {pane_id}` focuses **by id** (the CLI only has
  the zoom-cycle hack). `pane run` = `pane.send_input {pane_id, text, keys:["Enter"]}`.
- Method names/params: `herdr api schema --json`, or `src/api/schema*` in the herdr source.

Mouse in plugin TUIs: herdr forwards clicks/motion/wheel to a pane app that enables mouse
capture — but **right-click is always intercepted** for herdr's pane context menu unless the
click carries the modifier configured in `[ui] right_click_passthrough_modifier` (config.toml;
e.g. `"ctrl"` → Ctrl+right-click reaches the app with ctrl stripped; a modifier is required,
plain-right-click passthrough is not supported). Same-tab `pane.move` is a deliberate no-op
(`SameTab`) — restructure within a tab by bouncing the pane through `--new-tab` and back
(herdr auto-closes the emptied temp tab).

Pane identity & titles:

- `pane.report_metadata {pane_id, source, tokens:{name:value}}` attaches **metadata tokens**
  that show up in `pane.list` — a durable pane identity that survives label changes. The
  filetree TUI tags its pane this way so its detection works while the label is cleared.
- `report_metadata` **MERGES** the token map: sending `tokens: {}` is a no-op, it does NOT
  clear previously-reported tokens. To remove a token, report it with an explicit **null
  value** (`tokens: {name: null}`) — verified live. A `source` can also report tokens whose
  keys belong to another plugin's namespace (the merged Sidebar pane reports both plugins'
  identity tokens so both launchers recognize the one pane).
- Pane border titles come from `border_label`: metadata title → manual label (`pane rename`)
  → detected-agent label. The raw terminal (OSC) title is NOT used — clear the label on a
  non-agent pane and the border shows **no title at all**.
- `layout.apply` does NOT edit a tab in place: it materializes the tree into a **new tab with
  new panes** (and clamps ratios to the same 0.1–0.9 as everything else). Not a way around
  the ratio floor, and it leaves a duplicate tab to clean up.

herdr config: `%APPDATA%\herdr\config.toml`; `herdr server reload-config` applies edits to
the running server ("status":"applied" + diagnostics in the reply).

Terminal fonts for icon glyphs (Windows, verified live):

- Nerd Font "**Mono**" builds squeeze icons into one cell (tiny); the **non-Mono** build
  ("CaskaydiaCove Nerd Font") draws them up to double-width — use it when icons look too small.
- Match the font by its **DirectWrite/typographic family name** (name-table ID 16, e.g.
  "CaskaydiaCove Nerd Font Mono"), NOT the GDI name System.Drawing reports ("CaskaydiaCove
  NFM") — VS Code/WT silently fall back to tofu with the wrong one. Newly installed fonts
  need a VS Code window reload to be seen.
- Sextants (U+1FB00 Symbols for Legacy Computing) and braille are covered by the Cascadia
  family; arbitrary glyph rotation is impossible in terminals — herdr can forward Kitty
  graphics to the host terminal, but Windows Terminal doesn't render that protocol.

Building herdr itself from source (for local patches): needs Zig ≥ 0.15.2 on PATH or via
`ZIG=<path>` (build.rs compiles the vendored `libghostty-vt`); the 0.15.2 zig build failed on
this machine with the known Zig-0.15-Windows linking issue mentioned in libghostty's
HACKING.md — budget time for that before promising a patched build.

### Terminal/TUI gotchas (both plugins)

- Without keyboard-enhancement protocols (not enabled in herdr panes), **modifier+Enter is
  indistinguishable from plain Enter** in most Windows terminals — a "Ctrl+Enter" binding
  silently means "Enter". Design keymaps so unmodified keys suffice (herdr-aa-git's commit
  box accepts plain Enter for this reason).
- Emoji with variation-selector (VS16) sequences render at inconsistent widths across
  terminal emulators and break column alignment — the shared icon map avoids them; keep it
  that way when adding icons.

### Unified sidebar (both plugins, see each crate's `sidebar.rs`)

- Both panels can combine into one **"Sidebar"** pane with a VS Code-style activity bar.
  User-facing wording is **"Unified sidebar: on/off"**, toggled in the ⚙ Settings modal
  (`s` key or the gear button) — never "merge"/"detach" in UI text, and the toggle is
  deliberately silent (no footer flash; the layout change is the feedback).
- The two views share the pane by **process swap**: the first binary is the HOST; switching
  runs the other binary with `--sidebar-guest` in the same terminal and waits; the guest
  exits with code 42 (`EXIT_SWITCH`) to hand back. The host restores the terminal before
  spawning and re-inits after — two TUIs never own the pty at once.
- The sticky setting lives in `%APPDATA%\herdr\aa-sidebar.json` (`{merged, active}`); a fresh
  sidebar opens on the last-active view. `sidebar.rs`, `icons.rs`, and `ipc.rs` are
  **copy-mirrored** between the crates (no shared workspace, deliberately) — edit both.
- The Sidebar pane reports BOTH plugins' metadata tokens so either toggle action finds it;
  turning unified off clears the other token (null value) and splits the other view back out.
- Gotcha: after the ✨ suggestion lands, panel focus moves to the message box — letter keys
  then type text instead of triggering actions (Esc returns to the list).

### Explorer specifics (herdr-aa-filetree)

- Clicking a file (or Enter on it) opens it in a PREVIEW PANE beside the sidebar (the
  tree stays visible): one viewer process (`herdr-aa-filetree --preview <control-file>`)
  per tab, found by its `herdr-aa-filetree-preview` metadata token and steered through a
  control file it polls every 250ms — further clicks reload in place, no pane churn. The
  pane lands between sidebar and editor via split-right-neighbor + swap; `q`/Esc/✕ closes
  it (the viewer pane.closes itself). Double-clicking a folder name toggles it (450ms
  same-row window — crossterm has no native double-click event).
- The activity bar row is 3 rows tall (blank padding above/below the icons).

### Source Control panel specifics (herdr-aa-git)

- **Multi-repo**: `Git::discover_all` lists the repo containing the cwd plus child repos two
  levels down (`.git` dir or file), skipping `target`/`node_modules`/`.claude` (the agent
  worktrees under `.claude/worktrees` would otherwise show up as repos). With >1 repo the
  layout mirrors VS Code's: each repo section carries its OWN inline message box (3-line
  bordered list row) and ✓ Commit button, and the repo header row shows `⎇branch*` (star =
  dirty) plus clickable ⟳ sync / ✓ commit icons in the fixed last-6 columns. List rows now
  have VARIABLE HEIGHT — mouse hit-testing walks `Row::height()`, and j/k skip the widget
  rows (`Row::selectable()`). The ✧ suggest / S sync keys act on the ACTIVE repo — the one
  the selection is in (named in the panel header).
- **Sync Changes** (`S` or the ⇅ button, shown only when ahead/behind ≠ 0): `pull --rebase
  --autostash` then `push`, on a background thread polled from tick(). Ahead/behind parse
  from the porcelain `## branch...upstream [ahead N, behind M]` header.
- Footer hotkeys render as keycap chips (`wrap_hints` takes `(key, label)` pairs now —
  copy-mirrored between the crates). The ✧ suggest button uses MDI "creation" (`\u{f0674}`,
  the outline ✨ silhouette) in the material theme.

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
