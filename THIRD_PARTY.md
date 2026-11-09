# Third-party components

- **dav1d** (BSD-2-Clause) and **wasi-libc** (MIT, with parts under BSD-2-Clause and CC0), in
  `packages/web/dav1d/dav1d.wasm`, which the server embeds. Licences:
  [packages/web/dav1d/COPYING-dav1d](packages/web/dav1d/COPYING-dav1d) and
  [packages/web/dav1d/COPYING-wasi-libc](packages/web/dav1d/COPYING-wasi-libc).
- **Inter** (SIL Open Font License 1.1), the web client's font in `packages/web/inter.woff2`.
  Licence: [packages/web/inter-LICENSE.txt](packages/web/inter-LICENSE.txt).
- **x264** (GPL-2.0-or-later), linked by the default build (the `x264` feature). A binary built with
  x264 is distributed under GPL-2.0-or-later as a combined work; starfold's own source stays MIT. Each
  release attaches the exact x264 source of its Linux, macOS and Windows builds, kept in
  [third_party/x264](third_party/x264/README.md).
- **OpenH264** (BSD-2-Clause), built from source and linked only by a build with the `openh264`
  feature. `--no-default-features --features openh264` builds without x264.
