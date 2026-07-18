"""Blur rectangles in an image: blur_region.py <img> <x> <y> <w> <h> [...more rects]"""
import sys
from PIL import Image, ImageFilter

path = sys.argv[1]
img = Image.open(path)
args = [int(a) for a in sys.argv[2:]]
for i in range(0, len(args), 4):
    x, y, w, h = args[i:i + 4]
    region = img.crop((x, y, x + w, y + h))
    region = region.filter(ImageFilter.GaussianBlur(9))
    img.paste(region, (x, y))
img.save(path, "PNG")
print("blurred", path, len(args) // 4, "region(s)")
