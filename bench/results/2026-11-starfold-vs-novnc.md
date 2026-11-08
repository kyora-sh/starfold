# starfold vs noVNC, November 2026

starfold (`--encoder auto`, OpenH264 on Linux, `<video>` presentation, plus one `?present=canvas` arm) against noVNC as kyora runs it,
on kyora Linux VMs at 1920x1080, 30 fps source, for typing, scrolling and video. Harness: `bench/vs/` (usage in the headers of `run.sh` and `driver.mjs`).
Code: WAN sets at 38c19ca, dense sets at 0aa0693 (same scoring and reader code), LAN sets at 5c9731b, the RFB tap at 59e4c42.
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
  (event-driven: a 2D canvas is read only after something drew on it). Each key toggles a tagged region that carries its letter; keys and
  tags are paired in order within a 5 s cap, preferring the shortest latencies. A tag carries a letter, not a sequence number, so when the
  same letter repeats within the cap a stale frame can be paired with the later press: the pairing is optimistic, can only understate
  latency, and matters most for the slowest arms (noVNC at 2 and 20 Mbit/s), so it errs in noVNC's favour.
- Display updates are counted as compositor swaps (Chrome trace), a proxy for painted updates, not a count of distinct remote updates.
  Tables show the median interval between swaps; swap counts in these logs ran past the measured window, so no swap rates are reported.
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

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 256/256 | 36 [34..37] | 57 [54..59] | 0.00 [0.00..0.00] | 3 [3..3] | 8 [7..10] | 200 [183..200] | - |
| starfold-video | 5/5 | 0 | - | - | - | 250/250 | 27 [26..28] | 35 [33..42] | 0.03 [0.02..0.03] | 4 [4..5] | 9 [7..11] | 72 [66..80] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 252/252 | 26 [24..28] | 53 [45..58] | 0.02 [0.02..0.03] | 4 [4..5] | 39 [36..40] | 82 [66..94] | 0,0,0,0,0 |

main / scroll

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | 29.0 [29.0..29.3] | 50 [50..53] | 0 [0..1] | 146/146 | 56 [53..62] | 85 [78..90] | 90.48 [89.54..90.74] | 42 [40..46] | 100 [93..117] | 33 [33..33] | - |
| starfold-video | 5/5 | 0 | 29.9 [29.8..30.0] | 67 [67..67] | 0 [0..0] | 145/145 | 34 [32..38] | 49 [48..50] | 7.23 [6.42..8.11] | 26 [22..33] | 26 [21..33] | 29 [28..29] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | 30.0 [30.0..30.1] | 67 [50..67] | 0 [0..0] | 143/143 | 46 [43..55] | 63 [59..63] | 7.39 [6.86..8.39] | 24 [23..28] | 73 [68..81] | 33 [33..33] | 0,0,0,0,0 |

main / video

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | 29.7 [29.1..29.9] | 53 [49..54] | 0 [0..0] | 144/144 | 60 [58..64] | 85 [83..87] | 112.24 [110.60..112.78] | 40 [38..42] | 130 [123..144] | 33 [33..33] | - |
| starfold-video | 5/5 | 0 | 30.0 [29.7..30.1] | 50 [50..50] | 0 [0..0] | 145/145 | 37 [33..38] | 54 [50..56] | 14.47 [14.42..14.48] | 42 [37..44] | 29 [26..33] | 29 [29..29] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | 30.0 [30.0..30.0] | 50 [50..50] | 0 [0..0] | 144/144 | 55 [51..58] | 71 [60..74] | 14.47 [14.43..14.48] | 42 [40..47] | 76 [74..85] | 33 [33..33] | 0,0,0,0,0 |

#### LAN, no-reader control (reader v1, 5c9731b)
VM and CALIB not recorded (this set predates the CALIB rule)

no-readback / typing

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 0/251 | - | - | 0.00 [0.00..0.00] | 3 [3..4] | 3 [3..3] | 204 [191..206] | - |
| starfold-video | 5/5 | 0 | - | - | - | 0/252 | - | - | 0.02 [0.02..0.03] | 4 [4..5] | 7 [6..9] | 70 [61..85] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 0/252 | - | - | 0.02 [0.02..0.03] | 5 [4..6] | 14 [12..17] | 74 [50..85] | 0,0,0,0,0 |

no-readback / scroll

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 0/144 | - | - | 90.65 [89.49..90.77] | 45 [41..47] | 103 [91..110] | 29 [28..30] | - |
| starfold-video | 5/5 | 0 | - | - | - | 0/145 | - | - | 7.82 [7.31..8.26] | 26 [25..32] | 24 [23..29] | 29 [28..29] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 0/145 | - | - | 8.13 [6.28..8.98] | 26 [25..26] | 49 [47..50] | 32 [31..32] | 0,0,0,0,0 |

no-readback / video

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 0/145 | - | - | 111.77 [111.48..112.41] | 39 [39..43] | 127 [126..140] | 33 [32..33] | - |
| starfold-video | 5/5 | 0 | - | - | - | 0/144 | - | - | 14.47 [14.42..14.48] | 41 [39..54] | 27 [26..36] | 29 [28..29] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 0/144 | - | - | 14.43 [14.42..14.48] | 43 [39..61] | 52 [48..70] | 31 [30..32] | 0,0,0,0,0 |

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
a swap every ~154 ms on scroll) or a slow mode (14.2-14.5 Mbit/s, ~213 ms); in the 20 Mbit/s rate sets below, host steal was at most 0.1% in both. An RFB tap at the relay (`bench/vs/rfb-tap.py`, which parses the client stream) recorded 24 connections; 2 exceeded the steal
rule (8.8% and 2.5%) and are left out, leaving 8 fast and 14 slow. Within each workload the server side is the same per update in both
modes: request to first byte 10-18 ms on scroll and 4-5 ms on video, the same bytes per update (380-390 KB scroll, 472-473 KB video) and
the same Xvfb CPU (40-44% scroll, 21-24% video, one VM run). The difference is the time from the last byte leaving the relay to the next
request: 135-139 vs 202-204 ms on scroll, 176-177 vs 233-234 ms on video. The relay sits before the shaper, so that time includes the
update's passage through the shaped link. The slow cycle is a strict request-then-response loop at link speed: 383 KB per scroll update is 153 ms
at 20 Mbit/s plus one 50 ms round trip, ~203 ms predicted vs 211 ms measured (video: 239 vs 245 ms). The fast cycle is shorter than
transfer plus round trip, so some request must overlap the previous update's delivery. In a diagnostic run with a WebSocket timing hook, every
connection was in the slow mode, and noVNC sent its next request 0.1 ms after the last WebSocket message reached its handler: in the slow
mode it does not hold the request in the page, so the extra time is spent before the message reaches the page. That is an inference for
the slow mode only; no fast-mode connection was timed this way. The share of fast connections is not stable across VM runs: video was
fast in 1 of 12 no-reader runs here, and in 9 of 10 in a run later dropped by the CALIB rule. Page hooks seem to shift noVNC toward the
slow mode (scroll: 0 of 12 fast with the reader vs 4 of 12 without; the WebSocket hook: 0 of 24), though on video the reader runs were
fast 2 of 12 vs 1 of 12 without, so this is a tendency, not a rule. **The cause is open.**
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

