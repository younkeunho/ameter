"""Renders the ameter icon into assets/ameter.ico (and assets/ameter.png, 256 px).

A dark tile with a gauge: three filled segments (white, Claude orange, Higgsfield lime) over a
track, and a needle at the fill level. Every size is drawn from the geometry below at 16x and
downsampled, so small sizes stay sharp instead of being shrunk from the 256 px image.

    python assets/icon.py [preview.png]
"""

import io
import math
import struct
import sys
from pathlib import Path

from PIL import Image, ImageDraw

SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
# Sizes up to this one are stored as 32-bit DIBs, larger ones as PNG.
DIB_MAX = 64

TILE_TOP = (0x2C, 0x2E, 0x33)
TILE_BOTTOM = (0x16, 0x17, 0x1A)
TRACK = (0x3C, 0x3F, 0x46)
WHITE = (0xF5, 0xF5, 0xF5)
ORANGE = (0xD7, 0x77, 0x57)
LIME = (0xC8, 0xE6, 0x4C)

# Gauge geometry in a 256-unit square. PIL angles: 0 = 3 o'clock, clockwise.
CENTER = (128, 150)
RADIUS = 78          # stroke centre
START, SWEEP = 150, 240
LEVEL = 0.74         # needle position along the sweep


def render(size: int) -> Image.Image:
    ss = 16
    px = size * ss
    u = px / 256.0
    small = size <= 24
    stroke = (36 if small else 28) * u
    gap = 0 if small else 5

    img = Image.new("RGBA", (px, px), (0, 0, 0, 0))

    # Tile with a vertical gradient, masked to a rounded square.
    margin = (8 if small else 12) * u
    gradient = Image.new("RGBA", (px, px))
    gd = ImageDraw.Draw(gradient)
    for y in range(px):
        t = y / (px - 1)
        gd.line([(0, y), (px, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(TILE_TOP, TILE_BOTTOM)) + (255,))
    mask = Image.new("L", (px, px), 0)
    ImageDraw.Draw(mask).rounded_rectangle([margin, margin, px - margin, px - margin], radius=54 * u, fill=255)
    img.paste(gradient, (0, 0), mask)
    if not small:
        ImageDraw.Draw(img).rounded_rectangle(
            [margin, margin, px - margin, px - margin], radius=54 * u, outline=(255, 255, 255, 30), width=max(1, round(2 * u))
        )

    d = ImageDraw.Draw(img)
    cx, cy = CENTER[0] * u, CENTER[1] * u
    r = RADIUS * u
    box = [cx - r - stroke / 2, cy - r - stroke / 2, cx + r + stroke / 2, cy + r + stroke / 2]
    width = round(stroke)

    level_angle = START + SWEEP * LEVEL
    d.arc(box, level_angle, START + SWEEP, fill=TRACK + (255,), width=width)
    span = (level_angle - START - 2 * gap) / 3
    for i, colour in enumerate((WHITE, ORANGE, LIME)):
        a = START + i * (span + gap)
        d.arc(box, a, a + span, fill=colour + (255,), width=width)

    # Round caps at both ends of the gauge.
    for angle, colour in ((START, WHITE), (START + SWEEP, TRACK)):
        ex = cx + r * math.cos(math.radians(angle))
        ey = cy + r * math.sin(math.radians(angle))
        d.ellipse([ex - stroke / 2, ey - stroke / 2, ex + stroke / 2, ey + stroke / 2], fill=colour + (255,))

    # Needle: a tapered wedge from the hub toward the fill level.
    a = math.radians(level_angle)
    length = r - stroke / 2 - (4 if small else 3) * u
    base = (13 if small else 12) * u
    tip = (5 if small else 4) * u
    nx, ny = math.cos(a), math.sin(a)
    px_, py_ = -ny, nx
    needle = [
        (cx + px_ * base, cy + py_ * base),
        (cx + nx * length + px_ * tip, cy + ny * length + py_ * tip),
        (cx + nx * length - px_ * tip, cy + ny * length - py_ * tip),
        (cx - px_ * base, cy - py_ * base),
    ]
    d.polygon(needle, fill=WHITE + (255,))
    hub = (22 if small else 19) * u
    d.ellipse([cx - hub, cy - hub, cx + hub, cy + hub], fill=WHITE + (255,))
    if not small:
        dot = 7 * u
        d.ellipse([cx - dot, cy - dot, cx + dot, cy + dot], fill=TILE_BOTTOM + (255,))

    return img.resize((size, size), Image.Resampling.LANCZOS)


def dib(img: Image.Image) -> bytes:
    """BITMAPINFOHEADER + bottom-up BGRA rows + an all-zero AND mask (alpha decides)."""
    w, h = img.size
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    pixels = img.tobytes("raw", "BGRA")
    rows = b"".join(pixels[y * w * 4:(y + 1) * w * 4] for y in reversed(range(h)))
    mask_row = ((w + 31) // 32) * 4
    return header + rows + bytes(mask_row * h)


def write_ico(images: list[Image.Image], path: Path) -> None:
    blobs = []
    for img in images:
        if img.width <= DIB_MAX:
            blobs.append(dib(img))
        else:
            buf = io.BytesIO()
            img.save(buf, "PNG", optimize=True)
            blobs.append(buf.getvalue())
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for img, blob in zip(images, blobs):
        dim = img.width if img.width < 256 else 0
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    path.write_bytes(out + b"".join(blobs))


def main() -> None:
    here = Path(__file__).resolve().parent
    images = [render(s) for s in SIZES]
    write_ico(images, here / "ameter.ico")
    images[-1].save(here / "ameter.png", optimize=True)
    if len(sys.argv) > 1:
        # Each size enlarged 8x (nearest) on a dark and a light strip, plus the 256 image.
        cells = [img.resize((img.width * 8, img.height * 8), Image.Resampling.NEAREST) for img in images[:6]]
        width = sum(c.width for c in cells) + 20 * (len(cells) + 1) + 276
        height = max(c.height for c in cells) * 2 + 60
        sheet = Image.new("RGBA", (width, max(height, 296)), (0x20, 0x20, 0x20, 255))
        ImageDraw.Draw(sheet).rectangle([0, height // 2, width, height], fill=(0xF3, 0xF3, 0xF3, 255))
        x = 20
        for c in cells:
            sheet.alpha_composite(c, (x, 20))
            sheet.alpha_composite(c, (x, height // 2 + 10))
            x += c.width + 20
        sheet.alpha_composite(images[-1], (x, 20))
        sheet.save(sys.argv[1])


if __name__ == "__main__":
    main()
