## What

<!-- One or two sentences: what does this change do, user-visibly? -->

## Why

<!-- Motivation / linked issue. -->

## Checklist

- [ ] `cargo build --release` succeeds (run from `plugins/herdr-aa-sidebar`)
- [ ] `cargo test` passes (add/adjust tests for behavior changes)
- [ ] `cargo clippy --release -- -D warnings` is clean
- [ ] Exercised in a live herdr pane if the change touches the TUI, launcher
      scripts, or persistence (see CLAUDE.md "Plugin dev workflow")
- [ ] README / CLAUDE.md updated if keys, persistence, or the manifest changed
