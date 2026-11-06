# starfold vs noVNC, November 2026

starfold (`--encoder auto`, OpenH264 on Linux, `<video>` presentation, plus one `?present=canvas` arm) against noVNC as kyora runs it,
on kyora Linux VMs at 1920x1080, 30 fps source, for typing, scrolling and video. Harness: `bench/vs/` (usage in the headers of `run.sh` and `driver.mjs`).
Code: WAN sets at 38c19ca, dense sets at 0aa0693 (same scoring and reader code), LAN sets at 5c9731b.
Shared VMs are noisy: trust direction over absolutes, and compare arms only within one VM run.

## Arms and links

- **noVNC**: x11vnc on Xvfb with damage on, `-wait 2 -defer 2 -nonap`, unix socket; noVNC 1.6.0 stock, `qualityLevel 7`, `compressionLevel 2`,
  `scaleViewport`, `shared`. `novnc-low` is kyora's low profile (`qualityLevel 4`, `compressionLevel 6`), on shaped links only.
  websockify stands in for kyora's vmd relay; vmd's own relay cost and its effect on request timing are not measured.
- **starfold-video / starfold-canvas**: the same server, `<video>` default presentation and the canvas fallback.
- **Links**: unshaped VM-local (LAN); WAN 20 Mbit/s and 2 Mbit/s with 50 ms RTT through `bench/shaper.py` (userspace, 250 KB queue;
  the VM kernel has no netem). A bulk TCP and a WebSocket transfer through the shaper and websockify reach 20.0 and 2.0 Mbit/s
  (`bench/vs/link-check.py`), so the shaper is not the limit.

## Method

- Arms interleaved with rotating order inside one VM run, n>=5 per cell (n=12 for the rate sets); median [min..max].
- Failures count: every cell shows valid/attempted, and the failure ledger is in the logs. One viewer per run, checked mid-run and at the end.
- Frame timing and key-to-paint come from a page-side reader: the source draws a marker strip, the reader decodes it after each draw
  (event-driven: a 2D canvas is read only after something drew on it). Keys toggle a tagged region, so a frame is provably caused by a key.
- Rate (Mbit/s) is TCP payload acked by the viewer per server connection (`ss`), the same count for every arm.
- Quality: browser screenshots against an `xwd` of the X server after the picture freezes (`bench/vs/quality.py`), SSIM and PSNR on luma,
  with the marker strip and pointer masked; text SSIM only counts windows with detail in the truth.
- **Reader parity**: every rate set pairs each arm with and without the reader in the same VM run (`PARITY_ROUNDS`). The reader is
  neutral for starfold everywhere and for noVNC on LAN and at 2 Mbit/s. It is not neutral for noVNC at 20 Mbit/s (see below).
- **Admission rules**, fixed before the data they apply to: at most 2 rate VMs at once; every row records the host's steal share over its
  measured window, and a row with steal > 2% is dropped; every VM run logs its CPU model and a fixed single-thread timing (CALIB) before
  and after, and a run whose CALIB drifts > 15% is dropped and rerun. Each table shows its VM's CALIB. CPU percentages are only
  comparable within one VM run: solo VMs differ by ~1.8x for the same work with no steal.

## Results

### LAN

LAN parity held with this reader (noVNC 84.1 vs 83.9 Mbit/s scroll, 109 vs 112 video; starfold 8.6 vs 8.2 and 14.5 vs 14.5), so the
LAN sets below, run with the previous reader at 5c9731b, stand. Browser CPU for the canvas arm comes from the no-reader control,
since its reader still has to read the canvas.

#### LAN, reader rows (reader v1, 5c9731b)
VM and CALIB not recorded (this set predates the CALIB rule)

main / typing

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 256/256 | 36 [34..37] | 57 [54..59] | 0.00 [0.00..0.00] | 3 [3..3] | 8 [7..10] | 5.1 [5.1..5.2] | - |
| starfold-video | 5/5 | 0 | - | - | - | 250/250 | 27 [26..28] | 35 [33..42] | 0.03 [0.02..0.03] | 4 [4..5] | 9 [7..11] | 12.7 [12.2..13.2] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 252/252 | 26 [24..28] | 53 [45..58] | 0.02 [0.02..0.03] | 4 [4..5] | 39 [36..40] | 12.5 [11.9..12.6] | 0,0,0,0,0 |

