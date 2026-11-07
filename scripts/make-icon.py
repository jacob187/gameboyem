#!/usr/bin/env python3
"""Draw the app icon and write it as PNGs (standard library only).

    python3 scripts/make-icon.py            # writes assets/icon/<N>x<N>.png
    python3 scripts/make-icon.py OUT_DIR    # writes them to OUT_DIR instead

The icon is an original design: a cartridge silhouette (coral, one corner cut,
grip ridges, contact strip) on a dark rounded tile, with "gb" on its label. It is drawn once at 1024 px from signed-distance shapes
(analytic edge anti-aliasing) and the smaller sizes are box-filtered from it.
The packager (cargo-packager) turns the PNGs into .icns for macOS and installs
them into the hicolor theme for Linux, so no other format is generated here.
"""

import math
import struct
import sys
import zlib
from array import array
from pathlib import Path

BASE = 1024
SIZES = (1024, 512, 256, 128, 32)

# Palette (r, g, b), 0-255.
TILE = (36, 40, 84)
CART = (238, 112, 84)
CART_SHADE = (186, 74, 62)
LABEL = (244, 240, 228)
INK = (36, 40, 84)
CONTACTS = (58, 48, 96)


# Signed distances in unit coordinates (0..1, y down): negative inside.
def rounded_rect(cx, cy, hw, hh, r):
    def d(x, y):
        qx = abs(x - cx) - (hw - r)
        qy = abs(y - cy) - (hh - r)
        return math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - r

    return d


def circle(cx, cy, r):
    return lambda x, y: math.hypot(x - cx, y - cy) - r


def ring(cx, cy, r, w):
    return lambda x, y: abs(math.hypot(x - cx, y - cy) - r) - w / 2


def segment(ax, ay, bx, by, w):
    """A line with round caps and stroke width w."""
    dx, dy = bx - ax, by - ay
    len2 = dx * dx + dy * dy

    def d(x, y):
        t = max(0.0, min(1.0, ((x - ax) * dx + (y - ay) * dy) / len2))
        return math.hypot(x - ax - t * dx, y - ay - t * dy) - w / 2

    return d


def union(*shapes):
    return lambda x, y: min(s(x, y) for s in shapes)


def intersect(a, b):
    return lambda x, y: max(a(x, y), b(x, y))


def layers():
    """(sdf, colour, bbox) from back to front; bbox is (x0, y0, x1, y1) in unit coords."""
    out = []

    def add(sdf, colour, bbox):
        out.append((sdf, colour, bbox))

    add(rounded_rect(0.5, 0.5, 0.5, 0.5, 0.225), TILE, (0, 0, 1, 1))

    # Cartridge: a rounded body with its top-right corner cut off, over a darker
    # copy of itself that shows as a lower edge.
    cut = lambda x, y: (x - y - 0.52) / math.sqrt(2)
    cart = lambda dy: intersect(rounded_rect(0.5, 0.5 + dy, 0.26, 0.39, 0.05), cut)
    add(cart(0.014), CART_SHADE, (0.24, 0.1, 0.76, 0.91))
    add(cart(0.0), CART, (0.24, 0.1, 0.76, 0.89))

    # Grip ridges across the top.
    for y in (0.185, 0.225, 0.265):
        add(segment(0.32, y, 0.56, y, 0.02), CART_SHADE, (0.3, y - 0.02, 0.58, y + 0.02))

    # Label with the letters "g" and "b".
    add(rounded_rect(0.5, 0.475, 0.205, 0.17, 0.03), LABEL, (0.29, 0.3, 0.71, 0.65))
    w, r, cy = 0.045, 0.055, 0.48
    gx, bx = 0.42, 0.58  # centres of the two bowls
    g = union(
        ring(gx, cy, r, w),
        segment(gx + r, cy - r * 0.2, gx + r, cy + r * 2.1, w),
        segment(gx + r, cy + r * 2.1, gx - r * 0.45, cy + r * 2.1, w),
    )
    b = union(ring(bx, cy, r, w), segment(bx - r, cy - r * 2.0, bx - r, cy + r * 0.2, w))
    add(g, INK, (0.33, 0.33, 0.52, 0.62))
    add(b, INK, (0.49, 0.33, 0.67, 0.62))

    # Contact strip along the bottom edge.
    add(rounded_rect(0.5, 0.795, 0.17, 0.028, 0.01), CONTACTS, (0.32, 0.76, 0.68, 0.83))
    return out


def render():
    """Premultiplied RGBA float planes at BASE x BASE."""
    n = BASE * BASE
    planes = [array("f", bytes(4 * n)) for _ in range(4)]  # r, g, b, a
    for sdf, colour, (x0, y0, x1, y1) in layers():
        px0 = max(0, int(x0 * BASE) - 2)
        px1 = min(BASE, int(math.ceil(x1 * BASE)) + 2)
        py0 = max(0, int(y0 * BASE) - 2)
        py1 = min(BASE, int(math.ceil(y1 * BASE)) + 2)
        cr, cg, cb = (c / 255 for c in colour)
        for py in range(py0, py1):
            y = (py + 0.5) / BASE
            row = py * BASE
            for px in range(px0, px1):
                cov = 0.5 - sdf((px + 0.5) / BASE, y) * BASE
                if cov <= 0.0:
                    continue
                if cov > 1.0:
                    cov = 1.0
                i = row + px
                keep = 1.0 - cov
                planes[0][i] = cr * cov + planes[0][i] * keep
                planes[1][i] = cg * cov + planes[1][i] * keep
                planes[2][i] = cb * cov + planes[2][i] * keep
                planes[3][i] = cov + planes[3][i] * keep
    return planes


def halve(planes, size):
    """Average 2x2 blocks: size x size -> size/2 x size/2."""
    half = size // 2
    out = [array("f", bytes(4 * half * half)) for _ in range(4)]
    for src, dst in zip(planes, out):
        for y in range(half):
            top = (2 * y) * size
            bottom = top + size
            base = y * half
            for x in range(half):
                a = 2 * x
                dst[base + x] = (src[top + a] + src[top + a + 1] + src[bottom + a] + src[bottom + a + 1]) / 4
    return out


def to_rgba8(planes, size):
    """Straight-alpha 8-bit RGBA bytes."""
    buf = bytearray(4 * size * size)
    r, g, b, a = planes
    for i in range(size * size):
        alpha = a[i]
        if alpha <= 0.0:
            continue
        o = 4 * i
        buf[o] = min(255, round(r[i] / alpha * 255))
        buf[o + 1] = min(255, round(g[i] / alpha * 255))
        buf[o + 2] = min(255, round(b[i] / alpha * 255))
        buf[o + 3] = min(255, round(alpha * 255))
    return bytes(buf)


def png(size, rgba):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    stride = 4 * size
    raw = b"".join(b"\x00" + rgba[y * stride : (y + 1) * stride] for y in range(size))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main():
    out_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent / "assets" / "icon"
    out_dir.mkdir(parents=True, exist_ok=True)
    planes, size = render(), BASE
    while True:
        if size in SIZES:
            path = out_dir / f"{size}x{size}.png"
            path.write_bytes(png(size, to_rgba8(planes, size)))
            print(path)
        if size == min(SIZES):
            break
        # 1024 -> 512 -> 256 -> 128 -> 64 -> 32
        planes, size = halve(planes, size), size // 2


if __name__ == "__main__":
    main()
