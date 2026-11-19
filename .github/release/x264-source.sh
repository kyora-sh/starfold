#!/usr/bin/env bash
# x264-source.sh linux <dpkg arch> | macos <libx264.a> | windows <vcpkg root>: fails unless the
# x264 this build links is the one whose source third_party/x264 holds (and the release attaches).
#   linux:   the installed libx264-dev:<arch> was built from the source package and version the
#            attached .dsc names, and the attached .orig.tar.gz and .debian.tar.xz match the .dsc's
#            own Checksums-Sha256.
#   macos:   <libx264.a>, the archive the build links (by its explicit path), belongs to Homebrew's
#            installed x264 keg, and that keg's formula is the one reviewed: its source is upstream
#            commit b35605ac and it has no patches (the formula file's checksum is pinned below, and
#            absent patch metadata fails rather than counting as none).
#   windows: vcpkg's x264 port is byte-identical to vcpkg-2026.10.16-ports-x264, the patch set
#            attached, and builds the same upstream commit.
#   macos and windows: the attached upstream tarball's SHA-512 is the one vcpkg's portfile pins
#   for commit b35605ac.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
x264=$root/third_party/x264
fail() {
	echo "::error::x264 source check: $*"
	exit 1
}
commit=b35605ace3ddf7c1a5d67a2eb553f034aef41d55
# Homebrew's Formula/x/x264.rb that builds $commit from upstream's git with no patches.
formula_sha256=4422aa0dc15b342f13ff629d935f377e0e36d84acbe3e2474270899753fd2fb5
tarball_sha512() {
	local want got
	want=$(sed -n 's/^ *SHA512 \([0-9a-f]\{128\}\)$/\1/p' "$x264/vcpkg-2026.10.16-ports-x264/portfile.cmake")
	if command -v sha512sum >/dev/null; then
		got=$(sha512sum "$x264/x264-$commit.tar.gz" | cut -d' ' -f1)
	else
		got=$(shasum -a 512 "$x264/x264-$commit.tar.gz" | cut -d' ' -f1)
	fi
	echo "x264-$commit.tar.gz sha512 $got; vcpkg's portfile pins $want"
	[ -n "$want" ] && [ "$got" = "$want" ] || fail "the attached x264-$commit.tar.gz isn't the archive vcpkg's portfile pins"
}
case ${1:?usage: x264-source.sh linux <arch> | macos <libx264.a> | windows <vcpkg root>} in
linux)
	arch=${2:?linux needs the dpkg architecture}
	dsc=$(echo "$x264"/x264_*.dsc)
	want=$(python3 -I "$root/.github/release/versions.py" dsc "$dsc")
	src=$(awk '/^Source: / { print $2; exit }' "$dsc")
	got=$(dpkg-query -W -f '${source:Package} ${source:Version}' "libx264-dev:$arch")
	echo "libx264-dev:$arch from source $got; attached: $src $want"
	[ "$got" = "$src $want" ] || fail "libx264-dev:$arch is built from $got, the release attaches $src $want"
	listed=$(python3 -I "$root/.github/release/versions.py" dsc-sha256 "$dsc") || fail "$dsc has no readable Checksums-Sha256"
	[ "$(cut -d' ' -f3 <<<"$listed" | sort)" = "$("$root/.github/release/expected-files.sh" x264-source | grep '^x264_' | grep -v '\.dsc$' | sort)" ] ||
		fail "the .dsc's Checksums-Sha256 names $(cut -d' ' -f3 <<<"$listed" | tr '\n' ' '), not exactly the attached Ubuntu files"
	while read -r sum size name; do
		[ -f "$x264/$name" ] && [ "$(sha256sum "$x264/$name" | cut -d' ' -f1)" = "$sum" ] &&
			[ "$(wc -c <"$x264/$name" | tr -d ' ')" = "$size" ] || fail "$name doesn't match the .dsc's Checksums-Sha256"
		echo "$name matches the .dsc"
	done <<<"$listed"
	;;
macos)
	lib=${2:?macos needs the libx264.a the build links}
	keg=$(cd "$(brew --prefix x264)" && pwd -P)
	[ "$(cd "$(dirname "$lib")" && pwd -P)/$(basename "$lib")" = "$keg/lib/libx264.a" ] ||
		fail "the build links $lib, not Homebrew's $keg/lib/libx264.a"
	read -r url revision patches local_patches checksum < <(brew info --json=v2 x264 | python3 -I -c '
import json, sys
f = json.load(sys.stdin)["formulae"][0]
s = f["urls"]["stable"]
print(s["url"], s["revision"], len(f["patches"]), len(f["local_patch_checksums"]), f["ruby_source_checksum"]["sha256"])
') || fail "Homebrew's x264 metadata lacks the source, patch or formula checksum fields"
	echo "Homebrew x264: $url at $revision, $patches patches, $local_patches local patches, formula sha256 $checksum; links $lib"
	[ "$url" = https://code.videolan.org/videolan/x264.git ] || fail "Homebrew builds x264 from $url"
	[ "$revision" = "$commit" ] || fail "Homebrew builds x264 commit $revision, the release attaches $commit"
	[ "$patches" = 0 ] && [ "$local_patches" = 0 ] || fail "Homebrew patches x264 ($patches + $local_patches patches)"
	[ "$checksum" = "$formula_sha256" ] || fail "Homebrew's x264 formula is $checksum, not the reviewed $formula_sha256"
	# The keg's archive must be the one in the formula's bottle: brew fetch checks the bottle against
	# the formula's sha256, and the archive inside it must equal the keg's, byte for byte (a keg
	# built or patched locally under the same version doesn't).
	brew fetch --force-bottle x264 >/dev/null || fail "can't fetch Homebrew's x264 bottle"
	bottle=$(brew --cache --force-bottle x264 2>/dev/null || brew --cache x264)
	in_bottle=$(tar -xOzf "$bottle" "x264/$(basename "$keg")/lib/libx264.a" | shasum -a 256 | cut -d' ' -f1)
	in_keg=$(shasum -a 256 "$keg/lib/libx264.a" | cut -d' ' -f1)
	echo "libx264.a: keg $in_keg, bottle $(basename "$bottle") $in_bottle"
	[ "$in_keg" = "$in_bottle" ] || fail "the keg's libx264.a isn't the one in Homebrew's x264 bottle"
	tarball_sha512
	;;
windows)
	vcpkg=${2:?windows needs the vcpkg root}
	echo "vcpkg at $(git -C "$vcpkg" rev-parse HEAD)"
	diff -r "$vcpkg/ports/x264" "$x264/vcpkg-2026.10.16-ports-x264" ||
		fail "vcpkg's x264 port differs from third_party/x264/vcpkg-2026.10.16-ports-x264"
	grep -q -x "set(ref $commit)" "$vcpkg/ports/x264/portfile.cmake" || fail "vcpkg's x264 port doesn't build $commit"
	tarball_sha512
	;;
*) fail "unknown platform $1" ;;
esac
echo "x264 source matches the build"
