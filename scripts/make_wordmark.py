#!/usr/bin/env python3
"""Make assets/logo-wordmark.png and assets/logo-wordmark-dark.png for the README: the dog (assets/logo.png) and the title (assets/title.svg)
one above the other, as one picture, in black and in white for GitHub's dark theme. Needs Pillow. From the repository root:

    python scripts/make_wordmark.py

The title is read as make_logo.py reads it.
"""
from pathlib import Path

from PIL import Image

import make_logo

ROOT = Path(__file__).resolve().parent.parent
WIDTH = 1200          # of the picture, in pixels
TITLE_OF_DOG = 1.5    # the title's width, as a part of the dog's: the app's welcome page and chat set 150:95 and 160:112


def main():
    """The dog above, the name under it, both centred, as the app sets them: the name starts where the dog's line ends."""
    make_logo.TITLE_WIDTH = WIDTH
    title = make_logo.title()
    dog = make_logo.trim(Image.open(ROOT / "assets" / "logo.png").convert("RGBA").getchannel("A"))
    dog = dog.resize((round(WIDTH / TITLE_OF_DOG), round(dog.height * WIDTH / TITLE_OF_DOG / dog.width)), Image.LANCZOS)

    ink = Image.new("L", (WIDTH, dog.height + title.height), 0)
    ink.paste(dog, ((WIDTH - dog.width) // 2, 0))
    ink.paste(title, (0, dog.height))

    for name, shade in (("logo-wordmark.png", 17), ("logo-wordmark-dark.png", 255)):
        picture = Image.new("RGBA", ink.size, (shade, shade, shade, 0))
        picture.putalpha(ink)
        picture.save(ROOT / "assets" / name, optimize=True)


if __name__ == "__main__":
    main()
