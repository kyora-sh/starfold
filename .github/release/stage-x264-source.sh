#!/usr/bin/env bash
# stage-x264-source.sh <out dir>: the x264 source files expected-files.sh lists, into <out dir>,
# each with a .sha256. A file from third_party/x264 must be listed in both SHA256SUMS and
# SHA512SUMS and match both; the vcpkg port tarball is made from the port directory, whose files
# must match SHA256SUMS too, as a reproducible tar (sorted, fixed times and owners, gzip -n).
set -euo pipefail
out=${1:?usage: stage-x264-source.sh <out dir>}
here=$(cd "$(dirname "$0")" && pwd)
x264=$(cd "$here/../../third_party/x264" && pwd)
fail() {
	echo "::error::stage-x264-source.sh: $*"
	exit 1
}
check() { # check <path relative to third_party/x264>: it matches its line in both manifests
	local f=$1 sum
	for sum in 256 512; do
		line=$(awk -v f="$f" '$2 == f' "$x264/SHA${sum}SUMS")
		[ -n "$line" ] || fail "$f isn't in third_party/x264/SHA${sum}SUMS"
		(cd "$x264" && "sha${sum}sum" -c --quiet <<<"$line") || fail "$f doesn't match SHA${sum}SUMS"
	done
}
mkdir -p "$out"
port=vcpkg-2026.10.16-ports-x264
for f in $("$here/expected-files.sh" x264-source); do
	if [ "$f" = "$port.tar.gz" ]; then
		while IFS= read -r p; do
			check "$port/$p"
		done < <(cd "$x264/$port" && find . -type f | sed 's|^\./||' | sort)
		[ "$(awk -v d="$port/" 'index($2, d) == 1' "$x264/SHA256SUMS" | wc -l)" = "$(find "$x264/$port" -type f | wc -l)" ] ||
			fail "$port has files SHA256SUMS doesn't list, or the reverse"
		tar --sort=name --mtime='2026-10-16 00:00:00Z' --owner=0 --group=0 --numeric-owner \
			-cf - -C "$x264" "$port" | gzip -n >"$out/$f"
	else
		check "$f"
		cp "$x264/$f" "$out/$f"
	fi
	(cd "$out" && sha256sum "$f" >"$f.sha256")
done
ls -l "$out"
