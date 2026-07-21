"""shot.py <pane_id[,pane_id]> <name> [--no-motion] — shoot-session capture.

Run from the directory where crops should land (cwd).

Capture the herdr-shoot WT window into raw-new-<name>.png and
crop-new-<name>.png (standard 8 48 1744 940 crop) in this directory.
While the capture powershell boots (~3s of Add-Type JIT — longer than the
3s title-button linger), keep injecting SGR mouse motion into the sidebar
pane at an EMPTY list row so the hover title-action buttons stay visible
without adding a row-hover highlight."""
import json
import os
import subprocess
import sys
import threading
import time

TOOLS = r"C:\Users\Alex\Projects\herdr\tools\screenshots"
SOCK = r"C:\Users\Alex\AppData\Roaming\herdr\sessions\shoot\herdr.sock"
HERE = os.getcwd()

panes, name = sys.argv[1].split(","), sys.argv[2]
motion = "--no-motion" not in sys.argv
raw = os.path.join(HERE, f"raw-{name}.png")
crop = os.path.join(HERE, f"crop-{name}.png")

stop = threading.Event()

def pump_motion():
    while not stop.is_set():
        for p in panes:
            subprocess.run(
                ["python", os.path.join(TOOLS, "herdr_rpc.py"), SOCK, "pane.send_input"],
                input=json.dumps({"pane_id": p, "text": "\x1b[<35;8;40M"}),
                capture_output=True, text=True)
        time.sleep(1.0)

if motion:
    t = threading.Thread(target=pump_motion, daemon=True)
    t.start()

try:
    subprocess.run(
        ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass",
         "-File", os.path.join(TOOLS, "capture_exact.ps1"), "herdr-shoot", raw],
        check=True, capture_output=True, text=True)
finally:
    stop.set()

subprocess.run(
    ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass",
     "-File", os.path.join(TOOLS, "crop.ps1"), raw, crop, "8", "48", "1510", "955"],
    check=True, capture_output=True, text=True)
print("captured", crop)