The latency tables below need the reader, and **every noVNC run in them (wan-ts, wan-v) was in the slow mode, so their noVNC latency
is slow-mode latency.** The only fast-mode noVNC latency samples are the 2 video reader runs of the rate set above, perturbed by the
reader: 463 and 466 ms p50 at 5.3 FPS, against 361-393 ms at 4.0 FPS for the 10 slow ones in the same run. In those two runs the fast
mode moved more data without cutting key-to-paint, consistent with updates queueing behind overlapped requests; n=2. Next to them, the no-reader rate sets give the best available measure of noVNC in its natural mix of modes: on scroll, 4 of 12 connections fast (20.0 Mbit/s, a swap every ~154 ms) and 8 slow (14.2-14.5 Mbit/s, ~213 ms);
on video, 1 of 12 fast (20.0 Mbit/s, ~189 ms) and 11 slow (15.4 Mbit/s, ~245 ms).

#### WAN 20 Mbit/s / 50 ms: typing + scroll (wan-ts)
VM: AMD EPYC; CALIB 1.95 s / 2.16 s (drift 11.1%)

main / typing

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | - | - | - | 248/248 | 88 [85..89] | 109 [108..113] | 0.00 [0.00..0.00] | 6 [6..7] | 11 [11..16] | 200 [200..201] | - |
| novnc-low | 5/5 | 0 | - | - | - | 246/246 | 87 [85..91] | 111 [108..112] | 0.00 [0.00..0.00] | 6 [6..7] | 11 [10..12] | 203 [200..216] | - |
| starfold-video | 5/5 | 0 | - | - | - | 253/253 | 70 [68..73] | 91 [86..99] | 0.01 [0.01..0.01] | 4 [4..5] | 14 [13..16] | 98 [92..124] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | - | - | - | 250/250 | 77 [74..94] | 103 [100..132] | 0.01 [0.01..0.01] | 4 [4..5] | 48 [44..70] | 109 [104..117] | 0,0,0,0,0 |

main / scroll

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 4/5 | 1 | 3.6 [2.7..4.0] | 575 [466..800] | 35 [26..39] | 117/117 | 357 [340..364] | 456 [434..462] | 13.54 [13.26..13.64] | 46 [46..47] | 42 [40..45] | 233 [232..234] | - |
| novnc-low | 4/5 | 1 | 4.6 [4.5..5.8] | 424 [317..500] | 46 [44..57] | 118/118 | 230 [218..242] | 300 [288..317] | 9.75 [9.49..10.01] | 48 [46..48] | 44 [41..47] | 150 [135..151] | - |
| starfold-video | 4/5 | 1 | 28.4 [27.6..28.8] | 75 [67..100] | 4 [4..9] | 118/118 | 88 [87..94] | 156 [137..181] | 6.72 [6.35..7.20] | 27 [22..33] | 31 [26..36] | 27 [26..27] | 0,0,0,0 |
| starfold-canvas | 5/5 | 0 | 29.2 [28.3..29.4] | 67 [67..67] | 2 [0..4] | 145/145 | 108 [96..112] | 135 [120..159] | 6.00 [5.36..6.14] | 29 [23..32] | 89 [79..99] | 32 [32..32] | 0,0,0,0,0 |

#### WAN 20 Mbit/s / 50 ms: video (wan-v)
VM: AMD EPYC; CALIB 1.84 s / 1.86 s (drift 1.0%)

main / video

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | 4.1 [4.0..4.1] | 250 [250..283] | 40 [39..40] | 146/146 | 389 [362..396] | 486 [484..492] | 15.42 [15.35..15.43] | 16 [15..16] | 31 [27..32] | 250 [250..250] | - |
| novnc-low | 5/5 | 0 | 8.5 [8.4..8.5] | 133 [117..133] | 82 [82..84] | 144/144 | 171 [167..193] | 231 [225..235] | 10.26 [10.25..10.38] | 17 [15..17] | 30 [29..33] | 117 [117..117] | - |
| starfold-video | 5/5 | 0 | 30.0 [29.9..30.1] | 51 [50..67] | 0 [0..0] | 144/144 | 109 [108..110] | 130 [126..133] | 14.47 [14.44..14.47] | 39 [38..43] | 31 [28..37] | 29 [28..29] | 0,0,0,0,0 |
| starfold-canvas | 5/5 | 0 | 30.0 [30.0..30.1] | 50 [50..50] | 0 [0..0] | 145/145 | 125 [123..130] | 145 [138..146] | 14.45 [14.42..14.48] | 39 [37..41] | 71 [67..78] | 33 [33..33] | 0,0,0,0,0 |

In the slow mode, the only mode in which noVNC scroll latency was measured, noVNC's scroll key-to-paint is 357 ms p50 against
starfold's 88 ms, at 3.6 vs 28.4 FPS; its fast-mode scroll latency is not measured.

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

| arm | valid/att | flagged | FPS | iv p95 ms | gaps>100 | key seen | i2p p50 ms | i2p p95 ms | Mbit/s | server % | browser % | swap p50 ms | keyframes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| novnc | 5/5 | 0 | 0.7 [0.6..0.7] | 1668 [1649..3466] | 6 [5..6] | 37/146 | 1743 [1622..1794] | 2870 [1917..3007] | 1.92 [1.92..1.92] | 19 [18..20] | 14 [11..15] | 1601 [1583..1634] | - |
| novnc-low | 5/5 | 0 | 1.3 [1.3..1.4] | 817 [817..833] | 12 [12..13] | 61/144 | 963 [860..1027] | 1165 [1148..1476] | 1.84 [1.84..1.86] | 19 [18..20] | 16 [14..18] | 767 [764..782] | - |
| starfold-video | 5/5 | 0 | 12.5 [12.3..13.2] | 233 [216..250] | 32 [29..35] | 137/146 | 306 [268..396] | 526 [494..631] | 1.80 [1.79..1.82] | 11 [11..12] | 13 [12..16] | 39 [37..42] | 0,0,0,0,0 |

