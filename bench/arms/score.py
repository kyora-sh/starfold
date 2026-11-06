#!/usr/bin/env python3
"""Holds each decoded picture until the next unit's source frame, as a viewer would see it, so a
frame the encoder skipped scores against the last one shown instead of shifting the rest.
Usage: score.py <decoded.yuv> <units> <frames> <frame bytes> <out.yuv>; prints skipped=<n>."""
import sys

decoded, units, frames, size, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
shown = [int(line.split()[0]) for line in open(units)]
if not shown or shown[0] != 0 or shown != sorted(set(shown)):
    sys.exit(f"bad unit order in {units}")
with open(decoded, "rb") as src, open(out, "wb") as dst:
    unit, picture = 0, b""
    for frame in range(frames):
        if unit < len(shown) and shown[unit] == frame:
            src.seek(unit * size)
            picture = src.read(size)
            if len(picture) != size:
                sys.exit(f"decoded stream ends at unit {unit}")
            unit += 1
        dst.write(picture)
print(f"skipped={frames - len(shown)}")
