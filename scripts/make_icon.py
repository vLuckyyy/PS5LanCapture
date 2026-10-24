"""Generates assets/icon.png (256 px) and assets/app.ico from code, no third-party packages."""
import math
import struct
import sys
import zlib
from pathlib import Path


def render(size):
    pixels = bytearray(size * size * 4)
    radius = size * 0.22
    for y in range(size):
        for x in range(size):
            cx = min(max(x + 0.5, radius), size - radius)
            cy = min(max(y + 0.5, radius), size - radius)
            dist = math.hypot(x + 0.5 - cx, y + 0.5 - cy)
            alpha = max(0.0, min(1.0, radius - dist + 0.5))
            t = (x + y) / (2 * size)
            r = int(0x1d + (0x3a - 0x1d) * t)
            g = int(0x4e + (0x8d - 0x4e) * t)
            b = int(0xc8 + (0xff - 0xc8) * t)
            u, v = (x + 0.5) / size, (y + 0.5) / size
            screen = 0.20 <= u <= 0.80 and 0.24 <= v <= 0.64
            inner = 0.25 <= u <= 0.75 and 0.29 <= v <= 0.59
            stand = (0.44 <= u <= 0.56 and 0.64 <= v <= 0.72) or (0.34 <= u <= 0.66 and 0.72 <= v <= 0.76)
            px, py = (u - 0.45) / 0.14, (v - 0.44) / 0.10
            play = 0 <= px <= 1 and abs(py) <= 1 - px
            if (screen and not inner) or stand or play:
                r, g, b = 255, 255, 255
            i = (y * size + x) * 4
            pixels[i:i + 4] = bytes((r, g, b, int(alpha * 255)))
    return pixels


def png(size, pixels):
    raw = b"".join(b"\x00" + bytes(pixels[y * size * 4:(y + 1) * size * 4]) for y in range(size))
    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xffffffff)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def main(out_dir):
    out = Path(out_dir)
    sizes = [16, 24, 32, 48, 64, 256]
    images = {s: png(s, render(s)) for s in sizes}
    (out / "icon.png").write_bytes(images[256])
    header = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries, blobs = b"", b""
    for s in sizes:
        data = images[s]
        entries += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
        blobs += data
    (out / "app.ico").write_bytes(header + entries + blobs)


if __name__ == "__main__":
    main(sys.argv[1])
