#!/usr/bin/env bash
# open-explorer.sh — unix launcher for the herdr-aa-sidebar explorer pane.
#
# Idempotent "launch-or-focus, toggle on repeat", scoped to the current tab:
#   - no Explorer pane in the current tab       -> open one, DOCKED ON THE LEFT edge
#   - an Explorer pane exists but isn't focused -> focus it
#   - the focused pane IS the Explorer pane     -> close it (toggle off)
#
# Left dock: herdr's `pane split` only splits right/down, so we split the tab's
# LEFTMOST pane (the one touching the spaces/agents sidebar) to the right with a
# small left-slot ratio, then `pane swap` the new pane into that left slot. The
# split `--ratio` is the ORIGINAL pane's share; after a swap, focus stays with
# the SLOT, not the pane (both verified against herdr 0.7.1).
#
# All ids/ratios come from the binary's unit-tested stdin modes
# (--launch-decision / --focused-pane / --open-plan), never ad-hoc JSON parsing;
# the ids it emits are validated flag-safe before reaching an argv.
set -uo pipefail

herdr_bin="${HERDR_BIN_PATH:-herdr}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
bin="$script_dir/../target/release/herdr-aa-sidebar"

# Without the binary there is no decision logic; fall back to herdr's declarative
# pane open (right split, not left-docked — degraded but functional).
if [ ! -x "$bin" ]; then
  exec "$herdr_bin" plugin pane open \
    --plugin herdr-aa-sidebar \
    --entrypoint filetree \
    --placement split \
    --direction right \
    --focus
fi

panes="$("$herdr_bin" pane list 2>/dev/null || true)"

open_pane() {
  local fp fid fcwd plan target ratio out np
  fp="$(printf '%s' "$panes" | "$bin" --focused-pane 2>/dev/null || true)"
  fid="${fp%%	*}"
  fcwd="${fp#*	}"
  if [ -z "$fid" ]; then
    exec "$herdr_bin" plugin pane open --plugin herdr-aa-sidebar \
      --entrypoint filetree --placement split --direction right --focus
  fi

  target="$fid"
  ratio="0.25"
  plan="$("$herdr_bin" pane layout --pane "$fid" 2>/dev/null | "$bin" --open-plan 2>/dev/null || true)"
  if [ -n "$plan" ]; then
    target="${plan%%	*}"
    ratio="${plan#*	}"
  fi

  out="$("$herdr_bin" pane split "$target" --direction right --ratio "$ratio" \
    ${fcwd:+--cwd "$fcwd"} --no-focus 2>/dev/null || true)"
  np="$(printf '%s' "$out" | sed -n 's/.*"pane_id":"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$np" ] || exit 1

  # Move the new pane into the left slot, then start the explorer in it.
  "$herdr_bin" pane swap --source-pane "$np" --target-pane "$target" >/dev/null 2>&1 || true
  "$herdr_bin" pane run "$np" "exec \"$bin\""
  "$herdr_bin" pane rename "$np" Explorer >/dev/null 2>&1 || true
  # herdr has no focus-by-id; a zoom on/off cycle focuses deterministically.
  "$herdr_bin" pane zoom "$np" --on >/dev/null 2>&1 || true
  exec "$herdr_bin" pane zoom "$np" --off
}

decision="OPEN"
if [ -n "$panes" ]; then
  decision="$(printf '%s' "$panes" | "$bin" --launch-decision 2>/dev/null || echo OPEN)"
fi

# Snooze markers keep the auto-ensure hook from reopening a sidebar the user
# toggled closed; toggling open clears the tab's marker again.
snooze_dir="${TMPDIR:-/tmp}/herdr-aa-sidebar-snooze"
tab="$(printf '%s' "$panes" | "$bin" --focused-tab 2>/dev/null || true)"

case "$decision" in
  "FOCUS "*)
    pid="${decision#FOCUS }"
    "$herdr_bin" pane zoom "$pid" --on >/dev/null 2>&1 || true
    exec "$herdr_bin" pane zoom "$pid" --off
    ;;
  "CLOSE "*)
    pid="${decision#CLOSE }"
    if [ -n "$tab" ]; then
      mkdir -p "$snooze_dir" 2>/dev/null
      : > "$snooze_dir/${tab//:/_}"
    fi
    exec "$herdr_bin" pane close "$pid"
    ;;
  *)
    [ -n "$tab" ] && rm -f "$snooze_dir/${tab//:/_}" 2>/dev/null
    open_pane
    ;;
esac
