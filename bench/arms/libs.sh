#!/usr/bin/env bash
# Native libraries for the software encoder arms, for a throwaway VM only: builds the pinned
# releases into $ARM_PREFIX (default ~/arm-libs). Then: source <(libs.sh --env).
# Usage: libs.sh <libvpx|libaom|svt-av1|kvazaar|x265>...
set -euo pipefail
prefix=${ARM_PREFIX:-$HOME/arm-libs}
if [[ ${1:-} == --env ]]; then
	echo "export PKG_CONFIG_PATH=$prefix/lib/pkgconfig:$prefix/lib/x86_64-linux-gnu/pkgconfig:\${PKG_CONFIG_PATH:-}"
	echo "export LD_LIBRARY_PATH=$prefix/lib:$prefix/lib/x86_64-linux-gnu:\${LD_LIBRARY_PATH:-}"
	exit 0
fi
export DEBIAN_FRONTEND=noninteractive
sudo -n apt-get update -qq >/dev/null
sudo -n apt-get install -y -qq build-essential cmake nasm yasm pkg-config libclang-dev >/dev/null
src=$(mktemp -d)
trap 'rm -rf "$src"' EXIT
jobs=$(nproc)
# Each release is pinned to the commit of its tag at the project's own repository, so a mirror
# on a host the VM's proxy allows gives exactly the same tree (libaom: m-ab-s/aom on GitHub).
fetch() {
	git clone -q --depth 1 -b "$2" "$1" "$src/$4"
	[[ $(git -C "$src/$4" rev-parse HEAD) == "$3" ]] || { echo "$4 $2 is not commit $3" >&2; exit 1; }
}
cmake_install() {
	local dir=$1
	shift
	cmake -S "$dir" -B "$dir/out" -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON -DCMAKE_INSTALL_PREFIX="$prefix" -DCMAKE_INSTALL_LIBDIR=lib "$@" >/dev/null
	cmake --build "$dir/out" -j "$jobs" >/dev/null
	cmake --install "$dir/out" >/dev/null
}
for lib in "$@"; do
	case $lib in
		libvpx)
			fetch https://github.com/webmproject/libvpx.git v1.17.0 6df3ec34557879fff673706f4a1d9fbd0f3a6f0e libvpx
			(cd "$src/libvpx" && ./configure --prefix="$prefix" --enable-shared --disable-static --enable-pic --disable-examples --disable-tools --disable-docs --disable-unit-tests >/dev/null && make -j "$jobs" >/dev/null && make install >/dev/null)
			;;
		libaom)
			fetch https://github.com/m-ab-s/aom.git v3.15.3 b141d9c45db236335351c8a4b2a14b3ade27c937 aom
			cmake_install "$src/aom" -DCONFIG_REALTIME_ONLY=1 -DENABLE_TESTS=0 -DENABLE_DOCS=0 -DENABLE_EXAMPLES=0 -DENABLE_TOOLS=0
			;;
		svt-av1)
			fetch https://github.com/AOMediaCodec/SVT-AV1.git v4.3.0 e3d63405f9ba8219913a7276c2c47d10e5d022fe svt
			cmake_install "$src/svt" -DBUILD_APPS=OFF -DBUILD_TESTING=OFF
			;;
		kvazaar)
			fetch https://github.com/ultravideo/kvazaar.git v2.3.2 6040962bed5cc68c5ad01234c38c08b8b2822068 kvazaar
			cmake_install "$src/kvazaar" -DBUILD_TESTS=OFF
			;;
		x265)
			# Ubuntu 24.04's 3.5: 4.2's only source is a host the VM's proxy refuses.
			sudo -n apt-get install -y -qq libx265-dev >/dev/null
			;;
		*)
			echo "unknown library $lib" >&2
			exit 2
			;;
	esac
	echo "built $lib"
done
