#!/usr/bin/env python3
"""Make assets/logo-wordmark.png and assets/logo-wordmark-dark.png for the README: the dog (assets/logo.png) and the title (assets/title.svg)
side by side, as one picture, in black and in white for GitHub's dark theme. Needs Pillow. From the repository root:

    python scripts/make_wordmark.py

The title is read as make_logo.py reads it.
"""
from pathlib import Path

from PIL import Image

import make_logo

ROOT = Path(__file__).resolve().parent.parent
WIDTH = 1200          # of the picture, in pixels
DOG = 360             # the dog's height, and the lockup's, before scaling
TITLE_OF_DOG = 0.52   # the title's height, as a part of the dog's
GAP = 0.10            # between them, as a part of the dog's height


def main():
    dog = make_logo.trim(Image.open(ROOT / "assets" / "logo.png").convert("RGBA").getchannel("A"))
    dog = dog.resize((round(dog.width * DOG / dog.height), DOG), Image.LANCZOS)

    make_logo.TITLE_WIDTH = 2400
    title = make_logo.title()
    height = round(DOG * TITLE_OF_DOG)
    title = title.resize((round(title.width * height / title.height), height), Image.LANCZOS)

    gap = round(DOG * GAP)
    ink = Image.new("L", (dog.width + gap + title.width, DOG), 0)
    ink.paste(dog, (0, 0))
    ink.paste(title, (dog.width + gap, (DOG - title.height) // 2))
    ink = ink.resize((WIDTH, round(ink.height * WIDTH / ink.width)), Image.LANCZOS)

    for name, shade in (("logo-wordmark.png", 17), ("logo-wordmark-dark.png", 255)):
        picture = Image.new("RGBA", ink.size, (shade, shade, shade, 0))
        picture.putalpha(ink)
        picture.save(ROOT / "assets" / name, optimize=True)


if __name__ == "__main__":
    main()
