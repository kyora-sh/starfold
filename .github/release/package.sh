#!/usr/bin/env bash
# package.sh <version> <archive name> <binary> <tar.gz|zip> <out dir> [licence file or dir...]
# A release archive, <out dir>/<archive name>.<format>, and its .sha256. RELEASE_FLAVOURS (the
# flavour names, space-separated) must name the archive's flavour, and expected-files.sh must list
# the archive's name.
#
# The archive holds the binary, LICENSE, THIRD_PARTY.md, README.md, and licenses/ with the files
# notices.list names for it plus the given licence files and directories (the Rust crates', Rust's
# standard library's and the statically linked native libraries'). An x264 archive (starfold-x264-*)
# also gets COPYING (GPL-2.0, from x264's source in third_party/x264) and X264-SOURCE.txt.
#
# The archive must hold every notice its platform and flavour need (the list below and
# notices.list); a missing one fails, as does a file THIRD_PARTY.md links that notices.list lacks.
set -euo pipefail
version=${1:?usage: package.sh <version> <name> <binary> <tar.gz|zip> <out dir> [licence file or dir...]}
name=$2 bin=$3 format=$4
mkdir -p "$5"
out=$(cd "$5" && pwd)
shift 5
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
fail() {
	echo "::error::package.sh: $*"
	exit 1
}
# shellcheck disable=SC2086
expected=$("$here/expected-files.sh" "$version" ${RELEASE_FLAVOURS:?set RELEASE_FLAVOURS})
grep -q -x -F "$name/$name.$format" <<<"$expected" || fail "$name.$format isn't one of the release's files (expected-files.sh)"
case $name in
*-linux) platform=linux ;;
*-macos) platform=macos ;;
*-windows) platform=windows ;;
*) fail "no platform in $name" ;;
esac
case $name in
starfold-x264-*) x264=1 ;;
*) x264=0 ;;
esac

for link in $(grep -o -E '\]\([^)#]+\)' "$root/THIRD_PARTY.md" | sed 's/^](//; s/)$//' | grep -v -E '^[a-z]+:'); do
	awk -v l="$link" '$1 == l { f = 1 } END { exit !f }' "$here/notices.list" || fail "THIRD_PARTY.md links $link, which notices.list doesn't name"
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
dir=$work/$name
mkdir -p "$dir/licenses"
cp "$bin" "$dir/"
cp "$root/LICENSE" "$root/THIRD_PARTY.md" "$root/README.md" "$dir/"
required=()
while read -r path as where; do
	case $path in '#'* | '') continue ;; esac
	case $where in all | x264 | linux | macos | windows) ;; *) fail "notices.list: '$where' for $path isn't all, x264, linux, macos or windows" ;; esac
	[ -s "$root/$path" ] || fail "notices.list names $path, which is missing or empty"
	if [ "$where" = all ] || { [ "$where" = x264 ] && [ $x264 = 1 ]; } || [ "$where" = "$platform" ]; then
		cp "$root/$path" "$dir/licenses/$as"
		required+=("licenses/$as")
	fi
done <"$here/notices.list"
for f in "$@"; do
	cp -R "$f" "$dir/licenses/"
done

# What each binary compiles in, by platform and flavour: the Rust crates and standard library
# everywhere (std's COPYRIGHT and every licence text it refers to); OpenH264 without x264; Opus
# statically on macOS and Windows; on Linux glibc's and GCC's startup and runtime objects; on
# Windows libyuv, the MinGW-w64 runtime and GCC's runtime libraries. GCC's are GPL-3.0 with the GCC
# Runtime Library Exception, glibc's LGPL-2.1 with its own linking permission.
required+=(licenses/rust-crates.txt licenses/rust-std/COPYRIGHT-library.html)
for f in Apache-2.0 BSD-2-Clause CC-BY-SA-4.0 GCC-exception-3.1 GPL-2.0-only GPL-3.0-or-later ISC LLVM-exception MIT NCSA OFL-1.1 Unicode-3.0; do
	required+=("licenses/rust-std/$f.txt")
done
if [ $x264 = 1 ]; then
	src=$(echo "$root"/third_party/x264/x264_*.orig.tar.gz)
	tar -xOzf "$src" "$(tar -tzf "$src" | grep -m1 '^[^/]*/COPYING$')" >"$dir/COPYING"
	grep -q 'GNU GENERAL PUBLIC LICENSE' "$dir/COPYING" && grep -q 'Version 2, June 1991' "$dir/COPYING" ||
		fail "x264's COPYING isn't the GPL-2.0"
	cat >"$dir/X264-SOURCE.txt" <<EOF
This binary links x264 (GPL-2.0-or-later), so it is distributed under GPL-2.0-or-later as a
combined work (COPYING); starfold's own source stays MIT (LICENSE).

The exact source of the x264 it links is attached to the same GitHub release,
https://github.com/kyora-sh/starfold/releases/tag/v${version}, and kept in the repository's
third_party/x264 (licenses/x264-source-README.md says which file each platform's build uses).
EOF
	required+=(COPYING X264-SOURCE.txt)
else
	required+=(licenses/OpenH264-LICENSE.txt)
fi
case $platform in
linux) required+=(licenses/glibc-copyright.txt licenses/LGPL-2.1.txt licenses/GCC-runtime-copyright.txt licenses/GPL-3.0.txt) ;;
macos) required+=(licenses/Opus-COPYING.txt) ;;
windows) required+=(licenses/Opus-COPYING.txt licenses/libyuv-LICENSE.txt licenses/MinGW-w64-copyright.txt
	licenses/GCC-runtime-copyright.txt licenses/GPL-3.0.txt) ;;
esac

for f in "${required[@]}"; do
	[ ! -e "$dir/$f" ] || [ -s "$dir/$f" ] || fail "$f is empty"
done
cd "$work"
# zip adds to an archive that exists; each archive starts from nothing.
rm -f "$out/$name.$format" "$out/$name.$format.sha256"
case $format in
tar.gz)
	tar -czf "$out/$name.tar.gz" "$name"
	listing=$(tar -tzf "$out/$name.tar.gz")
	;;
zip)
	zip -qr "$out/$name.zip" "$name"
	listing=$(unzip -Z1 "$out/$name.zip")
	;;
*) fail "unknown format $format" ;;
esac
missing=0
for f in "${required[@]}"; do
	if ! grep -q -x -F "$name/$f" <<<"$listing"; then
		echo "::error::$name.$format lacks $f"
		missing=1
	fi
done
if [ $missing = 1 ]; then
	rm -f "$out/$name.$format"
	exit 1
fi
echo "$name.$format holds all ${#required[@]} required notices"
cd "$out"
if command -v sha256sum >/dev/null; then
	sha256sum "$name.$format" >"$name.$format.sha256"
else
	shasum -a 256 "$name.$format" >"$name.$format.sha256"
fi
cat "$name.$format.sha256"
