# herdr-sidebar monorepo

**This file is a living doc — always capture findings.** Whenever you discover something
non-obvious the hard way (a herdr behavior, a Windows quirk, a manifest gotcha, a build issue),
record it here in the relevant section before finishing the task, the way the Windows caveats
below were captured. If you're working in a feature worktree, commit the CLAUDE.md update on
your branch so it lands on main with the merge.

One herdr plugin, a VS Code-style sidebar for the terminal, as a **self-contained Rust crate**:

- `plugins/herdr-sidebar` — file explorer + source control in ONE binary (ratatui TUI).
  Unified mode shows both views in a single "Sidebar" pane with an activity-bar switcher
  (in-process, instant); the ⚙ settings can split them into separate Explorer /
  Source Control panes (`--view explorer|git` pins a pane's starting view). `--preview`
  runs the file-preview pane. Views live in `src/explorer_app.rs` / `src/scm_app.rs`
  (bin modules); shared pieces (icons, ipc, launch parsing, state, ui helpers) are lib
  modules — nothing is copy-mirrored anymore.

There is deliberately **no root cargo workspace**: `herdr plugin install <owner>/<repo>/<subdir>`
treats the subdirectory as the plugin root, and each plugin's `herdr-plugin.toml` points at
`./target/release/<bin>` — a shared workspace would hoist `target/` to the repo root and break
that path. Keep every crate buildable standalone from its own directory.

## Build / test / lint

Run from inside the plugin directory, not the repo root:

```
cd plugins/herdr-sidebar
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
- herdr.dev/docs/plugins lists an `[[actions]]` `contexts` field as REQUIRED, but no
  working plugin ships it (checked herdr-file-viewer, herdr-spreader, ours, herdr-notes)
  — doc/implementation drift; leave it out.

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
  `herdr plugin action invoke herdr-sidebar.redeploy-windows` after rebuilding: it closes
  every herdr-aa pane in every workspace, kills stragglers, and re-docks the focused workspace;
  the others re-dock via the focus hook the moment they're next visited.

### herdr behavior findings (verified live against herdr 0.7.1)

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
  a pane can get. The socket API's `layout.set_split_ratio` ({pane_id?, tab_id?, path:[bool],
  ratio}) sets a split's ratio absolutely (path [] = the tab's root split) — but it clamps to
  the SAME 0.1 floor (requested 0.04, server set exactly 0.1; verified live). There is **no way
  to make a pane narrower than 10% of the tab** short of patching herdr — which is why the
  sidebar HIDES (closes) rather than collapsing to a sliver. (Panes inside a NESTED split can be narrower than 10% of the
  window — the floor is per-split-rect — but the sidebar's column is a root-split child.)
- There is no focus-by-id; focusing a pane is a `pane zoom <id> --on` / `--off` cycle.
- `pane send-keys` accepts only a limited key-name set: `Up`/`Down`/`Enter`/`Escape`/`Tab`
  and plain characters work, but `Home` is rejected with `invalid_key` and
  `PageDown`/`PgDn`/`page-down`/`pgdn` are all rejected as unsupported too. Give TUIs
  single-char fallbacks (`g`/`G` for Home/End) so they stay drivable via send-keys.
- A `pane list` snapshot goes stale the moment you `pane close` a pane: if the closed pane
  was the focused one, the old snapshot still reports it as focused, so deriving a
  split/layout target from it yields `pane_not_found`. Re-run `pane list` AFTER any close
  before computing where to open a replacement pane (bit both notes launchers' REPLACE path).
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
  panes — `Get-Process herdr-sidebar | Stop-Process` before `cargo build --release`.

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
  sidebar TUI tags its pane this way so its detection works while the label is cleared.
- `report_metadata` **MERGES** the token map: sending `tokens: {}` is a no-op, it does NOT
  clear previously-reported tokens. To remove a token, report it with an explicit **null
  value** (`tokens: {name: null}`) — verified live. Token values must be **strings** —
  numbers are rejected with `invalid_request` (and a `let _ =` swallows it silently). A `source` can also report tokens whose
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
- **A TUI cannot detect whether the terminal font renders a glyph** — missing glyphs
  (tofu) still occupy their cells, so cursor-position probing sees nothing. The icon
  theme therefore resolves env → persisted `icons` in state.json → a "Nerd Font
  installed?" probe (Windows font registries via `reg query` / `fc-list` elsewhere),
  and any manual toggle persists (`set_theme`) so a wrong guess is corrected exactly
  once. Installed ≠ selected in the terminal profile: switching WT color schemes via
  the settings UI can silently DROP profiles.defaults.font, reverting the terminal to
  a non-Nerd font while the probe still says material (bit Alex live).
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
  silently means "Enter". Design keymaps so unmodified keys suffice (the commit
  box accepts plain Enter for this reason).
- **AltGr arrives from Windows as CONTROL|ALT on the Char event** in crossterm (no AltGr
  normalization): a guard like `modifiers.contains(CONTROL) => shortcut, return` silently
  swallows `@ { [ ] } \` on German/French/Nordic layouts. Treat CONTROL+ALT chars as text
  to insert, only CONTROL-without-ALT as a shortcut.
- Emoji with variation-selector (VS16) sequences render at inconsistent widths across
  terminal emulators and break column alignment — the shared icon map avoids them; keep it
  that way when adding icons.

### Pane liveness (heartbeat tokens)

- **You cannot detect a dead TUI from outside**: `pane.process_info` shows only the shell
  in the foreground group whether the TUI child is alive or not (verified live), and a dead
  pane keeps its label AND metadata tokens — which used to block the ensure hook's re-dock
  forever. The fix: every TUI **re-stamps its identity token with the unix time** (string!)
  every ~5s; launch decisions treat a stamp older than `HEARTBEAT_STALE_SECS` (20s) — or a
  "Sidebar" label with no token at all — as a corpse and return `REPLACE <id>`: close the
  pane, dock a fresh one. Ensure hook and all launcher scripts handle it.
- **Stamp the heartbeat on EVERY event-loop iteration, not only in the poll-timeout
  branch**: sustained input with <500ms gaps (held-key auto-repeat, a long paste) keeps
  `event::poll` returning true, starving a timeout-branch heartbeat until the launcher
  deems the live pane stale and REPLACE-kills it mid-edit. Same for a debounced autosave
  flush. Both self-throttle, so calling them unconditionally each iteration is free.
- **`pane close` kills the TUI process with no chance to flush** (no signal/console-close
  it can catch in practice) — any debounced-autosave state inside the debounce window dies
  with it. Toggle-off launchers should first drive a graceful save+quit via
  `pane send-keys <id> Escape q` (design the keymap so Esc-then-q saves and quits from
  every mode), short sleep, THEN `pane close` as the cleanup.

### Unified sidebar (see `src/state.rs`)

- Both views ship in ONE binary: the activity bar switches them **in process** (instant,
  no flash — the terminal session is held across switches). The old two-crate host/guest
  process-swap protocol is gone.
- User-facing wording is **"Unified sidebar: on/off"**, toggled in the ⚙ Settings modal
  (`s` key or the gear button) — never "merge"/"detach" in UI text, and the toggle is
  silent (the layout change is the feedback). Off spawns a second pane of the same binary
  pinned with `--view`, and each pane pins to its own view.
- The sticky setting lives in `HERDR_PLUGIN_STATE_DIR/state.json` (resolves to
  `%LOCALAPPDATA%\herdr\plugins\herdr-sidebar\` here) per the herdr plugin docs; herdr
  injects that env for hooks/actions but NOT panes, so every `pane.split` we issue
  forwards it via the `env` param (`state::spawn_env`). Legacy
  `%APPDATA%\herdr\aa-sidebar.json` is migrated on first load. A fresh sidebar opens on
  the last-active view.
- The unified pane reports BOTH identity tokens (`herdr-sidebar-explorer`,
  `herdr-sidebar-git`) so either launcher decision finds it; turning unified off clears
  the other token (null value — report_metadata MERGES token maps).
- `c` (or "Change Folder…" in the context menu / both ⚙ Settings modals) re-roots the
  sidebar via the NATIVE OS folder picker (`rfd`, Windows IFileDialog / macOS NSOpenPanel;
  verified live on Windows). The dialog runs on a BACKGROUND thread polled from the event
  loop — a blocking call would freeze the TUI and the liveness heartbeat would declare the
  pane a corpse after 20s. "Change Folder (Type Path)…" keeps the typed prompt (absolute,
  relative, or ~-prefixed); either path swaps the PROCESS cwd, so the other view follows
  on its next switch.
- Gotcha: after the ✧ suggestion lands, panel focus moves to the message box — letter keys
  then type text instead of triggering actions (Esc returns to the list).

### Source Control view specifics (`src/scm_app.rs`)

- **Multi-repo**: `Git::discover_all` lists the repo containing the cwd plus child repos two
  levels down (`.git` dir or file), skipping `target`/`node_modules`/`.claude` (the agent
  worktrees under `.claude/worktrees` would otherwise show up as repos). With >1 repo the
  layout mirrors VS Code's: each repo section carries its OWN inline message box (3-line
  bordered list row) and ✓ Commit button, and the repo header row shows `⎇branch*` (star =
  dirty) plus clickable ⟳ sync / ✓ commit icons in the fixed last-6 columns. List rows now
  have VARIABLE HEIGHT — mouse hit-testing walks `Row::height()`, and j/k skip the widget
  rows (`Row::selectable()`). The ✧ suggest / S sync keys act on the ACTIVE repo — the one
  the selection is in (named in the panel header).
- **Git drawers** (title-case names, incl. Worktrees): drawer lines carry parsed
  refs (`DrawerRef` — commit hash / stash index / branch / remote / tag / worktree path,
  see `parse_drawer_ref`). Click or ⏎ shows the ref
  via colored `git show --stat --patch` in the SAME preview pane (`show/<root>/<spec>[/<path>]`
  control requests; FILE HISTORY narrows to the followed file). Ctrl+right-click opens
  per-type menus (checkout / merge / cherry-pick / revert / reset / stash apply-pop-drop /
  fetch / delete / copy); destructive ones route through the generic `Overlay::ConfirmGit`
  y/N prompt. Hovered file rows show a `+`/`−` glyph (click zone = last 5 columns) and the
  section headers a section-wide one (last 6); a dim "ctrl+rclick for menus" hint sits on
  the « footer line whenever the footer is otherwise empty.
- **Sync Changes** (`S` or the ⇅ button, shown only when ahead/behind ≠ 0): `pull --rebase
  --autostash` then `push`, on a background thread polled from tick(). Ahead/behind parse
  from the porcelain `## branch...upstream [ahead N, behind M]` header.
- Hotkey hints render as keycap chips (`wrap_hints` takes `(key, label)` pairs, shared in
  `ui.rs`). They live in the ⚙ Settings modal; the FOOTER copy is opt-in via the
  "Footer hotkeys" setting (persisted as `hotkeys` in the state file, default hidden —
  it clipped in narrow panes). The ✧ suggest button uses MDI "creation" (`\u{f0674}`,
  the outline ✨ silhouette) in the material theme.
- There is NO collapse-to-sliver mode anymore (herdr's 10% ratio floor made the sliver
  a wide empty strip — user-rejected). « bottom-right / `b` HIDE the sidebar instead:
  per-tab snooze marker + `pane.close` of its own pane (`hide()` in both apps,
  `src/snooze.rs` shared with the ensure hook, `launch::tab_of`). The herdr keybinding
  `prefix+b` (config.toml `[[keys.command]]` → the toggle action, like the other plugin
  binds) brings it back — or hides it again when it's focused.
- **Esc must never exit a sidebar TUI** — a stray Esc used to drop the pane back to the
  shell prompt (user-reported). Esc closes overlays, then closes the tab's preview pane
  (`viewer::close_in_tab`); only `q` quits.

### Diff preview

- Clicking a changed file in Source Control (or `o`, or the context menu's Open Diff)
  shows its colored `git diff` in the SAME preview pane the explorer uses: the control
  file carries typed requests (`file/<path>` / `diff/<root>/<rel>/<kind>`, tab-separated),
  diffs render VS Code-style via the in-crate `diffview.rs` — OUR parse of plain
  `git diff` (dual old/new gutters, full-width red/green row tints padded at draw time,
  darker word-level tint on paired changed lines, syntax-highlighted code through two
  stateful `LineHighlighter`s for old/new contexts). `ansi.rs` (SGR parser) still renders
  `git show` output (ansi-to-tui pins an older ratatui — don't add it), and diffs re-run
  every ~2s so they live-update.
  Staged rows show `--cached`; untracked files render via `diff --no-index NUL <file>`.

### Syntax highlighting (file preview)

- `syntect` with `regex-fancy` (pure Rust — the default oniguruma engine needs a C build
  that's pain on Windows). syntect's BUNDLED grammar set is Sublime's defaults and lacks
  TypeScript, TOML, Dockerfile and friends — `two-face` supplies bat's extended set
  (`two_face::syntax::extra_newlines()`), themes still from syntect's `ThemeSet` (theme
  data is grammar-independent). Foreground colors only: the terminal owns the background.
  See `src/syntax.rs`; unknown extensions fall back to plain lines.

### Verifying a plugin TUI end-to-end

Drive the real binary in a throwaway herdr pane instead of unit-testing rendering:
`pane split --current --no-focus --cwd <scratch repo>`, then `pane run <id> "& '<abs path to exe>'"`
(PS call operator — a bare path splits on spaces), then `pane send-keys <id> Down Enter …`,
capture with `pane read <id> --source visible`, and confirm side effects with plain `git`
commands in the scratch repo. Close the pane when done. Cheap, and it catches layout
truncation bugs unit tests can't.

## README screenshots (how-to)

The framed screenshots in `plugins/herdr-sidebar/docs/media/` are produced with the
scripts in `tools/screenshots/` (capture → crop → frame). Full reshoot procedure, verified
end-to-end twice:

0. **Shared backdrop (shoot session)** — shots are taken in the isolated
   `herdr --session shoot` server so herdr's left chrome shows a DUMMY roster, kept
   IDENTICAL to the herdr-aa-notes repo's shots (mirrored in that repo's CLAUDE.md):
   spaces `acme-app [main ↑1]` / `acme-api [main]` / `acme-web [dev]` /
   `billing-service [main]`; agents in acme-app's 2×2 grid: `auth-refactor` (claude),
   `checkout-tests` (codex), `api-docs` (codex, unsubmitted composer text),
   `rate-limiter` (claude, unsubmitted composer text); plus FAKE agent rows declared
   via the socket's `pane.report_agent` (persists over herdr's own detection, no CLI
   spawned): `flaky-tests` (codex, working, acme-api), `reviewer` (claude, idle,
   acme-web), `migrations` (codex, working, billing-service). Control the session with
   `HERDR_SOCKET_PATH` = `C:\Users\Alex\AppData\Roaming\herdr\sessions\shoot\herdr.sock`;
   its WT window is titled `herdr-shoot` (launched via `attach_shoot.ps1`, which clears
   inherited HERDR_* env — herdr refuses nested attach). Capture/resize with
   `capture_titled.ps1 'herdr-shoot' <out>` / `resize_titled.ps1` — the un-titled
   variants grab the FIRST WT window and are ambiguous with two open. Link the plugin
   INSIDE the session (`herdr plugin link .` with the socket env set); the ensure hook
   then docks sidebars on tab focus. Keep agent panes ≤63 cols (compact no-email
   banner). Leave the session running for the other repo's reshoots.
1. **Window**: `resize_wt.ps1 1760 996` (note the size it prints as "was" and RESTORE it
   afterwards — the user's own size drifts between shoots). All shots assume 1760×996;
   the crop for it is `crop.ps1 <raw> <out> 8 48 1744 940` (keeps herdr's full chrome —
   tab strip + spaces/agents sidebar — and drops the WT titlebar).
2. **Demo repo**: `setup_demo.sh` rebuilds `C:/Users/Alex/Projects/acme-app` (staged
   docs/auth.md, modified routes.rs, dirty `acme-sdk` child repo, 1 commit ahead of a bare
   `.acme-origin.git`) — multi-repo + sync + diff all have something to show.
3. **Stage**: new tab in this workspace with `--cwd` = acme-app, `herdr tab focus` it,
   invoke `herdr-sidebar.open-sidebar-windows`, close the tab's shell pane.
4. **Shots** (drive via `pane send-keys`, capture via `capture.ps1 <raw.png>`):
   *preview* — explorer view, expand src/api (`Down Down Enter`, `Down Enter`), select
   routes.rs, Enter opens the preview pane. *scm* — `2`, Down×4 to routes.rs, `o` opens
   the diff. *separated* — `s`, Enter toggles unified off (capture, then toggle back).
   *hero* — explorer view, Esc closes the preview, split a 2×2 agent grid to the right
   (0.25 sidebar split, then 0.5, then two down-splits), `claude` + `codex --model gpt-5.5`
   workers with prompts, fresh `claude` and `codex` for the spawn banners. *settings* —
   `s` over the hero layout.
5. **Frame**: `python tools/screenshots/frame_all.py <dir with crop-*.png>` writes the
   framed set straight into docs/media (gradient backdrop + macOS-style titlebar).
6. **Teardown**: close the tab, PEB-scan-kill any process whose cwd is under acme-app
   (see the feature-worktree skill for the snippet), delete acme-app + .acme-origin.git,
   restore the window size.

Hard-won capture gotchas:

- `capture.ps1` is a **screen-space** copy of the WT window rect: whatever overlays that
  region wins the pixels. A fullscreen game on the same monitor, or WT itself hung
  ("Not Responding" — happened during a GPU/TDR wedge), silently yields garbage frames;
  **verify every capture by actually viewing it** before shipping.
- The visible tab is whatever the WT window shows — `herdr tab focus <staging tab>` first,
  and re-check before each capture; pane closes/spawns can bounce focus to another tab.
- Claude's welcome banner is width-dependent: ≲60 cols renders the compact box (no email),
  wider panes render the two-column banner **including the user's email** — keep agent
  panes narrow (even 2×2 columns) or blur with `blur_region.py <img> <x> <y> <w> <h>`.
- Codex: `pane run` delivers the prompt via bracketed paste — send a separate
  `pane send-keys <pane> Enter` to submit. Capture fresh codex panes quickly; an
  intermittent MCP 401 warning can appear ~8s after spawn.
- WT resize calls block while WT's UI thread is busy (modal loops, drags, hangs) — use a
  timeout, and fall back to `resize_wt_async.ps1` (`SetWindowPos` with SWP_ASYNCWINDOWPOS)
  if MoveWindow wedges.

## SCM playground repo

`C:/Users/Alex/Projects/scm-playground` is a PERSISTENT sandbox for exercising the Source
Control view without touching real repos: branches, a second worktree
(`scm-playground-search`), two stashes, two tags, a local bare `origin`
(`.scm-playground-origin.git`, main 1 ahead) plus a `github` remote URL, and a
staged/modified/untracked spread. Rebuild it any time with `tools/setup-playground.sh`
(destructive: wipes and recreates all three directories).

## Herdr workspace

`herdr-layout.yaml` at the repo root describes the workspace (Coordinator tab running claude,
shell tab, git tab with lazygit). The Coordinator session delegates feature work to sibling
panes — see `.claude/skills/feature-worktree/` (one feature = one git worktree = one pane).
