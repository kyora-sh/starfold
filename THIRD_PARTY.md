# Third-party components

- **dav1d** (BSD-2-Clause) and **wasi-libc** (MIT, with parts under BSD-2-Clause and CC0), in
  `packages/web/dav1d/dav1d.wasm`, which the server embeds. Licences:
  [packages/web/dav1d/COPYING-dav1d](packages/web/dav1d/COPYING-dav1d) and
  [packages/web/dav1d/COPYING-wasi-libc](packages/web/dav1d/COPYING-wasi-libc).
- **Inter** (SIL Open Font License 1.1), the web client's font in `packages/web/inter.woff2`; the
  wordmarks in `assets/` use its outlines. Licence: [packages/web/inter-LICENSE.txt](packages/web/inter-LICENSE.txt).
- **OpenH264** (BSD-2-Clause), built from source, in builds that include it.
- **x264** (GPL-2.0-or-later), in builds with the `x264` feature. A binary built with x264 is
  distributed under GPL-2.0-or-later as a combined work; starfold's own source stays MIT.

`starfold --version` lists the encoders a binary includes.
