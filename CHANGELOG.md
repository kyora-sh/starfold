# Changelog

## Unreleased

The first release of kyora starfold. Prebuilt binaries for Linux (x86_64, aarch64), macOS (Apple
silicon) and Windows (x86_64) are attached to each release, each as an MIT build with OpenH264 and a
GPL-2.0-or-later build with x264; see [Install](README.md#install).

### Features

- `POST /input` for keyboard and mouse input, and the Python SDK (#2)
- Encoder capability registry with startup probes, and VideoToolbox H.264 and HEVC on macOS (#5)
- Codec negotiation between server and browser, encoder cohorts and switching codec mid-session (#6)
- `--encoder auto`: hardware encoding by default where the host has it (#7)
- Decoding in a web worker, with a dav1d (WebAssembly) fallback for AV1 (#8)
- Recordings as MP4 segments that survive encoder changes (#9)
- A server warning when a recording response drops segments (#10)
- Refinement state kept per encoder cohort (#12)
- Benchmark harness against noVNC, with the November 2026 results (#13)
- Benchmark key-delivery check; a key press is never painted as a frame of its own (#17)
- Software encoder arms for the benchmark: libvpx, libaom, SVT-AV1, Kvazaar and x265 (#16)
- Encoder frame budget at the frame rate actually taken, and the full rate while typing (#20)
- Hardware encoder probes run in child processes, so a crashing driver can't take the server down (#21)
- Experimental VA-API H.264, HEVC and AV1 encoders (#23)
- Opt-in 120 fps with hardware VideoToolbox (`--high-fps`, server side) (#22)
- Experimental NVENC H.264, HEVC and AV1 encoders (#25)
- Display rate, a time-based decoder queue and slow-viewer handling for `--high-fps` (#24)
- Experimental Media Foundation H.264, HEVC and AV1 encoders on Windows (#26)
- File endpoints hardened: paths resolve beneath the root by file descriptor, with an explicit `--files-root` (#35)
- New names for the flags and environment variables inherited from upstream, with the old ones as deprecated aliases (#38)
- macOS: a flag to show the host cursor in the video (#47)
- Touch: a double tap double-clicks on the same spot (#50)
- x264 is the default H.264 encoder again; OpenH264 is optional and gives an MIT-only build (#3, #36)
- x264 refines a frozen picture by QP (#41)
- Intel Quick Sync on Windows, in a helper process the server can kill (experimental) (#56)
- The Quick Sync helper logs the mode it opens its session with (#61)
- Embeddable web client: `mount()` with a per-connect preamble (#77)
- Recording is off by default and recording ids are visible only to their owner; `--read-only` is decided in one place (#79)
- The embeddable client works end to end over an exec-style byte bridge (#80)

### Fixes

- Shifted main-row keys no longer land on the keypad with the Mac key map (#4)
- Stalled viewers are detected by acknowledgement silence, not frame age (#14)
- Refinement converges on a still picture (#15)
- Refinement works through the padding wrapper for odd capture sizes (#18)
- Refinement ladder passes stay at the 200 ms size; only tail passes grow (#19)
- Web client reuses its canvas 2D context instead of requesting it again (#46)
- Windows loads its DLLs from System32 only (#54)
- VA-API encoding works on AMD radeonsi (#55)
- No filler data in VA-API constant-bitrate streams (#57)
- Quick Sync gives up through one path on every missed deadline (#58)
- VA-API takes the full encode entrypoint when the driver also offers low-power (#59)
- Quick Sync opens the full encoder first and LowPower only if that is refused (#60)
- `/stats` no longer lists a backend that is off as chosen (#64)
- An encoder that holds its bitrate cap gets exactly its share of the area (#65)
- A viewer that connects during a resize ends on the newest hello (#67)
- Hellos no longer offer a backend that is off (#66)
- OpenH264's forced intra frame gets the layer id it expects (#73)
- A paint-over doubles the bitrate cap only on an encoder that doesn't hold it (#70)
- The embeddable client signals when there is no common codec, ends the session on a worker error and keeps the host page's focus (#81)
- A paste in the embeddable client that a blur drops sends nothing; unmounting after a worker error still reports a close (#83)

### Documentation, benchmarks and CI

- Monorepo layout, fork attribution, CI and the upstream sync (#1)
- An A/A arm in every benchmark set by default (#27)
- vcpkg download cache and bounded retries in the Windows cross-check (#30)
- Benchmark results refreshed against main (#29)
- Steal share in the benchmark's quality rows (#31)
- The README, logo, social card and licensing notes (#34)
- Neutral provenance comments and results wording (#39)
- A File transfer section and the supported-versions note (#40)
- Package installs in CI fail fast with a timeout and retry once (#42)
- A blocked upstream sync ends green with a warning (#43)
- An upstream skip list with skipped and held commits (#44)
- One upstream commit per sync run (#45)
- Synced commit messages without upstream's HTML, and the sync status by kind (#49)
- Upstream sync bookkeeping (#51, #52)
- Test fakes for the Quick Sync helper read their Open request before replying (#62)
- A CI job for the SVT-AV1 and x265 benchmark arms (#68)
- VA-API docs say when low-power is taken (#69)
