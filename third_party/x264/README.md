# x264 source

starfold binaries link x264, which is GPL-2.0-or-later, so they are GPL-2.0-or-later as a combined
work. These files are the exact x264 source of each platform's build, kept here so that every
release can attach them (GPL source offer) without fetching anything. Nothing builds from them.

| Platform | x264 | Files | Fetched from |
| --- | --- | --- | --- |
| Linux | Ubuntu 24.04 `x264 2:0.164.3108+git31e19f9-1` (r3108, upstream `31e19f92f00c7003fa115047ce50978bc98c3a0d`) | `x264_0.164.3108+git31e19f9.orig.tar.gz`, `x264_0.164.3108+git31e19f9-1.debian.tar.xz`, `x264_0.164.3108+git31e19f9-1.dsc` | https://archive.ubuntu.com/ubuntu/pool/universe/x/x264/ |
| Windows (vcpkg 2026.10.16), macOS (Homebrew) | upstream r3222, `b35605ace3ddf7c1a5d67a2eb553f034aef41d55` | `x264-b35605ace3ddf7c1a5d67a2eb553f034aef41d55.tar.gz` | https://code.videolan.org/videolan/x264/-/archive/b35605ace3ddf7c1a5d67a2eb553f034aef41d55/x264-b35605ace3ddf7c1a5d67a2eb553f034aef41d55.tar.gz |
| Windows (vcpkg) | vcpkg's x264 port at tag 2026.10.16 (the patches vcpkg applies) | `vcpkg-2026.10.16-ports-x264/` | https://github.com/microsoft/vcpkg/tree/2026.10.16/ports/x264 |

All fetched on 2026-11-09. The hashes in `SHA256SUMS` and `SHA512SUMS` were checked against
independent pins: the Ubuntu files against the `.dsc` (itself from the archive's signed index), and the
r3222 archive's SHA-512 against the one in vcpkg's `portfile.cmake`. Homebrew builds the same r3222
commit unpatched. CI checks every hash on every run.
