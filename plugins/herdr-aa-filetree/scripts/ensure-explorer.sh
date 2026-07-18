#!/usr/bin/env bash
# ensure-explorer.sh — unix [[events]] hook body: make sure the FOCUSED tab has
# an Explorer pane docked on the left, WITHOUT stealing the user's focus.
#
# Runs on tab.focused / workspace.focused, so it must be idempotent and quiet:
# already present → exit; else open unfocused (see ensure-explorer.ps1 for the
# focus-follows-the-slot rationale behind the final `pane focus`).
set -uo pipefail

herdr_bin="${HERDR_BIN_PATH:-herdr}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
bin="$script_dir/../target/release/herdr-aa-filetree"
[ -x "$bin" ] || exit 0

# Focus events arrive in bursts (tab.focused + workspace.focused for one switch)
# and concurrent ensures each open an explorer — serialize with an atomic mkdir
# lock. Losing the race skips this ensure; the next focus event re-fires it.
lock_dir="${TMPDIR:-/tmp}/herdr-aa-filetree-ensure.lock"
if ! mkdir "$lock_dir" 2>/dev/null; then
  # Break locks older than 30s (a crashed ensure), otherwise yield.
  now="$(date +%s)"
  born="$(stat -c %Y "$lock_dir" 2>/dev/null || stat -f %m "$lock_dir" 2>/dev/null || echo "$now")"
  [ $((now - born)) -gt 30 ] || exit 0
  rm -rf "$lock_dir" 2>/dev/null
  mkdir "$lock_dir" 2>/dev/null || exit 0
fi
trap 'rmdir "$lock_dir" 2>/dev/null' EXIT

# Snapshot AFTER acquiring the lock, so a just-finished ensure's rename is visible.
panes="$("$herdr_bin" pane list 2>/dev/null || true)"
[ -n "$panes" ] || exit 0

decision="$(printf '%s' "$panes" | "$bin" --launch-decision 2>/dev/null || true)"
[ "$decision" = "OPEN" ] || exit 0

# Respect a tab the user toggled closed (open-explorer.sh writes the marker) —
# otherwise the very next focus event would reopen what they just closed.
snooze_dir="${TMPDIR:-/tmp}/herdr-aa-filetree-snooze"
tab="$(printf '%s' "$panes" | "$bin" --focused-tab 2>/dev/null || true)"
[ -n "$tab" ] && [ -f "$snooze_dir/${tab//:/_}" ] && exit 0

fp="$(printf '%s' "$panes" | "$bin" --focused-pane 2>/dev/null || true)"
fid="${fp%%	*}"
fcwd="${fp#*	}"
[ -n "$fid" ] || exit 0

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
[ -n "$np" ] || exit 0

"$herdr_bin" pane swap --source-pane "$np" --target-pane "$target" >/dev/null 2>&1 || true
"$herdr_bin" pane run "$np" "exec \"$bin\""
"$herdr_bin" pane rename "$np" Explorer >/dev/null 2>&1 || true

# Hand focus back if the swap left it on the explorer (focus follows the slot).
if [ "$target" = "$fid" ]; then
  "$herdr_bin" pane focus --direction right --pane "$np" >/dev/null 2>&1 || true
fi
exit 0
