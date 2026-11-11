"""Renders FEMME's mark (assets/logo.svg: ink square, sunset disc, four slats) into the PNGs the
icon and splash generators want. Pillow only; drawn 4x and downscaled for clean edges.
    python3 brand.py <out dir>"""
import sys
from PIL import Image, ImageDraw

INK = (0x1E, 0x1E, 0x1E, 255)
STOPS = [(0.10, (0xFF, 0xC8, 0x57)), (0.55, (0xFF, 0x4F, 0xB4)), (1.0, (0x9B, 0x3F, 0xD0))]
SLATS = [(17, 18.4), (20, 21.8), (23.4, 25.6), (27, 29)]  # y ranges in the 32-unit logo space
CX, CY, R = 16, 17, 11


def grad(t):
    t = max(STOPS[0][0], min(1.0, t))
    for (t0, c0), (t1, c1) in zip(STOPS, STOPS[1:]):
        if t <= t1:
            k = (t - t0) / (t1 - t0)
            return tuple(int(round(a + (b - a) * k)) for a, b in zip(c0, c1)) + (255,)
    return STOPS[-1][1] + (255,)


def draw(size, *, bg=None, scale=1.0, rounded=False, flat=None, cut=False):
    """flat: a solid colour for the disc instead of the gradient; cut: slats are transparent."""
    ss = 4
    px = size * ss
    u = px / 32  # one logo unit in pixels
    img = Image.new("RGBA", (px, px), bg or (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    if rounded:
        d.rounded_rectangle((0, 0, px - 1, px - 1), radius=7 * u, fill=INK)

    def sc(v, c):  # scale a logo coordinate around the centre (16)
        return (16 + (v - 16) * scale) * u

    disc = Image.new("RGBA", (px, px), (0, 0, 0, 0))
    top, bot = sc(CY - R, 0), sc(CY + R, 0)
    dd = ImageDraw.Draw(disc)
    for y in range(int(top), int(bot) + 1):
        dd.line((0, y, px, y), fill=flat or grad((y - top) / (bot - top)))
    mask = Image.new("L", (px, px), 0)
    ImageDraw.Draw(mask).ellipse((sc(CX - R, 0), sc(CY - R, 0), sc(CX + R, 0), sc(CY + R, 0)), fill=255)
    img.paste(disc, (0, 0), mask)
    d = ImageDraw.Draw(img)
    for y0, y1 in SLATS:
        box = (sc(5, 0), sc(y0, 0), sc(27, 0), sc(y1, 0))
        if cut:
            cutter = Image.new("RGBA", (px, px), (0, 0, 0, 0))
            m = Image.new("L", (px, px), 255)
            ImageDraw.Draw(m).rectangle(box, fill=0)
            img = Image.composite(img, cutter, m)
            d = ImageDraw.Draw(img)
        else:
            d.rectangle(box, fill=INK)
    return img.resize((size, size), Image.LANCZOS)


out = sys.argv[1]
draw(1024, bg=INK).save(f"{out}/icon.png")  # iOS light (iOS rounds the corners)
draw(1024).save(f"{out}/icon-dark.png")  # iOS 18 dark: transparent, the system paints the tile
draw(1024, flat=(255, 255, 255), cut=True).save(f"{out}/icon-tinted.png")  # iOS 18 tinted
draw(1024, scale=0.8).save(f"{out}/icon-fg.png")  # Android adaptive foreground (66 dp safe zone)
draw(1024, scale=0.8, flat=(255, 255, 255), cut=True).save(f"{out}/icon-mono.png")  # Android 13 themed
draw(512, rounded=True).save(f"{out}/splash.png")  # launch screen centre image
# Android 12 splash: 1152 px canvas, the logo inside the 768 px circle it is clipped to
a12 = Image.new("RGBA", (1152, 1152), (0, 0, 0, 0))
logo = draw(560, rounded=True)
a12.paste(logo, ((1152 - 560) // 2, (1152 - 560) // 2), logo)
a12.save(f"{out}/splash-a12.png")
print("ok")
