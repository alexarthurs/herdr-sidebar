r"""Deterministic single-tab capture for the README shoot (Windows).

Captures one tab of an isolated `herdr --session <name>` shoot server: finds
(or relaunches) the shoot's Windows Terminal window, sizes it, focuses the
tab, optionally pins the sidebar split ratio and pumps hover motion, then
grabs the window with PrintWindow and crops the terminal content out.

Why this exists: WT is one shared process, so hand-driving Win32 resizes and
PrintWindow per shot freezes the operator's own terminal and the window
churns; doing the whole grab in one deterministic pass keeps it to a single
window op. See AGENTS.md "README screenshots" for the full playbook.

Usage:
  python shoot_capture.py <tab_id> <crop_name> [--session shoot]
      [--out DIR] [--ratio R] [--motion] [--match acme-app]
      [--size 1848x1011]

  <tab_id>     herdr tab id, e.g. w6:tE  (from `herdr tab list --workspace <id>`)
  <crop_name>  base name; writes <out>/crop-<crop_name>.png (+ raw-<name>.png)
  --ratio R    set the tab root split's ratio (0..1) AFTER the resize, so the
               sidebar's on-resize width re-apply can't clobber it. Verify by
               pixels: at 1848x1011 the hero/preview sidebars match at ~0.27.
  --motion     pump SGR hover motion into the tab's Sidebar pane (empty row)
               so the title-action buttons render for the shot.

Notes:
- The shoot named pipe's NAME is the full socket_path from `herdr session
  list --json`; RPC opens \\.\pipe\<socket_path>.
- Requires Pillow. Windows only (uses user32/gdi32 + WT window class).
"""
import argparse
import ctypes
import ctypes.wintypes as wt
import json
import os
import subprocess
import sys
import tempfile
import time
import uuid

WT_CLASS = "CASCADIA_HOSTING_WINDOW_CLASS"
u32 = ctypes.windll.user32
gdi = ctypes.windll.gdi32


def session_socket(name):
    """Resolve the shoot session's socket_path (== the named-pipe name)."""
    out = subprocess.run(["herdr", "session", "list", "--json"],
                         capture_output=True, text=True).stdout
    for s in json.loads(out).get("sessions", []):
        if s.get("name") == name:
            if not s.get("running"):
                raise SystemExit(f"session {name!r} is not running")
            return s["socket_path"]
    raise SystemExit(f"session {name!r} not found")


def rpc(sock, method, params):
    req = {"id": "s-" + uuid.uuid4().hex[:8], "method": method, "params": params}
    with open(r"\\.\pipe\\" + sock, "r+b", buffering=0) as f:
        f.write((json.dumps(req) + "\n").encode())
        out = b""
        while not out.endswith(b"\n"):
            c = f.read(65536)
            if not c:
                break
            out += c
    return json.loads(out.decode())


def herdr(sock, *args):
    env = dict(os.environ, HERDR_SOCKET_PATH=sock)
    return subprocess.run(["herdr", *args], capture_output=True, text=True, env=env)


class RECT(ctypes.Structure):
    _fields_ = [("l", ctypes.c_long), ("t", ctypes.c_long),
                ("r", ctypes.c_long), ("b", ctypes.c_long)]


def find_window(match):
    """hwnd of the visible WT window whose title contains `match`, skipping
    zero-area ghosts of a dying window."""
    res = {"h": 0}
    Proc = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

    def cb(h, _l):
        cn = ctypes.create_unicode_buffer(256)
        u32.GetClassNameW(h, cn, 256)
        if cn.value == WT_CLASS and u32.IsWindowVisible(h):
            t = ctypes.create_unicode_buffer(256)
            u32.GetWindowTextW(h, t, 256)
            if match in t.value:
                r = RECT()
                u32.GetWindowRect(h, ctypes.byref(r))
                if (r.r - r.l) > 200 and (r.b - r.t) > 200:
                    res["h"] = h
        return True

    u32.EnumWindows(Proc(cb), 0)
    return res["h"]


def ensure_window(match, session, tools_dir):
    h = find_window(match)
    if h:
        return h
    attach = os.path.join(tools_dir, "attach_shoot.ps1")
    subprocess.Popen(["wt.exe", "-w", "new", "nt", "--title", f"herdr-{session}",
                      "powershell", "-NoProfile", "-ExecutionPolicy", "Bypass",
                      "-File", attach, session])
    for _ in range(20):
        time.sleep(1)
        h = find_window(match)
        if h:
            return h
    return 0


