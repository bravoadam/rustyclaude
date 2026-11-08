#!/usr/bin/env python3
"""Draws icon.png: the R and the Claude mark on a dark tile. Run from assets/, then
`cd ../app && npx tauri icon ../assets/icon.png`. claude-mark.png is the mark's alpha mask."""
from PIL import Image, ImageDraw, ImageFilter, ImageFont

S = 4096  # draw large, shrink at the end for clean edges
k = S // 1024
FONT = "/home/adam/.local/share/fonts/Inter/InterDisplay-Black.otf"


def gradient(top, bottom):
    col = Image.new("RGB", (1, S))
    for y in range(S):
        t = y / (S - 1)
        col.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return col.resize((S, S))


def tile_mask(inset=100, radius=185):
    m = Image.new("L", (S, S), 0)
    ImageDraw.Draw(m).rounded_rectangle([inset * k, inset * k, (1024 - inset) * k, (1024 - inset) * k], radius * k, fill=255)
    return m


icon = Image.new("RGBA", (S, S), (0, 0, 0, 0))
tile = tile_mask()
shadow = Image.new("RGBA", (S, S), (0, 0, 0, 90))
shadow.putalpha(tile.point(lambda v: v * 90 // 255).filter(ImageFilter.GaussianBlur(14 * k)).transform((S, S), Image.AFFINE, (1, 0, 0, 0, 1, -12 * k)))
icon.alpha_composite(shadow)
icon.paste(gradient((52, 48, 44), (26, 24, 22)), (0, 0), tile)

# R
r = Image.new("L", (S, S), 0)
font = ImageFont.truetype(FONT, 590 * k)
ImageDraw.Draw(r).text((430 * k, 770 * k), "R", font=font, fill=255, anchor="ms")
icon.paste(gradient((251, 247, 240), (230, 223, 211)), (0, 0), r)

# Claude mark, upscaled, with edges re-sharpened after the resize
size = 350 * k
mark = Image.open("claude-mark.png").convert("L").resize((size, size), Image.BICUBIC)
mark = mark.filter(ImageFilter.GaussianBlur(k * 1.5)).point(lambda v: max(0, min(255, (v - 96) * 4)))
layer = Image.new("L", (S, S), 0)
layer.paste(mark, (548 * k, 118 * k))
# A dark ring keeps the mark readable where it overlaps the R.
ring = layer.filter(ImageFilter.MaxFilter(4 * k + 1)).filter(ImageFilter.GaussianBlur(k))
icon.paste(Image.new("RGB", (S, S), (34, 31, 28)), (0, 0), Image.composite(ring, Image.new("L", (S, S), 0), tile))
icon.paste(Image.new("RGB", (S, S), (217, 119, 87)), (0, 0), layer)

icon.resize((1024, 1024), Image.LANCZOS).save("icon.png")
