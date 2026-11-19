#!/usr/bin/env bash
# x264-source.sh linux <dpkg arch> | macos | windows <vcpkg root>: fails unless the x264 this build
# links is the one whose source third_party/x264 holds (and the release attaches).
#   linux:   the installed libx264-dev:<arch> was built from the source package version in the .dsc.
#   macos:   Homebrew's x264 formula builds upstream commit b35605ac (the attached tarball's) with no
#            patches.
#   windows: vcpkg's x264 port is byte-identical to vcpkg-2026.10.16-ports-x264, the patch set
#            attached, and builds the same upstream commit.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
x264=$root/third_party/x264
fail() {
	echo "::error::x264 source check: $*"
	exit 1
}
commit=b35605ace3ddf7c1a5d67a2eb553f034aef41d55
case ${1:?usage: x264-source.sh linux <arch> | macos | windows <vcpkg root>} in
linux)
	arch=${2:?linux needs the dpkg architecture}
	want=$(sed -n 's/^Version: //p' "$x264"/x264_*.dsc)
	src=$(sed -n 's/^Source: //p' "$x264"/x264_*.dsc)
	got=$(dpkg-query -W -f '${source:Package} ${source:Version}' "libx264-dev:$arch")
	echo "libx264-dev:$arch from source $got; attached: $src $want"
	[ "$got" = "$src $want" ] || fail "libx264-dev:$arch is built from $got, the release attaches $src $want"
	;;
macos)
	json=$(brew info --json=v2 x264)
	read -r url revision patches local_patches < <(python3 -I -c '
import json, sys
f = json.load(sys.stdin)["formulae"][0]
s = f["urls"]["stable"]
print(s["url"], s["revision"], len(f.get("patches") or []), len(f.get("local_patch_checksums") or {}))
' <<<"$json")
	echo "Homebrew x264: $url at $revision, $patches patches, $local_patches local patches"
	[ "$url" = https://code.videolan.org/videolan/x264.git ] || fail "Homebrew builds x264 from $url"
	[ "$revision" = "$commit" ] || fail "Homebrew builds x264 commit $revision, the release attaches $commit"
	[ "$patches" = 0 ] && [ "$local_patches" = 0 ] || fail "Homebrew patches x264 ($patches + $local_patches patches)"
	grep -q "  x264-$commit.tar.gz\$" "$x264/SHA256SUMS" || fail "x264-$commit.tar.gz isn't in third_party/x264"
	;;
windows)
	vcpkg=${2:?windows needs the vcpkg root}
	echo "vcpkg at $(git -C "$vcpkg" rev-parse HEAD)"
	diff -r "$vcpkg/ports/x264" "$x264/vcpkg-2026.10.16-ports-x264" ||
		fail "vcpkg's x264 port differs from third_party/x264/vcpkg-2026.10.16-ports-x264"
	grep -q -x "set(ref $commit)" "$vcpkg/ports/x264/portfile.cmake" || fail "vcpkg's x264 port doesn't build $commit"
	;;
*) fail "unknown platform $1" ;;
esac
echo "x264 source matches the build"
