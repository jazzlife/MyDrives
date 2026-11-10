#!/usr/bin/env python3
"""Draws the MyDrives mark and writes the icon files a package needs.

The agent draws the same drive outline for the menu bar (`tray.rs`); this is
the filled version, on the accent colour, that an app icon wants. Keeping it in
code means there is no binary asset to keep in step with it - and no image
library to install on a build machine.

    python3 packaging/icon.py <output directory>

writes `icon.png` (1024), `icon.ico` (Windows) and `icon.iconset/` (ready for
`iconutil -c icns`).
"""

import os
import struct
import sys
import zlib

ACCENT = (0x00, 0x7A, 0xFF)
WHITE = (0xFF, 0xFF, 0xFF)
MASTER = 1024
# Supersampling factor for the master render; everything else is averaged down
# from it, which is both faster and smoother than redrawing per size.
SS = 2
# macOS leaves air around the rounded square; Windows does not mind it either.
INSET = 0.10
ICO_SIZES = (16, 32, 64, 128, 256)
# `iconutil` insists on exactly these names.
ICONSET = [
    (16, "icon_16x16.png"),
    (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"),
    (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"),
    (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"),
    (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"),
    (1024, "icon_512x512@2x.png"),
]


def in_rounded(px, py, left, top, width, height, radius):
    """The rounded-rectangle test the tray icon uses, in floating point."""
    if px < left or py < top or px >= left + width or py >= top + height:
        return False
    cx = min(max(px, left + radius), left + width - radius)
    cy = min(max(py, top + radius), top + height - radius)
    dx, dy = px - cx, py - cy
    return dx * dx + dy * dy <= radius * radius


def render_master():
    """RGBA rows for the 1024 icon, supersampled and averaged down."""
    n = MASTER * SS
    # The rounded square the mark sits on.
    pad = n * INSET
    bg = (pad, pad, n - 2 * pad, n - 2 * pad, (n - 2 * pad) * 0.225)
    # The drive body, centred: the tray glyph is 24 x 16 on a 32 grid.
    body_w = (n - 2 * pad) * 0.56
    body_h = body_w * 16.0 / 24.0
    body_l = (n - body_w) / 2
    body_t = (n - body_h) / 2
    radius = body_h * 0.25
    border = body_h * 0.1
    # The activity light, towards the right of the body.
    led_x = body_l + body_w - body_h * 0.42
    led_y = body_t + body_h / 2
    led_r = body_h * 0.13

    rows = []
    for y in range(MASTER):
        row = bytearray()
        for x in range(MASTER):
            r = g = b = a = 0
            for sy in range(SS):
                py = y * SS + sy + 0.5
                for sx in range(SS):
                    px = x * SS + sx + 0.5
                    if not in_rounded(px, py, *bg):
                        continue
                    outer = in_rounded(px, py, body_l, body_t, body_w, body_h, radius)
                    inner = in_rounded(
                        px,
                        py,
                        body_l + border,
                        body_t + border,
                        body_w - 2 * border,
                        body_h - 2 * border,
                        max(radius - border, 1.0),
                    )
                    dx, dy = px - led_x, py - led_y
                    led = dx * dx + dy * dy <= led_r * led_r
                    mark = (outer and not inner) or led
                    colour = WHITE if mark else ACCENT
                    r += colour[0]
                    g += colour[1]
                    b += colour[2]
                    a += 255
            samples = SS * SS
            if a == 0:
                row += bytes(4)
            else:
                # Average over the covered samples so the edge keeps its colour
                # and only the alpha fades.
                covered = a // 255
                row += bytes((r // covered, g // covered, b // covered, a // samples))
        rows.append(bytes(row))
    return rows


def halve(rows, size):
    """Box-filter an RGBA image down to half its size."""
    out = []
    for y in range(size // 2):
        top, bottom = rows[2 * y], rows[2 * y + 1]
        row = bytearray()
        for x in range(size // 2):
            i = 8 * x
            for c in range(4):
                row.append(
                    (top[i + c] + top[i + 4 + c] + bottom[i + c] + bottom[i + 4 + c]) // 4
                )
        out.append(bytes(row))
    return out


def pyramid():
    """Every power-of-two size from the master down to 16."""
    sizes = {MASTER: render_master()}
    size = MASTER
    while size > 16:
        sizes[size // 2] = halve(sizes[size], size)
        size //= 2
    return sizes


def png(rows, size):
    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    raw = b"".join(b"\x00" + row for row in rows)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def ico(images):
    """Windows .ico with PNG payloads, which Vista and later read."""
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries, payloads = b"", b""
    for size, data in images:
        entries += struct.pack(
            "<BBBBHHII",
            0 if size >= 256 else size,
            0 if size >= 256 else size,
            0,
            0,
            1,
            32,
            len(data),
            offset,
        )
        payloads += data
        offset += len(data)
    return header + entries + payloads


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: icon.py <output directory>")
    out = sys.argv[1]
    iconset = os.path.join(out, "icon.iconset")
    os.makedirs(iconset, exist_ok=True)

    sizes = pyramid()
    files = {size: png(rows, size) for size, rows in sizes.items()}

    with open(os.path.join(out, "icon.png"), "wb") as f:
        f.write(files[MASTER])
    with open(os.path.join(out, "icon.ico"), "wb") as f:
        f.write(ico([(s, files[s]) for s in ICO_SIZES]))
    for size, name in ICONSET:
        with open(os.path.join(iconset, name), "wb") as f:
            f.write(files[size])
    print(f"icon.png, icon.ico and icon.iconset/ written to {out}")


if __name__ == "__main__":
    main()
