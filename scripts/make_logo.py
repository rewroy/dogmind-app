#!/usr/bin/env python3
"""Make assets/logo.a8 and assets/title.a8 from the drawings, assets/logo.png and assets/title.svg. Needs Pillow; Dogmind's own Python has it:

    ..\\Dogmind\\.venv\\Scripts\\python.exe scripts\\make_logo.py

What the app needs of a drawing is how much ink each pixel holds, so that it can draw it in the theme's text colour: white on the dark
background and black on the light one. Two little-endian u16s, width and height, then a byte a pixel.

The title is one filled SVG path with the even-odd rule, of moves, lines and cubic curves, which is all this reads of SVG.
"""
import re
import struct
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
LOGO_LONGEST = 320
TITLE_WIDTH = 640
OVERSAMPLE = 4


def trim(ink):
    return ink.crop(ink.point(lambda value: 255 if value > 16 else 0).getbbox())


def save(ink, name):
    (ROOT / "assets" / name).write_bytes(struct.pack("<HH", ink.width, ink.height) + ink.tobytes())
    print(f"{name}: {ink.width} x {ink.height}")


def subpaths(data):
    """Each closed figure of the path as a list of points, its curves in short lines."""
    tokens = re.findall(r"[MLCZHVmlczhv]|-?\d*\.?\d+(?:[eE][-+]?\d+)?", data)
    figures, points, at, command = [], [], 0, None
    x, y = 0.0, 0.0

    def number():
        nonlocal at
        at += 1
        return float(tokens[at - 1])

    while at < len(tokens):
        if tokens[at].isalpha():
            command = tokens[at]
            at += 1
            if command in "Zz":
                if points:
                    figures.append(points)
                points = []
                continue
        relative = command.islower()
        kind = command.upper()
        if kind == "M":
            dx, dy = number(), number()
            x, y = (x + dx, y + dy) if relative else (dx, dy)
            if points:
                figures.append(points)
            points = [(x, y)]
            command = "l" if relative else "L"
        elif kind == "L":
            dx, dy = number(), number()
            x, y = (x + dx, y + dy) if relative else (dx, dy)
            points.append((x, y))
        elif kind == "H":
            dx = number()
            x = x + dx if relative else dx
            points.append((x, y))
        elif kind == "V":
            dy = number()
            y = y + dy if relative else dy
            points.append((x, y))
        elif kind == "C":
            values = [number() for _ in range(6)]
            if relative:
                values = [value + (x if index % 2 == 0 else y) for index, value in enumerate(values)]
            (x1, y1, x2, y2, x3, y3) = values
            for step in range(1, 25):
                t = step / 24
                u = 1 - t
                points.append((u * u * u * x + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t * t * t * x3, u * u * u * y + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t * t * t * y3))
            x, y = x3, y3
        else:
            raise ValueError(f"not read: {command}")
    if points:
        figures.append(points)
    return figures


def title():
    text = (ROOT / "assets" / "title.svg").read_text(encoding="utf-8")
    left, top, width, height = [float(value) for value in re.search(r'viewBox="([^"]+)"', text).group(1).split()]
    ratio = TITLE_WIDTH * OVERSAMPLE / width
    size = (round(width * ratio), round(height * ratio))
    ink = Image.new("L", size, 0)
    for data in re.findall(r'\sd="([^"]+)"', text):
        for figure in subpaths(data):
            one = Image.new("L", size, 0)
            ImageDraw.Draw(one).polygon([((px - left) * ratio, (py - top) * ratio) for px, py in figure], fill=255)
            ink = ImageChops.logical_xor(ink.convert("1"), one.convert("1")).convert("L")
    ink = trim(ink)
    return ink.resize((round(ink.width / OVERSAMPLE), round(ink.height / OVERSAMPLE)), Image.LANCZOS)


def main():
    ink = trim(Image.open(ROOT / "assets" / "logo.png").convert("RGBA").getchannel("A"))
    ratio = LOGO_LONGEST / max(ink.size)
    save(ink.resize((round(ink.width * ratio), round(ink.height * ratio)), Image.LANCZOS), "logo.a8")
    save(title(), "title.a8")


if __name__ == "__main__":
    main()
