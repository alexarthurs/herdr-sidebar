# herdr plugins

Monorepo of [herdr](https://github.com/ogulcancelik/herdr) plugins — VS Code-style panels, rebuilt for the terminal.

| Plugin | What it is |
| --- | --- |
| [`plugins/herdr-aa-filetree`](plugins/herdr-aa-filetree) | File explorer: a tree view of the workspace repo in a herdr split pane, closely aligned with VS Code's Explorer. |
| [`plugins/herdr-aa-git`](plugins/herdr-aa-git) | Source control panel: staged/unstaged changes, diffs, commit flow, closely aligned with VS Code's Source Control view. |

Each plugin is a **self-contained Rust crate** (own `Cargo.toml`, own `target/`) so it can be
installed straight from its subdirectory:

```
herdr plugin install <owner>/herdr/plugins/herdr-aa-filetree
herdr plugin install <owner>/herdr/plugins/herdr-aa-git
```

## Local development

```
cd plugins/herdr-aa-filetree
cargo build --release
herdr plugin link .
```

`herdr plugin action list` shows the plugin's actions; `herdr plugin log list --plugin <id>`
shows its logs. See `CLAUDE.md` for the full dev workflow and Windows caveats.
