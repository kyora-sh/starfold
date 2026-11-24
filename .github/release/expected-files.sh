#!/usr/bin/env bash
# The one list of what a release attaches, and the only place the targets are named.
#
#   expected-files.sh <version> <flavour...>
#     every file, as <artifact directory>/<file>, one per line: each flavour's archive for each target
#     and its .sha256 (directory: the archive's name), the Python SDK's wheel and sdist
#     (starfold-python-<version>), the embeddable client's tarball (starfold-web-embed-<version>),
#     the npm packages that carry the default binary and the SDK pinned to them (starfold-npm-<version>),
#     the platform wheels (starfold-wheels-<version>; the Linux ones only with LINUX_WHEELS=1, the
#     default) and x264's source (x264-source), each file with a .sha256. The release attaches exactly
#     these.
#   expected-files.sh archive <version> <flavour> <target>
#     the archive file name of that flavour and target; fails for a target not listed here.
#   expected-files.sh native <version>
#     one line per target: <target> <npm package suffix> <archive> <npm tarball> <wheel>, the last
#     three as <artifact directory>/<file>, the wheel "-" when LINUX_WHEELS=0 leaves it out. The
#     archive is the default (MIT) flavour's, which is the only one the packages carry.
#   expected-files.sh x264-source
#     x264's source files from third_party/x264, and the vcpkg port tarball made from it.
set -euo pipefail
targets=(x86_64-linux:tar.gz aarch64-linux:tar.gz aarch64-macos:tar.gz x86_64-windows:zip)
# target, @starfold/server-<suffix>, the wheel's platform tag. The macOS tag carries the binary's
# LC_BUILD_VERSION minos, which python-platform-wheel.py reads: a new minos fails the release
# check until it is written here.
packages=(
	"x86_64-linux linux-x64-gnu manylinux_2_39_x86_64"
	"aarch64-linux linux-arm64-gnu manylinux_2_39_aarch64"
	"aarch64-macos darwin-arm64 macosx_11_0_arm64"
	"x86_64-windows win32-x64 win_amd64"
)
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
case ${LINUX_WHEELS:-1} in 0 | 1) ;; *) fail "LINUX_WHEELS is 0 or 1, not $LINUX_WHEELS" ;; esac
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
native() {
	local line target suffix tag name wheel
	for line in "${packages[@]}"; do
		read -r target suffix tag <<<"$line"
		name=$(archive "$1" starfold "$target")
		wheel=starfold-wheels-$1/starfold-$1-py3-none-$tag.whl
		case $target in *-linux) [ "${LINUX_WHEELS:-1}" = 1 ] || wheel=- ;; esac
		echo "$target $suffix starfold-$1-$target/$name starfold-npm-$1/starfold-server-$suffix-$1.tgz $wheel"
	done
}
case ${1:?usage: expected-files.sh <version> <flavour...> | archive <version> <flavour> <target> | native <version> | x264-source} in
archive)
	[ $# = 4 ] || fail "usage: expected-files.sh archive <version> <flavour> <target>"
	archive "$2" "$3" "$4"
	;;
native)
	[ $# = 2 ] || fail "usage: expected-files.sh native <version>"
	native "$2"
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
	while read -r _ _ _ tgz wheel; do
		echo "$tgz"
		echo "$tgz.sha256"
		if [ "$wheel" != - ]; then
			echo "$wheel"
			echo "$wheel.sha256"
		fi
	done < <(native "$version")
	echo "starfold-npm-$version/starfold-$version.tgz"
	echo "starfold-npm-$version/starfold-$version.tgz.sha256"
	for f in "${x264_source[@]}"; do
		echo "x264-source/$f"
		echo "x264-source/$f.sha256"
	done
	;;
esac
