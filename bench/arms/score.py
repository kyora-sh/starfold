#!/usr/bin/env python3
"""Holds each decoded picture until the next unit's source frame, as a viewer would see it, so a
frame the encoder skipped scores against the last one shown instead of shifting the rest. Then
scores luma only where the source has detail: 16x16 blocks whose range is over 32, as quality.py's
ssimText ignores white space. Usage: score.py <decoded.yuv> <units> <frames> <source.yuv> <out.yuv>
<width> <height>; prints skipped=<n> text_psnr_db=<all frames> text_psnr_last30_db=<last 30>."""
import sys

import numpy as np

decoded, units, frames, source, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5]
w, h = int(sys.argv[6]), int(sys.argv[7])
size, B = w * h * 3 // 2, 16
shown = [int(line.split()[0]) for line in open(units)]
if not shown or shown[0] != 0 or shown != sorted(set(shown)):
    sys.exit(f"bad unit order in {units}")


def blocks(y):
    return y[: h // B * B, : w // B * B].reshape(h // B, B, w // B, B).swapaxes(1, 2)


errors, counts = [], []
with open(decoded, "rb") as dec, open(source, "rb") as src, open(out, "wb") as dst:
    unit, picture = 0, b""
    for frame in range(frames):
        if unit < len(shown) and shown[unit] == frame:
            dec.seek(unit * size)
            picture = dec.read(size)
            if len(picture) != size:
                sys.exit(f"decoded stream ends at unit {unit}")
            unit += 1
        dst.write(picture)
        truth = src.read(size)
        if len(truth) != size:
            sys.exit(f"source ends at frame {frame}")
        t = blocks(np.frombuffer(truth, np.uint8, w * h).reshape(h, w).astype(np.int32))
        d = blocks(np.frombuffer(picture, np.uint8, w * h).reshape(h, w).astype(np.int32))
        detail = (t.max(axis=(2, 3)) - t.min(axis=(2, 3))) > 32
        errors.append(float(((t - d) ** 2)[detail].sum()))
        counts.append(int(detail.sum()) * B * B)


def db(e, n):
    return 99.0 if n == 0 or e == 0 else 10 * np.log10(255 * 255 * n / e)


print(
    f"skipped={frames - len(shown)} text_psnr_db={db(sum(errors), sum(counts)):.2f}"
    f" text_psnr_last30_db={db(sum(errors[-30:]), sum(counts[-30:])):.2f}"
)
