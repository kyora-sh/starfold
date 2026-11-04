#!/bin/bash
# Rebuilds dav1d.wasm: dav1d (BSD-2-Clause) plus shim.c, single-threaded, 8-bit, for
# wasm32-wasip1 with wasi-sdk. Needs curl, meson and ninja on x86_64 Linux.
# Usage: packages/web/dav1d/build.sh [workdir]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=${1:-$(mktemp -d)}
mkdir -p "$work"
cd "$work"

DAV1D=1.5.4
DAV1D_SHA256=a1d5b63d2d38ec9bd03acf643caa51fa22edd1e89c5a109c4807717216bbec07
WASI_SDK=34
WASI_SDK_SHA256=b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4

fetch() {
	[ -f "$1" ] || curl -fsSL --retry 5 --retry-all-errors -o "$1" "$2"
	echo "$3  $1" | sha256sum -c - >/dev/null
}
fetch dav1d.tar.gz "https://github.com/videolan/dav1d/archive/refs/tags/$DAV1D.tar.gz" "$DAV1D_SHA256"
fetch wasi-sdk.tar.gz "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-$WASI_SDK/wasi-sdk-$WASI_SDK.0-x86_64-linux.tar.gz" "$WASI_SDK_SHA256"
rm -rf dav1d sdk build
mkdir dav1d sdk
tar -xzf dav1d.tar.gz -C dav1d --strip-components=1
tar -xzf wasi-sdk.tar.gz -C sdk --strip-components=1
sdk=$work/sdk
sed -i -e "s/thread_dependency = dependency('threads')/thread_dependency = declare_dependency()/" \
	-e "s/if cc.has_function('clock_gettime', prefix : '#include <time.h>', args : test_args)/if true/" dav1d/meson.build

flags="'--target=wasm32-wasip1', '--sysroot=$sdk/share/wasi-sysroot', '-msimd128'"
cat >cross.txt <<EOF
[binaries]
c = '$sdk/bin/clang'
ar = '$sdk/bin/llvm-ar'
strip = '$sdk/bin/llvm-strip'

[built-in options]
c_args = [$flags]
c_link_args = [$flags]

[host_machine]
system = 'wasi'
cpu_family = 'wasm32'
cpu = 'wasm32'
endian = 'little'
EOF
meson setup build dav1d --cross-file=cross.txt --default-library=static --buildtype=release \
	-Db_ndebug=true -Dbitdepths=8 -Denable_asm=false -Denable_tools=false -Denable_tests=false \
	-Denable_examples=false -Dlogging=false
ninja -C build src/libdav1d.a

"$sdk/bin/clang" --target=wasm32-wasip1 --sysroot="$sdk/share/wasi-sysroot" -mexec-model=reactor \
	-O3 -msimd128 -I dav1d/include -I build/include -I build/include/dav1d \
	"$here/shim.c" build/src/libdav1d.a -o dav1d.wasm -Wl,--strip-all -Wl,--gc-sections
cp dav1d.wasm "$here/dav1d.wasm"
cp dav1d/COPYING "$here/COPYING-dav1d"
sha256sum "$here/dav1d.wasm"