def sidebar_pane(sock, tab):
    j = json.loads(herdr(sock, "pane", "list").stdout)
    for p in j["result"]["panes"]:
        if p["tab_id"] == tab and p.get("label") == "Sidebar":
            return p["pane_id"]
    return None


def capture(h, path, crop):
    from PIL import Image
    r = RECT()
    u32.GetWindowRect(h, ctypes.byref(r))
    w, ht = r.r - r.l, r.b - r.t
    if w <= 0 or ht <= 0:
        raise RuntimeError(f"bad window rect {w}x{ht} (window dying?)")
    hdc = u32.GetWindowDC(h)
    mdc = gdi.CreateCompatibleDC(hdc)
    bmp = gdi.CreateCompatibleBitmap(hdc, w, ht)
    gdi.SelectObject(mdc, bmp)
    ok = u32.PrintWindow(h, mdc, 2)  # PW_RENDERFULLCONTENT

    class BMI(ctypes.Structure):
        _fields_ = [("s", ctypes.c_uint32), ("w", ctypes.c_long), ("h", ctypes.c_long),
                    ("pl", ctypes.c_uint16), ("bc", ctypes.c_uint16), ("cmp", ctypes.c_uint32),
                    ("si", ctypes.c_uint32), ("x", ctypes.c_long), ("y", ctypes.c_long),
                    ("cu", ctypes.c_uint32), ("ci", ctypes.c_uint32), ("j", ctypes.c_uint32 * 3)]
    bmi = BMI()
    bmi.s = 40; bmi.w = w; bmi.h = -ht; bmi.pl = 1; bmi.bc = 32; bmi.cmp = 0
    buf = ctypes.create_string_buffer(w * ht * 4)
    gdi.GetDIBits(mdc, bmp, 0, ht, buf, ctypes.byref(bmi), 0)
    img = Image.frombuffer("RGB", (w, ht), buf, "raw", "BGRX", 0, 1)
    gdi.DeleteObject(bmp); gdi.DeleteDC(mdc); u32.ReleaseDC(h, hdc)
    raw = os.path.join(os.path.dirname(path), "raw-" + os.path.basename(path)[len("crop-"):])
    img.save(raw)
    # crop the WT chrome (title bar + borders) off the terminal content
    left, top, marg = crop
    img.crop((left, top, w - left, ht - marg)).save(path)
    return w, ht, bool(ok)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("tab")
    ap.add_argument("name")
    ap.add_argument("--session", default="shoot")
    ap.add_argument("--out", default=os.path.join(tempfile.gettempdir(), "herdr-shots"))
    ap.add_argument("--ratio", type=float, default=None)
    ap.add_argument("--motion", action="store_true")
    ap.add_argument("--match", default="acme-app")
    ap.add_argument("--size", default="1848x1011")
    args = ap.parse_args()

    tools_dir = os.path.dirname(os.path.abspath(__file__))
    os.makedirs(args.out, exist_ok=True)
    sock = session_socket(args.session)
    W, H = (int(v) for v in args.size.lower().split("x"))

    h = ensure_window(args.match, args.session, tools_dir)
    if not h:
        print("NO WINDOW"); sys.exit(1)
    u32.ShowWindow(h, 9)                                   # restore
    u32.SetWindowPos(h, 0, 100, 60, W, H, 0x14)           # NOZORDER|NOACTIVATE
    time.sleep(1.5)
    herdr(sock, "tab", "focus", args.tab)
    time.sleep(1.2)

    if args.ratio is not None:
        # AFTER the resize, else the sidebar's on-resize width re-apply wins.
        try:
            rpc(sock, "layout.set_split_ratio",
                {"tab_id": args.tab, "path": [], "ratio": args.ratio})
        except Exception as e:
            print("ratio warn:", e)
        time.sleep(0.5)

    if args.motion:
        sb = sidebar_pane(sock, args.tab)
        if sb:
            try:  # SGR motion at an empty row so no tree row hover-highlights
                rpc(sock, "pane.send_input", {"pane_id": sb, "text": "\u001b[<35;20;30M"})
            except Exception as e:
                print("motion warn:", e)
            time.sleep(0.3)

    # crop: 8px side margins, 48px top (WT tab bar), 8px bottom
    cp = os.path.join(args.out, f"crop-{args.name}.png")
    w, ht, ok = capture(h, cp, crop=(8, 48, 8))
    print(f"OK hwnd={h} {w}x{ht} printwindow={ok} -> {cp}")


if __name__ == "__main__":
    main()
