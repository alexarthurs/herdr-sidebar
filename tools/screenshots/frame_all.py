"""Frame cropped captures into docs/media.

Usage: python frame_all.py <dir with crop-*.png>
Reads crop-{hero,preview,scm,separated,settings}.png from that directory and
writes the framed set to plugins/herdr-aa-sidebar/docs/media/.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from frame_pil import frame

CROPS = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
MEDIA = os.path.join(REPO, "plugins", "herdr-aa-sidebar", "docs", "media")

JOBS = [
    ("crop-hero.png", "hero.png", "herdr — sidebar + agents"),
    ("crop-preview.png", "preview.png", "herdr sidebar — explorer + file preview"),
    ("crop-scm.png", "source-control.png", "herdr sidebar — source control + diff"),
    ("crop-separated.png", "separated.png", "herdr sidebar — separated panels"),
    ("crop-settings.png", "settings.png", "herdr sidebar — settings"),
]

for src, out, title in JOBS:
    path = os.path.join(CROPS, src)
    if os.path.exists(path):
        frame(path, os.path.join(MEDIA, out), title)
    else:
        print("skip (missing):", src)
print("done")