main / scroll

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | 29.0 [29.0..29.3] | 50 [50..53] | 0 [0..1] | 146/146 | 56 [53..62] | 85 [78..90] | 90.48 [89.54..90.74] | 42 [40..46] | 100 [93..117] | 48.7 [48.2..50.1] | - |
| starfold-video | 5/5 | 0 | 29.9 [29.8..30.0] | 67 [67..67] | 0 [0..0] | 145/145 | 34 [32..38] | 49 [48..50] | 7.23 [6.42..8.11] | 26 [22..33] | 26 [21..33] | 55.3 [55.2..55.7] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | 30.0 [30.0..30.1] | 67 [50..67] | 0 [0..0] | 143/143 | 46 [43..55] | 63 [59..63] | 7.39 [6.86..8.39] | 24 [23..28] | 73 [68..81] | 53.3 [52.5..53.5] | 0,0,0,0,0 |

main / video

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | 29.7 [29.1..29.9] | 53 [49..54] | 0 [0..0] | 144/144 | 60 [58..64] | 85 [83..87] | 112.24 [110.60..112.78] | 40 [38..42] | 130 [123..144] | 45.1 [44.9..45.6] | - |
| starfold-video | 5/5 | 0 | 30.0 [29.7..30.1] | 50 [50..50] | 0 [0..0] | 145/145 | 37 [33..38] | 54 [50..56] | 14.47 [14.42..14.48] | 42 [37..44] | 29 [26..33] | 54.8 [54.4..55.2] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | 30.0 [30.0..30.0] | 50 [50..50] | 0 [0..0] | 144/144 | 55 [51..58] | 71 [60..74] | 14.47 [14.43..14.48] | 42 [40..47] | 76 [74..85] | 53.0 [52.3..53.3] | 0,0,0,0,0 |

#### LAN, no-reader control (reader v1, 5c9731b)
VM and CALIB not recorded (this set predates the CALIB rule)

no-readback / typing

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 0/251 | - | - | 0.00 [0.00..0.00] | 3 [3..4] | 3 [3..3] | 4.9 [4.9..5.3] | - |
| starfold-video | 5/5 | 0 | - | - | - | 0/252 | - | - | 0.02 [0.02..0.03] | 4 [4..5] | 7 [6..9] | 12.6 [12.5..12.9] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 0/252 | - | - | 0.02 [0.02..0.03] | 5 [4..6] | 14 [12..17] | 12.7 [12.2..13.2] | 0,0,0,0,0 |

no-readback / scroll

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 0/144 | - | - | 90.65 [89.49..90.77] | 45 [41..47] | 103 [91..110] | 51.4 [49.3..52.7] | - |
| starfold-video | 5/5 | 0 | - | - | - | 0/145 | - | - | 7.82 [7.31..8.26] | 26 [25..32] | 24 [23..29] | 55.0 [54.7..55.2] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 0/145 | - | - | 8.13 [6.28..8.98] | 26 [25..26] | 49 [47..50] | 54.3 [54.2..54.9] | 0,0,0,0,0 |

no-readback / video

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 0/145 | - | - | 111.77 [111.48..112.41] | 39 [39..43] | 127 [126..140] | 45.1 [44.9..45.6] | - |
| starfold-video | 5/5 | 0 | - | - | - | 0/144 | - | - | 14.47 [14.42..14.48] | 41 [39..54] | 27 [26..36] | 55.4 [54.8..55.6] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 0/144 | - | - | 14.43 [14.42..14.48] | 43 [39..61] | 52 [48..70] | 54.3 [52.9..54.3] | 0,0,0,0,0 |

LAN quality (n=5, PSNR dB / text SSIM; settled = +500 ms to +8 s after the picture freezes, identical across that range):

| workload | starfold-video | noVNC (q7) | noVNC low |
|---|---|---|---|
| typing, settled | 49.1 / 0.9996 | 43.2 / 0.9984 | 36.4 / 0.9918 |
| scroll, settled | 48.3 / 0.9992 | 42.4 / 0.9983 | 35.7 / 0.9914 |
| scroll, in motion | 41.0 / 0.9897 | 42.5 / 0.9983 | 35.8 / 0.9912 |
| video, settled | 44.6 / 0.9963 | 40.1 / 0.9748 | 38.9 / 0.9465 |
| video, in motion | 38.3 / 0.9795 | 40.1 / 0.9743 | 38.8 / 0.9467 |

