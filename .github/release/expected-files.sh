#!/usr/bin/env bash
# The one list of what a release attaches, and the only place the targets are named.
#
#   expected-files.sh <version> <flavour...>
#     every file, as <artifact directory>/<file>, one per line: each flavour's archive for each target
#     and its .sha256 (directory: the archive's name), the Python SDK's wheel and sdist
#     (starfold-python-<version>), the embeddable client's tarball (starfold-web-embed-<version>)
#     and x264's source (x264-source), each file with a .sha256. The release attaches exactly these.
#   expected-files.sh archive <version> <flavour> <target>
#     the archive file name of that flavour and target; fails for a target not listed here.
#   expected-files.sh x264-source
#     x264's source files from third_party/x264, and the vcpkg port tarball made from it.
set -euo pipefail
targets=(x86_64-linux:tar.gz aarch64-linux:tar.gz aarch64-macos:tar.gz x86_64-windows:zip)
x264_source=(
	x264_0.164.3108+git31e19f9.orig.tar.gz
	x264_0.164.3108+git31e19f9-1.debian.tar.xz
	x264_0.164.3108+git31e19f9-1.dsc
	x264-b35605ace3ddf7c1a5d67a2eb553f034aef41d55.tar.gz
	vcpkg-2026.10.16-ports-x264.tar.gz
)
fail() {
	echo "expected-files.sh: $*" >&2
	exit 1
}
archive() {
	local t
	for t in "${targets[@]}"; do
		if [ "${t%%:*}" = "$3" ]; then
			echo "$2-$1-$3.${t#*:}"
			return
		fi
	done
	fail "no target $3"
}
case ${1:?usage: expected-files.sh <version> <flavour...> | archive <version> <flavour> <target> | x264-source} in
archive)
	[ $# = 4 ] || fail "usage: expected-files.sh archive <version> <flavour> <target>"
	archive "$2" "$3" "$4"
	;;
x264-source)
	printf '%s\n' "${x264_source[@]}"
	;;
*)
	version=$1
	shift
	[ $# -gt 0 ] || fail "no flavours"
	for flavour in "$@"; do
		for t in "${targets[@]}"; do
			name=$(archive "$version" "$flavour" "${t%%:*}")
			echo "${name%."${t#*:}"}/$name"
			echo "${name%."${t#*:}"}/$name.sha256"
		done
	done
	for f in "starfold-$version-py3-none-any.whl" "starfold-$version.tar.gz"; do
		echo "starfold-python-$version/$f"
		echo "starfold-python-$version/$f.sha256"
	done
	echo "starfold-web-embed-$version/starfold-web-embed-$version.tgz"
	echo "starfold-web-embed-$version/starfold-web-embed-$version.tgz.sha256"
	for f in "${x264_source[@]}"; do
		echo "x264-source/$f"
		echo "x264-source/$f.sha256"
	done
	;;
esac