At 2 Mbit/s a key counts as seen only if its tagged frame arrives within 5 s; noVNC answered 37 of 146 keys in time (low: 61 of 144),
starfold 137 of 146. Latency percentiles cover answered keys only, so noVNC's are flattered by the keys it never answered in time.

Quality at 2 Mbit/s, scroll page (n=5, PSNR dB / text SSIM after the picture freezes):

| arm | +500 ms | +1 s | +3 s | +5 s .. +8 s | in motion |
|---|---|---|---|---|---|
| noVNC (q7) | 13.7 / 0.00 | 13.7 / 0.00 | 42.7 in 3 of 5 runs, 13.7-13.8 in 2 | 42.7 / 0.9983 | 42.6 / 0.9983 |
| noVNC low | 13.9 / -0.01 | 35.9 / 0.9912 | 36.0 / 0.9912 | 36.0 / 0.9912 | 35.8 / 0.9912 |
| starfold-video | 30.4 / 0.9532 | 33.3 / 0.9788 | 33.3 / 0.9788 | 33.3 [30.5..35.7] / 0.9788 | 27.7 / 0.9032 |

starfold shows a usable picture first (+500 ms) but settles at 33.3 dB and stays there, below both noVNC profiles once they catch up
(+1 s low; q7 by +3 s in 3 of 5 runs and by +5 s in all). noVNC's in-motion score is high because each shot shows one complete, if stale, update at 0.7 FPS; it measures
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
- WAN sets run with the previous reader (wan-ts, wan-v, thin-scroll at 5c9731b): with that reader noVNC ran at 14.3 Mbit/s in every run, against 20.0 in every run without it (n=5), before the two modes were known; rerun here.
- The first dense set: settle points stopped at +8 s, before any arm had caught up.
- Headline runs dropped by the CALIB rule (drift 19-26%, steal bursts of 10-20% from other tenants), and rerun.

Open: the cause of the occasional bad reads (noVNC with the reader: 0-15 per run at 20 Mbit/s, 22-44 on LAN scroll) is not settled; they
do not change the rate parity.

## Licences and patents

Code: starfold and its upstream voxl are MIT; OpenH264 is BSD-2-Clause; noVNC is MPL-2.0; x11vnc is GPL-2.0, used here as an external
process. x264 is GPL and only behind an off-by-default cargo feature. Patents, descriptively and without legal conclusions: HEVC has
established patent pools that can reach anyone distributing an encoder. AV1 and VP9 are offered royalty-free by their developers,
though third-party pools also assert claims over them, so "no exposure" is not accurate. H.264 has its own pool; the owner has accepted
that residual risk for self-built OpenH264. This needs legal review before any wide ship and is not a finding of this bench.

## Refresh, 8 November 2026: #13 against main

The same matrix re-measured on main (86703ce), with #13's merge commit (f678862) run alongside it on the same VMs. The numbers above are #13's and stand as history.

**What moved, all at 2 Mbit/s** (20 Mbit/s and LAN moved only in small byte counts, below):

| cell | #13 build | main | A/A S | responsible |
|---|---|---|---|---|
| scroll page, settled at +8 s | 33.0 dB / 0.978 | 48.1 dB / 0.999 | 0.99 dB | #14/#15 (step +13.6 dB) |
| dense text page, settled at +32 s | 30.7 dB / 0.991 | 42.0 dB / 0.9995 | 0.01 dB | not bracketed; #15 is the likely source (it changed the refinement cap and tail) |
| scroll page, in motion, no keys typed | 27.7 dB | 39.4 dB | 3.2 dB | #20 (step +10.4 dB) |
| **scroll page, usable picture at +500 ms** | **10/12 runs** | **1/12 runs** | - | **#20 (step -14.3 dB): a regression** |
| typing during scroll, rounds with over 5% of keys lacking their own frame | 3/12 | 11/12 | - | #20 |

- Settled, the scroll and dense pages now lead noVNC q7 (scroll 48.7 vs 42.6 dB, dense 42.0 vs 34.4 dB), where #13 trailed it on both. Starfold's lead over noVNC in frame rate and latency is unchanged.
- Everything else, on every link, is within the A/A spread between the #13 build and main, except a small rise in bytes while typing (LAN no reader 0.02 -> 0.03 Mbit/s; 20 Mbit/s 12 -> 14 kbit/s; cause not bracketed) and 0.04 dB in LAN typing settled quality. Both are negligible.
- #21, #22, #23, #24, #25, #26 and #27 show no effect on the first picture (bracket below). #16 adds arms that are off by default and is not separately measured.

## Method of the refresh

- **Arms**: `now` = starfold-video at 86703ce, first in each set so that the harness's A/A twin (`~aa`) is a starfold arm; `then` = the harness's `before` arm with f678862's binary; noVNC with #13's settings, plus `novnc-low` and `starfold-canvas` where #13 had them (canvas at main only). Binaries were built once and checked by sha256 in every run: now `c1b76508`, then `90a25761`, 8e32269 `87c0bb91`, 079c7ca `14d2119e`, 3e7f661 `db562a66`, 25698ac `5827969f`, 87d84e9 `e4618ff4`.
- **Unchanged from #13**: workloads, links, the 10 s window, rounds, settle points, noVNC config. With more arms a set no longer fits one VM's budget, so sets are split across VM runs (by workload, or 6+6 rounds with the rotation carried on). Every comparison stays interleaved within a VM run. noVNC rates come from no-reader runs; noVNC latency at 20 Mbit/s is slow-mode latency, as in #13. A run's build step was replaced by copying the prebuilt binary in, on the VM only.
- **Judging**: S = the larger end of the 95% bootstrap CI of twin minus now (`bench/vs/aa.py`). A difference counts only if its CI excludes 0 and it exceeds S. Cells marked "within A/A" in the noVNC column are close and claim nothing. Comparing a metric's noVNC and starfold medians across the table is still within one VM run; CPU is not compared across VM runs.
- **Admission, fixed in writing before the counted runs**: `lscpu`, one discarded warm-up CALIB, the counted CALIB, the workload, CALIB again. Drift over 15% flags the run. Steal over 2% drops a row. A run left with under half the rows of any cell also counts as flagged (added after p2-scroll-1, then applied to every run). A flagged run is replaced by an equal-sized one, up to 3 flags per set. Every counted run has the warm-up reading.
- **Not counted, no warm-up reading** (they predate the rule): p20-scroll runs 0 and 1 (old rule: run 0 9.3% OK, run 1 18.6% FLAGGED; not evaluable under the new rule), lanq-v and dense-0 (old rule 4.3% and 2.6%, not evaluable either).
- **Flagged and replaced**, all from other tenants' load on the shared host: dense-0w (CALIB 1.767 -> 3.141 s, 77.8%), lanc-ts (1.697 -> 2.666, 57.1%, late rows at up to 13.8% steal), lanc-ts2 (2.433 -> 1.906, 21.7%), wan-s (1.514 -> 2.191, 44.7%, steal to 7.3%), p20-scroll-1w (2.051 -> 2.474, 20.6%), lanq-vw (1.956 -> 2.300, 17.6%), sw-lat-1 (1.899 -> 2.329, 22.6%), sw-lat-1r (2.922 -> 2.012, 31.1%), br-q-2 (2.673 -> 1.808, 32.4%), and p2-scroll-1 (drift 9.8%, but 44 of 48 rows over 2% steal, up to 29.8%). No set reached 3 flags; lanc-ts and the stepwise latency part each counted on their third run.

