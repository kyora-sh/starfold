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

## What the published packages carry

- **`starfold` on crates.io** (the server's source), `MIT AND BSD-2-Clause AND OFL-1.1`: starfold's
  code (MIT, `LICENSE`) and, in `web/`, the web client it embeds, with dav1d.wasm (dav1d,
  BSD-2-Clause, `COPYING-dav1d`; wasi-libc, MIT with parts under BSD-2-Clause and CC0,
  `COPYING-wasi-libc`) and the Inter font (OFL-1.1, `inter-LICENSE.txt`). Built with its default
  features it links libaom from `starfold-aom-sys`; `--features x264` makes the binary
  GPL-2.0-or-later as below.
- **`starfold-aom-sys` on crates.io**, `MIT AND BSD-2-Clause AND BSD-3-Clause AND ISC`: its
  bindings and build script (MIT, `LICENSE`) and libaom's source (BSD-2-Clause and the Alliance for
  Open Media Patent License 1.0, `aom/LICENSE`, `aom/PATENTS`) with its `third_party/` code:
  fastfeat, highway, libwebm and libyuv (BSD-3-Clause), vector (MIT) and x86inc (ISC), each with
  its licence beside it.
- **`@starfold/web` on npm**, `MIT AND BSD-2-Clause`: the embeddable client (MIT) with
  dav1d.wasm (as above).
- **`starfold` on npm**, MIT: the TypeScript SDK. It carries no binary; it depends on the server
  package for its platform.
- **`starfold` on PyPI**: the sdist and the pure wheel (`py3-none-any`) are the Python client
  alone, MIT.
- **The npm server packages and `starfold-server`'s platform wheels on PyPI**: each holds the default binary of one
  platform's release archive with the archive's `LICENSE`, this file and `licenses/`, so it
  carries what [What each release binary carries](#what-each-release-binary-carries) lists for its
  platform. Each declares that list as one SPDX expression (`license` in the npm package's
  package.json, `License-Expression` in the wheel's metadata); per platform (archive target, npm
  package, wheel tag):
  - `x86_64-linux` (`@starfold/server-linux-x64-gnu`, `manylinux_2_39_x86_64`): `MIT AND BSD-2-Clause AND BSD-3-Clause AND ISC AND Apache-2.0 AND Unicode-3.0 AND Zlib AND OFL-1.1 AND CC0-1.0 AND LicenseRef-AOM-Patent-License-1.0 AND LGPL-2.1-or-later AND GPL-3.0-or-later WITH GCC-exception-3.1`
  - `aarch64-linux` (`@starfold/server-linux-arm64-gnu`, `manylinux_2_39_aarch64`): `MIT AND BSD-2-Clause AND BSD-3-Clause AND ISC AND Apache-2.0 AND Unicode-3.0 AND Zlib AND OFL-1.1 AND CC0-1.0 AND LicenseRef-AOM-Patent-License-1.0 AND LGPL-2.1-or-later AND GPL-3.0-or-later WITH GCC-exception-3.1`
  - `aarch64-macos` (`@starfold/server-darwin-arm64`, `macosx_11_0_arm64`): `MIT AND BSD-2-Clause AND BSD-3-Clause AND ISC AND Apache-2.0 AND Unicode-3.0 AND Zlib AND OFL-1.1 AND CC0-1.0 AND LicenseRef-AOM-Patent-License-1.0`
  - `x86_64-windows` (`@starfold/server-win32-x64`, `win_amd64`): `MIT AND BSD-2-Clause AND BSD-3-Clause AND ISC AND Apache-2.0 AND Unicode-3.0 AND Zlib AND OFL-1.1 AND CC0-1.0 AND LicenseRef-AOM-Patent-License-1.0 AND ZPL-2.1 AND GPL-3.0-or-later WITH GCC-exception-3.1`

  LGPL-2.1-or-later is glibc's startup files, `GPL-3.0-or-later WITH GCC-exception-3.1` GCC's
  startup files (Linux) and runtime libraries (Windows), ZPL-2.1 the MinGW-w64 runtime; the
  binary itself stays distributed under MIT (`starfold --version`).

## What each release binary carries

Each release has exactly two flavours of the binary for each platform. The archive's root holds
the binary, `LICENSE` (starfold's MIT licence), this file and `README.md`, and in an x264 archive
`COPYING` and `X264-SOURCE.txt`; `licenses/` holds every other text named here.

- **Default** (`starfold-<version>-<platform>`), distributed under MIT: starfold (MIT); libaom
  (BSD-2-Clause and the Alliance for Open Media Patent License 1.0) with fastfeat (BSD-3-Clause),
  vector (MIT) and, on x86_64, x86inc.asm (ISC); dav1d (BSD-2-Clause) and wasi-libc (MIT, with
  parts under BSD-2-Clause and CC0); Inter (OFL-1.1); the Rust crates in `licenses/rust-crates.txt`
  (MIT, Apache-2.0, BSD-3-Clause, ISC, Unicode-3.0 and Zlib, each crate under the first of these
  it offers) and the Rust standard library (`licenses/rust-std/`). By platform: on Linux, glibc's
  startup files (LGPL-2.1-or-later, with permission to link them into any program) and GCC's
  (GPL-3.0-or-later with the GCC Runtime Library Exception); on macOS, Opus (BSD-3-Clause); on
  Windows, Opus and libyuv (both BSD-3-Clause), the MinGW-w64 runtime (ZPL-2.1, with parts under
  MIT-style licences and in the public domain; `licenses/MinGW-w64-copyright.txt`), GCC's runtime
  libraries (GPL-3.0-or-later with the GCC Runtime Library Exception;
  `licenses/GCC-runtime-copyright.txt`), and what is transcribed from
  the Intel libvpl API headers (MIT). On Linux, libyuv, Opus and
  PulseAudio's client library are the system's shared libraries and are not part of the binary.
- **x264** (`starfold-x264-<version>-<platform>`), distributed under GPL-2.0-or-later as a combined
  work: all of the above plus x264 (GPL-2.0-or-later; `COPYING`, and `X264-SOURCE.txt` for where
  its source is). On Linux x264 is the system's shared library, `libx264.so.164`; on macOS and
  Windows it is linked in.