starfold leads once the picture settles and trails noVNC q7 slightly while scrolling or playing video. 0 keyframes in every starfold run.

### WAN 20 Mbit/s, 50 ms RTT

**noVNC runs in one of two modes here.** Without the reader, each noVNC connection settles into either a fast mode (20.0 Mbit/s,
an update every ~154 ms on scroll) or a slow mode (14.2-14.5 Mbit/s, ~213 ms). Per update the server side is identical in both
(request to first byte ~5 ms, same bytes per update, same x11vnc CPU; measured with an RFB tap at the relay, `bench/vs/rfb-tap.py`),
and host steal is ~0 in both. The slow cycle is a strict request-then-response loop at link speed: 383 KB per scroll update is 153 ms
at 20 Mbit/s plus one 50 ms round trip, ~203 ms predicted vs 211 ms measured (video: 239 vs 245 ms). The fast cycle is shorter than
transfer plus round trip, so some request must overlap the previous update's delivery. In the browser, noVNC sends its next request
0.1 ms after the last WebSocket message reaches its handler, so it does not hold the request; the time is lost before the message
reaches the page. The share of fast connections is not stable across VM runs: video was fast in 1 of 12 here, and in 9 of 10 in a
run later dropped by the CALIB rule. Any page hook (the frame reader, or a WebSocket timing hook) shifts noVNC toward the slow mode. **The cause is open.**
kyora's vmd relay may behave differently from websockify.

#### WAN 20 Mbit/s scroll, rate (p20-scroll)
VM: AMD EPYC; CALIB 1.84 s / 1.72 s (drift 6.8%)

| arm | reader | valid/att | flagged | Mbit/s each run | median | fast-mode share | swap p50 ms (fast / slow) |
|---|---|---|---|---|---|---|---|
| novnc (scroll) | no | 12/12 | 0 | 14.3, 14.4, 20.0, 20.0, 14.2, 20.0, 14.4, 14.5, 14.4, 20.0, 14.4, 14.4 | 14.43 | 4/12 | 154 [154..155] / 213 [211..215] |
| novnc (scroll) | yes | 12/12 | 0 | 14.4, 14.4, 14.3, 14.3, 14.5, 14.3, 14.5, 14.3, 14.4, 14.3, 14.3, 14.3 | 14.34 | 0/12 | - / 216 [216..217] |
| starfold-video (scroll) | no | 12/12 | 0 | 7.0, 6.5, 5.8, 6.6, 6.6, 7.1, 5.5, 6.7, 7.0, 5.8, 6.6, 5.3 | 6.58 | - | 27 [27..27] |
| starfold-video (scroll) | yes | 12/12 | 0 | 6.5, 5.7, 6.3, 6.3, 5.8, 5.0, 5.9, 8.0, 5.9, 6.4, 6.9, 5.5 | 6.11 | - | 27 [26..28] |

#### WAN 20 Mbit/s video, rate (p20-video, rerun)
VM: AMD EPYC; CALIB 1.76 s / 1.91 s (drift 8.3%)

| arm | reader | valid/att | flagged | Mbit/s each run | median | fast-mode share | swap p50 ms (fast / slow) |
|---|---|---|---|---|---|---|---|
| novnc (video) | no | 12/12 | 0 | 15.4, 15.4, 15.4, 20.0, 15.3, 15.4, 15.4, 15.4, 15.4, 15.4, 15.4, 15.4 | 15.38 | 1/12 | 189 [189..189] / 245 [245..246] |
| novnc (video) | yes | 12/12 | 0 | 20.1, 15.5, 15.4, 15.4, 15.4, 15.4, 20.0, 15.4, 15.4, 15.4, 15.4, 15.4 | 15.42 | 2/12 | 184 [184..184] / 250 [250..250] |
| starfold-video (video) | no | 12/12 | 0 | 14.4, 14.4, 14.4, 14.5, 14.4, 14.4, 14.4, 14.4, 14.4, 14.4, 14.4, 14.4 | 14.44 | - | 29 [28..29] |
| starfold-video (video) | yes | 12/12 | 0 | 14.4, 14.4, 14.4, 14.5, 14.4, 14.4, 14.4, 14.4, 14.5, 14.4, 14.4, 14.5 | 14.44 | - | 28 [28..29] |

