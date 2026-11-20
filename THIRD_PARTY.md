# Third-party components

- **dav1d** (BSD-2-Clause) and **wasi-libc** (MIT, with parts under BSD-2-Clause and CC0), in
  `packages/web/dav1d/dav1d.wasm`, which the server embeds. Licences:
  [packages/web/dav1d/COPYING-dav1d](packages/web/dav1d/COPYING-dav1d) and
  [packages/web/dav1d/COPYING-wasi-libc](packages/web/dav1d/COPYING-wasi-libc).
- **Inter** (SIL Open Font License 1.1), the web client's font in `packages/web/inter.woff2`. Licence:
  [packages/web/inter-LICENSE.txt](packages/web/inter-LICENSE.txt).
- **Geist** (SIL Open Font License 1.1, The Geist Project Authors,
  [vercel/geist-font](https://github.com/vercel/geist-font)): the lettering in `assets/` uses its outlines.
- **x264** (GPL-2.0-or-later), linked only by a build with the `x264` feature. A binary built with
  x264 is distributed under GPL-2.0-or-later as a combined work; starfold's own source stays MIT. Each
  release attaches the exact x264 source of its Linux, macOS and Windows x264 builds, kept in
  [third_party/x264](third_party/x264/README.md).
- **libaom** (BSD-2-Clause, Copyright Alliance for Open Media, with the Alliance for Open Media
  Patent License 1.0), [aomedia.googlesource.com/aom](https://aomedia.googlesource.com/aom)
  v3.15.3, commit b141d9c45db236335351c8a4b2a14b3ade27c937: the AV1 encoder, built from that
  source (the git submodule `crates/starfold-aom-sys/aom`) and linked statically by the default
  build (the `libaom` feature), which stays under starfold's MIT licence. Licence and patent licence:
  [third_party/libaom/LICENSE](third_party/libaom/LICENSE) and
  [third_party/libaom/PATENTS](third_party/libaom/PATENTS); a binary distribution must carry both.
  The library starfold builds also contains three components libaom bundles in its
  `third_party/`: **fastfeat** (BSD-3-Clause, Edward Rosten,
  [LICENSE-fastfeat](third_party/libaom/LICENSE-fastfeat)), **vector** (MIT, Peter Goldsborough,
  [LICENSE-vector](third_party/libaom/LICENSE-vector)) and, in x86 builds, **x86inc.asm** (ISC,
  the x264 project, [LICENSE-x86inc](third_party/libaom/LICENSE-x86inc)).
- **Intel libvpl API headers** (MIT, Copyright Intel Corporation,
  [intel/libvpl](https://github.com/intel/libvpl) v2.17.0, commit
  d77f9195cf495b937631607333288fd917ae8939): the Windows Quick Sync encoder's struct layouts and
  constants are transcribed from them. Nothing of libvpl is vendored or linked; the oneVPL runtime
  it calls ships with the user's Intel graphics driver. Licence:
  [third_party/libvpl/LICENSE](third_party/libvpl/LICENSE).

`starfold --version` lists the encoders a binary includes and its licence.

## What each release binary carries

Each release has exactly two flavours of the binary for each platform. The archive's `licenses/`
holds every text named here.

- **Default** (`starfold-<version>-<platform>`), distributed under MIT: starfold (MIT); libaom
  (BSD-2-Clause and the Alliance for Open Media Patent License 1.0) with fastfeat (BSD-3-Clause),
  vector (MIT) and, on x86_64, x86inc.asm (ISC); dav1d (BSD-2-Clause) and wasi-libc (MIT, with
  parts under BSD-2-Clause and CC0); Inter (OFL-1.1); the Rust crates in `licenses/rust-crates.txt`
  (MIT, Apache-2.0, BSD-3-Clause, ISC, Unicode-3.0 and Zlib, each crate under the first of these
  it offers) and the Rust standard library (`licenses/rust-std/`). By platform: on Linux, glibc's
  startup files (LGPL-2.1-or-later, with permission to link them into any program) and GCC's
  (GPL-3.0-or-later with the GCC Runtime Library Exception); on macOS, Opus (BSD-3-Clause); on
  Windows, Opus and libyuv (both BSD-3-Clause), the MinGW-w64 runtime and GCC's runtime
  libraries (GPL-3.0-or-later with the GCC Runtime Library Exception), and what is transcribed from
  the Intel libvpl API headers (MIT). On Linux, libyuv, Opus and
  PulseAudio's client library are the system's shared libraries and are not part of the binary.
- **x264** (`starfold-x264-<version>-<platform>`), distributed under GPL-2.0-or-later as a combined
  work: all of the above plus x264 (GPL-2.0-or-later; `COPYING`, and `X264-SOURCE.txt` for where
  its source is). On Linux x264 is the system's shared library, `libx264.so.164`; on macOS and
  Windows it is linked in.
