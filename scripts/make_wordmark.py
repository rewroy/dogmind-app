# Builds assets/logo-wordmark.png and assets/logo-wordmark-dark.png: the dog (assets/logo.png) beside "Dogmind"
# in Inter ExtraBold (SIL OFL), its corners rounded to match the dog's line. Run from the repo root.
# Needs Pillow and numpy; set FONT to any bold sans if Inter is not installed.
import sys
from PIL import Image, ImageDraw, ImageFont, ImageFilter

SS = 4                      # supersample
LOGO_H = 360 * SS           # dog height in the lockup
FONT = "/usr/share/fonts/opentype/inter/Inter-ExtraBold.otf"
TEXT = "Dogmind"

logo = Image.open("assets/logo.png").convert("RGBA")
logo = logo.crop(logo.getchannel("A").getbbox())
logo = logo.resize((round(logo.width * LOGO_H / logo.height), LOGO_H), Image.LANCZOS)

# text mask, corners rounded by blur+threshold
size = round(LOGO_H * 0.62)
font = ImageFont.truetype(FONT, size)
l, t, r, b = font.getbbox(TEXT)
pad = 40 * SS
m = Image.new("L", (r - l + pad * 2, b - t + pad * 2), 0)
ImageDraw.Draw(m).text((pad - l, pad - t), TEXT, font=font, fill=255)
rad = 7 * SS
m = m.filter(ImageFilter.GaussianBlur(rad))
m = m.point(lambda v: 255 if v > 118 else 0).filter(ImageFilter.GaussianBlur(1.2 * SS / 2))
m = m.crop(m.getbbox())

gap = round(LOGO_H * 0.10)
W = logo.width + gap + m.width
H = LOGO_H
base_a = Image.new("L", (W, H), 0)
base_a.paste(logo.getchannel("A"), (0, 0))
# center text on the dog's face vertically (slightly low to sit on the baseline of the muzzle)
ty = (H - m.height) // 2 + round(H * 0.03)
base_a.paste(m, (logo.width + gap, ty))

# dog's own pixels are black; keep alpha only, recolor
final_w = 1200
scale = final_w / W
a = base_a.resize((final_w, round(H * scale)), Image.LANCZOS)
for name, col in (("logo-wordmark.png", 0), ("logo-wordmark-dark.png", 255)):
    img = Image.new("RGBA", a.size, (col, col, col, 0))
    img.putalpha(a)
    img.save("assets/" + name, optimize=True)
    print(name, img.size)
