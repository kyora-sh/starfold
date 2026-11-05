"""Scores screenshots against what the source drew: SSIM and PSNR on luma, marker strip and pointer masked.
ssimText only counts windows with detail in the truth, so white space can't flatter text. Motion shots
name their own frame in the marker strip. Usage: quality.py <dir> <typing|scroll|video> <source dump>"""
import glob, json, os, struct, sys
import numpy as np
from PIL import Image

W, H, CELL, LOOP, STEP, PAGE = 1920, 1080, 16, 30, 20, 1080 + 6000
STRIP_W, STRIP_H, WIN = 20 * CELL, 3 * CELL, 8
root, wl, dump = sys.argv[1:4]


def luma(rgb):
    return 0.299 * rgb[..., 0] + 0.587 * rgb[..., 1] + 0.114 * rgb[..., 2]


def from_bgrx(raw):
    return np.stack([raw[..., 2], raw[..., 1], raw[..., 0]], axis=-1).astype(np.float64)


def png(path):
    return np.asarray(Image.open(path).convert("RGB"), dtype=np.float64)


def xwd(path):
    data = open(path, "rb").read()
    head = struct.unpack(">25I", data[:100])
    size, order, bpp, stride, ncolors = head[0], head[7], head[11], head[12], head[19]
    if order != 0 or bpp != 32:
        raise SystemExit(f"xwd layout: byte order {order}, {bpp} bpp")
    raw = np.frombuffer(data, np.uint8, offset=size + ncolors * 12).reshape(-1, stride // 4, 4)
    return from_bgrx(raw[:H, :W])


def box(a):
    i = np.zeros((a.shape[0] + 1, a.shape[1] + 1))
    i[1:, 1:] = a.cumsum(0).cumsum(1)
    return (i[WIN:, WIN:] - i[:-WIN, WIN:] - i[WIN:, :-WIN] + i[:-WIN, :-WIN]) / WIN**2


def mask():
    m = np.ones((H, W), bool)
    m[:STRIP_H, :STRIP_W] = False
    m[H // 2 - 48 : H // 2 + 48, W // 2 - 48 : W // 2 + 48] = False
    return m


def score(shot, truth):
    a, b = luma(shot), luma(truth)
    m = mask()
    ma, mb = box(a), box(b)
    va, vb, cov = box(a * a) - ma * ma, box(b * b) - mb * mb, box(a * b) - ma * mb
    c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
    s = ((2 * ma * mb + c1) * (2 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
    keep = box(m.astype(np.float64)) > 0.999
    text = keep & (vb > 100)
    mse = ((a - b)[m] ** 2).mean()
    return {
        "ssim": float(s[keep].mean()),
        "ssimText": float(s[text].mean()) if text.any() else None,
        "psnr": float(10 * np.log10(255**2 / mse)) if mse > 0 else 99.0,
        "textShare": float(text.mean()),
    }


def marker(shot):
    v = 0
    for b in range(32):
        col, row = b % 20, 1 if b >= 20 else 0
        if shot[row * CELL + CELL // 2, col * CELL + CELL // 2].sum() > 382:
            v |= 1 << b
    ck = 0
    for b in range(32, 40):
        if shot[CELL + CELL // 2, (b % 20) * CELL + CELL // 2].sum() > 382:
            ck |= 1 << (b - 32)
    return (v >> 20) if ((v ^ v >> 8 ^ v >> 16 ^ v >> 24) & 0xFF) == ck else None


def drawn(n):
    if wl == "video":
        return from_bgrx(np.memmap(dump, np.uint8, "r", shape=(H * LOOP, W, 4))[(n % LOOP) * H : (n % LOOP + 1) * H])
    off = STEP * n % (PAGE - H)
    return from_bgrx(np.memmap(dump, np.uint8, "r", shape=(PAGE, W, 4))[off : off + H])


def mean(rows, key):
    v = [r[key] for r in rows if r[key] is not None]
    return float(np.mean(v)) if v else None


truth = xwd(f"{root}/truth.xwd")
out = {}
for path in sorted(glob.glob(f"{root}/settled-*.png")):
    ms = os.path.basename(path)[8:-4]
    for k, v in score(png(path), truth).items():
        out[f"settled{ms}{k[0].upper()}{k[1:]}"] = v
rows, torn = [], 0
for path in sorted(glob.glob(f"{root}/motion-*.png")):
    shot = png(path)
    n = marker(shot)
    if n is None:
        torn += 1
        continue
    rows.append(score(shot, drawn(n)))
if rows or torn:
    out.update(motionSsim=mean(rows, "ssim"), motionSsimText=mean(rows, "ssimText"), motionPsnr=mean(rows, "psnr"), motionShots=len(rows), motionTorn=torn)
print(json.dumps(out))
