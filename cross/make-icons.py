#!/usr/bin/env python3
"""Generate Cascade's app icons from the same hexagon the web UI draws.

The mark is the one in api/web.html's header: a pointy-top hexagon OUTLINE on a
17x19 viewBox, stroked in --accent at 1.3 units. Everything here is that shape scaled
up, so the icon and the UI cannot drift apart by accident.

No image libraries — this writes PNG (zlib + a type-0 filter per scanline) and ICO
containers directly, so it runs on any machine with a stock Python 3.

    python3 cross/make-icons.py            # -> assets/{icon_1024.png, cascade.ico, Cascade.icns}
"""
import math, os, struct, subprocess, sys, zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT  = os.path.join(ROOT, "assets")

# --accent from web.html's :root (the default dark theme).
COLOR = (0x4d, 0xa3, 0xff)

# web.html: viewBox "0 0 17 19", polygon 8.5,1 16,5 16,14 8.5,18 1,14 1,5, stroke 1.3.
VB_W, VB_H   = 17.0, 19.0
POINTS       = [(8.5, 1.0), (16.0, 5.0), (16.0, 14.0), (8.5, 18.0), (1.0, 14.0), (1.0, 5.0)]
STROKE       = 1.3
MASTER       = 1024


def seg_dist(px, py, ax, ay, bx, by):
    """Distance from a point to a line segment."""
    dx, dy = bx - ax, by - ay
    d2 = dx * dx + dy * dy
    t = 0.0 if d2 == 0.0 else max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / d2))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


def render(size):
    """RGBA rows for the hexagon outline, antialiased on the distance field.

    The viewBox is 17x19 — taller than wide — so it is fitted into the square with the
    SAME scale on both axes and centred. Fitting each axis independently would stretch
    the hexagon into something that is no longer the brand mark.
    """
    scale = size / max(VB_W, VB_H)
    ox = (size - VB_W * scale) / 2.0
    oy = (size - VB_H * scale) / 2.0
    pts = [(x * scale + ox, y * scale + oy) for x, y in POINTS]
    half = STROKE * scale / 2.0
    # Antialias over one pixel of distance either side of the stroke edge.
    aa = 0.5
    r, g, b = COLOR

    rows = []
    for y in range(size):
        py = y + 0.5
        row = bytearray()
        for x in range(size):
            px = x + 0.5
            d = min(seg_dist(px, py, *pts[i], *pts[(i + 1) % len(pts)])
                    for i in range(len(pts)))
            # Coverage: 1 well inside the stroke, 0 well outside, linear across the edge.
            cov = max(0.0, min(1.0, (half + aa - d) / (2 * aa))) if half + aa > 0 else 0.0
            a = int(round(cov * 255))
            # Straight (unpremultiplied) alpha, which is what PNG stores.
            row += bytes((r, g, b, a))
        rows.append(bytes(row))
    return rows


def downscale(rows, src, dst):
    """Area-average box filter. Alpha-weighted so edge pixels do not darken."""
    out = []
    for y in range(dst):
        y0, y1 = y * src // dst, max(y * src // dst + 1, (y + 1) * src // dst)
        row = bytearray()
        for x in range(dst):
            x0, x1 = x * src // dst, max(x * src // dst + 1, (x + 1) * src // dst)
            asum = 0
            n = 0
            for sy in range(y0, y1):
                r = rows[sy]
                for sx in range(x0, x1):
                    asum += r[sx * 4 + 3]
                    n += 1
            a = asum // n if n else 0
            row += bytes((COLOR[0], COLOR[1], COLOR[2], a))
        out.append(bytes(row))
    return out


def png(rows, size):
    raw = b"".join(b"\x00" + r for r in rows)          # filter type 0 per scanline
    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))


def ico(images):
    """ICO with PNG-compressed entries — supported by Windows Vista and later."""
    n = len(images)
    header = struct.pack("<HHH", 0, 1, n)
    offset = 6 + 16 * n
    entries, blobs = b"", b""
    for size, blob in images:
        # 0 means 256 in the directory's one-byte width/height fields.
        w = h = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", w, h, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
        blobs += blob
    return header + entries + blobs


def main():
    os.makedirs(OUT, exist_ok=True)
    print(f"rendering {MASTER}x{MASTER} master...", flush=True)
    master = render(MASTER)

    cache = {MASTER: master}
    def at(size):
        if size not in cache:
            cache[size] = downscale(master, MASTER, size)
        return cache[size]

    with open(os.path.join(OUT, "icon_1024.png"), "wb") as f:
        f.write(png(master, MASTER))

    # macOS menu bar. Drawn as a TEMPLATE image, which means AppKit uses only the alpha
    # channel and paints it in the system colour — so this adapts to light and dark menu
    # bars, and to being highlighted, automatically. The blue in the RGB channels is simply
    # ignored there, which is why the same render serves both uses.
    # 128px covers @2x on any current display for an ~18pt item.
    with open(os.path.join(OUT, "menubar.png"), "wb") as f:
        f.write(png(at(128), 128))

    # Windows .ico
    ico_sizes = [16, 24, 32, 48, 64, 128, 256]
    print("ico:", " ".join(map(str, ico_sizes)), flush=True)
    with open(os.path.join(OUT, "cascade.ico"), "wb") as f:
        f.write(ico([(s, png(at(s), s)) for s in ico_sizes]))

    # macOS .icns, via iconutil on the required iconset names.
    if sys.platform == "darwin":
        iconset = os.path.join(OUT, "Cascade.iconset")
        os.makedirs(iconset, exist_ok=True)
        want = [(16, "16x16", 1), (32, "16x16", 2), (32, "32x32", 1), (64, "32x32", 2),
                (128, "128x128", 1), (256, "128x128", 2), (256, "256x256", 1),
                (512, "256x256", 2), (512, "512x512", 1), (1024, "512x512", 2)]
        print("iconset:", " ".join(sorted({str(s) for s, _, _ in want})), flush=True)
        for size, name, scale in want:
            suffix = "" if scale == 1 else "@2x"
            with open(os.path.join(iconset, f"icon_{name}{suffix}.png"), "wb") as f:
                f.write(png(at(size), size))
        subprocess.run(["iconutil", "-c", "icns", iconset,
                        "-o", os.path.join(OUT, "Cascade.icns")], check=True)

    for f in sorted(os.listdir(OUT)):
        p = os.path.join(OUT, f)
        if os.path.isfile(p):
            print(f"  {f:22} {os.path.getsize(p):>8} bytes")


if __name__ == "__main__":
    main()