The latency table below needs the reader, so **its noVNC rows are slow-mode only; no noVNC latency was measured in the fast mode.**
Next to them, the no-reader rate sets give the best available measure of noVNC in its natural mix of modes: on scroll, 4 of 12 connections fast (20.0 Mbit/s, 9.8 updates/s, ~154 ms per update) and 8 slow (14.2-14.5 Mbit/s, 7.1 updates/s, ~213 ms);
on video, 1 of 12 fast (20.0 Mbit/s, 8.0 updates/s, ~189 ms) and 11 slow (15.4 Mbit/s, 6.2 updates/s, ~245 ms).

#### WAN 20 Mbit/s / 50 ms: typing + scroll (wan-ts)
VM: AMD EPYC; CALIB 1.95 s / 2.16 s (drift 11.1%)

main / typing

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 248/248 | 88 [85..89] | 109 [108..113] | 0.00 [0.00..0.00] | 6 [6..7] | 11 [11..16] | 5.0 [4.9..5.0] | - |
| novnc-low | 5/5 | 0 | - | - | - | 246/246 | 87 [85..91] | 111 [108..112] | 0.00 [0.00..0.00] | 6 [6..7] | 11 [10..12] | 4.9 [4.9..5.0] | - |
| starfold-video | 5/5 | 0 | - | - | - | 253/253 | 70 [68..73] | 91 [86..99] | 0.01 [0.01..0.01] | 4 [4..5] | 14 [13..16] | 11.6 [11.3..11.8] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 250/250 | 77 [74..94] | 103 [100..132] | 0.01 [0.01..0.01] | 4 [4..5] | 48 [44..70] | 11.3 [11.0..11.6] | 0,0,0,0,0 |

main / scroll

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 4/5 | 1 | 3.6 [2.7..4.0] | 575 [466..800] | 35 [26..39] | 117/117 | 357 [340..364] | 456 [434..462] | 13.54 [13.26..13.64] | 46 [46..47] | 42 [40..45] | 6.6 [6.3..6.7] | - |
| novnc-low | 4/5 | 1 | 4.6 [4.5..5.8] | 424 [317..500] | 46 [44..57] | 118/118 | 230 [218..242] | 300 [288..317] | 9.75 [9.49..10.01] | 48 [46..48] | 44 [41..47] | 10.0 [9.9..10.7] | - |
| starfold-video | 4/5 | 1 | 28.4 [27.6..28.8] | 75 [67..100] | 4 [4..9] | 118/118 | 88 [87..94] | 156 [137..181] | 6.72 [6.35..7.20] | 27 [22..33] | 31 [26..36] | 54.6 [54.3..55.1] | 0,0,0,0 |
| starfold-canvas | 5/5 | 0 | 29.2 [28.3..29.4] | 67 [67..67] | 2 [0..4] | 145/145 | 108 [96..112] | 135 [120..159] | 6.00 [5.36..6.14] | 29 [23..32] | 89 [79..99] | 52.1 [51.8..52.7] | 0,0,0,0,0 |

#### WAN 20 Mbit/s / 50 ms: video (wan-v)
VM: AMD EPYC; CALIB 1.84 s / 1.86 s (drift 1.0%)

main / video

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | 4.1 [4.0..4.1] | 250 [250..283] | 40 [39..40] | 146/146 | 389 [362..396] | 486 [484..492] | 15.42 [15.35..15.43] | 16 [15..16] | 31 [27..32] | 6.2 [6.2..6.2] | - |
| novnc-low | 5/5 | 0 | 8.5 [8.4..8.5] | 133 [117..133] | 82 [82..84] | 144/144 | 171 [167..193] | 231 [225..235] | 10.26 [10.25..10.38] | 17 [15..17] | 30 [29..33] | 12.9 [12.8..13.0] | - |
| starfold-video | 5/5 | 0 | 30.0 [29.9..30.1] | 51 [50..67] | 0 [0..0] | 144/144 | 109 [108..110] | 130 [126..133] | 14.47 [14.44..14.47] | 39 [38..43] | 31 [28..37] | 55.1 [54.9..55.8] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | 30.0 [30.0..30.1] | 50 [50..50] | 0 [0..0] | 145/145 | 125 [123..130] | 145 [138..146] | 14.45 [14.42..14.48] | 39 [37..41] | 71 [67..78] | 47.2 [46.7..51.0] | 0,0,0,0,0 |

