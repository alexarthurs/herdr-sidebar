"""Frame a screenshot like the fancy screenshot services: gradient backdrop,
rounded window with a macOS-style titlebar, soft drop shadow."""
import os
import sys
from PIL import Image, ImageDraw, ImageFilter, ImageFont

PAD = 64
BAR = 44
RADIUS = 14


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def gradient(w, h):
    """Diagonal indigo-violet gradient with a soft glow top-left."""
    top = (16, 18, 34)
    mid = (26, 30, 56)
    bot = (40, 30, 72)
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(0, w, 4):
            t = (x / w + y / h) / 2
            c = lerp(top, mid, t * 2) if t < 0.5 else lerp(mid, bot, (t - 0.5) * 2)
            for dx in range(min(4, w - x)):
                px[x + dx, y] = c
    glow = Image.new("L", (w, h), 0)
    gd = ImageDraw.Draw(glow)
    gd.ellipse([-w * 0.3, -h * 0.5, w * 0.55, h * 0.45], fill=70)
    glow = glow.filter(ImageFilter.GaussianBlur(120))
    tint = Image.new("RGB", (w, h), (99, 102, 241))
    img = Image.composite(Image.blend(img, tint, 0.35), img, glow)
    glow2 = Image.new("L", (w, h), 0)
    gd = ImageDraw.Draw(glow2)
    gd.ellipse([w * 0.55, h * 0.6, w * 1.25, h * 1.4], fill=60)
    glow2 = glow2.filter(ImageFilter.GaussianBlur(120))
    tint2 = Image.new("RGB", (w, h), (56, 189, 248))
    img = Image.composite(Image.blend(img, tint2, 0.3), img, glow2)
    return img


def frame(shot_path, out_path, title):
    shot = Image.open(shot_path).convert("RGB")
    sw, sh = shot.size
    ww, wh = sw + 2, sh + BAR + 2  # 1px border
    cw, ch = ww + PAD * 2, wh + PAD * 2

    canvas = gradient(cw, ch).convert("RGBA")

    # Soft shadow under the window.
    shadow = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    sd = ImageDraw.Draw(shadow)
    sd.rounded_rectangle(
        [PAD - 6, PAD + 14, PAD + ww + 6, PAD + wh + 26], RADIUS + 6, fill=(0, 0, 0, 190)
    )
    shadow = shadow.filter(ImageFilter.GaussianBlur(26))
    canvas = Image.alpha_composite(canvas, shadow)

    # Window: border + titlebar + screenshot, clipped to rounded corners.
    win = Image.new("RGBA", (ww, wh), (24, 25, 32, 255))
    wd = ImageDraw.Draw(win)
    wd.rectangle([0, 0, ww - 1, BAR], fill=(30, 31, 40, 255))
    wd.line([0, BAR, ww, BAR], fill=(50, 52, 64, 255))
    for i, color in enumerate([(255, 95, 87), (254, 188, 46), (40, 200, 64)]):
        cx = 20 + i * 22
        wd.ellipse([cx - 6, BAR // 2 - 6, cx + 6, BAR // 2 + 6], fill=color)
    try:
        font = ImageFont.truetype("segoeui.ttf", 15)
    except OSError:
        font = ImageFont.load_default()
    tw = wd.textlength(title, font=font)
    wd.text(((ww - tw) / 2, (BAR - 18) / 2), title, fill=(150, 155, 170), font=font)
    win.paste(shot, (1, BAR + 1))
    wd.rounded_rectangle([0, 0, ww - 1, wh - 1], RADIUS, outline=(255, 255, 255, 26), width=1)

    mask = Image.new("L", (ww, wh), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, ww - 1, wh - 1], RADIUS, fill=255)
    canvas.paste(win, (PAD, PAD), mask)
    canvas.convert("RGB").save(out_path, "PNG")
    print("framed", out_path, f"{cw}x{ch}")


if __name__ == "__main__":
    sp = os.path.dirname(os.path.abspath(__file__))
    media = sys.argv[1]
    jobs = [
        ("crop-hero.png", "hero.png", "herdr sidebar — explorer + preview"),
        ("crop-scm.png", "source-control.png", "herdr sidebar — source control"),
        ("crop-separated.png", "separated.png", "herdr sidebar — separated panels"),
        ("crop-settings.png", "settings.png", "settings"),
    ]
    for src, out, title in jobs:
        frame(os.path.join(sp, src), os.path.join(media, out), title)
