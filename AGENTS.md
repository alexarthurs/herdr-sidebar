# herdr-sidebar monorepo

**This file is a living doc — always capture findings.** Whenever you discover something
non-obvious the hard way (a herdr behavior, a Windows quirk, a manifest gotcha, a build issue),
record it here in the relevant section before finishing the task, the way the Windows caveats
below were captured. If you're working in a feature worktree, commit the AGENTS.md update on
your branch so it lands on main with the merge.

One herdr plugin, a VS Code-style sidebar for the terminal, as a **self-contained Rust crate**:

- `plugins/herdr-sidebar` — file explorer + source control in ONE binary (ratatui TUI).
  Unified mode shows both views in a single "Sidebar" pane with an activity-bar switcher
  (in-process, instant); the ⚙ settings can split them into separate Explorer /
  Source Control panes (`--view explorer|git` pins a pane's starting view). `--preview`
  runs the file-preview pane. Views live in `src/explorer_app.rs` / `src/scm_app.rs`
  (bin modules); shared pieces (icons, ipc, launch parsing, state, ui helpers, git
  plumbing + `gitdeco` decorations) are lib modules — nothing is copy-mirrored anymore.

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

`plugins/herdr-sidebar/scripts/.gitattributes` pins every shell script to LF. The
repository has Windows contributors and `core.autocrlf` is common, but these files are
executed by Bash on Linux/macOS and mixed or CRLF endings fail before the launcher runs.

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

### Windows caveats (verified against herdr 0.7.1 and 0.8.x)

- Since herdr **0.8.0**, relative action/event programs resolve against the plugin root, so the
  Windows hooks/actions invoke `.\target\release\herdr-sidebar-ensure.exe` directly. Relative
  `[[panes]]` commands still fail on Windows (separate upstream path, herdrdev/herdr#3024), so
  computed docking still uses `pane split` + a shell launch inside the new pane.
- A pane's shell is user-configurable. Never type PowerShell's `& "path"` or sh's `exec "path"`:
  prepend the plugin binary directory to the split pane's `PATH` env and type the bare
  `herdr-sidebar` executable name. Viewer control paths likewise travel in
  `HERDR_SIDEBAR_PREVIEW_CONTROL`, not a shell-quoted argv. This works in PowerShell, pwsh,
  cmd.exe, sh/bash, and nushell.
- Action ids must be **globally unique** across platforms — use `-windows`-suffixed ids for
  the Windows variants and gate both with the item-level `platforms` key.
- herdr panes on this machine run **Windows PowerShell 5.1**: chain with `;` / `if ($?)`,
  never `&&`.
- **PS 5.1 mangles native-command arguments containing double quotes** — even inside a
  single-quoted here-string: `git commit -m @'…"quoted text"…'@` splits the message at the
  embedded `"` into multiple pathspec args (bit an agent live). Write multi-line/quoted
  commit messages to a temp file and use `git commit -F <file>` instead.
- **PS 5.1 prepends a UTF-8 BOM when piping into a native process's stdin** (`$json | my.exe`
  delivers `EF BB BF{...}`; verified live by both plugins). serde_json rejects a BOM before
  `{`, so anything parsing herdr JSON from stdin must strip a leading `\u{feff}` first (see
  `strip_bom` in both plugins' `launch.rs`).
- `cargo build --release` fails with **os error 5 (Access is denied)** while the plugin's TUI is
  running in a pane — Windows locks running exes. Close/quit the pane first, rebuild, relaunch.
  Alternative that avoids racing the ensure hook's re-dock: Windows allows RENAMING a running
  exe — move `herdr-sidebar.exe`/`herdr-sidebar-ensure.exe` aside (`*-old.exe`), build, then
  redeploy; delete the `-old` files after every straggler process exits.
- **PS 5.1 Get-Content/Set-Content mojibake**: round-tripping a BOM-less UTF-8 file
  (`(Get-Content x -Raw) -replace ... | Set-Content x`) reads it as ANSI and corrupts every
  non-ASCII char (— becomes â€”) plus adds a BOM on write. Never edit UTF-8 files with
  PS 5.1 string cmdlets; and write `git commit -F` message files via
  `[IO.File]::WriteAllText(..., UTF8Encoding($false))` (Out-File utf8 BOM leaks U+FEFF into
  the commit subject).
- **CI does not run on first-time contributors' PRs** (needs maintainer approval), so a green
  dependabot row next to a checkless human PR means nothing — run test + clippy locally before
  merging; clippy findings differ per-OS (a unix-only helper passes CI's ubuntu leg but fails
  `-D warnings` dead_code on Windows).
- **Propagating a rebuild to every workspace**: plugin registration is global (one `plugin link`
  serves all workspaces), but panes that survive a rename-aside rebuild keep running old binaries;
  corpse healing replaces dead/tokenless panes, not a still-live pane on the old build. Run
  `herdr plugin action invoke herdr-sidebar.redeploy-windows` after rebuilding: it closes
  Explorer/Source Control/Sidebar panes in every workspace, reaps only the ensure sidecar, and
  re-docks the focused workspace; preview/editor panes survive so unsaved buffers are preserved.
  The others re-dock via the focus hook the moment they're next visited.
- Toggle/ensure behavior has ONE implementation (`ensure.rs`). Unix actions/hooks enter it through
  `herdr-sidebar --ensure|--toggle|--toggle-git`; Windows uses the GUI-subsystem
  `herdr-sidebar-ensure` sidecar with the same mode flags. Do not reintroduce shell launchers whose
  locking, liveness, or settings behavior can drift from the native path.
- Unix opens sidebar TUIs through `plugin.pane.open`, which starts the manifest argv directly and
  never exposes an intermediary shell prompt. Herdr 0.8.2 resolves a relative pane executable
  against the requested cwd, not the plugin root, so omit the API `cwd`: pass the project path in
  `HERDR_SIDEBAR_SPAWN_CWD` and let `main` change directory after process start. Windows retains the
  raw split + PATH-injected shell launch because relative declarative pane commands do not work there.
- **os error 5 can come from ANOTHER Windows account**: if a second account's herdr session
  runs sidebar panes from this same checkout, its processes show empty Path/StartTime in
  `Get-Process`, `Stop-Process` fails silently on them, and redeploy from this account can't
  reach them — the lock only clears when that other session restarts. Rename-aside (above)
  still unblocks the build.

### Release flow

- Settings shows the running binary's Cargo version. `updates.rs` checks stable GitHub
  releases on a background thread (bounded curl request, 30-minute cache); local/third-party
  registrations never offer self-update. The explicit Update action uses Herdr's staged
  `plugin install --ref vX.Y.Z --yes`, preserving the existing checksum-verifying build
  scripts, then verifies the registered version before invoking the NEW installation's
  refresh action. Herdr 0.9.1 exposes install only through its CLI, not the socket API;
  its plugin action runner waits for child completion without an action timeout.
  Update helpers serialize through a private runtime lock (`updates/install.lock`, an OS lock
  the kernel releases on death) and keep install/error logs under the runtime `updates`
  directory. Windows subprocesses use CREATE_NO_WINDOW with redirected streams. Local
  development deployments must still build and refresh explicitly; no automatic background
  installs.
- Update flow invariants (`updates.rs`, `ensure::refresh_all`; hermetic tests drive both
  through injected `UpdateHost` / `RefreshHost` fakes — no production URL/env overrides):
  - The updater `set_current_dir`s into the private runtime dir before installing: herdr runs
    actions with cwd = plugin root, and a process's cwd is the one handle that can block the
    managed-directory rename on Windows.
  - It ALWAYS hands off to the refresh action, even when the latest release is already
    installed: a retry after an install whose refresh failed must restart the sidebars that
    still offer the update. Install only happens for a strictly newer, verified release.
  - Status is `installing` → `refreshing` → `finished`/`failed`, and `finished` is written by
    the REFRESH action once every sidebar really restarted — never when it is merely queued.
    An in-flight record whose install lock is free and that is older than 60s reads as
    "Update interrupted" (retryable), so a killed updater cannot pin "installing…" for 30 min.
  - The Settings row requests the update action on a worker thread (herdr runs actions
    asynchronously, but the IPC round trip must not sit on the TUI loop).
  - **Refresh never hard-closes a sidebar.** SCM commit drafts are NOT persisted per keystroke
    (only on close/quit/switch paths), so `pane.close` would lose them. Every sidebar gets
    Ctrl+Q (save drafts, close self) — even one whose snapshot said "starting", since it may be
    running by the time input lands. Refresh waits up to 5s, reopens only the views whose panes
    actually closed, and counts a reopen only when a fresh sidebar with that view's token is
    present afterwards (`open` can return Ok without docking). Survivors (unsaved draft that
    failed to save, hung TUI) keep running the old build and are reported. Preview, editor,
    inline and takeover viewers are never closed. Sidebar-only tabs are not touched (closing
    the last pane closes the tab) and are reported, so such a refresh is not "finished".
- Live Windows evidence (maintainer host, herdr 0.9.1, 2026-09-30, disposable isolated session
  with `XDG_CONFIG_HOME`/`XDG_STATE_HOME` redirected and socket/context env removed):
  a REAL staged `plugin install` from v0.13.0 to v0.14.0 passed while an old v0.13 Sidebar was
  running in the session — a loaded exe alone does NOT block herdr's managed-directory swap.
  A failing install (nonexistent `--ref`) preserved the v0.14 registration and binary. The old
  `.tmp-install` / previous checkout stayed behind because the still-running old exe prevented
  full cleanup; nothing auto-deletes old viewers. A long scratch root made the source-build
  fallback fail with MSVC LNK1104 (MAX_PATH); a short root succeeded. The NEW updater could
  not be exercised end-to-end because published v0.14 has no refresh action yet. With fresh
  debug builds of the new code in the same kind of isolated managed session, a typed but
  unsubmitted SCM draft (`keep-this-draft`) survived `--refresh-sidebars`: the sidebar came
  back with a NEW pane id and the draft visible, and a second sidebar in the session was
  refreshed too (`target/refresh-smoke.ps1`, maintainer-local).

- Bump the version in THREE files: `Cargo.toml`, `herdr-plugin.toml`, and `Cargo.lock`
  (any cargo command regenerates the lock entry). Commit as `vX.Y.Z`, `git tag vX.Y.Z`,
  push branch + tag.
- Pushing the tag triggers `.github/workflows/release.yml`, which creates or updates the GitHub
  release and uploads SHA-256-verified prebuilt assets. Wait for that workflow before announcing
  the release; edit its generated notes to the house style (headline, Fixes/New sections with
  issue/PR credit, and the install snippet) rather than creating a competing release manually.
- Prebuilt selection is version-based and checksum-verified. Windows needs BOTH the main binary
  and the GUI-subsystem ensure sidecar; a partial download must fall back to the source build.
- Release assets use `herdr-sidebar[-ensure]-<rust-target>[.exe]` plus `SHA256SUMS`.
  Linux publishes both x86-64 and ARM64 musl binaries; the Unix installer maps
  `aarch64`/`arm64` hosts to `aarch64-unknown-linux-musl` before falling back to source.
  Platform-gated `[[build]]` entries fetch only the crate's declared version, verify before
  moving either binary into `target/release`, and fall back to `cargo build --release` for
  unsupported targets, missing assets, or mismatches. The `HS_*` seams are enabled only by
  `HS_TEST_MODE=1`; never make production download origin or executable selection env-driven.
- Merging contributor PRs locally (`git fetch origin pull/N/head:pr-N`, `git merge --no-ff
  pr-N`, push main) marks them MERGED on GitHub, and `Fixes #N`/`Closes #N` in commit
  messages auto-close the issues on push. Branch protection ("changes must be made through
  a pull request") is bypassable as repo admin — the push prints a rule-violation warning
  but succeeds.
- Before merging fetched contributor commits, inspect `gh pr diff N --name-only | rg -i
  '^\.(claude|codex)/|^AGENTS\.md$|^CLAUDE\.md$|^\.github/workflows/'` for agent instructions and
  workflow changes. `.claude/commands/` and `.codex/prompts/` are
  maintainer-local, ignored, rejected by ordinary CI, and independently rejected by a
  `pull_request_target` workflow that never checks out contributor code. A PR can still alter
  `AGENTS.md`, add another root instruction file, or alter tracked skills/workflows, so review
  those paths as instructions, not data.

### Testing hooks headless (no TUI attached)

- Bare `herdr server` STARTS a foreground server (it does not print help) and restores the
  persisted session — all workspaces and panes respawn. Handy for exercising plugin hooks
  from a script; test in throwaway workspaces and close them after.
- Creation hooks dock into their event scope even when `workspace create --no-focus` or
  `tab create --no-focus` targets the background. After docking they re-read `pane.list` and
  restore the previously focused pane only if the split/swap itself focused the new sidebar;
  a concurrent intended focus transition must win.
- Drive the live TUI for verification with `pane send-keys <id> s` (⚙ Settings) etc., then
  `pane read <id> --source visible` to assert on the rendered modal.

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
- `pane send-keys` accepts only a limited key-name set: `Up`/`Down`/`Enter`/`Escape`/`Tab`,
  lowercase modifier chords such as `ctrl+q`, and plain characters work, but `Home` is
  rejected with `invalid_key` and
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
  FOUR duplicate panes on one switch. Serialize native launcher bodies with
  `File::lock`/`try_lock` (Rust 1.89+, OS-backed and crash-released) and snapshot `pane list` only
  after acquiring it. Focus hooks may skip a busy lock; explicit toggles and `tab.created` block
  in the kernel so those discrete actions are not dropped. Do not poll a mkdir lock with sleeps.
- The manifest hooks both `tab.focused` and `workspace.focused`. Herdr 0.8's workspace event
  payload carries only `workspace_id`, so the launcher must prefer the authoritative
  `HERDR_TAB_ID` context injected by Herdr 0.9 when it matches that workspace. The workspace
  hook heals restored sidebars after a v0.9 server restart; `tab.focused` remains the
  unambiguous fallback for ordinary tab switches and older hosts.
- Workspace-scoped create events have no tab-level snooze to respect. Never borrow the
  globally focused tab's marker for a different workspace; an empty/legacy scope may still
  fall back to the focused tab.
- A launcher that hooks `pane.*` and also creates panes must pre-stamp identity while holding
  the shared launcher lock; otherwise its own layout/focus events form a duplicate-pane loop.

Pane environment: `HERDR_PANE_ID` is set inside every pane; `HERDR_BIN_PATH` is injected for
**actions/hooks but not panes** — fall back to `herdr` on PATH. A binary started via terminal input
gets no `HERDR_PLUGIN_CONTEXT_JSON`. Raw split paths root it with the split `cwd`; direct Unix
plugin panes use `HERDR_SIDEBAR_SPAWN_CWD` for the resolution reason above. Spawn env always
forwards `HERDR_PLUGIN_STATE_DIR` and prepends the binary directory to `PATH`.

Console flashes from hooks (Windows 11, verified live):

- Any **console process in a hook/action chain briefly flashes a Windows Terminal window**
  when WT is the default terminal — even though herdr spawns plugin commands with
  CREATE_NO_WINDOW. Two hooks per tab switch made every pane-focus flash multiple windows.
  Fix: keep the whole chain GUI-subsystem — a Rust sidecar built with
  `#![cfg_attr(windows, windows_subsystem = "windows")]` talks to the **socket API directly**
  and spawns nothing. Herdr 0.8+ can invoke that sidecar by its plugin-relative path, so no
  `wscript`/VBS bootstrap is needed.
- Hook/action commands run with **cwd = plugin root** (`runtime.rs` sets `current_dir`), so a
  relative script **argument** (`scripts/x.vbs`) resolves — the *program* itself still cannot
  be a relative path (resolved against herdr's own dir).
- Rebuilds fail while any plugin exe is running; never kill `herdr-sidebar.exe` broadly because
  a surviving preview pane may contain an unsaved editor buffer. Close preview/editor tabs
  deliberately before `cargo build --release`, or use the rename-aside flow above and let old
  processes exit naturally. Redeploy reaps only the short-lived ensure sidecar.

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

Plugin panes cannot read herdr's private UI palette. The `terminal` color theme therefore uses
ANSI named colors that inherit the terminal profile; `vscode` remains the compatibility default
with the historical fixed RGB values. Terminal selections use ANSI `DarkGray`/`White`, not
reverse-video: reverse also swaps per-span git decoration colors and turns a green status dot into
a green background block. Keep every shared accent in `ui::Palette` so Explorer and Source Control
cannot drift.

Light terminal backgrounds (`color_theme = light`, the third value the ⚙ Settings row cycles
through — `ColorTheme::next()` is a rotation, not a toggle):

- `vscode` is VS Code's DARK palette, so on a white terminal the selection/hover/activity-bar
  chip read as dark blocks and the wheat `modified` accent washes out. `light` is the same
  vocabulary drawn for white (VS Code Light+ decorations, GitHub-light diff tints).
- The dark palettes let the terminal's own foreground show through a selected row; a light
  background needs an explicit one, hence `selection_fg`/`selection_unfocused_fg`/`hover_fg`
  in `Palette` (`Color::Reset` = "leave it alone", which is what the dark themes store).
- **Anything drawn outside `ui::Palette` breaks a light theme silently.** The offenders found
  and folded back in: `diffview`'s six tint consts, the preview/editor mouse-selection
  `DarkGray`, `Color::Yellow` advisories, and `LightBlue` glyphs. Add new colors to `Palette`,
  never as a module const.
- `syntect`'s themes are foregrounds only, so a dark grammar theme on white is exactly the
  washed-out case: `syntax::assets()` loads base16-ocean.dark AND InspiredGitHub and
  `syntaxes_and_theme()` picks per `ui::is_light()`. An already-highlighted preview keeps its
  colors until it reloads; diffs re-render on their own ~2s refresh.
- **A named color is the terminal profile's color, so it is not safe on a filled button**:
  `accent_fg: Color::White` emits ANSI 15, which a light profile draws as a pale grey — ✓ Commit
  and the Changes count badge read grey-on-blue (user-reported). Filled buttons now take
  `button_bg`/`button_focus_bg`/`button_fg`, stated in RGB: the dark themes keep the solid accent
  fill, and the light theme uses a soft tint (`#d8eafc`, focus `#b6d8f8`) with `#0a4a86` text,
  because a saturated blue block dominates a white pane (user-rejected). `accent` stays what it
  always was — the focused message-box BORDER — and must not be repurposed as a fill. Sync Changes
  sits on `sync_bg`, not on a button fill, so it has its own `sync_fg`; it used to borrow the
  button's white, i.e. white on a light grey button.
- Icon colors (`icons::material`) stay ONE table; `ui::icon_style` caps their relative
  luminance for light backgrounds instead of a second table that would drift.
- **The `--preview` viewer is its own process and used to skip `set_color_theme` entirely**
  (main.rs returns before the sidebar's own call) — every preview pane ran the default palette
  whatever the user picked. main.rs now sets it before `viewer::run`, and the viewer re-reads
  it on its 5s heartbeat like the sidebar apps do for other shared settings.
- Verifying colors headlessly: `pane read <id> --format ansi` prints the real SGR sequences, so
  `grep -o '4[08];2;[0-9;]*'` asserts the exact RGB a pane emitted — much stronger than the
  text-only capture.

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
  installed?" probe (Windows font registries via `reg query`; macOS's standard
  font directories; Linux `fc-list`),
  and any manual toggle persists (`set_theme`) so a wrong guess is corrected exactly
  once. Installed ≠ selected in the terminal profile: switching WT color schemes via
  the settings UI can silently DROP profiles.defaults.font, reverting the terminal to
  a non-Nerd font while the probe still says material (bit Alex live).
- First run without a Nerd Font: `fontsetup.rs` shows a fullscreen install offer
  (winget on Windows with a curl+bsdtar+HKCU-registry fallback; curl+unzip into the
  user font dir on mac/Linux), on a background thread so the heartbeat keeps beating.
  Answer persists as `font_prompt` in state.json; `HERDR_SIDEBAR_FONT_PROMPT=force|off`
  overrides for testing. Verified live both ways (decline → emoji; install → winget
  registered the family machine-wide). UX invariants (user-reported clip on a fresh
  machine: a ~34-col pane cut the copy off after "Download and install", so the Y/N
  affordances were invisible and the prompt read as static text): every screen
  re-wraps to the pane width and drops blocks lowest-priority-first when short —
  the keycap options ([Y]/[N], and [C] copy on failure) are NEVER dropped; only an
  explicit N/Esc/q declines (stray keys are ignored, Enter = Y); the failure screen
  shows the error plus the exact manual command (`winget install
  DEVCOM.JetBrainsMonoNerdFont` on Windows) copyable with `c`; Esc during a hung
  install stops waiting WITHOUT persisting a theme, so the next start re-probes.
  Test hooks: `HERDR_SIDEBAR_FONT_INSTALL=fail|ok` simulates the installer outcome
  (~2s delay, no real install), and pointing `HERDR_PLUGIN_STATE_DIR` at a scratch
  dir keeps the GLOBAL state.json out of live tests. The prompt also stamps the
  pane's identity heartbeat itself (`ipc::report_identity`, shared with both apps'
  PaneCtl): it runs BEFORE the app loop's first stamp, and a token-less "Sidebar"
  pane older than the launcher's ~6s wait gets REPLACE-killed by the corpse rule
  while the user is still reading the question.
- **WT's bundled Cascadia (checked 1.24: CascadiaCode.ttf/CascadiaMono.ttf) contains NO
  Nerd Font glyphs** — F07B/F0674/E725/E628 all absent from their cmaps (verified with
  fontTools). The "Cascadia now includes Nerd Font symbols" release is the separate
  "NF" variant, which WT does not ship. So there is NO zero-install path to material
  icons, and a "running under WT ⇒ assume glyphs" heuristic would be wrong.
- Sextants (U+1FB00 Symbols for Legacy Computing) and braille are covered by the Cascadia
  family; arbitrary glyph rotation is impossible in terminals — herdr can forward Kitty
  graphics to the host terminal, but Windows Terminal doesn't render that protocol.

Building herdr itself from source (for local patches): needs Zig ≥ 0.15.2 on PATH or via
`ZIG=<path>` (build.rs compiles the vendored `libghostty-vt`); the 0.15.2 zig build failed on
this machine with the known Zig-0.15-Windows linking issue mentioned in libghostty's
HACKING.md — budget time for that before promising a patched build.

### Terminal/TUI gotchas (both plugins)

- **crossterm honors `NO_COLOR`** — and Claude Code's Bash tool sets `NO_COLOR=1`, so a
  herdr SERVER (re)started from an agent shell passes it to every pane it ever spawns and
  all crossterm-drawn UI silently goes monochrome (raw-SGR output still renders, which
  makes it look like a plugin bug; bit Alex live). Both TUIs + the viewer now call
  `crossterm::style::force_color_output(true)` at startup — a TUI's colors are interface,
  not pipeable output. Claude panes inside such a server stay pale until the server is
  restarted from a clean (non-agent) shell.

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

### Following the neighbour pane's folder

- A pane's `cwd` is its SPAWN directory; only `foreground_cwd` is live after a shell
  `cd` or an agent project switch. Following therefore requires `foreground_cwd` and
  deliberately has NO fallback to `cwd`: an absent live value is safer than silently
  re-rooting to a stale directory.
- "Follow pane folder" is persisted as `follow_cwd` in `state.json` and defaults on,
  including for state files written before the field existed. Both views sample on the
  existing ~5s heartbeat and share one process-local `CwdFollower`, so precedence survives
  an app rebuild and unified-view switch.
- Multi-pane precedence is deterministic: focused eligible sibling, then the previously
  followed sibling, then lexical pane id. Candidate lists are sorted before selection;
  `pane.list` response order is never treated as meaningful. Explorer, Source Control,
  Sidebar, and Preview panes are ineligible so plugin panes cannot chase each other.
- A successful typed/native folder choice is a manual override. Focus changes, response
  reordering, and newly-created panes do not overwrite it; following resumes only when an
  already-observed eligible pane's `foreground_cwd` changes. Turning following back on
  resets the baseline and immediately adopts the deterministic neighbour on the next beat.
- Source Control pauses cwd-follow while any repository has a non-empty commit-message draft;
  rebuilding the app for another cwd would otherwise destroy that in-memory draft.

### Pane liveness (heartbeat tokens)

- **You cannot detect a dead TUI from outside**: `pane.process_info` shows only the shell
  in the foreground group whether the TUI child is alive or not (verified live), and a dead
  pane keeps its label AND metadata tokens — which used to block the ensure hook's re-dock
  forever. The fix: every TUI **re-stamps its identity token with the unix time** (string!)
  every ~5s; launch decisions treat a stamp older than `HEARTBEAT_STALE_SECS` (20s) — or a
  "Sidebar" label with no token at all — as a corpse and return `REPLACE <id>`: close the
  pane, dock a fresh one. The native ensure/toggle launcher handles it.
- **Server-restart resume creates corpses that NO event heals by itself**: herdr
  restores panes with their labels and scrollback, but the process inside is a fresh
  shell and metadata tokens are gone; restore and client attach emit NO hookable events
  at all (verified live — tab.created/workspace.created/pane.focused all silent).
  The fix is two-part: (1) label-without-token now counts as a corpse for ALL our
  labels (Sidebar/Explorer/Source Control/Preview), and (2) the ensure hook also runs
  on `pane.focused` + `workspace.focused` + `tab.created` + `workspace.created`, so the
  user's FIRST interaction after attach heals the tab. Hooking `pane.*` is safe because every native launch
  holds the shared lock until it has reported a fresh heartbeat, before swap/focus can release
  queued hooks. A directly spawned Unix TUI also stamps `herdr-sidebar-starting` at process entry;
  the Windows raw-split launcher stamps it before starting the command. The first full identity
  report clears that marker. A toggle may close a still-starting pane directly because it cannot
  contain unsaved in-memory state yet. Label-only panes remain unambiguously restored corpses.
- A focus event may yield when the launcher lock is held because another focus event follows, but
  `tab.created` is discrete and must block for the OS lock or a preview tab can permanently miss
  its sidebar (issue #32). Herdr's `EventEnvelope` serializes the JSON discriminator as
  `tab_created`; manifest hook names remain dotted (`tab.created`). Creation hooks still dock
  their event scope when it is in the background, then conditionally repair focus only when the
  new sidebar stole it, so `--no-focus` remains authoritative without reopening issue #32's
  missed-preview-tab race. Ordinary focus hooks restore the scoped pane because their first
  global snapshot can still point at the tab/workspace the client just left.
- A crashing startup must never create an unbounded `REPLACE` loop. Quiet hooks persist a
  per-tab replacement budget in the private runtime directory: three attempts per minute, then
  leave the corpse in place until another event after the window or an explicit sidebar action.
  Focus/swap events do not clear it; explicit actions bypass and clear it, and expired marker
  files are swept after two windows.
- **Stamp the heartbeat on EVERY event-loop iteration, not only in the poll-timeout
  branch**: sustained input with <500ms gaps (held-key auto-repeat, a long paste) keeps
  `event::poll` returning true, starving a timeout-branch heartbeat until the launcher
  deems the live pane stale and REPLACE-kills it mid-edit. Same for a debounced autosave
  flush. Both self-throttle, so calling them unconditionally each iteration is free.
- **`pane close` kills the TUI process with no chance to flush** (no signal/console-close
  it can catch in practice). A live-pane toggle sends Ctrl+Q and returns immediately; Ctrl+Q is
  handled before overlays/focus modes, persists any SCM draft, and makes the TUI close its own
  pane without writing a snooze marker; the Explorer launcher records snooze after requesting a
  close, while user-invoked `b` / « still snooze inside either app. A save failure keeps the live
  pane open with its error notice. The launcher directly closes only `herdr-sidebar-starting`
  panes, which have not reached an event loop and cannot own a draft. Absorbing a separated pane
  into unified mode uses this same graceful request, then restores the survivor's width from its
  resize event. Do not reintroduce launcher-side acknowledgement polling. Escape remains reserved
  for a tab-scoped preview/editor. SCM snapshots include commit drafts keyed by repo root, so
  graceful close and ordinary `q` restore unfinished text on the next Source Control pane.

### Unified sidebar (see `src/state.rs`)

- Explorer, Search, and Source Control ship in ONE binary: the activity bar switches them
  **in process** (instant,
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
- Sidebar width is a persisted column target (32 by default, 24–80 in 4-column steps), not
  a frozen split ratio. A change to the whole tab area re-applies it through the ratio-aware
  resize path; a pane-only divider resize is respected for the current layout instead of
  snapping back. The existing 15%–50% share bounds still win at extreme tab widths.
- Every Settings action uses `state::update_state`, a lock-protected read-modify-write.
  State/tree/SCM/root writers use persistent sibling `.lock` files with OS-backed locks; the
  kernel releases ownership on process death, so do not poll, age, or delete those files.
  Never write an app's startup snapshot wholesale: preview tabs run independent sidebar
  processes, and a stale snapshot silently reverts newer settings from another tab.
- Separated Explorer and Source Control panes periodically re-read shared display settings,
  including `color_theme`, `strict_toggle`, and `focus_on_open`; theme changes also update
  the process palette immediately. The Settings modal scrolls to keep its selected row visible
  when the pane is too short for all rows and hotkey hints.
- `git_footer` (default true) keeps a one-line branch + sync strip at the bottom of Explorer,
  Search, and Source Control whenever the root belongs to a repository. The normal
  compact `m / ctrl+rclick for menus` hint stays dark-gray and right-aligned directly above it when
  no notice/hotkey content exists, and the strip yields its last three cells to the « hide button.
  The Settings row can hide it.
  Explorer gets its branch/ahead/behind state
  from the same one-at-a-time background status worker as decorations, but status polling stays
  available when decorations are disabled; ignored-path scans do not.
- `dock_right` in the same state file (default false) drives the “Dock on the right” Settings
  row. Launch target/ratio/swap, resize direction, and full-height repair all mirror from that
  one persisted choice. Preview tabs inherit it when the `tab.created` hook docks their sidebar.
- The unified pane reports BOTH identity tokens (`herdr-sidebar-explorer`,
  `herdr-sidebar-git`) so either launcher decision finds it; turning unified off clears
  the other token (null value — report_metadata MERGES token maps).
- `c` (or "Change Folder…" in the context menu / both ⚙ Settings modals) re-roots the
  sidebar via the native OS folder picker: `rfd`/IFileDialog on Windows and an `osascript`
  `choose folder` subprocess on macOS; Linux keeps the typed-path prompt. The dialog runs
  on a BACKGROUND thread polled from the event loop — a blocking call would freeze the TUI
  and the liveness heartbeat would declare the pane a corpse after 20s. "Change Folder
  (Type Path)…" accepts absolute, relative, or ~-prefixed paths. A successful choice marks
  the shared cwd follower as manually overridden before rebuilding either view.
- "Open with Default App" (`actions::open_external`, in the Explorer menu for FILE rows
  only and in the SCM file menu unless the entry's status letter is `D`) hands the path to
  the OS shell association. Windows uses `explorer.exe <path>`, NOT `cmd /c start`: explorer
  is GUI-subsystem, so no console is created and Windows 11 never flashes a Windows Terminal
  window; it resolves the association exactly like a double click (verified live — a .html
  row launched the ChromeHTML handler; a broken/unregistered association falls back to the
  shell's own "Open with" dialog, which is correct behavior). Its exit code is unreliable
  (explorer routinely returns 1 on success), so only the SPAWN is reported. Directories are
  deliberately excluded — their association IS the file manager, which "Reveal in File
  Explorer" already covers.
- "Reveal in File Explorer" opens a selected directory itself; only files are revealed by
  opening their parent with the file selected. Applying file-style reveal semantics to folders
  lands one level too high and looks like the clicked tree row was ignored. Pass the row/menu's
  known directory bit into `actions::reveal`. Explorer deliberately restats symlinks once while
  building rows so links to directories are expandable; downstream actions must trust that bit.
- List UX invariants (both views): NOTHING is highlighted until the user selects
  (hover stays subtle); the wheel scrolls the VIEW only (`scroll_view`) and never
  moves the selection; keyboard nav snaps the view to the selection; overflow shows a
  right-edge scrollbar (`ui::draw_scrollbar`). Implementation note: ratatui's stateful
  List AUTO-SCROLLS to keep its selection visible, which fights wheel-scrolling — both
  views therefore window their rows manually (selected/scroll/snap fields) and render
  a plain List of the visible slice.
- A single left-click anywhere on an Explorer folder row expands/collapses it; the chevron is
  not the only mouse target. Suppress the second click on the folder name inside the 450ms
  double-click window so the first toggle is not immediately undone, while repeated explicit
  chevron clicks continue to toggle each time.
- **`m` opens the context menu from the keyboard** in BOTH views (issue #18: moshi and
  other mobile herdr clients have no right-click at all, and `pane send-keys` can't send
  one either). It routes through the same builder ctrl+right-click uses, so the menus
  can never drift apart; the explorer anchors the popup under the selected row via
  `selection_anchor`, the SCM view via its existing `row_y`. The footer hint reads
  `m / ctrl+rclick: menu` — same width as the old `ctrl+rclick for menus`, so it still
  fits a ~34-col pane.
- Gotcha: after the ✧ suggestion lands, panel focus moves to the message box — letter keys
  then type text instead of triggering actions (Esc returns to the list).
- Explorer Quick Open is `Ctrl+P` and reuses the normal preview client rather than inventing
  a second open path. Its in-process filename index is capped at 20,000 files, never descends
  into `.git`, follows the Explorer dotfile toggle, does not follow directory symlinks, honors
  `.gitignore`/global git excludes through the bundled `ignore` walker, and uses a
  case-insensitive subsequence rank. Never require an external `rg` executable: released
  prebuilts must behave consistently on fresh machines. Build and cache the index on a worker
  polled from `App::tick`: a user-selected root can be enormous, and a synchronous walk can
  starve the heartbeat long enough for the launcher to replace a healthy pane as stale.
- Host-remappable activity actions (`show-explorer`, `show-search`, `show-git`, and
  `quick-open`, each with a Windows-suffixed twin) route through native `ensure::Mode::Activate`.
  They never toggle closed: an existing pane receives an F9–F12 transport key that the PTY decoder
  emits reliably, while a fresh pane receives `HERDR_SIDEBAR_INITIAL_ACTIVITY` so it starts on the
  exact requested view without racing terminal input against its shell/TUI startup. Do not use
  synthetic Ctrl+number here: legacy terminal encoding turns Ctrl+3 into Escape. This is also how
  a host `cmd+p` binding opens Quick Open without pretending terminals can portably report the
  Command key.
- Project content search accepts both `Ctrl+F` and `Ctrl+Shift+F`: terminals that collapse the
  shifted chord still reach the same action. It uses the bundled `ignore` walker on a worker,
  follows the Explorer hidden-file setting, skips `.git`, binary files, and files over 1 MiB,
  and caps both indexed files and returned matches. Results are grouped by relative file path;
  selecting a match sends a line-bearing file request through the existing preview client.
  Search is a persistent first-class activity view, not a popup: it updates after a 300 ms typing
  debounce and exposes VS Code-style match-case, whole-word, and regex toggles. `1`, `2`, and `3`
  select Explorer, Search, and Source Control — the SAME left-to-right order as the activity bar
  and VS Code (Search is `2`, Source Control is `3`). Switching INTO Search does NOT focus the
  search box (`open_content_search(false)` → `SearchFocus::Results`): the box stays unfocused so
  bare `1`/`2`/`3` keep switching views, and a literal digit is still searchable once you focus the
  box. `Ctrl+F` is the "find" gesture that opens/focuses the box ready to type
  (`open_content_search(true)`); `Tab` or a click focus it too. The focus intent crosses the
  cross-view `Exit::Search { focus_query }` boundary (SCM `Ctrl+F` focuses, SCM `2`/`Ctrl+2`/click
  don't; a resumed search restores unfocused). Once a text field IS focused (a search field or the
  SCM commit box) it captures bare digits, so the switch also answers to `Ctrl+1` / `Ctrl+2` /
  `Ctrl+3` from ANY focus in both apps — handled at the top of each `on_key` before the
  overlay/focus dispatch, the keyboard way out of a focused search/commit field. A modal opened
  from Search (branch picker via a footer click, or ⚙ Settings) is parked over the search overlay
  (`suspended_search`) and restored with its query on close, instead of dropping back to the tree.
  The overflow control reveals include/exclude glob
  filters. Unfocused empty inputs render dim placeholders without mutating input state; focusing an
  empty input hides its placeholder and puts the block caret in the first cell. The Replace field is
  always visible without a disclosure chevron, and remains deliberately inert until replacement can
  ship with explicit confirmation, previews, and safe failure semantics. Result line numbers use the
  focus accent, while every visible portion of a literal/whole-word/regex match uses a bold
  header-accent foreground. Match byte ranges come from the search matcher before display clipping
  (including Unicode lowercase-to-source mapping); match styling must not set a background because
  the selected-result row owns it. An untouched empty search shows no helper/status text and does
  not reserve a blank status row; loading, errors, no-results, and result counts still render there.
  Search's Refresh / Clear / overflow toolbar stays visible and uses the same full-size codicon,
  three-cell chip geometry, and dim/keycap hover treatment as Explorer's title actions; do not give
  this one surface permanent filled buttons. Material glyphs are cod-refresh EB37, cod-clear_all
  EABF, and cod-ellipsis EA7C; text-mode fallbacks remain one-cell symbols.
- Custom terminal editors are opt-in. The saved command is parsed into argv and launched directly,
  never through a shell; `{file}` is substituted in arguments or appended when absent. Mouse file
  clicks may open the command in a new herdr tab, while keyboard Enter always retains the built-in
  preview. Editor panes are keyed by a canonical absolute-path hash in `hs-editor-path`, scoped to
  their workspace, and heartbeat every 5s against the common 20s stale limit; clicking that file
  again focuses the live pane through `focus_tab_for_client` instead of launching a duplicate. The
  token is cleared when the editor exits. The saved command wins over `HERDR_SIDEBAR_EDITOR`,
  `VISUAL`, and `EDITOR` fallbacks.
- **Title-bar action buttons** (`ui.rs` `TitleAction`/`title_action_spans`): VS Code-style
  hover buttons at the header's top-right (Explorer: New File / New Folder / Refresh /
  Collapse All; SCM: Refresh / Collapse All), left of the standalone ⚙. Terminals emit NO
  "mouse left the pane" event, so hover is approximated: any mouse event shows them, and
  they fade `TITLE_ACTIONS_LINGER` (3s) after the last one — motion re-shows them before a
  click ever lands, and click zones are only populated while drawn, so a click can never
  trigger an invisible button. Material theme uses the Nerd Font's bundled **codicons**
  (cod-new_file EA7F / cod-new_folder EA80 / cod-refresh EB37 / cod-collapse_all EAC5 —
  VS Code's own icons; verified in the CaskaydiaCove cmap). Chips are a plain ` X ` (one
  space each side, NO activity-bar-style slack cell): the **Mono NF build renders these
  single-cell**, and a trailing slack cell pushes the glyph's right edge to the chip's
  center (user-reported live); the non-Mono build just overflows into the trailing space
  like the tree's file icons do.
- Inactive activity-bar icons and the standalone gear use a faded shade of the active selection;
  do not reuse the smaller/subtler title-action keycap treatment. Hover extends through the same
  three-row half-block geometry as selection, so its target never looks shorter than the selected
  button. The active view's selection chip always wins over hover.
  Hit zones come from each rendered glyph's actual width, so emoji and Nerd Font themes stay aligned.

### Explorer git decorations & staging (`src/gitdeco.rs`, issues #19/#20)

- The Explorer decorates rows from `Git::discover_all` (the SAME repo set the SCM view
  shows) and the SAME letters `parse_status` produces — `M/A/D/R/C`, `U` untracked, `!`
  conflict — plus `I` for ignored. `ui::status_color` is the ONE color table both views
  read; don't reintroduce a per-view copy.
- Decorations are **foreground-only** (`row_bg` owns selection/hover backgrounds,
  `row_line` owns the content). A decorated row must stay readable while selected, so
  never express a status as a background.
- Files show the letter right-aligned; a directory shows a `●` for the loudest status
  among its DESCENDANTS (conflict > tracked change > untracked). Ignored rows get a
  dimmed name and NO marker.
- Row anatomy with a marker is `[prefix][name][pad][marker][2 trailing]`: the two
  trailing cells keep the marker clear of the overflow scrollbar (which overdraws the
  last column), and the NAME ellipsizes so a narrow pane never loses the status.
- **Ignored paths must NOT ride along on the main `-uall` status call**: with `-uall` git
  expands every file inside `target/`/`node_modules/`. `Git::ignored()` is therefore a
  separate `ls-files --others --ignored --exclude-standard --directory` run, where wholly
  ignored directories collapse to one `dir/` entry (including empty ignored directories). On the
  reported 263k-file ignored tree this cut the real query from 110+s to ~75ms, but the defensive
  path still uses `GIT_OPTIONAL_LOCKS=0`, a 5s child timeout, and a non-blocking OS file lock in
  that worktree's git dir (temp fallback for unusual repos), shared across sidebar processes and
  accounts. Windows attaches the child to a Job Object so timeout terminates descendants before
  the pipe readers are released. Busy/timed-out scans preserve cached ignored roots and back off
  for 60s; forced stage/refresh actions still update tracked status but do not bypass that ignored
  backoff. These background reads must never hold `.git/index.lock`, strand a worker, or multiply
  across preview tabs. The main background `status` read also disables optional locks.
- With `-uall`, an **embedded git repo is reported by the parent as one untracked entry**
  (`?? vendor/lib/`, git never descends into it). So a nested repo root can carry BOTH an
  outer `U` and its own aggregate — `Decorations::letter` shows the louder of the two, or
  the folder reads as merely untracked while holding real changes.
- Refresh runs on ONE background worker per Explorer app, requested on a 2s throttle plus
  immediately after staging and on `r`/Refresh. Periodic work backs off while that sidebar
  pane is unfocused — preview tabs each have their own sidebar, so polling every hidden copy
  multiplies git processes. `⚙ Settings → Git decorations` (persisted `git_deco`, exposed
  from both views) turns polling off entirely. Separated panes re-read that field from shared
  state on tick, or toggling it in one view leaves the other's running settings/tree stale.
  Heartbeat/tick collection runs after every event-loop iteration so sustained input cannot
  starve liveness.
- **Staging never hands git a directory.** `git add -A -- <dir>` on a directory holding an
  unregistered inner repo records a gitlink ("adding embedded git repository"), so
  `Git::stage_under` enumerates the repo's OWN status paths under the target, drops
  anything at or inside a nested repo root, and adds those explicitly (batched at 64 to
  stay under Windows' ~32k command line). Ownership comes from `Git::owner_of` =
  `git rev-parse` from the path itself, i.e. the NEAREST enclosing repo — so staging
  inside a nested checkout stages *there*, and staging the parent stops at the boundary.
  Verified live both ways.
- Rename candidates always carry both destination and source paths. Staging only the
  destination leaves the old path as an unstaged deletion, so a row or directory containing
  either side stages the coherent rename pair.
- `stage_under` returns `Staged { count, skipped_nested }`: a stage that skipped
  everything must SAY it hit a nested repo, or the boundary rule reads as a silent no-op.
- The Explorer is a filesystem tree, so a **deleted file has no row** — its `D` shows in
  Source Control, and the containing folder's `●` is what reveals it in the tree.

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
- Branch labels are actions, not decoration: clicking the Source Control panel header, a
  multi-repo header's branch label, or either view's Git footer opens the shared `BranchPicker`.
  Local choices use a normal checkout; a remote choice creates its local tracking branch.
  Symbolic `<remote>/HEAD` aliases are omitted. Dirty-worktree checkout failures surface intact
  and never force, stash, discard, or otherwise mutate work to make the switch succeed.
- Periodic Source Control status/drawer refresh backs off while its pane is unfocused, just
  like Explorer decorations. Suggestion/sync worker results are still collected first so a
  hidden pane never strands completed background work.
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
  shell prompt (user-reported). Esc closes overlays, then closes this tab's preview
  pane if one is docked here (`viewer::close_in_tab`); only `q` quits. Inside a
  preview TAB, Esc/`q`/the ✕ button close the whole preview tab (`close_own_pane`);
  closing only its viewer leaves a sidebar-only husk that still looks interactive. Whole-tab
  closure requires a post-move `hs-preview-dedicated` ownership token AND an all-plugin pane
  whitelist; if the user added a shell/agent pane, only the viewer closes.

### Preview placement and tabs

`Preview opens in` now cycles `tab` (unchanged default), `pane`, and `replace`.
`replace` is temporary takeover (#64): `takeover.rs` journals the original split
tree and terminal identities under a private, session-and-tab-keyed path BEFORE
moving working panes to a snoozed background tab, then opens one inline viewer.
All layout work holds `LaunchLock`, so queued creation hooks cannot dock a sidebar
into the parking tab. Invariants (each has a hermetic test against a simulated
host in `takeover::tests`):
- **A present viewer is never moved or closed by recovery**, whatever its heartbeat:
  a stale stamp can be a suspended laptop or a stalled loop holding unsaved edits.
  Only the journal's own closing viewer (`restore(tab, closing_viewer)`), or a
  viewer that is gone, lets a restore run. Replace clicks over a present viewer
  report instead of spawning another.
- **Restore is best-effort and always finishes**: panes the user closed or moved to
  a third tab are pruned from the tree; a hidden sidebar is replaced by the first
  surviving pane as anchor; panes added during the preview stay where they are; a
  subtree whose move fails stays in the parking tab, relabelled
  `Working panes (not restored)` and un-snoozed. The journal is deleted either way,
  so a lost pane can never trap the tab. A closed previewed tab (or ids changed by a
  server restart) abandons the journal on the next hook sweep of THIS session only.
- **Hook retries are budgeted** (3 real attempts per 60s). A hook that finds the
  viewer still open does not spend budget, so focus bursts never delay recovery.
- If the viewer's own restore fails, the first q/Esc shows why and puts the
  control file back to the current document; only a second deliberate close
  abandons the layout (panes stay in the relabelled tab).
- **Running-TUI transport (M1) — partly verified.** Takeover moves RUNNING user panes
  across tabs, and the v0.8.0 finding above says moving a running crossterm TUI can
  invalidate its Windows input handle. Evidence on the maintainer's Windows 11 host
  (herdr 0.9.1, 2026-09-29), both ignored live tests run serially (8.91s), PASSED:
  `takeover::tests::live_parked_running_tui_still_takes_input_after_restore` parked a
  RUNNING read-only crossterm preview viewer in the temporary tab, restored it, and it
  still took a key afterwards; `takeover::tests::live_takeover_roundtrip` restored
  left/right-docked nested geometry exactly and recovered after a forced viewer close.
  Both need an attached Herdr session (`HERDR_ENV=1`) and a fresh build on PATH, and
  touch only their own disposable tabs. **Still untested:** third-party TUIs (Claude
  Code, vim, other agents) and other hosts/versions — check those manually before
  recommending `replace` on Windows.

Stale previews are never killed from routing or Esc anymore: a click excludes a
stamped-but-stale viewer from routing without waiting (an unrelated stale viewer
costs nothing; a stale inline viewer in the caller's own tab is handed the request
and reported), and the sidebar's Esc writes a close request. Only label-only panes
from a server resume (no heartbeat token) are closed directly, and the orphan
control sweep keeps a present viewer's control file. The viewer's heartbeat
cadence uses wall-clock time: `Instant` stops during suspend, so after resume the
old stamp would stay stale for up to 5s while recovery reads it.
The ignored `takeover::tests::live_takeover_roundtrip` test requires an attached
Herdr session and the newly built sidebar on PATH; it creates disposable test tabs,
checks left/right docks with nested work panes, and verifies real `q` restoration.

SCM tree/list mode (#66/#76) is toggled with `t`, the title action, or Settings.
The mode is ONE global setting (`scm_tree_view`), re-read on every tick like the
other shared settings; a per-folder copy used to override it forever. Collapsed
folders persist per repository and staged/unstaged scope and are pruned to folders
that still exist — judged against ALL current compacted folder paths, not the visible
rows (a child hidden inside a collapsed parent keeps its state), and never against a
not-yet-loaded, empty status. Folder
stage/unstage goes through `Git::stage_under` / `Git::unstage_under` — batched and
nested-repository-safe, never one process per file on the UI thread. The folder
button only fires on the hovered row (a tap folds instead); keyboard parity is
←/→ fold and `m` for a Stage/Unstage Folder menu.
The branch picker supports New branch (#77); branch deletion first uses `-d`
and asks separately before `-D` (#78). Keep `--` before branch names in both paths.
Preview Space/`b` page down/up (#73); Shift+Space is indistinguishable in legacy
terminal keyboard mode and must not be used as the sole page-up gesture.

Windows agent-command note: if ordinary tool shells create visible Windows Terminal
windows, run shell calls with a headless PTY (`tty: true`) and `login: false`.
MCP helper processes can also create startup consoles; identify their parent PID
before stopping any, and never kill the shared WindowsTerminal process.

Tab placement follows **VS Code's editor-tab semantics**, mapped onto herdr TABS.
It replaced the original pane-ID-keyed park/restore implementation; the optional
`replace` mode above uses a new journal instead of reviving that implementation.

- Clicking a file opens it in **its own tab** and jumps there. Clicking a different
  file **overwrites that same tab** — it is EPHEMERAL, and its tab label reads
  `name · preview` (`*` looked like an unsaved edit). **Double-clicking** pins it
  (the suffix drops, `pin_target`); the next file
  then gets a fresh ephemeral tab. Selecting something that already has a tab jumps
  to it instead of opening a second one. Both gestures work in the Explorer AND the
  Source Control view (staged/unstaged diffs, and git-graph refs — commits, stashes,
  branches, tags).
- `Preview opens in: pane` is an explicit opt-in that instead keeps one inline viewer
  in the sidebar's own tab and reuses it per caller tab. `tab` remains the default.
  Inline placement never parks or moves the user's panes to another tab, never claims
  `hs-preview-dedicated`, and `q`/Esc closes only the viewer pane. Placement is stamped
  on the viewer with `hs-preview-inline`; do not infer it later from mutable settings.
- Why the inversion: full-size mode evacuated the CURRENT tab (parking the user's
  terminals into a background "· preview" tab), and the park plan was keyed by the
  SIDEBAR's pane id — which churns on every redeploy and every ensure-hook heal. The
  "already parked?" guard therefore never fired for the new id, so each preview
  parked the same terminals into yet another new tab and orphaned the previous plan;
  nothing ever restored (eight orphaned plans in one workspace, observed). Moving the
  preview OUT instead of moving the user's panes aside removes the failure class: the
  user's tab is never touched and there is no restore plan to go stale.
- State lives ON THE PANE, so it cannot outlive what it describes: the viewer stamps
  `hs-preview-path` (a fixed 16-hex fingerprint of the document key) and pinning adds
  `hs-preview-pinned`, both read
  back out of `pane.list` (`previews_in` / `preview_for_doc` / `reusable_preview`).
  Document keys distinguish a file, a diff OF that file, and a `git show` touching it
  (`doc_key_for_file` / `doc_key_for_diff` / `doc_key_for_show`).
- The launcher stamps document/control/heartbeat metadata synchronously after `pane.split`,
  BEFORE moving or starting the viewer. It then moves the still-idle shell pane into the new tab
  and starts the TUI there: moving a running crossterm TUI can invalidate its Windows input handle,
  leaving a frozen first frame and a control file nobody reads. Viewer pane labels keep stable
  ` · preview` / ` · editor` suffixes so a server-resumed pane with lost tokens is still
  reclaimable; the classifier also retains the legacy `Preview · ` / `Editor · ` prefixes.
- Preview routing never routes to a stale viewer, but only CLOSES label-only server-resumed
  viewers (no heartbeat token) — their whole tab only when every pane is recognizably
  plugin-owned; a real shell/agent pane forces narrow viewer cleanup. A stamped-but-stale
  viewer may hold unsaved edits and is left alone (see Preview placement above). Redeploy closes/restarts sidebar panes but NEVER kills the shared
  `herdr-sidebar` process name wholesale: a spared Preview may contain an unsaved editor buffer.
- Herdr truncates long metadata token values (an absolute `%TEMP%` control path was shortened to
  a different, valid-looking filename). Control metadata therefore carries only a compact basename,
  reconstructed under the private scratch directory; document metadata uses the fixed fingerprint
  above so long project paths cannot be truncated into routing collisions. Control files get a
  unique pre-spawn path, carried in
  `HERDR_SIDEBAR_PREVIEW_CONTROL` so every configurable pane shell can launch the bare
  `herdr-sidebar --preview` command without quoting a path. The viewer stamps that path in
  `hs-preview-control`; any sidebar can then steer the tab after its pane is moved.
  `sweep_orphan_controls` removes legacy pane-keyed files and abandoned pre-spawn files.
- Routing is scoped to the **caller's workspace** — a session-wide search reused
  another project's ephemeral tab, rewrote it, and yanked focus into that space.
- A double click pins the tab the FIRST click returned (`PreviewTarget`), not one
  re-resolved by document key: re-resolving raced the viewer's first token stamp
  inside the 450ms double-click window and spawned a duplicate tab.
- Pinning also verifies that the viewer's current `hs-preview-path` ACKNOWLEDGES that
  document. A dirty editor may reject the first click's switch; pinning its target before
  acknowledgement would pin the old document when the user cancels. A clean switch is automatic:
  `pin_target` polls that acknowledgement for up to 800 ms so a fast double-click does not expose
  the normal control-file/heartbeat delay as a bogus confirmation warning.
- The first operation that actually dirties the experimental editor pins its tab immediately.
  Clean edit mode remains reusable; dirty editor tabs are excluded from `reusable_preview`, so a
  file click opens a fresh preview instead of presenting a switch prompt in the dirty buffer.
- Pinning is driven from the TREE, not the tab bar: herdr exposes no tab-bar mouse
  event and no pin concept to plugins, so `tab.rename` (the textual preview suffix) is the only
  display lever a plugin has.
- New tabs come up **mirroring the view you clicked from** — the `tab.created` hook
  docks a sidebar into the preview's tab, and it reads back the explorer's tree
  (`tree.json`: expanded dirs + selected row, keyed by ROOT) and the SCM view
  (`scm.json`: expanded drawers, active repo, selected row by repo-qualified stable id,
  FILE HISTORY target, scroll — keyed by workspace cwd AND each discovered repo root so a
  nested-repo preview tab restores the originating state). SCM keys and active roots normalize
  `\` to `/`: Git commonly reports forward slashes on Windows while pane cwd uses backslashes,
  and treating them as different paths silently loses the mirror. Explorer panes also re-read
  their root-keyed tree entry on each idle tick and adopt changed expansion/selection, so
  same-root tabs converge live and a preview tab lands on the file that opened it. Reads share
  the writer's OS lock and normalize expansion order before comparing, avoiding partial JSON and
  needless rebuild loops. SCM remains startup-only and saves on user-action paths to avoid timer
  write-churn. Paths outside the tree's root are dropped, since one file serves every workspace.
- Sidebar roots are remembered per **workspace label + normalized spawn cwd**. A workspace
  can hold unrelated project tabs, so label-only keys race and leak roots across those tabs;
  tab ids change across server restarts and would grow `roots.json` forever. The project-path
  key also preserves an explicit manual root across restarts. v0.10 label-only entries migrate
  only when the remembered path contains the tab's spawn cwd. Every successful manual or
  followed re-root is written to `roots.json`; a read-only `load_root` API is dead behavior.
- The ensure hook roots a docked sidebar from **the event's own tab**
  (`event_scope_in` → `launch_decision_in` / `focused_pane_in`): during a workspace
  switch the globally focused pane is still the space you came from. Both the Unix main-binary
  entrypoint and Windows sidecar enter the same `ensure.rs` implementation. (Historically PR #15
  scoped only the removed Unix shell path, leaving the Windows cross-space bug.)
  `pane.focused` has no `tab_id`, so resolve its `pane_id` through the same `pane.list`
  snapshot; a workspace scope with several tabs is ambiguous and must not pick one.
  Spawn roots prefer `foreground_cwd` but fall back to `cwd` because Windows herdr 0.8 does
  not currently emit the live field; continuous following still requires `foreground_cwd`
  and never resurrects stale `cwd`. Toggles stay unscoped (a deliberate act on the focused tab).

### Diff preview

- Clicking a changed file in Source Control (or `o`, or the context menu's Open Diff)
  shows its colored `git diff` in a preview tab, the same way the explorer opens
  files: the control file carries typed requests (`file/<path>` /
  `diff/<root>/<rel>/<kind>`, tab-separated), diffs render VS Code-style via the
  in-crate `diffview.rs` — OUR parse of plain `git diff` (dual old/new gutters,
  full-width red/green row tints, darker word-level tint on paired changed lines,
  syntax-highlighted code through two stateful `LineHighlighter`s for old/new
  contexts). `ansi.rs` (SGR parser) still renders `git show` output (ansi-to-tui pins
  an older ratatui — don't add it), and diffs re-run every ~2s so they live-update.
  The refresh runs on a worker thread so a slow Git process cannot starve the viewer heartbeat;
  unchanged output is left in place so a mouse selection is not erased every two seconds.
  Staged rows show `--cached`; untracked files render via `diff --no-index NUL <file>`.

### Long-line wrapping in the preview (`src/wrap.rs`)

- Long lines WRAP by default; `w` toggles wrapping off for the current document
  (per-document — a newly loaded doc starts wrapped again), and the footer shows which
  state you are in.
- Tabs always expand to 4-column stops through `wrap_line`, even with wrapping off or when
  ratatui's raw Unicode width says the line fits (it counts C0 tab as width zero).
- **Do NOT wrap with ratatui's `Paragraph::wrap`** here. The viewer slices the visible
  lines out of the doc BEFORE handing them to the Paragraph, so the widget's
  continuation rows render past the bottom of the pane and a scroll that counts SOURCE
  lines can never bring them back: the tail of a wrapped file is unreachable, and a
  line taller than the pane is permanently clipped. (That was the flaw in PR #17 as
  submitted.)
- The fix: the viewer wraps the lines ITSELF (`wrap::wrap_line`) into a `Vec<Row>` of
  RENDERED rows, each tagged with the source line it came from, and `doc.scroll`
  indexes THOSE. Every continuation is then an ordinary scrollable row — ↑↓, page,
  `g`/`G`, the wheel and the end-of-doc clamp all count rendered rows. `wrap_line` is a
  greedy word wrap over a flat (char, width, style) run: it breaks at the last space
  that fits, hard-breaks a word wider than the pane (nothing is ever dropped), never
  trims indentation, measures with `unicode-width` (wide CJK = 2 cells), preserves
  per-span styles across a break, and carries the LINE style so a wrapped diff row
  keeps its tint — every row is padded to the pane edge, so continuations get the
  full-width band too.
- Rows are cached per `(width, wrap)` and rebuilt only when one of those changes — a
  resize or a `w` press, not every frame.
- Both the `w` toggle and the ~2s diff live-refresh re-anchor the scroll by SOURCE
  LINE (`Doc::top_src` → `Doc::pending_src`), so the reader keeps their place even
  though every row index underneath them moved.
- The line-number gutter numbers the FIRST row of a source line and indents
  continuations to the same column, like an editor.
- Read-only previews still support terminal-native interaction despite mouse capture: click/drag
  selects rendered text, Shift+click extends it, and Ctrl/Cmd+C copies it. Selection preserves
  syntax/diff styling on screen, omits file line-number gutters, and does not insert newlines at
  visual wrap boundaries.
- Preview return focus is durable pane metadata (`hs-preview-origin-tab`), not
  `PreviewTarget.tab_id` (that is the preview tab itself). Reusing an ephemeral preview from
  its own sidebar preserves the original origin; focus it before closing because closing the
  current tab can kill the viewer before any follow-up IPC runs.
- Preview requests load on a worker after immediately replacing the pane with a lightweight
  `loading preview…` document. File reads, syntax setup, `glow`, image decode, video extraction,
  and git subprocesses must not block the viewer's event loop before it can acknowledge a click.
  A newer control-file request replaces the receiver; a late result is applied only when its
  request still matches the current document. Collect the receiver before drawing each frame and
  use the 16ms `LOAD_POLL` only while a worker is active: checking it after the normal 250ms idle
  event wait added a second polling interval, making otherwise-fast swaps take about half a second.
- `glow` receives the actual preview-body width and the configured light/dark color mode. Its
  terminal-formatted output bypasses the generic text wrapper; first-load and resize mismatches
  schedule one background reload at the new width while preserving the reader's scroll offset.
- Raster image previews are decoded in-process and rendered as true-color `▀` cells (foreground
  = upper pixel, background = lower pixel), so they work through herdr's terminal compositor
  without Kitty/Sixel passthrough. They preserve aspect ratio, center, and rebuild from the
  decoded source when the pane width or height changes. Common video extensions ask `ffmpeg`
  for a bounded first-frame PNG and render it through the same path; lookup resolves an absolute
  executable from PATH while rejecting relative/project-local entries (Windows process lookup
  otherwise searches cwd first), and extraction has a 4s timeout plus a 16 MiB output cap.
  Images stop at 32 MiB encoded / 12 megapixels / 64 MiB decoder allocation. Without `ffmpeg`,
  the pane shows a capability message rather than launching an external app. Media stays read-only.

### Syntax highlighting (file preview)

- `syntect` with `regex-fancy` (pure Rust — the default oniguruma engine needs a C build
  that's pain on Windows). syntect's BUNDLED grammar set is Sublime's defaults and lacks
  TypeScript, TOML, Dockerfile and friends — `two-face` supplies bat's extended set
  (`two_face::syntax::extra_newlines()`), themes still from syntect's `ThemeSet` (theme
  data is grammar-independent). Foreground colors only: the terminal owns the background.
  See `src/syntax.rs`; unknown extensions fall back to plain lines.

- The syntect theme is configurable per mode in `syntax-theme.json` (beside `state.json`,
  NOT in `State`, which must stay `Copy` — same reason as `editor-command.txt`; and
  `write_state` rewrites the whole file, so unknown keys added to `state.json` get erased).
  Values are a bundled name or a `.tmTheme` path; anything unresolvable falls back to
  `base16-ocean.dark` / `InspiredGitHub`. The setting is re-read per highlighted document,
  so no viewer restart is needed. User theme files are cached by mtime and LEAKED for the
  `'static` lifetime `LineHighlighter` needs. Loading `.tmTheme` needs syntect's
  `plist-load` feature. base16-ocean.dark colors the WHOLE `markup.list`/`markup.quote`
  scope (why Markdown previews looked flat orange); the bundled `assets/themes/Darcula.tmTheme`
  colors only list/quote markers. Markdown scope names in two-face's grammar: list marker
  `punctuation.definition.list_item` (numbered: `markup.list.numbered.bullet`), item text
  `meta.paragraph.list`, link text `meta.link.inline.description`.
- Markdown grammar quirks the bundled themes work around: a lone `~` ("~50 %") opens a GFM
  `markup.strikethrough` that never closes, so a gray strikethrough rule greys the REST of
  the line — the themes keep strikethrough at the plain foreground. `[LABEL]` shortcut
  references get no scope at all; `syntax::shortcut_references` finds them and restyles
  only plain-foreground regions with the theme's style for the synthetic scope
  `herdr-sidebar.shortcut-reference.markdown`. That scope must stay under its OWN top-level
  name: base16-ocean.dark's generic `meta.link` rule matched a `meta.link.*` variant and
  silently changed the default theme's output.
- `"background": true` in `syntax-theme.json` paints the viewer (`paint_preview_background`)
  with the theme's background/foreground and the gutter with `gutterForeground`. Off by
  default. The config is cached for 2s in `syntax::configured` because the viewer asks
  for it every frame.

### Experimental in-pane editor (issue #22)

- `e` enters edit mode ONLY from a regular file preview. Diff / `git show`, binary,
  invalid-UTF-8, >1 MiB, and >5000-line content stays read-only. Editor state lives in
  `src/editor.rs`; do not fold editable text back into `Doc`, whose styled lines are the
  deliberately read-only preview/diff representation.
- Editor positions are Unicode-scalar columns, never byte offsets. Visual navigation and
  scrolling use `wrapped_rows` (word-boundary first, hard-wrap only when a word cannot fit),
  so `scroll` is a WRAPPED-ROW index rather than a source-line index. UTF-8 BOM and the
  detected LF/CRLF convention round-trip on explicit Ctrl/Cmd+S saves.
- The loaded/saved raw bytes are the external-change baseline. A clean buffer reloads a
  valid external change; a dirty buffer warns and save returns a conflict until the user
  explicitly chooses overwrite or reload. Do not replace this with mtime-only detection —
  coarse timestamps and same-length rewrites can miss real changes.
- Sidebar Esc cannot directly `pane.close` a live preview anymore: pane close kills the TUI
  before it can confirm dirty state. `close_in_tab` writes a `close` control request; the
  viewer confirms save/discard/cancel and then closes itself (only label-only resumed husks
  are killed directly; a stale stamp gets the request and a notice). The same prompt guards control-file switches to another preview.
- Clipboard is best-effort and command-backed: `clip` / PowerShell `Get-Clipboard` on
  Windows, `pbcopy`/`pbpaste` on macOS, and wl-clipboard or xclip on Linux. Over SSH, copy
  emits bounded OSC 52 only when stdout is a terminal, following herdr's own clipboard path;
  Unix also falls back to OSC 52 when every native clipboard command fails in a terminal.
  Callers describe that as "sent" rather than confirmed because terminals do not acknowledge
  it. Editor cut keeps the selected text on that unacknowledged path because there is no undo.
  Ctrl and Cmd shortcuts are both accepted; CONTROL+ALT chars remain text so Windows AltGr
  layouts work.
- ANSI parsing consumes complete OSC payloads through BEL or ST. Glow 3 emits OSC 8 hyperlinks
  under forced color; dropping only ESC exposes the hyperlink metadata as visible preview text.
- Edit mode accepts terminal mouse input: click moves the caret, drag selects across logical and
  wrapped rows, and Shift+click extends the current selection. Coordinates account for the line
  number gutter, tabs, wide Unicode cells, and the editor's wrapped-row scroll offset.

### Verifying a plugin TUI end-to-end

Drive the real binary in a throwaway herdr pane instead of unit-testing rendering:
`pane split --current --direction right --no-focus --cwd <scratch repo>` (--direction is
required), then `pane run <id> "& '<abs path to exe>'"` (PS call operator — a bare path
splits on spaces), then `pane send-keys <id> Down Enter …`, capture with
`pane read <id> --source visible`, and confirm side effects with plain `git` commands in
the scratch repo. Close the pane when done. Cheap, and it catches layout truncation bugs
unit tests can't. **Build the verification binary into its OWN target dir**
(`cargo build --release --target-dir target/verify`): a different exe path can't collide
with the running sidebar's Windows exe lock, so no rename-aside dance and the user's live
panes keep running. Delete the dir afterwards. Run the TUI with `$env:HERDR_PANE_ID=''` so it skips identity-token
reporting — otherwise the test pane registers as a real sidebar and the tab's launcher/
ensure logic can fight over it.

**Mouse interactions ARE drivable** (verified live): `pane.send_input` text is fed to the
TUI's stdin, and crossterm parses SGR mouse sequences from it like any terminal input.
Send `ESC[<35;X;YM` (motion), `ESC[<0;X;YM` (left press), `ESC[<0;X;Ym` (release) with
1-based coords — put motion AND press/release in ONE send_input text: the event loop
draws between events, so the motion populates hover state/click zones before the click
lands (needed for anything hover-revealed). Two gotchas: `pane read` renders private-use
Nerd Font glyphs as blanks — launch with `HERDR_SIDEBAR_ICONS=emoji` when you need to SEE
icon positions in captures; and Claude Code's tools reject raw ESC bytes in commands, so
build sequences programmatically (`[char]27`, or JSON `\u001b` via a params file — see
`herdr_ipc.py` pattern: open `\\.\pipe\<HERDR_SOCKET_PATH>` from Python; PS 5.1's
FileStream refuses pipe paths).

## README screenshots (how-to)

The framed screenshots in `plugins/herdr-sidebar/docs/media/` are produced with the
scripts in `tools/screenshots/` (capture → crop → frame). Full reshoot procedure, verified
end-to-end twice:

**2026-09 reshoot findings (herdr 0.9) — read before reshooting:**
- **The elevated spaces/agents rail comes from config.** Set
  `[theme.custom] sidebar_bg = "#202331"` in `config.toml` (native since herdr 0.8.2;
  `panel_bg` colors herdr's tab/status chrome, `sidebar_bg` the desktop rail). It is
  CLIENT-rendered (`client/shell/render.rs`), so the client reads it at ATTACH — after a
  config edit just relaunch the WT client, NO server restart needed.
- **Window width: capture at 1848×1011** (tab area ~167×48); crop is
  `crop.ps1 <raw> <out> 8 48 1832 955`. (SUPERSEDED by 2026-09-13: the sidebar now targets
  ~39 cols via a per-tab split ratio ≈0.25, NOT the 48-col state field — see below.)
- **Claude agent panes must be in AUTO mode** for the hero: after `pane run <p> claude`, send
  `pane send-keys <p> shift+tab` until the footer reads `auto mode on` (fresh claude starts on
  "manual mode on").
- **Capture by HWND, not title.** herdr overrides the WT `--title`, so the shoot window shows
  `DESKTOP-…: acme-app` and COLLIDES with the real window's title. Grab the shoot window's HWND
  (foreground right after a fresh `wt` launch, or enumerate `CASCADIA_HOSTING_WINDOW_CLASS` and
  pick the `acme-app` one that is not the real session), then `PrintWindow` it with
  PW_RENDERFULLCONTENT (flag 2) — works even backgrounded/occluded, unlike screen-copy. WT
  reuses one process, so a fresh window's env/foreground is unreliable; dead clients revert to
  the literal `herdr-shoot` title once the server stops.
- **Server binary:** the real server runs `%LOCALAPPDATA%\Programs\Herdr\bin\herdr.exe`; `herdr`
  on PATH is the standalone `.herdr\packages\...\0.9.0` copy (same file via junction). Either
  serves the shoot session; the theme comes from config, not the binary.

**2026-09-13 reshoot findings — read these, they save an hour:**
- **WT is ONE process shared by every window, INCLUDING the terminal Claude Code runs in.**
  `SetWindowPos`/`MoveWindow` resizes, `PrintWindow` grabs, and repeated `wt -w new` launches
  block WT's UI thread and **freeze the user's own terminal** (they have to restart it, which
  kills the shoot window mid-capture → `bad rect 0x0`). Mitigate: launch the shoot client ONCE,
  resize ONCE, then capture every tab in that single session (see `batch_shots.py` below); never
  re-resize per shot; warn the user their terminal may blink/freeze for the ~2 min it runs.
- **The shoot named pipe's NAME is the full socket path.** The pipe is
  `\\.\pipe\C:\Users\Alex\AppData\Roaming\herdr\sessions\shoot\herdr.sock` — pass
  `herdr_rpc.py` the socket PATH (from `herdr session list` `socket_path`), NOT the file's
  contents (that "44684:…" string is a stale marker and fails). Python on this box has no
  `socket.AF_UNIX`; open the named pipe as a file instead (herdr_rpc.py already does).
- **Set pane widths with `layout.set_split_ratio` over RPC, NOT `pane resize`.** `pane resize
  --amount` is a ratio delta on the *nearest* split and mangles the nested 2×2 grid (it re-nests
  the sidebar into a sub-split). `layout.set_split_ratio {tab_id, path:[bool], ratio}` sets ONE
  split absolutely. **path bools: `false`=first child, `true`=second child; `[]`=root split.**
  The response echoes the whole tree — read it to learn the structure before setting ratios.
- **Preferred sidebar width is ~39 cols = root-split ratio ≈0.25** at 1848×1011 (154-col tab
  area); the old 48 cols read too wide (user-corrected). For the HERO's `[[sidebar|col1]|col2]`
  tree, a clean 2×2 with a 39-col sidebar is root `0.623`, `[false]` `0.406`, rows `0.5`.
- **A preview tab's auto-docked sidebar IGNORES `sidebar_width`** (it docks wide) — set its
  width via `layout.set_split_ratio` on that tab, not the state field. And set it **AFTER the
  final window resize**: the sidebar re-applies its persisted column target on every resize, so
  a ratio set before the resize gets clobbered back to ~48 cols. Verify by pixels, not cols — at
  1848×1011 the hero and preview sidebars match when the routes.rs selection highlight is ~399px
  wide (ratio ≈0.27 on a `[Sidebar|preview]` two-pane tab; the col count alone lied here).
- **Kill claude's "✘ Auto-update failed · Run claude doctor" banner** by relaunching the agent
  with `$env:DISABLE_AUTOUPDATER=1; $env:CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1; claude
  --model fable --permission-mode auto`. `--permission-mode auto` gives auto mode without the
  shift+tab dance. Exit the old claude first (Ctrl+C ×2, or `/exit`).
- **`report-agent --state` only accepts `idle|working|blocked|unknown`** — the old shots' blue
  "done" came from live claude at capture time and is NOT reproducible via the socket. `pane.list`
  does not echo the reported `agent_state`, but the rail renders it — verify in the capture.
- **Separated view (`SC | Explorer | diff`): give the Source Control pane MORE width (~44 cols)**
  than the others so the commit message box's right border stays aligned (user-reported). Build
  it by toggling unified off (`s` then Enter on the first Settings row → spawns Explorer+SC),
  `pane swap` SC into the left slot, set `preview_placement:"pane"` in state.json so `o` opens
  the diff INLINE in the same tab (avoids the cross-tab pane-move dance), then `pane swap` so the
  order reads SC | Explorer | diff and set the two splits' ratios.
- **`frame_all.py` writes straight into `docs/media`, which fails with OSError 22 if a PNG is
  locked** (File Explorer / an open viewer). Frame into a temp dir then `cp -f` over the media
  files (the lock only blocks the open/truncate, not the overwrite of a closed handle).
- **Global `state.json` is SHARED with the user's real session.** The shoot mutates
  `sidebar_width`, `active`, `merged`, `preview_placement`, `search_active` — ALWAYS restore them
  after (`merged=true`, `preview_placement="tab"`, `sidebar_width=48`, `active="explorer"`,
  `search_active=false`) or the user's live sidebar changes under them.
- **The hero grid's live claude/codex agents persist across the headless server** between
  sessions — reuse them (don't respawn) if their banners/composer text are still correct.
- **Deterministic capture pipeline — `tools/screenshots/shoot_capture.py`** (portable: resolves
  the socket from `herdr session list --json`, no hardcoded paths). One tab per run:
  `python shoot_capture.py <tab_id> <name> [--ratio R] [--motion] [--out DIR]`. It does exactly:
  ctypes
  `EnumWindows` → pick `CASCADIA_HOSTING_WINDOW_CLASS` whose title contains `acme-app` AND whose
  rect is >200×200 (skip 0×0 ghosts of a dying window), relaunch via `attach_shoot.ps1` if none;
  `SetWindowPos` to 1848×1011; `herdr tab focus`; optional hover motion via RPC
  `pane.send_input {text:"\u001b[<35;20;30M"}` into the tab's Sidebar pane at an EMPTY row (row
  30, not a file row — else it hover-highlights a tree row); `PrintWindow(hwnd, hdc, 2)` +
  `GetDIBits` (BGRX) → PIL; crop `(8,48,8+1832,48+955)`. One command per shot, no PowerShell
  Win32 hand-driving.
- **Number-key nav changed:** in the shoot how-to the SCM shot used to be `2`; Source Control is
  now `3` (Search is `2`). Switching a fresh sidebar to a view: send the digit key, or set
  `active` in state.json before docking.

0. **Shared backdrop (shoot session)** — shots are taken in the isolated
   `herdr --session shoot` server so herdr's left chrome shows a DUMMY roster, kept
   identical across the sidebar's screenshot set:
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
1. **Window (re-verified for the v0.5.x reshoot)**: the shoot window uses a dedicated
   WT profile `herdr-shoot` (fontSize 11; added to WT settings.json — backup saved as
   settings.json.herdr-shoot.bak) launched with
   `wt -w new nt -p herdr-shoot --title herdr-shoot` (attach_shoot.ps1 runs inside).
   Size the window until the TAB AREA is **138×48 cells** (1526×1011 px at font 11 —
   verify with `pane layout`, don't trust pixels); crop is
   `crop.ps1 <raw> <out> 8 48 1510 955`. Grid ratios: sidebar 38 cols
   (root ratio 0.2754), agent quadrants 50 cols each (TL/TR split 0.5) — an EQUAL 2×2.
   **Claude's banner includes the user's EMAIL at ≥73 cols** (re-measured v2.1.216;
   the old ≤63/74+ note was stale) — keep agent panes ≤72; 50 is the floor where the
   codex banner still fits unwrapped-ish. Old 1760×996 numbers are obsolete.
2. **Demo repo**: `setup_demo.sh` rebuilds `C:/Users/Alex/Projects/acme-app` (staged
   docs/auth.md, modified routes.rs, dirty `acme-sdk` child repo, 1 commit ahead of a bare
   `.acme-origin.git`) — multi-repo + sync + diff all have something to show.
3. **Stage**: new tab in this workspace with `--cwd` = acme-app, `herdr tab focus` it,
   invoke `herdr-sidebar.open-sidebar-windows`, close the tab's shell pane.
4. **Shots** (drive via `pane send-keys`, capture via `tools/screenshots/shot.py
   <sidebar_pane_id[,pane2]> <name>` — it pumps SGR mouse motion into the listed panes
   during capture so the hover title-bar buttons stay visible (their 3s linger is
   shorter than the capture powershell's startup), captures via **capture_exact.ps1**
   and crops; `--no-motion` for modal shots. ALWAYS `tab focus` the target tab first —
   staging the other tab leaves focus there and you capture the wrong tab):
   *preview* — explorer view, expand src/api (`Down Down Enter`, `Down Enter`), select
   routes.rs, Enter opens the preview pane. *scm* — `3` (Source Control; `2` is now Search),
   Down×4 to routes.rs, `o` opens
   the diff. *separated* — `s`, Enter toggles unified off (capture, then toggle back).
   *hero* — explorer view, Esc closes the preview, split a 2×2 agent grid to the right
   (0.25 sidebar split, then 0.5, then two down-splits), `claude` + `codex --model gpt-5.5`
   workers with prompts, fresh `claude` and `codex` for the spawn banners. *settings* —
   `s` over the hero layout.
5. **Frame**: `python tools/screenshots/frame_all.py <dir with crop-*.png>` writes the
   framed set straight into docs/media (gradient backdrop + macOS-style titlebar).
6. **Teardown**: close the disposable capture tab, stop only test processes still using
   acme-app, verify the scratch paths before deleting acme-app + .acme-origin.git,
   and restore the window size.

Hard-won capture gotchas:

- `capture.ps1`/`capture_titled.ps1` are **screen-space** copies: whatever overlays that
  region wins the pixels (a fullscreen game ate a whole round of captures), and
  capture_titled's SUBSTRING title match once grabbed the USER'S OWN terminal — Claude
  Code's auto-set terminal title happened to contain "herdr-shoot". Use
  **capture_exact.ps1** (exact title + PrintWindow PW_RENDERFULLCONTENT): immune to
  occlusion, monitors, and title collisions. Still **view every capture** before shipping.
- **NO_COLOR kills Claude Code's orange** (the `AGENTS.md` crossterm gotcha, shoot-server
  edition): agent tool shells carry NO_COLOR=1 (even when a nested probe shell says
  otherwise — check `$env:NO_COLOR` in the ACTUAL shell), every server started from one
  passes it to every pane, and claude renders monochrome. Start the shoot server with
  the var explicitly removed and verify a pane echoes `NO_COLOR=[]` before staging.
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` on claude spawns also suppresses the
  release-promo box that otherwise pushes the welcome banner out of short panes.
- Claude re-renders its banner on RESIZE: a narrow-launch trick does not survive a
  later widen — the email comes back. The pane width at capture time is what counts.
- The sidebar's `state.json` is **GLOBAL** (%LOCALAPPDATA%\herdr\plugins\herdr-sidebar
  — shared with the user's real session!). The separated shot toggles unified off and
  needs the diff BESIDE the sidebar rather than in its own tab — with preview tabs
  (the trial branch) the diff lives in a separate tab, so compose that shot by
  moving the preview pane back into the staging tab (`pane.move`) before capturing.
  RESTORE merged:true when done. (`preview_full` no longer exists.)
- Separated shot composition: after toggling, pin Source Control to 38 cols
  (root ratio 0.275), open the routes.rs diff in a preview tab, then `pane swap` the
  viewer to the far right so it reads SC | Explorer | diff.
- The spaces list order = workspace CREATION order; recreating a workspace mid-shoot
  sends it to the bottom — recreate ALL of them in roster order.
- Fake agent rows (pane.report_agent) do NOT survive a server restart — re-report them.
- codex shows an update notice on spawn; `npm install -g @openai/codex` then
  `cls; codex` relaunch gives a clean banner (update prompts eat the next keystroke
  as composer text when the skip choice was already saved).
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

## macOS (verified live against herdr 0.7.4 on macOS 26)

First clean install of both plugins on a Mac (driven over SSH), findings:

- Install: `curl -fsSL https://herdr.dev/install.sh | sh` (lands in `~/.local/bin`), then
  `herdr plugin install <owner>/<repo>[/subdir] --yes` — the `--yes` must come AFTER the
  target (before it, the arg parser rejects the whole command), and it is REQUIRED when
  stdin is not a TTY ("remote plugin install requires --yes when stdin is not interactive").
- Plugin state dir on unix resolves to `XDG_STATE_HOME` else
  `~/.local/state/herdr/plugins/<plugin-id>/` (our `state.rs` fallback; herdr injects
  `HERDR_PLUGIN_STATE_DIR` for hooks/actions only, same as Windows).
- **Headless/SSH herdr**: `herdr` needs a TTY. A pipeline-attached `ssh -tt` client gets a
  degenerate window (panes ~2 rows) and every `pane split` fails with
  `pane_split_failed: ghostty error -2` — the split result is smaller than a pane minimum.
  Fix: run the client under a fake sized TTY:
  `nohup script -q /dev/null /bin/zsh -c 'stty rows 54 cols 220; exec herdr' &`.
  The server survives client death, restoring the session on next attach — but
  `pkill -f 'herdr$'` matches the SERVER too; workspace ids change across that restart.
- **Unix launcher vs ensure-hook race**: the former shell launchers could race focus-burst hooks
  and dock two sidebars. Unix now runs the same native `ensure.rs` implementation as Windows;
  its OS-backed file lock serializes toggles with hooks without retry sleeps or stale-lock cleanup.
- **Stamp identity before releasing the launch lock**: left-docking calls `pane swap` and restores
  focus, and both operations re-emit focus events. A fresh label-only pane is indistinguishable
  from a server-restored corpse and caused the issue #29 unbounded close/spawn loop. Direct Unix
  plugin panes start the TUI atomically and the launcher reports a heartbeat before layout/focus;
  the Windows raw-split path stamps starting metadata before typing the command. No hook needs to
  wait for process startup.
- The `merged` (unified sidebar) default was still `false` from the experiment era — fresh
  installs came up as a pinned separate Explorer. Flipped to `true` (existing users keep
  their persisted value).
- **Do not use rfd for the background picker on macOS.** Its Cocoa backend requires the
  main thread or a running `NSApplication`; a terminal TUI has neither, so the worker-thread
  call aborts the pane. Moving it to the main thread would freeze the heartbeat instead.
  `actions::pick_folder` uses `osascript -e 'POSIX path of (choose folder …)'`: omit
  `default location` when the old root is no longer a directory, escape backslashes before
  quotes in AppleScript literals, treat a non-zero status (including cancel `-128`) as no
  selection, and trim the returned trailing slash without turning `/` into an empty path.
  The parser is separated from process launch so all of these cases run in cross-platform
  unit tests. `rfd` remains Windows-only; Linux behavior is unchanged.
- Everything else verified working on macOS unchanged: ensure hook docks on tab focus,
  unified view switch, SCM drawers/commit box in a real repo, the diff preview
  (full-size park/restore at the time; preview TABS on the trial branch),
  first-run Nerd Font prompt (curl+unzip path), heartbeat tokens,
  herdr-notes toggle.

## Herdr workspace

`herdr-layout.yaml` at the repo root describes the workspace (Coordinator tab running claude,
shell tab, git tab with lazygit). The Coordinator session delegates feature work to sibling
panes. Create feature worktrees only when explicitly requested; the old repository-local
feature-worktree skill has been retired.