## Results of the refresh

`then today` is f678862 on today's VMs; CALIB is before / after (drift).

### LAN

**LAN typing** (CALIB 1.74 / 2.00 s (14.7%); valid rows 5 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | starfold-canvas | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|
| key-to-paint p50 ms | 27 | 26 | 27 | 28 | 2.80 | -0.50 [-3.10, +1.20] | 35 | 24 | +8.30 [+6.10, +10.10] |
| key-to-paint p95 ms | 35 | 44 | 38 | 41 | 7.60 | +5.20 [-2.10, +9.90] | 58 | 54 | +19.40 [+13.60, +24.10] |
| Mbit/s | 0.03 | 0.03 | 0.03 | 0.03 | 0.00 | -0.00 [-0.01, -0.00] | 0.00 | 0.03 | -0.03 [-0.03, -0.02] |
| swap p50 ms | 72 | 46 | 59 | 48 | 20.95 | -13.30 [-25.37, -0.17] | 200 | 50 | +140.51 [+134.37, +157.20] |
| server % | 4 | 4 | 4 | 4 | 0.50 | +0.10 [-0.30, +0.60] | 3 | 4 | -0.60 [-1.20, -0.10] |
| browser % | 9 | 8 | 8 | 8 | 2.30 | +0.30 [-1.00, +2.50] | 9 | 37 | +0.50 [-2.40, +2.50] (within A/A) |

**LAN scroll** (CALIB 1.74 / 2.00 s (14.7%); valid rows 5 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | starfold-canvas | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|
| key-to-paint p50 ms | 34 | 32 | 32 | 33 | 4.30 | -0.50 [-2.80, +0.60] | 61 | 50 | +28.30 [+18.70, +32.80] |
| key-to-paint p95 ms | 49 | 49 | 47 | 43 | 8.30 | +2.00 [-1.10, +7.50] | 83 | 62 | +36.10 [+25.70, +42.20] |
| FPS | 29.9 | 29.9 | 30.0 | 30.0 | 0.40 | -0.09 [-0.30, +0.31] | 29.0 | 30.0 | -0.99 [-1.40, -0.60] |
| Mbit/s | 7.23 | 7.88 | 6.95 | 7.21 | 1.91 | +0.93 [-1.44, +2.03] | 90.38 | 7.88 | +83.43 [+80.42, +84.48] |
| swap p50 ms | 29 | 29 | 29 | 29 | 0.24 | -0.12 [-0.17, +0.05] | 33 | 33 | +4.43 [+4.30, +4.60] |
| server % | 26 | 24 | 25 | 24 | 4.90 | -0.70 [-3.29, +4.10] | 42 | 26 | +16.69 [+14.69, +20.90] |
| browser % | 26 | 24 | 24 | 25 | 5.69 | +0.30 [-2.90, +6.30] | 98 | 77 | +74.48 [+71.09, +85.19] |

**LAN video** (CALIB 1.82 / 1.83 s (0.8%); valid rows 5 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | starfold-canvas | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|
| key-to-paint p50 ms | 37 | 37 | 37 | 36 | 8.80 | -0.30 [-7.60, +7.30] | 64 | 56 | +27.40 [+20.10, +33.30] |
| key-to-paint p95 ms | 54 | 52 | 55 | 53 | 6.10 | -2.80 [-6.10, +4.90] | 91 | 71 | +36.40 [+24.30, +49.40] |
| FPS | 30.0 | 30.0 | 30.0 | 30.0 | 0.20 | +0.00 [-0.30, +0.19] | 29.6 | 30.0 | -0.40 [-1.40, -0.00] |
| Mbit/s | 14.47 | 14.43 | 14.47 | 14.43 | 0.05 | -0.04 [-0.05, +0.01] | 112.19 | 14.43 | +97.71 [+94.67, +98.52] |
| swap p50 ms | 29 | 28 | 28 | 29 | 0.37 | -0.08 [-0.16, +0.20] | 33 | 33 | +4.88 [+4.80, +5.17] |
| server % | 42 | 42 | 43 | 43 | 8.89 | -1.80 [-9.99, +1.90] | 40 | 46 | -3.00 [-11.19, +1.50] (within A/A) |
| browser % | 29 | 29 | 30 | 31 | 7.89 | -0.80 [-9.29, +2.20] | 137 | 81 | +107.16 [+97.37, +113.05] |

**LAN typing, no reader** (CALIB 1.77 / 1.88 s (6.3%); valid rows 5 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | starfold-canvas | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|
| Mbit/s | 0.02 | 0.02 | 0.03 | 0.03 | 0.00 | -0.01 [-0.01, -0.00] **real** | 0.00 | 0.03 | -0.02 [-0.03, -0.02] |
| swap p50 ms | 70 | 55 | 50 | 42 | 21.27 | +5.39 [-7.31, +14.54] | 200 | 47 | +150.15 [+139.56, +158.68] |
| server % | 4 | 4 | 4 | 4 | 0.90 | -0.40 [-1.00, +0.20] | 3 | 4 | -0.90 [-1.50, +0.20] (within A/A) |
| browser % | 7 | 6 | 7 | 6 | 2.70 | -0.90 [-2.40, +1.00] | 2 | 12 | -4.70 [-6.20, -3.60] |

**LAN scroll, no reader** (CALIB 1.77 / 1.88 s (6.3%); valid rows 5 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | starfold-canvas | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|
| Mbit/s | 7.82 | 7.41 | 6.81 | 7.56 | 1.80 | +0.61 [-0.74, +1.54] | 90.48 | 7.03 | +83.67 [+82.33, +85.36] |
| swap p50 ms | 29 | 29 | 29 | 29 | 0.16 | +0.07 [-0.11, +0.17] | 29 | 32 | +0.52 [-1.62, +1.83] (within A/A) |
| server % | 26 | 25 | 24 | 23 | 3.50 | +1.09 [-1.80, +4.00] | 41 | 24 | +17.39 [+15.10, +20.30] |
| browser % | 24 | 22 | 21 | 21 | 3.79 | +1.70 [-1.40, +3.80] | 93 | 47 | +71.87 [+61.60, +73.28] |

**LAN video, no reader** (CALIB 1.77 / 1.75 s (1.1%); valid rows 5 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | starfold-canvas | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|
| Mbit/s | 14.47 | 14.43 | 14.43 | 14.43 | 0.05 | +0.01 [-0.04, +0.04] | 111.81 | 14.47 | +97.39 [+96.34, +97.76] |
| swap p50 ms | 29 | 29 | 29 | 29 | 0.12 | -0.05 [-0.28, +0.06] | 33 | 30 | +4.09 [+3.81, +5.39] |
| server % | 41 | 40 | 39 | 40 | 2.61 | +0.38 [-2.23, +5.72] | 39 | 41 | -0.09 [-2.70, +1.81] (within A/A) |
| browser % | 27 | 26 | 26 | 27 | 1.50 | +0.39 [-0.92, +4.42] | 125 | 50 | +98.77 [+92.79, +106.86] |

### WAN 20 Mbit/s, 50 ms RTT

**Scroll rate, no reader (n=12)** (CALIB 1.69 / 1.77 s (5.0%); 1.76 / 1.79 s (2.2%); valid rows 12 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | noVNC - now |
|---|---|---|---|---|---|---|---|---|
| Mbit/s | 6.58 | 6.20 | 6.86 | 5.98 | 1.35 | -0.66 [-1.26, +0.35] | 17.28 | +10.42 [+7.38, +13.28] |
| swap p50 ms | 27 | 27 | 27 | 27 | 0.64 | +0.14 [-0.19, +0.63] | 182 | +155.33 [+126.76, +185.20] |

**Scroll rate, reader (n=12)** (CALIB 1.69 / 1.77 s (5.0%); 1.76 / 1.79 s (2.2%); valid rows now 11, twin 12, then 12, noVNC 12)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | noVNC - now |
|---|---|---|---|---|---|---|---|---|
| Mbit/s | 6.11 | 6.61 | 6.60 | 6.47 | 0.79 | +0.01 [-0.58, +0.82] | 14.34 | +7.74 [+7.31, +8.55] |
| swap p50 ms | 27 | 27 | 27 | 27 | 0.94 | -0.10 [-1.10, +0.19] | 216 | +189.28 [+188.28, +189.66] |

**Video rate, no reader (n=12)** (CALIB 1.79 / 1.87 s (4.5%); 1.76 / 1.88 s (6.7%); valid rows 12 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | noVNC - now |
|---|---|---|---|---|---|---|---|---|
| Mbit/s | 14.44 | 14.44 | 14.45 | 14.45 | 0.02 | -0.01 [-0.03, +0.01] | 15.40 | +0.94 [+0.91, +1.01] |
| swap p50 ms | 29 | 28 | 29 | 29 | 0.28 | -0.04 [-0.23, +0.14] | 244 | +215.71 [+215.46, +216.46] |

**Video rate, reader (n=12)** (CALIB 1.79 / 1.87 s (4.5%); 1.76 / 1.88 s (6.7%); valid rows 12 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | noVNC - now |
|---|---|---|---|---|---|---|---|---|
| Mbit/s | 14.44 | 14.45 | 14.45 | 14.44 | 0.03 | +0.01 [-0.02, +0.04] | 15.41 | +0.96 [+0.93, +0.98] |
| swap p50 ms | 28 | 28 | 28 | 28 | 0.15 | -0.07 [-0.26, +0.17] | 250 | +221.29 [+221.10, +221.49] |

Scroll, no reader: noVNC fast mode (>= 17.5 Mbit/s) in 6 of 12 runs.
Scroll, reader: noVNC fast mode (>= 17.5 Mbit/s) in 0 of 12 runs.
Video, no reader: noVNC fast mode (>= 17.5 Mbit/s) in 2 of 12 runs.
Video, reader: noVNC fast mode (>= 17.5 Mbit/s) in 0 of 12 runs.

**Typing** (CALIB 1.83 / 1.83 s (0.2%); valid rows now 5, twin 4, then 4, noVNC 4, noVNC low 5, canvas 5)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | novnc-low | starfold-canvas | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|---|
| key-to-paint p50 ms | 70 | 71 | 72 | 68 | 5.90 | -0.70 [-2.90, +4.05] | 86 | 89 | 78 | +14.25 [+12.05, +19.70] |
| key-to-paint p95 ms | 91 | 91 | 95 | 88 | 14.60 | -3.65 [-11.35, +7.85] | 111 | 111 | 106 | +16.50 [+8.80, +28.00] |
| Mbit/s | 0.01 | 0.01 | 0.01 | 0.01 | 0.00 | -0.00 [-0.00, -0.00] **real** | 0.00 | 0.00 | 0.01 | -0.01 [-0.01, -0.01] |
| swap p50 ms | 98 | 79 | 90 | 89 | 29.31 | -10.86 [-24.61, +26.96] | 200 | 200 | 101 | +109.52 [+88.92, +132.32] |

**Scroll** (CALIB 1.90 / 1.94 s (2.0%); valid rows 5 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | novnc-low | starfold-canvas | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|---|
| key-to-paint p50 ms | 88 | 92 | 90 | 89 | 5.80 | +2.20 [-1.80, +6.30] | 318 | 213 | 105 | +228.20 [+213.70, +251.40] |
| key-to-paint p95 ms | 156 | 141 | 149 | 150 | 24.80 | -8.40 [-35.40, +23.10] | 414 | 265 | 146 | +264.50 [+257.90, +283.30] |
| FPS | 28.4 | 28.3 | 28.8 | 28.4 | 1.30 | -0.50 [-1.10, +0.30] | 4.6 | 7.5 | 29.4 | -24.20 [-24.70, -23.70] |
| Mbit/s | 6.72 | 6.57 | 6.25 | 6.57 | 1.54 | +0.33 [-0.41, +2.09] | 14.20 | 10.82 | 6.01 | +7.96 [+7.22, +8.88] |
| swap p50 ms | 27 | 27 | 27 | 27 | 0.88 | -0.35 [-1.13, +0.32] | 216 | 133 | 33 | +189.33 [+188.87, +190.01] |
| server % | 27 | 25 | 24 | 26 | 3.11 | +1.10 [-0.70, +2.70] | 31 | 31 | 25 | +6.80 [+4.59, +8.50] |
| browser % | 31 | 25 | 24 | 26 | 15.69 | +0.89 [-1.50, +2.90] | 33 | 31 | 74 | +8.79 [+5.50, +11.30] (within A/A) |
| gaps > 100 ms | 4 | 2 | 2 | 2 | 6.00 | +0.00 [-6.00, +3.00] | 45 | 74 | 0 | +43.00 [+37.00, +45.00] |

**Video** (CALIB 1.77 / 1.74 s (1.3%); valid rows 5 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | novnc-low | starfold-canvas | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|---|
| key-to-paint p50 ms | 109 | 112 | 109 | 111 | 9.20 | +2.70 [-7.20, +6.70] | 389 | 182 | 128 | +279.60 [+247.10, +283.00] |
| key-to-paint p95 ms | 130 | 129 | 130 | 132 | 5.30 | -0.70 [-7.30, +5.50] | 482 | 236 | 146 | +352.50 [+329.40, +360.40] |
| FPS | 30.0 | 30.0 | 30.0 | 29.9 | 0.30 | +0.00 [-0.10, +0.40] | 4.1 | 8.5 | 30.0 | -25.90 [-25.99, -25.50] |
| Mbit/s | 14.47 | 14.45 | 14.45 | 14.44 | 0.03 | +0.01 [-0.03, +0.04] | 15.41 | 10.24 | 14.44 | +0.96 [+0.94, +0.98] |
| swap p50 ms | 29 | 29 | 28 | 28 | 0.32 | +0.27 [-0.36, +0.54] | 250 | 117 | 33 | +221.23 [+220.72, +221.50] |
| server % | 39 | 45 | 47 | 47 | 18.84 | -1.47 [-10.60, +12.42] | 16 | 18 | 47 | -30.06 [-39.19, -25.69] |
| browser % | 31 | 33 | 33 | 34 | 20.90 | +0.60 [-9.10, +15.39] | 32 | 40 | 83 | -1.20 [-10.89, +4.40] (within A/A) |
| gaps > 100 ms | 0 | 0 | 0 | 0 | 0.00 | +0.00 [+0.00, +0.00] | 40 | 83 | 0 | +40.00 [+39.00, +40.00] |

### WAN 2 Mbit/s, 50 ms RTT

**Scroll rate, no reader (n=12)** (CALIB 1.97 / 1.82 s (7.4%); 1.87 / 1.80 s (3.6%); valid rows now 12, twin 11, then 12, noVNC 12)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | noVNC - now |
|---|---|---|---|---|---|---|---|---|
| Mbit/s | 1.79 | 1.80 | 1.80 | 1.77 | 0.07 | -0.01 [-0.04, +0.02] | 1.93 | +0.13 [+0.09, +0.16] |
| swap p50 ms | 40 | 39 | 39 | 37 | 5.65 | -0.27 [-2.89, +2.82] | 1585 | +1546.17 [+1535.07, +1574.11] |

**Scroll rate, reader (n=12)** (CALIB 1.97 / 1.82 s (7.4%); 1.87 / 1.80 s (3.6%); valid rows now 12, twin 12, then 11, noVNC 12)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | noVNC - now |
|---|---|---|---|---|---|---|---|---|
| Mbit/s | 1.79 | 1.81 | 1.78 | 1.79 | 0.04 | +0.02 [-0.04, +0.04] | 1.92 | +0.13 [+0.09, +0.15] |
| swap p50 ms | 38 | 38 | 36 | 37 | 3.67 | +2.17 [-1.40, +3.90] | 1590 | +1553.46 [+1530.34, +1565.04] |

**Scroll latency (thin-scroll, n=5)** (CALIB 1.68 / 1.64 s (2.4%); valid rows 5 per arm)

| metric | #13 doc | then today | now | twin | A/A S | then - now | noVNC | novnc-low | noVNC - now |
|---|---|---|---|---|---|---|---|---|---|
| key-to-paint p50 ms | 306 | 330 | 363 | 354 | 107.00 | -33.00 [-131.10, +16.70] | 1728 | 892 | +1365.10 [+1254.00, +2865.10] |
| key-to-paint p95 ms | 526 | 488 | 664 | 694 | 232.20 | -176.10 [-348.20, +26.90] | 2855 | 1144 | +2190.60 [+1254.00, +3236.80] |
| keys seen per round | - | 27 | 24 | 27 | 6.00 | +3.00 [-1.00, +6.00] | 8 | 13 | -16.00 [-20.00, -13.00] |
| FPS | 12.5 | 12.5 | 11.5 | 11.4 | 1.10 | +1.00 [+0.00, +1.70] | 0.7 | 1.3 | -10.80 [-11.80, -10.20] |
| Mbit/s | 1.80 | 1.80 | 1.82 | 1.78 | 0.10 | -0.02 [-0.07, +0.04] | 1.92 | 1.85 | +0.10 [+0.07, +0.19] |
| swap p50 ms | 39 | 37 | 35 | 35 | 5.09 | +2.78 [-2.64, +7.67] | 1601 | 766 | +1566.22 [+1480.56, +1600.03] |

### Quality

**LAN typing** (PSNR dB / text SSIM, n: 5 per arm; CALIB 2.88 / 2.72 s (5.3%))

| point | #13 starfold | then today | now | twin | noVNC q7 | noVNC low | then - now PSNR (S) |
|---|---|---|---|---|---|---|---|
| +8 s | 49.1 / 0.9996 | 49.1 / 0.9996 | 49.1 / 0.9996 | 49.1 / 0.9996 | 43.2 / 0.9984 | 36.4 / 0.9918 | +0.04 [+0.04, +0.06] (S 0.02) **real** |

**LAN scroll** (PSNR dB / text SSIM, n: 5 per arm; CALIB 2.88 / 2.72 s (5.3%))

| point | #13 starfold | then today | now | twin | noVNC q7 | noVNC low | then - now PSNR (S) |
|---|---|---|---|---|---|---|---|
| +8 s | 48.3 / 0.9992 | 48.5 / 0.9992 | 48.7 / 0.9993 | 48.7 / 0.9992 | 42.4 / 0.9982 | 35.7 / 0.9909 | -0.24 [-0.30, +0.08] (S 0.38) |
| in motion | 41.0 / 0.9897 | 41.1 / 0.9954 | 43.3 / 0.9983 | 43.2 / 0.9983 | 19.0 / 0.6502 | 18.2 / 0.6751 | -2.23 [-5.43, +6.53] (S 8.66) |

**LAN video** (PSNR dB / text SSIM, n: 5 per arm; CALIB 1.66 / 1.78 s (7.7%))

| point | #13 starfold | then today | now | twin | noVNC q7 | noVNC low | then - now PSNR (S) |
|---|---|---|---|---|---|---|---|
| +8 s | 44.6 / 0.9963 | 44.7 / 0.9965 | 44.8 / 0.9965 | 44.7 / 0.9967 | 40.1 / 0.9743 | 38.9 / 0.9465 | -0.08 [-0.35, +0.06] (S 0.73) |
| in motion | 38.3 / 0.9795 | 38.3 / 0.9798 | 38.4 / 0.9796 | 38.9 / 0.9797 | 40.1 / 0.9744 | 38.8 / 0.9469 | -0.01 [-1.26, +0.56] (S 0.59) |

**2 Mbit/s scroll page** (PSNR dB / text SSIM, n: 5 per arm; CALIB 1.89 / 2.03 s (7.1%))

| point | #13 starfold | then today | now | twin | noVNC q7 | noVNC low | then - now PSNR (S) |
|---|---|---|---|---|---|---|---|
| +500 ms | 30.4 / 0.9532 | 30.0 / 0.9535 | 13.8 / 0.0275 | 14.0 / 0.0296 | 13.6 / -0.0010 | 13.7 / -0.0086 | +16.23 [+13.35, +17.08] (S 0.55) **real** |
| +1 s | 33.3 / 0.9788 | 33.5 / 0.9780 | 39.7 / 0.9959 | 34.8 / 0.9867 | 13.6 / -0.0010 | 15.0 / 0.1847 | -6.21 [-9.51, +19.57] (S 26.11) |
| +3 s | 33.3 / 0.9788 | 33.5 / 0.9780 | 45.5 / 0.9991 | 42.6 / 0.9981 | 42.6 / 0.9983 | 36.1 / 0.9915 | -11.95 [-13.03, -8.98] (S 4.38) **real** |
| +8 s | 33.3 / 0.9788 | 33.5 / 0.9780 | 48.7 / 0.9996 | 48.7 / 0.9996 | 42.6 / 0.9983 | 36.1 / 0.9915 | -15.22 [-15.89, -13.50] (S 0.33) **real** |
| in motion | 27.7 / 0.9032 | 29.9 / 0.9472 | 40.6 / 0.9932 | 38.1 / 0.9914 | 42.6 / 0.9983 | 35.7 / 0.9912 | -10.68 [-13.76, -7.11] (S 4.40) **real** |

**2 Mbit/s dense text page** (PSNR dB / text SSIM, n: 5 per arm; CALIB 2.06 / 1.87 s (9.1%); 1.75 / 1.83 s (4.3%))

| point | #13 starfold | then today | now | twin | noVNC q7 | noVNC low | then - now PSNR (S) |
|---|---|---|---|---|---|---|---|
| +4 s | - | 10.8 / -0.0136 | 20.2 / 0.8739 | 21.6 / 0.9130 | 9.3 / -0.0158 | 9.4 / -0.0163 | -9.39 [-10.58, -7.89] (S 3.23) **real** |
| +8 s | 10.8 / -0.01 | 12.3 / 0.3393 | 32.1 / 0.9931 | 33.2 / 0.9948 | 9.3 / -0.0158 | 27.8 / 0.9842 | -19.73 [-21.68, -2.54] (S 3.54) **real** |
| +12 s | 29.5 / 0.9883 | 29.5 / 0.9883 | 40.0 / 0.9991 | 40.1 / 0.9992 | 34.4 / 0.9971 | 27.8 / 0.9842 | -10.44 [-22.43, -10.32] (S 0.95) **real** |
| +32 s | 29.5 / 0.9883 | 30.7 / 0.9911 | 42.0 / 0.9995 | 42.0 / 0.9995 | 34.4 / 0.9971 | 27.8 / 0.9842 | -11.34 [-12.47, -11.28] (S 0.01) **real** |

Close starfold-vs-noVNC cells, within the A/A spread, so no difference is claimed:

- LAN typing, browser %: noVNC 9 vs now 8, S = 2.30
- LAN video, server %: noVNC 40 vs now 43, S = 8.89
- LAN typing, no reader, server %: noVNC 3 vs now 4, S = 0.90
- LAN scroll, no reader, swap p50 ms: noVNC 29 vs now 29, S = 0.16
- LAN video, no reader, server %: noVNC 39 vs now 39, S = 2.61
- Scroll, browser %: noVNC 33 vs now 24, S = 15.69
- Video, browser %: noVNC 32 vs now 33, S = 20.90

LAN scroll, noVNC in motion: 15-35 dB per round with torn shots (1-6 clean of 6) on a slow VM (CALIB ~2.8 s against ~1.8 s), against 42.5 dB in #13. This is the shot timing on that VM, not a change in noVNC; the in-motion column there says nothing about noVNC. The other points of that set agree with #13.

macOS, server side (`bench/vs/mac-encoders.sh`, n=5, Apple M6 VM, CI runs 42470161886 at f678862 and 42470164767 at main): bitrate is identical at both commits for every encoder and workload. Encode p50 differs by up to 2.4 ms (H.264 scroll 5.4 vs 7.7 ms, video 7.5 vs 8.5 ms). These are separate jobs with no interleaving and no A/A, so no change is claimed. Both are about half of #13's table (15.3 ms), which is the runner, not the code.

## Where the 2 Mbit/s changes came from

Pre-registered stepwise set: arms then, 8e32269 (#14 + #15), 079c7ca (adds #16-#19), 3e7f661 (#20), main and its twin, n=12 per arm over 3 VM runs each for picture and latency. Usable = PSNR >= 25 dB. **The picture rounds type nothing during motion, so #20's typing override is off in them; the latency rounds type throughout.**

| arm | usable +500 ms | usable +1 s | +500 ms PSNR / text SSIM | +8 s PSNR | in motion PSNR |
|---|---|---|---|---|---|
| f678862 | 10/12 | 12/12 | 28.0 / 0.924 | 33.0 | 27.7 |
| 8e32269 | 10/12 | 12/12 | 29.1 / 0.940 | 46.6 | 27.5 |
| 079c7ca | 12/12 | 12/12 | 28.2 / 0.922 | 47.9 | 27.9 |
| 3e7f661 | 1/12 | 12/12 | 13.9 / 0.009 | 48.8 | 38.3 |
| main | 1/12 | 5/12 | 14.0 / 0.030 | 48.1 | 39.4 |
| main, twin | 3/12 | 6/12 | 14.1 / 0.027 | 48.7 | 39.2 |

Real steps: +8 s at #14/#15 (+13.6 dB [+9.5, +16.9], S 0.99); +500 ms at #20 (-14.3 dB [-17.2, -12.1], S 10.3; text SSIM -0.913, S 0.53); in motion at #20 (+10.4 dB [+8.3, +11.3], S 3.2).

**This table suggests a second step between #20 and main at +1 s (12/12 against 11/24; PSNR -23.9 dB against S 22.7). It is not there.** A second pre-registered set (3e7f661, 25698ac, 87d84e9, main, twin; n=12, 3 VM runs, Fisher's exact test on the counts) found 3e7f661 usable at +1 s in 7/12, main in 7/12, the twin in 7/12, the others in 10/12 and 8/12. No step passes (smallest p 0.37), and PSNR, text SSIM, bytes by +500 ms and flow-blocked counts are all within the A/A. The stepwise 12/12 was on the high side. At +500 ms the second set's five arms, all from #20 on, were usable in 1-3 of 12, repeating the #20 result. At +2 s every arm of both sets is usable in all rounds. Across the two sets, builds from #20 on are usable at +1 s in 5-12 of 12 against 12/12 before; that comparison was not registered.

Latency, typing during scroll (n=12; sw-lat-1 twice replaced):

| arm | keys with own frame | rounds over 5% without | p95 answered | p95 censored | p50 |
|---|---|---|---|---|---|
| f678862 | 333/348 | 3/12 | 534 | 603 | 311 |
| 8e32269 | 334/346 | 2/12 | 545 | 570 | 332 |
| 079c7ca | 340/348 | 1/12 | 535 | 563 | 338 |
| 3e7f661 | 309/349 | 11/12 | 587 | >5000 | 325 |
| main | 301/349 | 11/12 | 669 | >5000 | 355 |
| main, twin | 317/350 | 9/12 | 602 | >5000 | 337 |

The censored p95 is judged by rank only, as registered: medians and the bootstrap take the upper middle value, so a censored value is never averaged, and A/A S = 0. On that rule the one real step is 079c7ca -> 3e7f661 (#20): its CI is wholly censored. **Main against f678862 and 3e7f661 against f678862 are not real: their CI is [0, inf], which includes 0.** The other steps are not real either. Answered-only p95 (S 165), p50 (S 71) and keys answered per round (#20 step -0.084, S 0.106) are within the A/A at every step, including now against then. So the thin-scroll n=5 table's slower-looking p95 is not shown to be a change.

**What the censored p95 means here.** Every key reached the source in every arm. An unanswered key had no displayed frame of its own within the 5 s cap, because the next key's tag replaced it first. It does not mean a 5 s wait. A pre-registered diagnostic (n=6, one VM run, builds instrumented on the VM only, so not comparable with the tables above) timed each key to the first displayed tag of that key or a later one. No key went unshown:

| arm | p50 | p95 | max | unanswered keys in the window's first 2 s |
|---|---|---|---|---|
| 079c7ca | 304 | 622 | 955 | 2/12 |
| 3e7f661 | 353 | 757 | 1048 | 9/15 |
| main | 407 | 974 | 1530 | 18/25 |
| main, twin | 350 | 954 | 1522 | 14/20 |

The table pools all of an arm's keys: at 2 Mbit/s a p95 of ~0.6 s becomes ~1 s, not 5 s (main 974 ms against 079c7ca 622 ms, +353 ms). The registered judgement is on each round's p95 instead, whose medians are main 1135, 3e7f661 670 and 079c7ca 576 ms. On those, main against 079c7ca is real (+559 ms, S 266). Main against 3e7f661 also passes the rule (+466 ms), but this is one VM run at n=6, and the n=12 latency set saw no such step, so no second step is claimed.

**Mechanism.** #20 budgets each frame for the rate frames are taken at, down to 10 fps, except within 2 s of a key. The instrumented builds confirm the override: after a window's first key, 100% of motion frames are budgeted at 30 fps in every arm. Before it, the builds from #20 on mostly run at 10 fps, with median frames of 21-26 KB against 10 KB at 079c7ca, falling to 8-12 KB right after the key. Of the unanswered keys, 68% (41/60) fall in the window's first 2 s, where 21% of keys are: the override takes effect at once, and keys wait behind what is already queued. After a stop, the first frame is 14-31 KB (median) before #20, encoded within 250 ms in 36/36 rounds, and 100-132 KB from #20 on (~0.4-0.5 s of the link), encoded by then in 37/96. That is over the 10 fps budget of 25 KB. Its QP is not exposed by any log or stats field and was not measured.

**The first picture does not trade against settled quality.** 079c7ca has both (47.9 dB settled, 12/12 at +500 ms), because they come from different PRs. The trade is between the first picture and #20's in-motion gain. Options, none implemented or measured:

- Cap the first frame after a stall at the size of a ladder pass. Aims at the 100+ KB frame, not at the wait behind the queue.
- Bound the queue by time on the link rather than by frames. Aims at the wait at a stop and at the first keys. Its cost to in-motion quality is unknown.
- Use the take-rate budget only above some bitrate. At 2 Mbit/s this is 079c7ca's measured behaviour, and it gives up ~10 dB in motion on that link. The threshold is unmeasured: 20 Mbit/s scroll shows no movement either way.
