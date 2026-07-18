#!/usr/bin/env bash
# redeploy.sh -- refresh every workspace onto the latest plugin builds.
# See redeploy.ps1 for the full story; this is the unix twin.
set -u

HERDR_BIN="${HERDR_BIN_PATH:-herdr}"

"$HERDR_BIN" workspace list | python3 -c '
import json, subprocess, sys

herdr = sys.argv[1]
labels = {"Explorer", "Source Control", "Sidebar", "Preview"}
workspaces = json.load(sys.stdin)["result"]["workspaces"]
for ws in workspaces:
    wid = ws["workspace_id"]
    out = subprocess.check_output([herdr, "pane", "list", "--workspace", wid])
    for pane in json.loads(out)["result"]["panes"]:
        tokens = pane.get("tokens") or {}
        if pane.get("label") in labels or any(k.startswith("herdr-aa") for k in tokens):
            subprocess.run([herdr, "pane", "close", pane["pane_id"]], capture_output=True)
            print("closed", wid, pane["pane_id"], pane.get("label"))
' "$HERDR_BIN"

pkill -f 'herdr-aa-(filetree|git)' 2>/dev/null
"$HERDR_BIN" plugin action invoke herdr-aa-filetree.open-explorer >/dev/null 2>&1
echo 'redeploy complete - other workspaces re-dock on next focus'
