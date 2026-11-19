#!/usr/bin/env bash
# package.sh <version> <archive name> <binary> <tar.gz|zip> <out dir> [licence file...]
# A release archive, <out dir>/<archive name>.<format>, and its .sha256: the binary, LICENSE,
# THIRD_PARTY.md, README.md, and licenses/ with the texts of what the binary embeds (dav1d,
# wasi-libc, Inter) plus the given licence files (the Rust crates' and statically linked
# native libraries'). An archive named starfold-x264-* also gets COPYING (GPL-2.0, from x264's
# source in third_party/x264) and X264-SOURCE.txt.
set -euo pipefail
version=${1:?usage: package.sh <version> <name> <binary> <tar.gz|zip> <out dir> [licence file...]}
name=$2 bin=$3 format=$4
mkdir -p "$5"
out=$(cd "$5" && pwd)
shift 5
root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
dir=$work/$name
mkdir -p "$dir/licenses"
cp "$bin" "$dir/"
cp "$root/LICENSE" "$root/THIRD_PARTY.md" "$root/README.md" "$dir/"
cp "$root/packages/web/dav1d/COPYING-dav1d" "$root/packages/web/dav1d/COPYING-wasi-libc" \
	"$root/packages/web/inter-LICENSE.txt" "$dir/licenses/"
for f in "$@"; do
	cp "$f" "$dir/licenses/"
done
case $name in
starfold-x264-*)
	src=$(echo "$root"/third_party/x264/x264_*.orig.tar.gz)
	tar -xOzf "$src" "$(tar -tzf "$src" | grep -m1 '^[^/]*/COPYING$')" >"$dir/COPYING"
	grep -q 'GNU GENERAL PUBLIC LICENSE' "$dir/COPYING"
	grep -q 'Version 2, June 1991' "$dir/COPYING"
	cat >"$dir/X264-SOURCE.txt" <<EOF
This binary links x264 (GPL-2.0-or-later), so it is distributed under GPL-2.0-or-later as a
combined work (COPYING); starfold's own source stays MIT (LICENSE).

The exact source of the x264 it links is attached to the same GitHub release,
https://github.com/kyora-sh/starfold/releases/tag/v${version}, and kept in the repository's
third_party/x264 (see its README.md for which file each platform's build uses).
EOF
	;;
esac
cd "$work"
case $format in
tar.gz) tar -czf "$out/$name.tar.gz" "$name" ;;
zip) zip -qr "$out/$name.zip" "$name" ;;
*)
	echo "package.sh: unknown format $format" >&2
	exit 1
	;;
esac
cd "$out"
if command -v sha256sum >/dev/null; then
	sha256sum "$name.$format" >"$name.$format.sha256"
else
	shasum -a 256 "$name.$format" >"$name.$format.sha256"
fi
cat "$name.$format.sha256"
