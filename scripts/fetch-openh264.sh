#!/usr/bin/env bash
# Development and benchmarks only, never part of a release: fetches Cisco's OpenH264 2.6.0
# for this machine from Cisco's server, checks it against the sha256 starfold pins
# (PINS in crates/starfold-server/src/encoder/cisco.rs) and prints its path, for
# --openh264-lib or STARFOLD_OPENH264_LIB. usage: scripts/fetch-openh264.sh [dir]
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
dir=${1:-${XDG_CACHE_HOME:-$HOME/.cache}/starfold}
case "$(uname -s)-$(uname -m)" in
Linux-x86_64) f=libopenh264-2.6.0-linux64.8.so ;;
Linux-aarch64 | Linux-arm64) f=libopenh264-2.6.0-linux-arm64.8.so ;;
*) echo "fetch-openh264.sh: starfold loads Cisco's OpenH264 only on Linux x86-64 and arm64, not $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
want=$(grep -B1 "^        \"$f\",$" "$here/crates/starfold-server/src/encoder/cisco.rs" | head -1 | tr -d ' ",')
[ ${#want} -eq 64 ] || { echo "fetch-openh264.sh: no pinned sha256 for $f" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
mkdir -p "$dir"
out=$dir/$f
if [ ! -f "$out" ] || [ "$(sha "$out")" != "$want" ]; then
	tmp=$(mktemp "$dir/.$f.XXXXXX")
	trap 'rm -f "$tmp" "$tmp.bz2"' EXIT
	curl -fsSL --retry 3 -o "$tmp.bz2" "https://ciscobinary.openh264.org/$f.bz2"
	bunzip2 -c "$tmp.bz2" > "$tmp"
	got=$(sha "$tmp")
	[ "$got" = "$want" ] || { echo "fetch-openh264.sh: $f has sha256 $got, not the pinned $want" >&2; exit 1; }
	chmod 0644 "$tmp"
	mv "$tmp" "$out"
fi
echo "$out"
