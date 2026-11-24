# Changelog

## 0.4.0

The client and the server are separate packages on npm and PyPI. `starfold` is the client alone,
with no binary and no `starfold` command; the server, its command and starting one from code are
`@starfold/server` on npm and `starfold-server` on PyPI, which depend on the client at the same
version. Staying on 0.3.2 changes nothing: starfold 0.3.2 on npm and on PyPI still carries the server. The crate on crates.io is still `starfold`, the server.

| 0.3.2                                                  | 0.4.0                                                |
| ------------------------------------------------------ | ---------------------------------------------------- |
| `import { startServer } from 'starfold'`               | `import { startServer } from '@starfold/server'`     |
| `npx starfold` / `npm i -g starfold` (the command)     | `npx @starfold/server` / `npm i -g @starfold/server` |
| `starfold.start_server(...)`                           | `starfold_server.start_server(...)`                  |
| `pip install starfold` (for the command, start_server) | `pip install starfold-server`                        |
| client imports (`Starfold`, `act`, the errors)         | unchanged                                            |
| `cargo install starfold`                               | unchanged                                            |

### npm

- `starfold` is the TypeScript client only: no dependencies, no `bin`, no optional dependencies; its exports are `Starfold`, `act`, the errors and the types. `startServer` and the rest of the server's API are not in it, with no stand-ins (a named import of one fails at link time and in `tsc`)
- New `@starfold/server`: the `starfold` command and `startServer`, `LocalServer` (whose `client()` is the `starfold` package's `Starfold`), `StartServerOptions`, `binaryPath`, `checkLibraries`, `missingLibraries`, `platformKey`, `serverPackage`, `LINUX_LIBRARIES_HINT`, `BinaryNotFoundError`, `MissingLibrariesError` and `ServerStartError` (still `StarfoldError`s, the client's), unchanged in behaviour; `STARFOLD_BIN` still works. It depends on `starfold` at exactly its own version and installs the binary from `@starfold/server-<platform>`, now its optional dependencies
- The four `@starfold/server-<platform>` packages are unchanged but for the version, and `@starfold/web` likewise

### PyPI

- `starfold` is the Python client alone: `Starfold`, `AsyncStarfold`, `act` and the errors, an
  sdist and a pure wheel, no binary and no `starfold` command.
- `starfold-server` (new, import `starfold_server`) is the server: the `starfold` command,
  `start_server()`, `LocalServer`, `binary_path()`, `check_libraries()`, `BinaryNotFoundError`,
  `MissingLibrariesError` and `ServerStartError` (still `starfold.StarfoldError`s), unchanged in
  behaviour; `LocalServer.client()` returns a `starfold.Starfold`. It requires exactly
  `starfold==0.4.0` and comes as four platform wheels (Linux x86_64 and aarch64, macOS arm64,
  Windows x86_64), each with that platform's binary; `STARFOLD_BIN` still names another one.
- `from starfold import start_server` (or any other moved name) raises an `ImportError` that
  names `starfold-server` and the new import.

## 0.3.2

