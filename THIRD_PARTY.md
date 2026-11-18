# Third-party components

- **dav1d** (BSD-2-Clause) and **wasi-libc** (MIT, with parts under BSD-2-Clause and CC0), in
  `packages/web/dav1d/dav1d.wasm`, which the server embeds. Licences:
  [packages/web/dav1d/COPYING-dav1d](packages/web/dav1d/COPYING-dav1d) and
  [packages/web/dav1d/COPYING-wasi-libc](packages/web/dav1d/COPYING-wasi-libc).
- **Inter** (SIL Open Font License 1.1), the web client's font in `packages/web/inter.woff2`. Licence:
  [packages/web/inter-LICENSE.txt](packages/web/inter-LICENSE.txt).
- **Geist** (SIL Open Font License 1.1, The Geist Project Authors,
  [vercel/geist-font](https://github.com/vercel/geist-font)): the lettering in `assets/` uses its outlines.
- **x264** (GPL-2.0-or-later), linked by the default build (the `x264` feature). A binary built with
  x264 is distributed under GPL-2.0-or-later as a combined work; starfold's own source stays MIT. Each
  release attaches the exact x264 source of its Linux, macOS and Windows builds, kept in
  [third_party/x264](third_party/x264/README.md).
- **OpenH264** (BSD-2-Clause), built from source and linked by a build with the `openh264` feature,
  by the test builds (the tests' dev-dependency), whatever their features, and by an application
  that embeds starfold and enables openh264-sys2's `source` feature itself.
  `--no-default-features --features openh264` builds without x264, and that binary stays under
  starfold's MIT licence.
- **Cisco's OpenH264 binary** (BSD-2-Clause, with Cisco's AVC/H.264 patent licence conditions): not
  bundled. On 64-bit little-endian Linux, x86-64 and arm64, with glibc, the `starfold` binary built with the
  `openh264-cisco` feature loads it at startup from a path the user gives: it checks the file
  against the pinned sha256 and loads the checked bytes from a sealed memfd. A program that embeds
  the library, and any other target, loads nothing and has no Cisco OpenH264. starfold neither contains nor downloads it. Licence, verbatim from Cisco:
  [licenses/openh264-cisco-binary.txt](licenses/openh264-cisco-binary.txt).
- **Intel libvpl API headers** (MIT, Copyright Intel Corporation,
  [intel/libvpl](https://github.com/intel/libvpl) v2.17.0, commit
  d77f9195cf495b937631607333288fd917ae8939): the Windows Quick Sync encoder's struct layouts and
  constants are transcribed from them. Nothing of libvpl is vendored or linked; the oneVPL runtime
  it calls ships with the user's Intel graphics driver. Licence:
  [third_party/libvpl/LICENSE](third_party/libvpl/LICENSE).

`starfold --version` lists the encoders a binary includes and its licence.