In noVNC's slow mode, which is the only mode its latency was measured in, its scroll key-to-paint is 357 ms p50 against starfold's 88 ms,
at 3.6 vs 28.4 FPS; its fast mode updates ~28% more often, and its latency there is not measured.

### WAN 2 Mbit/s, 50 ms RTT

#### WAN 2 Mbit/s scroll, rate (p2-scroll, rerun)
VM: AMD EPYC; CALIB 1.82 s / 1.91 s (drift 4.9%)

| arm | reader | valid/att | flagged | Mbit/s each run | median | fast-mode share | swap p50 ms (fast / slow) |
|---|---|---|---|---|---|---|---|
| novnc (scroll) | no | 12/12 | 0 | 1.9, 1.9, 1.9, 1.9, 1.9, 1.9, 1.9, 1.9, 2.0, 1.9, 1.9, 1.9 | 1.93 | - | 1593 [1572..1621] |
| novnc (scroll) | yes | 12/12 | 0 | 2.0, 1.9, 1.9, 1.9, 1.9, 2.0, 1.9, 1.9, 1.9, 1.9, 1.9, 1.9 | 1.92 | - | 1584 [1542..1615] |
| starfold-video (scroll) | no | 12/12 | 0 | 1.8, 1.8, 1.8, 1.8, 1.8, 1.8, 1.8, 1.8, 1.8, 1.7, 1.7, 1.7 | 1.79 | - | 40 [36..44] |
| starfold-video (scroll) | yes | 12/12 | 0 | 1.7, 1.8, 1.8, 1.8, 1.8, 1.8, 1.8, 1.7, 1.8, 1.8, 1.8, 1.8 | 1.79 | - | 38 [35..44] |

#### WAN 2 Mbit/s scroll, latency (thin-scroll, rerun)
VM: AMD EPYC; CALIB 1.51 s / 1.51 s (drift 0.2%)

main / scroll

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swaps/s | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | 0.7 [0.6..0.7] | 1668 [1649..3466] | 6 [5..6] | 37/146 | 1743 [1622..1794] | 2870 [1917..3007] | 1.92 [1.92..1.92] | 19 [18..20] | 14 [11..15] | 1.0 [1.0..1.0] | - |
| novnc-low | 5/5 | 0 | 1.3 [1.3..1.4] | 817 [817..833] | 12 [12..13] | 61/144 | 963 [860..1027] | 1165 [1148..1476] | 1.84 [1.84..1.86] | 19 [18..20] | 16 [14..18] | 1.9 [1.9..2.0] | - |
| starfold-video | 5/5 | 0 | 12.5 [12.3..13.2] | 233 [216..250] | 32 [29..35] | 137/146 | 306 [268..396] | 526 [494..631] | 1.80 [1.79..1.82] | 11 [11..12] | 13 [12..16] | 24.5 [24.2..24.5] | 0,0,0,0,0 |

At 2 Mbit/s a key counts as seen only if its tagged frame arrives within 5 s; noVNC answered 37 of 146 keys in time (low: 61 of 144),
starfold 137 of 146. Latency percentiles cover answered keys only, so noVNC's are flattered by the keys it never answered in time.

Quality at 2 Mbit/s, scroll page (n=5, PSNR dB / text SSIM after the picture freezes):

| arm | +500 ms | +1 s | +3 s | +5 s .. +8 s | in motion |
|---|---|---|---|---|---|
| noVNC (q7) | 13.7 / 0.00 | 13.7 / 0.00 | 42.7 / 0.9982 (4 of 5 runs) | 42.7 / 0.9983 | 42.6 / 0.9983 |
| noVNC low | 13.9 / -0.01 | 35.9 / 0.9912 | 36.0 / 0.9912 | 36.0 / 0.9912 | 35.8 / 0.9912 |
| starfold-video | 30.4 / 0.9532 | 33.3 / 0.9788 | 33.3 / 0.9788 | 33.3 [30.5..35.7] / 0.9788 | 27.7 / 0.9032 |