The first release of kyora starfold. Prebuilt binaries for Linux (x86_64, aarch64), macOS (Apple
silicon) and Windows (x86_64) are attached to each release, each as a default build with libaom
distributed under MIT and a GPL-2.0-or-later build with x264; see [Install](README.md#install).

### Changes

- The default encoder is AV1 from libaom, built from source and linked statically; the default build is distributed under MIT. On a host with a hardware H.264 encoder that isn't experimental, the default stays that encoder. Viewers need a protocol 2 client: WebCodecs decodes AV1 in current Chrome, Edge and Firefox, and in Safari where the device decodes AV1; elsewhere the web client decodes it with dav1d-wasm on the CPU, which on 4 vCPUs holds 30 fps up to 1600x900 and about 26 fps at 1080p in Chromium (#94)
- x264 is an optional feature, `--features x264` (needs the x264 library, e.g. `libx264-dev`); a build with it is GPL-2.0-or-later. Where there is no hardware H.264 encoder its base stream (recordings, protocol 1, viewers without AV1) is H.264 from x264 and browsers that decode AV1 get AV1 from libaom; with `--encoder x264` every viewer gets x264 (#94)
- A build without an H.264 encoder closes a protocol 1 viewer with close code 4406 and a reason; build with x264, or use hardware H.264, to keep protocol 1 viewers (#94)
- libaom picks its own thread count unless `--encoder-threads` is given: 4 up to 1080p, 8 above, at most the CPUs, but never under 2 per encoder (so 2 on a 1-CPU host). 4K at 30 fps needs 8 or more CPUs; on fewer the server sends fewer frames, dropping captures instead of queueing them (#94)
- Recordings of an AV1 stream are AV1 in MP4: they play in current Chrome, Edge and Firefox, VLC, mpv and ffmpeg, and in Safari and QuickTime only on Apple devices with AV1 hardware decode (#94)
- A lower frame rate no longer makes libaom's picture blurrier: each frame is budgeted for the new frame rate (#94)

### Features

- `POST /input` for keyboard and mouse input, and the Python SDK (#2)
- Encoder capability registry with startup probes, and VideoToolbox H.264 and HEVC on macOS (#5)
- Codec negotiation between server and browser, encoder cohorts and switching codec mid-session (#6)
- `--encoder auto`: a hardware encoder by default where the host has one that isn't experimental (VideoToolbox); the experimental VA-API, NVENC, Media Foundation and Quick Sync encoders stream only when pinned with `--encoder`, and `--experimental-hw-probes` opens them at startup to probe them (#7)
- Decoding in a web worker, with a dav1d (WebAssembly) fallback for AV1 (#8)
- Recordings as MP4 segments that survive encoder changes (#9)
- A server warning when a recording response drops segments (#10)
- Refinement state kept per encoder cohort (#12)
- Benchmark harness against noVNC, with the November 2026 results (#13)
- Benchmark key-delivery check, and a count of the keys not painted as a frame of their own (#17)
- Software encoder arms for the benchmark: libvpx, libaom, SVT-AV1, Kvazaar and x265 (#16)
- Encoder frame budget at the frame rate actually taken, and the full rate while typing (#20)
- Hardware encoder probes run in child processes, so a crashing driver can't take the server down (#21)
- Experimental VA-API H.264, HEVC and AV1 encoders (#23)
- Opt-in 120 fps with hardware VideoToolbox (`--high-fps`, server side) (#22)
- Experimental NVENC H.264, HEVC and AV1 encoders (#25)
- Display rate, a time-based decoder queue and slow-viewer handling for `--high-fps` (#24)
- Experimental Media Foundation H.264, HEVC and AV1 encoders on Windows (#26)
- File endpoints hardened, with an explicit `--files-root`: on Linux and macOS paths resolve beneath the root by file descriptor; Windows checks a path and then opens it by name (#35)
- New names for the flags and environment variables inherited from upstream, with the old ones as deprecated aliases (#38)
- macOS: a flag to show the host cursor in the video (#47)
- Touch: a double tap double-clicks on the same spot (#50)
- x264 is the software H.264 encoder (#3, #36)
- x264 refines a frozen picture by QP (#41)
- Intel Quick Sync on Windows, in a helper process the server can kill (experimental) (#56)
- The Quick Sync helper logs the mode it opens its session with (#61)
- Embeddable web client: `mount()` with a per-connect preamble (#77)
- Recording is off by default (`--enable-recording`), with owner-only recording ids; `--read-only` is decided in one place (#79)
- The embeddable client works end to end over an exec-style byte bridge (#80)
- A unix socket listener, `--listen-unix` (Linux), and a framed WebSocket mode, `--framed` (#85)
- `starfold attach DIR`: a per-connection relay to the unix socket listener (Linux) (#88)
- A recording of an AV1 encode is AV1 in MP4 (`av01`), not only H.264; a codec change mid-recording starts a new segment, and a frame no MP4 sample can be made of ends it with `bad_frame` (#93)

### Fixes

- Shifted main-row keys no longer land on the keypad with the Mac key map (#4)
- Stalled viewers are detected by acknowledgement silence, not frame age (#14)
- Refinement converges on a still picture (#15)
- Refinement works through the padding wrapper for odd capture sizes (#18)
- Refinement ladder passes stay at the 200 ms size; only tail passes grow (#19)
- Web client reuses its canvas 2D context instead of requesting it again (#46)
- Windows loads the system DLLs the encoders open (NVENC, CUDA, Media Foundation) from System32, not from the executable's or the current directory (#54)
- VA-API encoding works on AMD radeonsi (#55)
- No filler data in VA-API constant-bitrate streams (#57)
- Quick Sync gives up through one path on every missed deadline (#58)
- VA-API takes the full encode entrypoint when the driver also offers low-power (#59)
- Quick Sync opens the full encoder first and LowPower only if that is refused (#60)
- `/stats` no longer lists a backend that is off as chosen (#64)
- An encoder that holds its bitrate cap gets exactly its share of the area (#65)
- A viewer that connects during a resize ends on the newest hello (#67)
- Hellos no longer offer a backend that is off (#66)
- A paint-over doubles the bitrate cap only on an encoder that doesn't hold it (#70)
- The embeddable client signals when there is no common codec, ends the session on a worker error and keeps the host page's focus (#81)
- A paste in the embeddable client that a blur drops sends nothing; unmounting after a worker error still reports a close (#83)
- Capture reads an Xvfb `-shmem` framebuffer directly only when `STARFOLD_DIRECT_FB=1` asks for it, and only its own X server's (#82)

### Documentation, benchmarks and CI

- Monorepo layout, fork attribution, CI and the upstream sync (#1)
- An A/A arm by default in the sets of the starfold-vs-noVNC benchmark (#27)
- Tests change the system's FUSE settings only when `STARFOLD_TEST_SYSTEM_FUSE=1` asks for it (#87)
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
