#!/usr/bin/env bash
# expected-files.sh <version> <flavour...>: every file a release of <version> attaches besides x264's
# source, one per line: each flavour's archive for each target and its .sha256, the Python SDK's
# wheel and sdist and the embeddable client's tarball, each with its .sha256. The targets are listed
# here only; package.sh refuses an archive name this list lacks, and the release job refuses a set of
# files that differs from it.
set -euo pipefail
version=${1:?usage: expected-files.sh <version> <flavour...>}
shift
[ $# -gt 0 ] || {
	echo "expected-files.sh: no flavours" >&2
	exit 1
}
targets=(x86_64-linux:tar.gz aarch64-linux:tar.gz aarch64-macos:tar.gz x86_64-windows:zip)
for flavour in "$@"; do
	for t in "${targets[@]}"; do
		echo "$flavour-$version-${t%%:*}.${t#*:}"
		echo "$flavour-$version-${t%%:*}.${t#*:}.sha256"
	done
done
for f in "starfold-$version-py3-none-any.whl" "starfold-$version.tar.gz" "starfold-web-embed-$version.tgz"; do
	echo "$f"
	echo "$f.sha256"
done
