#!/usr/bin/env python3
"""Turn the source artwork into a macOS icon master (assets/icon_1024.png).

Crops to the artwork's opaque bounds, scales it to 824x824, and centers it on a
1024x1024 transparent canvas. That 824/1024 ratio is Apple's macOS icon grid —
the badge needs the surrounding margin or it renders oversized next to other
apps in the Dock.

Usage: python3 assets/prepare_icon.py [source.png] [out.png]
"""
import os
import struct
import subprocess
import sys
import tempfile
import zlib

CANVAS = 1024
BADGE = 824


def load_rgba(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", f"{path} is not a PNG"
    pos, idat, w, h, depth, ctype = 8, b"", 0, 0, 0, 0
    while pos < len(data):
        ln = struct.unpack(">I", data[pos : pos + 4])[0]
        tag = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + ln]
        if tag == b"IHDR":
            w, h, depth, ctype = struct.unpack(">IIBB", chunk[:10])
        elif tag == b"IDAT":
            idat += chunk
        pos += 12 + ln
    assert depth == 8 and ctype == 6, f"{path}: need 8-bit RGBA (depth={depth} type={ctype})"
    raw = zlib.decompress(idat)
    stride = w * 4
    out = bytearray(h * stride)
    prev = bytearray(stride)
    p = 0
    for y in range(h):
        f = raw[p]
        p += 1
        line = bytearray(raw[p : p + stride])
        p += stride
        if f == 1:
            for i in range(4, stride):
                line[i] = (line[i] + line[i - 4]) & 255
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 255
        elif f == 3:
            for i in range(stride):
                a = line[i - 4] if i >= 4 else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 255
        elif f == 4:
            for i in range(stride):
                a = line[i - 4] if i >= 4 else 0
                c = prev[i - 4] if i >= 4 else 0
                b = prev[i]
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 255
        out[y * stride : (y + 1) * stride] = line
        prev = line
    return w, h, out


def write_rgba(path, w, h, px):
    rows = bytearray()
    for y in range(h):
        rows.append(0)
        rows += px[y * w * 4 : (y + 1) * w * 4]

    def chunk(tag, payload):
        return (
            struct.pack(">I", len(payload))
            + tag
            + payload
            + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
        )

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(rows), 9))
    png += chunk(b"IEND", b"")
    open(path, "wb").write(png)


def alpha_bbox(w, h, px):
    minx, miny, maxx, maxy = w, h, -1, -1
    for y in range(h):
        row = y * w * 4
        for x in range(w):
            if px[row + x * 4 + 3] > 8:
                minx = min(minx, x)
                maxx = max(maxx, x)
                miny = min(miny, y)
                maxy = max(maxy, y)
    assert maxx >= 0, "source image is fully transparent"
    return minx, miny, maxx - minx + 1, maxy - miny + 1


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    src = sys.argv[1] if len(sys.argv) > 1 else os.path.join(here, "omnipty_icon_1024.png")
    dst = sys.argv[2] if len(sys.argv) > 2 else os.path.join(here, "icon_1024.png")

    w, h, px = load_rgba(src)
    x, y, bw, bh = alpha_bbox(w, h, px)
    # Square the crop around the artwork so scaling can't distort it.
    side = max(bw, bh)
    x -= (side - bw) // 2
    y -= (side - bh) // 2
    print(f"source {w}x{h} -> crop {side}x{side} at ({x},{y})")

    with tempfile.TemporaryDirectory() as tmp:
        cropped = os.path.join(tmp, "crop.png")
        scaled = os.path.join(tmp, "scaled.png")
        # sips resamples better than anything reasonable to hand-roll here.
        subprocess.run(
            ["sips", "-c", str(side), str(side), "--cropOffset", str(y), str(x),
             src, "--out", cropped],
            check=True, stdout=subprocess.DEVNULL,
        )
        subprocess.run(
            ["sips", "-z", str(BADGE), str(BADGE), cropped, "--out", scaled],
            check=True, stdout=subprocess.DEVNULL,
        )
        bw2, bh2, badge = load_rgba(scaled)

    canvas = bytearray(CANVAS * CANVAS * 4)
    ox = (CANVAS - bw2) // 2
    oy = (CANVAS - bh2) // 2
    for row in range(bh2):
        s = row * bw2 * 4
        d = ((row + oy) * CANVAS + ox) * 4
        canvas[d : d + bw2 * 4] = badge[s : s + bw2 * 4]
    write_rgba(dst, CANVAS, CANVAS, canvas)
    print(f"wrote {dst} ({CANVAS}x{CANVAS}, badge {bw2}x{bh2})")


if __name__ == "__main__":
    main()