starfold shows a usable picture first (+500 ms) but settles at 33.3 dB and stays there, below both noVNC profiles once they catch up
(+1 s low, +3 s q7). noVNC's in-motion score is high because each shot shows one complete, if stale, update at 0.7 updates/s; it measures
fidelity, not freshness. 0 keyframes in every starfold run on this page.

Dense text page (`bench/dense.html`, 11 px monospace filling the screen), scroll, n=5, settled PSNR / text SSIM:

| arm | +8 s | +12 s .. +32 s |
|---|---|---|
| noVNC (q7) | 9.3 / -0.01 (not caught up) | 34.4 / 0.9971 |
| starfold-video | 10.8 / -0.01 (not caught up) | 29.5 [28.3..30.7] / 0.9883 |
| noVNC low | 27.8 / 0.9842 (some runs) | 27.8 / 0.9842 |

Every arm converges between +8 and +12 s. On dense text at 2 Mbit/s starfold settles below noVNC q7 and above noVNC low (at LAN it
leads: 36.9-39.8 vs 34.4 dB). quality.py was checked on this page first: a frame against itself scores 99 dB, a 1 px shift 8.6 dB, and on
real LAN frames a +/-3 px shift search finds every arm aligned at 0 px. Scores while scrolling are not reported for this page, since the
motion truth assumes the scroll page's geometry. **starfold keeps sending keyframes on the frozen screen**: 1 during the scroll, then
1 by +2 s, 3 by +12 s and 5-6 by +32 s in all five runs.

### macOS, server side only

The kyora-macos runner lacks Screen Recording permission, so no browser could watch it; these are encoder numbers from
`bench/vs/mac-encoders.sh` on an Apple M6 VM (n=5, median):

| encoder | workload | kbit/s | encode p50 ms | encode p95 ms | CPU s/s |
|---|---|---|---|---|---|
| VideoToolbox H.264 | typing | 26 | 15.6 | 16.8 | 0.038 |
| VideoToolbox H.264 | scrolling | 1156 | 15.3 | 16.6 | 0.039 |
| VideoToolbox H.264 | video | 29605 | 18.0 | 19.4 | 0.033 |
| VideoToolbox HEVC | typing | 30 | 16.7 | 18.1 | 0.027 |
| VideoToolbox HEVC | scrolling | 1065 | 16.3 | 17.5 | 0.027 |
| VideoToolbox HEVC | video | 29766 | 16.4 | 18.2 | 0.012 |
| OpenH264 | typing | 3 | 2.0 | 2.1 | 0.061 |
| OpenH264 | scrolling | 2728 | 2.3 | 2.4 | 0.069 |
| OpenH264 | video | 9618 | 6.0 | 6.4 | 0.182 |

AV1 has no hardware encoder on any runner yet, so there is no AV1 arm.

## Discarded sets

- First sets 1 and 2: the original reader read the canvas every frame and held up noVNC's main thread, suppressing noVNC.
- First quality set: a synthetic page, and its motion column scored against the wrong frames.
- Sets scored with the earlier key matcher: a missed key matched the same letter ~26 s later and cascaded; the search is now capped at 5 s.
- WAN sets run with the previous reader (wan-ts, wan-v, thin-scroll at 5c9731b): that reader cut noVNC to 14.3 vs 20.0 Mbit/s (n=5); rerun here.
- The first dense set: settle points stopped at +8 s, before any arm had caught up.
- Headline runs dropped by the CALIB rule (drift 19-26%, steal bursts of 10-20% from other tenants), and rerun.

Open: the cause of the occasional bad reads (noVNC with the reader, 1-5 per run at 20 Mbit/s, 22-44 on LAN scroll) is not settled; they
do not change the rate parity.

## Licences and patents

Code: starfold and its upstream voxl are MIT; OpenH264 is BSD-2-Clause; noVNC is MPL-2.0; x11vnc is GPL-2.0, used here as an external
process. x264 is GPL and only behind an off-by-default cargo feature. Patents, descriptively and without legal conclusions: HEVC has
established patent pools that can reach anyone distributing an encoder. AV1 and VP9 are offered royalty-free by their developers,
though third-party pools also assert claims over them, so "no exposure" is not accurate. H.264 has its own pool; the owner has accepted
that residual risk for self-built OpenH264. This needs legal review before any wide ship and is not a finding of this bench.
