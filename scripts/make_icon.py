#!/usr/bin/env python3
"""Make assets/app.ico from the artwork, assets/app.png. Needs Pillow; Dogmind's own Python has it:

    ..\\Dogmind\\.venv\\Scripts\\python.exe scripts\\make_icon.py

The artwork is opaque, with near-white outside its rounded card: that outside, reached from the corners, becomes transparent.
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def with_transparent_corners(art):
    art = art.convert("RGB")
    outside = art.copy()
    marker = (255, 0, 255)
    width, height = art.size
    for corner in [(0, 0), (width - 1, 0), (0, height - 1), (width - 1, height - 1)]:
        ImageDraw.floodfill(outside, corner, marker, thresh=24)
    mask = Image.new("L", art.size, 255)
    mask.putdata([0 if pixel == marker else 255 for pixel in outside.getdata()])
    # A soft edge, drawn a little inside what was filled: the fill stops at the card's light rim.
    mask = mask.filter(ImageFilter.MinFilter(3)).filter(ImageFilter.GaussianBlur(1.5))
    out = art.convert("RGBA")
    out.putalpha(mask)
    return out


def main():
    art = with_transparent_corners(Image.open(ROOT / "assets" / "app.png"))
    art.save(ROOT / "assets" / "app.ico", sizes=[(size, size) for size in SIZES])
    print("assets/app.ico:", (ROOT / "assets" / "app.ico").stat().st_size, "bytes")


if __name__ == "__main__":
    main()
