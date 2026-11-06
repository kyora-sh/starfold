"""diag.py <run dir>: decodes stripe 0 with an independent decoder and reads the marker and tag from every
frame the server sent, with the page reader's sample points and threshold. Writes ref.json."""
import json, subprocess, sys
import numpy as np
d = sys.argv[1]
CELL, W = 16, 1920
s0 = f'{d}/vs-s0.h264'
probe = subprocess.run(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-show_entries', 'stream=width,height', '-of', 'csv=p=0', s0], capture_output=True, text=True).stdout.strip().split(',')
w, h = int(probe[0]), int(probe[1])
raw = subprocess.run(['ffmpeg', '-v', 'error', '-f', 'h264', '-i', s0, '-fps_mode', 'passthrough', '-f', 'rawvideo', '-pix_fmt', 'gray', '-'], capture_output=True).stdout
fr = np.frombuffer(raw, np.uint8).reshape(-1, h, w)
rows = []
for f in fr:
    on = lambda col, row: int(f[row * CELL + CELL // 2, col * CELL + CELL // 2] > 126)
    v = sum(on(b % 20, 1 if b >= 20 else 0) << b for b in range(32))
    ck = sum(on(b % 20, 1) << (b - 32) for b in range(32, 40))
    good = ((v ^ (v >> 8) ^ (v >> 16) ^ (v >> 24)) & 0xff) == ck
    tag = sum(on(b, 2) << b for b in range(5))
    inv = sum(on(b + 5, 2) << b for b in range(5))
    luma = [int(f[2 * CELL + CELL // 2, b * CELL + CELL // 2]) for b in range(10)]
    rows.append([v >> 20 if good else -1, tag if tag ^ inv == 31 else -1, luma])
json.dump({'w': w, 'h': h, 'rows': rows}, open(f'{d}/ref.json', 'w'))
