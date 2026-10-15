#!/usr/bin/env python3
"""Converts a raw 32 bpp framebuffer dump (BGRX, e.g. `dd if=/dev/fb0`) to PNG (stdlib only).

    python3 fb2png.py fb.raw out.png WIDTH HEIGHT [STRIDE_BYTES] [SCALE]
"""
import struct
import sys
import zlib

src, dst = sys.argv[1], sys.argv[2]
w, h = int(sys.argv[3]), int(sys.argv[4])
stride = int(sys.argv[5]) if len(sys.argv) > 5 else w * 4
scale = int(sys.argv[6]) if len(sys.argv) > 6 else 1

raw = open(src, "rb").read()
rows = []
for y in range(0, h, scale):
    row = bytearray([0])  # PNG filter: none
    base = y * stride
    for x in range(0, w, scale):
        b, g, r, _ = raw[base + x * 4 : base + x * 4 + 4]
        row += bytes((r, g, b))
    rows.append(bytes(row))


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)


header = struct.pack(">IIBBBBB", w // scale, h // scale, 8, 2, 0, 0, 0)
with open(dst, "wb") as f:
    f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(b"".join(rows))) + chunk(b"IEND", b""))
