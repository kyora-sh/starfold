#!/bin/bash
# measure.sh: release binary size and build time with and without libaom, on this VM.
set -u
cd /work
o=/work/.av1-out; mkdir -p "$o"
s() { local n=$1; shift; local t=$(date +%s); bash -o pipefail -c "$*" >"$o/$n.log" 2>&1; echo "step $n exit=$? $(( $(date +%s) - t ))s"; }
s submodule 'git submodule update --init --force --depth=1'
s apt 'sudo timeout -k 10 180 env DEBIAN_FRONTEND=noninteractive apt-get -q update && sudo timeout -k 10 720 env DEBIAN_FRONTEND=noninteractive apt-get -q install -y libx264-dev=2:0.164.3108+git31e19f9-1 libclang-dev libyuv-dev libopus-dev libpulse-dev libxcb1-dev esbuild cmake nasm perl'
s toolchain 'rustup toolchain install 1.93.0 --profile minimal'
export RUSTUP_TOOLCHAIN=1.93.0
echo "cpus $(nproc)"
s fetch 'cargo fetch --locked'
s release-default 'cargo build --release --locked'
cp target/release/starfold /tmp/sf-default
s release-libaom 'cargo build --release --locked --features libaom'
cp target/release/starfold /tmp/sf-libaom
s aom-alone 'rm -rf target/release/build/starfold-aom-sys-* && cargo build --release --locked -p starfold-aom-sys'
for b in default libaom; do
  f=/tmp/sf-$b; cp "$f" "$f.s"; strip "$f.s"
  echo "size $b $(stat -c %s "$f") stripped $(stat -c %s "$f.s")"
done
ldd /tmp/sf-libaom | grep -i aom || echo "no shared libaom"
/tmp/sf-libaom --version | head -20
echo measure-done
